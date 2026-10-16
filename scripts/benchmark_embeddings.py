#!/usr/bin/env python3
"""固定题库、固定CPU设置；先校验全部下载，再用独立子进程串行测候选。"""
import argparse
import hashlib
import json
import math
import os
import platform
import importlib.metadata
from pathlib import Path
import resource
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from sidecar.embedding_backend import Encoder, MODELS
from scripts.fetch_model import load_manifest, model_ready
SUITE = ROOT / 'models/eval/embedding-retrieval-v1.json'


def validate_suite(suite):
    docs, queries = suite['documents'], suite['queries']
    ids = {d['id'] for d in docs}
    assert len(ids) == len(docs) and docs
    assert len({q['id'] for q in queries}) == len(queries) and queries
    for q in queries:
        assert q['split'] in ('calibration', 'evaluation') and set(q['relevant']) <= ids
        assert all(d['scope'] in (None, q['scope']) for d in docs if d['id'] in q['relevant'])
    assert all(d['text'].strip() for d in docs)
    assert all(q['query'].strip() for q in queries)
    assert {bool(q['relevant']) for q in queries if q['split'] == 'calibration'} == {True, False}
    assert {bool(q['relevant']) for q in queries if q['split'] == 'evaluation'} == {True, False}


def cosine(a, b):
    if len(a) != len(b) or not all(math.isfinite(x) for x in [*a, *b]):
        raise ValueError('invalid vectors')
    return sum(x*y for x,y in zip(a,b))


def ranking(query, vector, docs, vectors, scoped=True):
    rows = [(d['id'], cosine(vector, v)) for d,v in zip(docs,vectors)
            if not scoped or d['scope'] is None or d['scope'] == query['scope']]
    return sorted(rows, key=lambda x:(-x[1],x[0]))


def choose_threshold(rows):
    calibration = [r for r in rows if r['split'] == 'calibration']
    scores = sorted({r['top'][0][1] for r in calibration})
    candidates = [scores[0]-1e-6, scores[-1]+1e-6]+[(a+b)/2 for a,b in zip(scores,scores[1:])]
    def value(t):
        positives = [r for r in calibration if r['relevant']]
        negatives = [r for r in calibration if not r['relevant']]
        return (sum(r['top'][0][1]>=t for r in positives)/len(positives)
                +sum(r['top'][0][1]<t for r in negatives)/len(negatives))/2
    # 同分时更保守，不根据正式测试集挑阈值。
    return max(candidates, key=lambda t:(value(t),t))


def summarize(rows, threshold):
    evaluated = [r for r in rows if r['split']=='evaluation']
    positives = [r for r in evaluated if r['relevant']]
    negatives = [r for r in evaluated if not r['relevant']]
    metrics = dict(positive_queries=len(positives), no_answer_queries=len(negatives),
        recall_at_1=sum(r['rank']==1 for r in positives)/len(positives),
        recall_at_3=sum(r['rank']<=3 for r in positives)/len(positives),
        mrr=sum(1/r['rank'] for r in positives)/len(positives),
        no_answer_rejection=sum(r['top'][0][1]<threshold for r in negatives)/len(negatives),
        positive_acceptance=sum(r['top'][0][1]>=threshold for r in positives)/len(positives),
        correct_after_threshold=sum(r['rank']==1 and r['top'][0][1]>=threshold for r in positives)/len(positives))
    metrics['categories']={}
    for category in sorted({r['category'] for r in positives}):
        group=[r for r in positives if r['category']==category]
        metrics['categories'][category]=dict(n=len(group), recall_at_1=sum(r['rank']==1 for r in group)/len(group),
            recall_at_3=sum(r['rank']<=3 for r in group)/len(group))
    return metrics


