#!/usr/bin/env python3
"""用已审核、按场景组划分的数据训练一个候选；不更换生产服务。"""
import argparse
from dataclasses import asdict
from datetime import datetime, timezone
import hashlib
from importlib.metadata import version
import json
import math
import os
from pathlib import Path
import resource
import time

from prepare_laya_data import DEFAULT, ROOT, SPLITS, canonical, row, validate, training_coverage


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024*1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def load_reviewed_splits(dataset, source):
    document = json.loads(source.read_text())
    samples = validate(document)
    if any(s['review_status'] == 'pending' for s in samples):
        raise ValueError('存在未审核标签，拒绝训练')
    manifest = json.loads((dataset/'manifest.json').read_text())
    if manifest['source_sha256'] != hashlib.sha256(canonical(document).encode()).hexdigest():
        raise ValueError('审核文件与导出数据的哈希不符')
    splits = {}
    for split in SPLITS:
        path = dataset/f'{split}.jsonl'
        if sha256(path) != manifest['files'][path.name]:
            raise ValueError(f'{split}: 导出数据被修改')
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        expected = [row(s) for s in samples if s['split']==split and s['review_status']=='approved']
        if not rows or rows != expected:
            raise ValueError(f'{split}: 导出内容与审核记录不符')
        splits[split] = rows
    return splits, manifest


def validation_rank(metrics):
    if not metrics['items'] or not all(math.isfinite(metrics[k]) for k in ('accuracy','loss')):
        raise ValueError('验证指标为空或非有限值')
    return metrics['accuracy'], -metrics['loss']


def save_candidate(model,tok,cfg,path):
    from laya.train import save_checkpoint
    from safetensors.torch import save_file
    save_checkpoint(model,tok,cfg,str(path))
    # 上游默认存 fp16，边界预测会在导出后翻转；候选先保持训练的 fp32 精度。
    save_file({k:v.detach().cpu().contiguous() for k,v in model.state_dict().items()},
              str(path/'model.safetensors'))


