#!/usr/bin/env python3
"""在同一已审核数据上串行比较两种损失与三个种子；只用验证集选候选。"""
import argparse
import json
from pathlib import Path
import statistics
import subprocess
import sys

from train_laya_project import ROOT, load_reviewed_splits, validation_rank

SEEDS=(20261015,20261016,20261017)
LOSSES=('rlcd','soft-ce')


def summarize(reports):
    expected={(loss,seed) for loss in LOSSES for seed in SEEDS}
    found=set()
    contracts=set()
    grouped={loss:[] for loss in LOSSES}
    for report in reports:
        config=report['config']
        key=(config['loss'],config['seed'])
        if key not in expected or key in found:
            raise ValueError('比较矩阵有重复或未知运行')
        found.add(key)
        if (report['status']!='completed_candidate_only' or not report.get('selection_only')
                or set(report['final'])!={'train','validation'}):
            raise ValueError('只接受完成且未评估终测的候选')
        validation_rank(report['final']['validation'])
        contract={k:v for k,v in config.items() if k not in ('loss','seed')}
        contracts.add(json.dumps([contract,report['data_manifest']['source_sha256'],
            report['base_weight_sha256'],report['items'],report['optimizer_updates']],sort_keys=True))
        grouped[key[0]].append(report)
    if found!=expected or len(contracts)!=1:
        raise ValueError('六组运行必须完整，数据、底座和其他训练条件必须相同')
    metrics={loss:{'validation_accuracy_mean':statistics.mean(r['final']['validation']['accuracy'] for r in rows),
                  'validation_accuracy_stdev':statistics.stdev(r['final']['validation']['accuracy'] for r in rows),
                  'validation_loss_mean':statistics.mean(r['final']['validation']['loss'] for r in rows)}
             for loss,rows in grouped.items()}
    # 按预先约定的均值选择方法；稳定排序使完全相同的结果保留上游默认 RLCD。
    loss=max(LOSSES,key=lambda name:(metrics[name]['validation_accuracy_mean'],-metrics[name]['validation_loss_mean']))
    best=max(sorted(grouped[loss],key=lambda r:r['config']['seed']),key=lambda r:validation_rank(r['final']['validation']))
    return {'status':'validation_comparison_complete','methods':metrics,
            'selected_loss':loss,'selected_seed':best['config']['seed'],
            'selected_checkpoint':f'{loss}-{best["config"]["seed"]}/checkpoint',
            'production_changed':False,'test_evaluated':False,'calibration_fitted':False}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--review',type=Path,required=True)
    parser.add_argument('--data',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--epochs',type=int,default=8)
    args=parser.parse_args()
    load_reviewed_splits(args.data,args.review)
    if args.epochs<1 or args.output.exists():
        parser.error('epochs 必须为正整数，输出目录必须尚不存在')
    args.output.mkdir(parents=True)
    reports=[]
    for seed in SEEDS:
        for loss in LOSSES:
            name=f'{loss}-{seed}'
            print(f'串行训练 {name}',flush=True)
            command=[sys.executable,str(ROOT/'scripts/train_laya_project.py'),
                     '--review',str(args.review.resolve()),'--data',str(args.data.resolve()),
                     '--output',str((args.output/name).resolve()),'--loss',loss,
                     '--seed',str(seed),'--epochs',str(args.epochs),'--selection-only']
            with (args.output/f'{name}.log').open('x') as log:
                subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True,cwd=ROOT)
            reports.append(json.loads((args.output/name/'training-report.json').read_text()))
    result=summarize(reports)
    result['runs']=[f'{loss}-{seed}/training-report.json' for seed in SEEDS for loss in LOSSES]
    with (args.output/'comparison.json').open('x') as out:
        json.dump(result,out,ensure_ascii=False,indent=2,allow_nan=False)
    print(json.dumps(result,ensure_ascii=False,indent=2,allow_nan=False))


if __name__=='__main__':
    main()
