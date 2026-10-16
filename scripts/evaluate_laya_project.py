#!/usr/bin/env python3
"""通过生产 /decide 契约比较原始与反向选项顺序；草案只报告标签一致率。"""
import argparse
from collections import defaultdict
import copy
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import statistics
import time
import urllib.parse
import urllib.request

from prepare_laya_data import DEFAULT, SPLITS, canonical, validate


def reverse_options(request):
    request = copy.deepcopy(request)
    for question in request['questions']:
        if question['kind']['type'] == 'choice':
            question['kind']['criteria'] = dict(reversed(list(question['kind']['criteria'].items())))
    return request


def compare(answers, expected, questions):
    chosen = {}
    for q in questions:
        qid = q['id']
        answer = answers[qid]
        if q['kind']['type'] == 'noul':
            value = answer['noul']
            if type(value) not in (int, float) or not 0 <= value <= 1:
                raise ValueError('非法 noul 概率')
            chosen[qid] = value >= .5
        else:
            chosen[qid] = answer['choice']
            labels = q['kind'].get('criteria', q['kind'].get('levels'))
            if chosen[qid] not in labels:
                raise ValueError('未知选项')
    return chosen, sum(chosen[k] == value for k, value in expected.items())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--data', type=Path, default=DEFAULT)
    parser.add_argument('--endpoint', default='http://127.0.0.1:17870/decide')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--split', choices=SPLITS, help='只评估指定划分；默认仍评估全部样例')
    args = parser.parse_args()
    endpoint = urllib.parse.urlsplit(args.endpoint)
    if endpoint.scheme != 'http' or endpoint.hostname not in ('127.0.0.1','localhost','::1'):
        parser.error('只允许本机 HTTP 决策服务')
    if args.output.exists():
        parser.error('报告已存在，请指定新的报告路径')
    samples = [s for s in validate(json.loads(args.data.read_text())) if s['review_status'] != 'rejected'
               and (args.split is None or s['split'] == args.split)]
    if not samples:
        parser.error('没有可评估样例')
    draft = any(s['review_status'] != 'approved' for s in samples)
    report = {'created_at':datetime.now(timezone.utc).isoformat(),
              'dataset_sha256':hashlib.sha256(args.data.read_bytes()).hexdigest(),
              'label_status': 'draft_not_human_ground_truth' if draft else 'human_reviewed',
              'metric': 'agreement_with_draft' if draft else 'accuracy', 'cases': [], 'areas': {}}
    totals = defaultdict(lambda: {'correct':0,'decisions':0,'order_changed':0})
    split_totals = defaultdict(lambda: {'correct':0,'decisions':0,'order_changed':0})
    latencies = []
    for sample in samples:
        request = json.loads(canonical(sample['request']))
        expected = sample['reviewed_answers'] if sample['review_status']=='approved' else sample['proposed_answers']
        runs = []
        for variant in (request, reverse_options(request)):
            start = time.monotonic()
            req = urllib.request.Request(args.endpoint, json.dumps(variant, ensure_ascii=False).encode(),
                                         {'Content-Type':'application/json'})
            with urllib.request.urlopen(req, timeout=30) as response:
                result = json.load(response)
            elapsed = (time.monotonic()-start)*1000
            chosen, correct = compare(result['answers'], expected, variant['questions'])
            latencies.append(elapsed)
            runs.append({'chosen':chosen,'correct':correct,'milliseconds':round(elapsed,2),
                         'model':result['model'],'calibrated':result.get('calibrated',False)})
        changed = sum(runs[0]['chosen'][k] != runs[1]['chosen'][k] for k in expected)
        for bucket in (totals['all'],totals[sample['area']],split_totals[sample['split']]):
            bucket['correct'] += runs[0]['correct']
            bucket['decisions'] += len(expected)
            bucket['order_changed'] += changed
        report['cases'].append({'id':sample['id'],'area':sample['area'],'split':sample['split'],
                               'expected':expected,'runs':runs})
    report['areas'] = dict(totals)
    report['splits'] = dict(split_totals)
    report['latency_ms'] = {'median':round(statistics.median(latencies),2),'max':round(max(latencies),2)}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open('x', encoding='utf-8') as output:
        json.dump(report, output, ensure_ascii=False, indent=2)
    print(json.dumps({k:v for k,v in report.items() if k!='cases'}, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
