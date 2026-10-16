#!/usr/bin/env python3
"""六组验证比较结束后，校准固定候选并终测；不修改生产权重。"""
import argparse
from datetime import datetime, timezone
import gc
import json
import os
from pathlib import Path

from compare_laya_training import LOSSES, SEEDS, summarize
from train_laya_project import ROOT, load_reviewed_splits, save_candidate, sha256


def selected_candidate(directory, manifest, base):
    reports = [json.loads((directory/f'{loss}-{seed}/training-report.json').read_text())
               for seed in SEEDS for loss in LOSSES]
    expected = summarize(reports)
    recorded = json.loads((directory/'comparison.json').read_text())
    if any(recorded.get(key) != value for key, value in expected.items()):
        raise ValueError('比较结果与六组验证指标不符，拒绝更换终测候选')
    report = next(r for r in reports if r['config']['loss'] == expected['selected_loss']
                  and r['config']['seed'] == expected['selected_seed'])
    checkpoint = directory/expected['selected_checkpoint']
    if report['data_manifest'] != manifest:
        raise ValueError('终测数据与六组训练使用的数据不符')
    if sha256(base/'model.safetensors') != report['base_weight_sha256']:
        raise ValueError('基础权重已变化')
    if sha256(checkpoint/'model.safetensors') != report['checkpoint_sha256']:
        raise ValueError('候选权重已变化')
    return checkpoint, expected


def metrics_by_area(records, rows, samples, temperature=None, buckets=None):
    from laya.train import evaluate_records
    if len(rows) != len(samples) or len(records) != sum(len(r['expected']) for r in rows):
        raise ValueError('终测记录数与标注数不符')
    areas, questions = {}, {}
    cursor = 0
    for row, sample in zip(rows, samples):
        for qid in row['questions']:
            record = records[cursor]
            areas.setdefault(sample['area'], []).append(record)
            questions.setdefault(qid, []).append(record)
            cursor += 1
    return {'all': evaluate_records(records, temperature, buckets),
            'areas': {key: evaluate_records(value, temperature, buckets) for key, value in areas.items()},
            'questions': {key: evaluate_records(value, temperature, buckets) for key, value in questions.items()}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--review', type=Path, required=True)
    parser.add_argument('--data', type=Path, required=True)
    parser.add_argument('--comparison', type=Path, required=True)
    parser.add_argument('--base', type=Path, default=ROOT/'data/models/laya-multilingual')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    splits, manifest = load_reviewed_splits(args.data, args.review)
    checkpoint, selection = selected_candidate(args.comparison, manifest, args.base)
    if args.output.exists():
        parser.error('输出目录必须尚不存在')
    os.environ.update(HF_HUB_OFFLINE='1', TRANSFORMERS_OFFLINE='1', PYTORCH_ENABLE_MPS_FALLBACK='0')
    import torch
    from laya.calibrate import fit_temperature_map
    from laya.train import load_checkpoint, items_from_rows, calibration_records, calibration_report
    if not torch.backends.mps.is_available():
        raise RuntimeError('本轮要求 MPS，不能静默改用 CPU')
    torch.set_num_threads(4)
    torch.mps.set_per_process_memory_fraction(min(1, 12*1024**3/torch.mps.recommended_max_memory()))
    device = torch.device('mps')
    samples = [s for s in json.loads(args.review.read_text())['samples']
               if s['split'] == 'test' and s['review_status'] == 'approved']
    report = {'status': 'evaluating', 'created_at': datetime.now(timezone.utc).isoformat(),
              'source_sha256': manifest['source_sha256'], 'selection': selection,
              'production_changed': False, 'calibrated': False, 'models': {}}
    args.output.mkdir(parents=True)
    # 顺序加载并释放；基线只读 test，只有已经锁定的候选能拟合 calibration。
    for name, path in [('base', args.base), ('candidate', checkpoint)]:
        model, tok, cfg = load_checkpoint(str(path))
        model.to(device)
        records = {}
        for split in (('test',) if name == 'base' else ('calibration', 'test')):
            items, skipped = items_from_rows(tok, splits[split], cfg['max_len'], cfg['head_max_len'])
            if skipped or len(items) != sum(len(r['expected']) for r in splits[split]):
                raise ValueError(f'{name}/{split}: 编码丢弃了样例：{skipped}')
            records[split] = calibration_records(model, tok, items, device,
                                                 cfg['max_len'], cfg['head_max_len'], batch_size=1)
        result = {'weight_sha256': sha256(path/'model.safetensors'),
                  'test_raw': metrics_by_area(records['test'], splits['test'], samples)}
        if name == 'candidate':
            fitted = fit_temperature_map(records['calibration'])
            result['calibration'] = calibration_report(records['calibration'], fitted['temperature'])
            result['fitted'] = fitted
            result['test_fitted'] = metrics_by_area(records['test'], splits['test'], samples,
                                                   fitted['temperature'], fitted['temperature_by_options'])
            out_cfg = dict(cfg, **fitted)
            out_cfg['training']['yunxi_project'].update(
                selection_only=False, calibrated=False, calibration=result['calibration'])
            save_candidate(model, tok, out_cfg, args.output/'checkpoint')
            result['export_weight_sha256'] = sha256(args.output/'checkpoint/model.safetensors')
            if result['export_weight_sha256'] != result['weight_sha256']:
                raise ValueError('校准导出改变了候选权重')
        else:
            result['test_serving_temperature'] = metrics_by_area(
                records['test'], splits['test'], samples, cfg.get('temperature'), cfg.get('temperature_by_options'))
        report['models'][name] = result
        del model, tok, items, records
        gc.collect()
        torch.mps.empty_cache()
    report['status'] = 'completed_candidate_only'
    with (args.output/'final-evaluation.json').open('x') as output:
        json.dump(report, output, ensure_ascii=False, indent=2, allow_nan=False)
    print(json.dumps({name: value['test_raw']['all'] for name, value in report['models'].items()},
                     ensure_ascii=False, indent=2, allow_nan=False))


if __name__ == '__main__':
    main()