def worker(args):
    suite=json.loads(args.suite.read_text());validate_suite(suite)
    started=time.perf_counter()
    encoder=Encoder(args.worker,args.home/'models'/args.worker)
    load_s=time.perf_counter()-started
    # 一条固定且不在题库中的热身输入，不计入查询延迟。
    encoder.encode(['模型热身，不参与评估。'],'query')
    vectors=[];start=time.perf_counter()
    for i in range(0,len(suite['documents']),8):
        vectors.extend(encoder.encode([d['text'] for d in suite['documents'][i:i+8]],'passage'))
        print(f'{args.worker} index {min(i+8,len(suite["documents"]))}/{len(suite["documents"])}',flush=True)
    index_s=time.perf_counter()-start
    rows=[];latencies=[]
    for i,q in enumerate(suite['queries']):
        start=time.perf_counter();v=encoder.encode([q['query']],'query')[0];latencies.append(time.perf_counter()-start)
        ranked=ranking(q,v,suite['documents'],vectors)
        rank=next((i+1 for i,(id,_) in enumerate(ranked) if id in q['relevant']),None)
        row=dict(q,rank=rank,top=ranked[:5],seconds=latencies[-1])
        if q['scope']:
            row['unfiltered_top']=ranking(q,v,suite['documents'],vectors,False)[:5]
        rows.append(row)
        if (i+1)%8==0: print(f'{args.worker} queries {i+1}/{len(suite["queries"])}',flush=True)
    threshold=choose_threshold(rows)
    norms=[math.sqrt(sum(x*x for x in v)) for v in vectors]
    assert all(abs(n-1)<1e-4 for n in norms)
    report=dict(model=args.worker,model_id=encoder.model_id,device='cpu',threads=4,dtype='float32',
        environment=dict(python=platform.python_version(),platform=platform.platform(),
            torch=importlib.metadata.version('torch'),transformers=importlib.metadata.version('transformers')),
        dimension=encoder.spec['dimension'],loaded_parameters=encoder.parameters,
        suite_sha256=hashlib.sha256(args.suite.read_bytes()).hexdigest(),
        load_seconds=load_s,index_seconds=index_s,index_documents_per_second=len(vectors)/index_s,
        query_median_ms=statistics.median(latencies)*1000,
        query_p95_ms=sorted(latencies)[math.ceil(len(latencies)*.95)-1]*1000,
        peak_process_rss_gib=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss/(1024**3 if sys.platform=='darwin' else 1024**2),
        threshold=threshold,metrics=summarize(rows,threshold),cases=rows)
    args.output.write_text(json.dumps(report,ensure_ascii=False,indent=2,allow_nan=False)+'\n')
    print(json.dumps({k:v for k,v in report.items() if k!='cases'},ensure_ascii=False),flush=True)


def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('--home',type=Path,default=ROOT/'data')
    ap.add_argument('--suite',type=Path,default=SUITE)
    ap.add_argument('--output',type=Path,default=ROOT/'data/benchmarks/embeddings')
    ap.add_argument('--worker',choices=list(MODELS))
    args=ap.parse_args()
    if args.worker:
        worker(args);return
    suite=json.loads(args.suite.read_text());validate_suite(suite)
    entries={e['name']:e for e in load_manifest()['models']}
    # 两个都完整才允许开始，防止中途变更比较条件。
    for name in MODELS:
        e=entries[name]
        if not model_ready(args.home/'models'/name,e['required_files'],e['sha256_files']):
            raise SystemExit(f'{name} 尚未完整下载或校验不符；不启动任何候选测试')
    args.output.mkdir(parents=True,exist_ok=True)
    env=dict(os.environ,HF_HUB_OFFLINE='1',TRANSFORMERS_OFFLINE='1',TOKENIZERS_PARALLELISM='false',OMP_NUM_THREADS='4')
    for name in MODELS:
        subprocess.run([sys.executable,__file__,'--worker',name,'--home',str(args.home),'--suite',str(args.suite),
            '--output',str(args.output/(name+'.json'))],env=env,check=True)
        # 子进程退出即释放权重，再开始下一个模型。
    print('串行测试完成。报告目录：',args.output)


if __name__=='__main__':
    main()