def finish_evaluation(measure, calibrate, selection_only):
    if selection_only:
        fitted={'temperature':[1.,1.,1.],'temperature_by_options':{}}
        calibration={'status':'deferred_until_selection_complete'}
        splits=('train','validation')
    else:
        fitted,calibration=calibrate()
        splits=SPLITS
    metrics={split:measure(split,fitted['temperature'],fitted['temperature_by_options'])
             for split in splits}
    return fitted,calibration,metrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data', type=Path, default=ROOT/'data/training/laya-project-v2')
    parser.add_argument('--review', type=Path, default=DEFAULT)
    parser.add_argument('--base', type=Path, default=ROOT/'data/models/laya-multilingual')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--epochs', type=int, default=8)
    parser.add_argument('--seed', type=int, default=20261015)
    parser.add_argument('--loss', choices=('rlcd','soft-ce'), default='soft-ce')
    parser.add_argument('--selection-only', action='store_true',
                        help='方法比较阶段只评估 train/validation，不拟合校准、不评估 test')
    parser.add_argument('--freeze-encoder', action='store_true')
    args = parser.parse_args()
    splits, manifest = load_reviewed_splits(args.data,args.review)
    if args.output.exists() or args.output.resolve().is_relative_to(args.base.resolve()):
        parser.error('必须使用独立且尚不存在的候选目录')
    if args.epochs < 1:
        parser.error('epochs 必须大于0')
    entry = next(e for e in json.loads((ROOT/'models/manifest.json').read_text())['models']
                 if e['name']=='laya-multilingual')
    for name, digest in entry['sha256_files'].items():
        if sha256(args.base/name) != digest:
            raise ValueError(f'基础模型校验失败：{name}')
    os.environ['HF_HUB_OFFLINE'] = '1'
    os.environ['TRANSFORMERS_OFFLINE'] = '1'
    os.environ['PYTORCH_ENABLE_MPS_FALLBACK'] = '0'
    import torch
    from laya.calibrate import fit_temperature_map
    from laya.train import (TrainConfig, load_checkpoint, items_from_rows, train_model,
                            calibration_records, evaluate_records, calibration_report,
                            optimizer_updates)
    from safetensors.torch import load_file
    if not torch.backends.mps.is_available():
        raise RuntimeError('本轮要求 MPS，不能静默改用 CPU')
    torch.set_num_threads(4)
    torch.manual_seed(args.seed)
    # 只限制本训练进程的 MPS 分配器，不声称限制整机或 CPU 内存。
    torch.mps.set_per_process_memory_fraction(min(1,12*1024**3/torch.mps.recommended_max_memory()))
    device = torch.device('mps')
    config = TrainConfig(epochs=args.epochs,micro_batch=1,grad_accum=8,
                         encoder_lr=1e-5,head_lr=5e-5,loss=args.loss,
                         shuffle_options=('choice',),calib_max=0,calib_frac=0,
                         freeze_encoder=args.freeze_encoder,
                         amp=False,gradient_checkpointing=True,seed=args.seed,log_every=8)
    model,tok,cfg = load_checkpoint(str(args.base))
    max_len,head_max_len = cfg['max_len'],cfg['head_max_len']
    items = {}
    for split,rows in splits.items():
        if args.selection_only and split not in ('train','validation'):
            continue
        encoded,skipped = items_from_rows(tok,rows,max_len,head_max_len)
        if skipped or len(encoded)!=sum(len(r['expected']) for r in rows):
            raise ValueError(f'{split}: 编码丢弃了样例：{skipped}')
        items[split] = encoded
    args.output.mkdir(parents=True)
    checkpoint = args.output/'checkpoint'
    report = {'status':'training','created_at':datetime.now(timezone.utc).isoformat(),
              'base_revision':entry['revision'],'base_weight_sha256':entry['sha256_files']['model.safetensors'],
              'data_manifest':manifest,'laya_version':version('laya'),'torch_version':torch.__version__,
              'training_coverage':training_coverage(json.loads(args.review.read_text())['samples']),
              'config':asdict(config),'device':'mps','mps_allocator_limit_gib':12,
              'items':{k:len(v) for k,v in items.items()},
              'optimizer_updates':optimizer_updates(len(items['train']),1,8,args.epochs),
              'epochs':[],'selected_epoch':None,'calibrated':False,
              'selection_only':args.selection_only}
    start = time.monotonic()
    def save_report():
        tmp = args.output/'training-report.tmp'
        tmp.write_text(json.dumps(report,ensure_ascii=False,indent=2,allow_nan=False)+'\n')
        tmp.replace(args.output/'training-report.json')
    def measure(split,temperature=None,buckets=None):
        records = calibration_records(model,tok,items[split],device,max_len,head_max_len,batch_size=1)
        return evaluate_records(records,temperature,buckets)
    model.to(device)
    report['base_validation'] = measure('validation',cfg.get('temperature'),cfg.get('temperature_by_options'))
    save_report()
    best = None
    def on_epoch(epoch,loss):
        nonlocal best
        if not math.isfinite(loss):
            raise ValueError('训练损失不是有限值')
        validation = measure('validation')
        rank = validation_rank(validation)
        if best is None or rank > best:
            best = rank
            report['selected_epoch'] = epoch+1
            save_candidate(model,tok,cfg,checkpoint)
        report['epochs'].append({'epoch':epoch+1,'train_loss':loss,'validation':validation,
                                'elapsed_seconds':round(time.monotonic()-start,2),
                                'mps_driver_gib':round(torch.mps.driver_allocated_memory()/1024**3,3)})
        print(json.dumps({'epoch':epoch+1,'train_loss':round(loss,4),
                          'validation_accuracy':validation['accuracy'],
                          'selected_epoch':report['selected_epoch'],
                          'mps_driver_gib':report['epochs'][-1]['mps_driver_gib']},ensure_ascii=False),flush=True)
        save_report()
        model.train()
        if config.freeze_encoder:
            model.encoder.eval()
    try:
        train_model(model,tok,items['train'],config,device,max_len,head_max_len,on_epoch_end=on_epoch)
        # 只依据 validation 选择权重；选择完成后才读取 calibration/test 的预测。
        model.load_state_dict(load_file(str(checkpoint/'model.safetensors')),strict=True)
        def calibrate():
            records = calibration_records(model,tok,items['calibration'],device,max_len,head_max_len,batch_size=1)
            fitted = fit_temperature_map(records)
            return fitted,calibration_report(records,fitted['temperature'])
        fitted,report['calibration'],report['final'] = finish_evaluation(measure,calibrate,args.selection_only)
        report['temperature'] = fitted['temperature']
        out_cfg = dict(cfg,fine_tuned=True,temperature=fitted['temperature'],
                       temperature_by_options=fitted['temperature_by_options'])
        # 不继承基础 checkpoint 的历史阈值或训练次数，避免误报为本轮校准证据。
        out_cfg['training'] = {'yunxi_project':{'source_sha256':manifest['source_sha256'],
                              'config':asdict(config),'selected_epoch':report['selected_epoch'],
                              'selection_only':args.selection_only,
                              'calibrated':False,'calibration':report['calibration']}}
        save_candidate(model,tok,out_cfg,checkpoint)
        report['checkpoint_precision'] = 'float32'
        report['checkpoint_sha256'] = sha256(checkpoint/'model.safetensors')
        report['status'] = 'completed_candidate_only'
    except Exception as exc:
        report['status'] = 'failed'
        report['error_type'] = type(exc).__name__
        raise
    finally:
        report['elapsed_seconds'] = round(time.monotonic()-start,2)
        report['peak_process_rss_gib'] = round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/1024**3,3)
        save_report()
    print(json.dumps({k:report[k] for k in ('status','selected_epoch','elapsed_seconds','peak_process_rss_gib','final')},ensure_ascii=False),flush=True)


if __name__=='__main__':
    main()
