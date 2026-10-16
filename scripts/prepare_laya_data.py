#!/usr/bin/env python3
"""校验人工标签和场景分组，导出 Laya 原生 JSONL；不训练、不修改运行模型。"""
import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from sidecar.laya_server import to_laya_questions

DEFAULT = ROOT / 'models/training/project-review-v2.json'
SPLITS = ('train', 'validation', 'calibration', 'test')


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, allow_nan=False)


def validate_answers(answers, questions):
    if not isinstance(answers, dict) or set(answers) != set(questions):
        raise ValueError('答案必须完整覆盖问题，不得添加未知问题')
    for qid, q in questions.items():
        value = answers[qid]
        if q['type'] == 'noul':
            valid = type(value) is bool
        elif q['type'] == 'choice':
            valid = isinstance(value, str) and value in q['criteria']
        else:
            valid = isinstance(value, str) and value in q['criteria']
        if not valid:
            raise ValueError(f'{qid}: 标签不属于该问题的封闭词汇表')


def validate(document):
    if document.get('schema_version') != 2 or not document.get('samples'):
        raise ValueError('需要非空的 v2 样例文件')
    canonical(document)
    ids, groups, inputs = set(), {}, {}
    for sample in document['samples']:
        sid, group, split = sample['id'], sample['group_id'], sample['split']
        if not isinstance(sid, str) or not sid or sid in ids:
            raise ValueError('样例 ID 为空或重复')
        ids.add(sid)
        if not isinstance(group, str) or not group or split not in SPLITS:
            raise ValueError(f'{sid}: 非法场景组或数据划分')
        if groups.setdefault(group, split) != split:
            raise ValueError(f'{sid}: 同一场景组不能跨数据划分')
        request = sample['request']
        if set(request) != {'state', 'questions'} or not isinstance(request['state'], dict) or not request['state']:
            raise ValueError(f'{sid}: request 必须只有非空 state 和 questions')
        questions = to_laya_questions(request['questions'])
        validate_answers(sample['proposed_answers'], questions)
        status = sample['review_status']
        if status not in ('pending', 'approved', 'rejected'):
            raise ValueError(f'{sid}: 未知审核状态')
        if status == 'approved':
            validate_answers(sample['reviewed_answers'], questions)
        elif sample.get('reviewed_answers') is not None:
            raise ValueError(f'{sid}: 未审核样例不能带已审核答案')
        signature = canonical(request)
        if signature in inputs:
            raise ValueError(f'{sid}: 与 {inputs[signature]} 重复输入，不能作为独立样例')
        inputs[signature] = sid
    return document['samples']


def row(sample, draft=False):
    # 训练输入沿用 Rust 的键排序；审核理由与目标标签绝不能拼进 state。
    request = json.loads(canonical(sample['request']))
    if not draft and sample['review_status'] != 'approved':
        raise ValueError(f'{sample["id"]}: 尚未人工审核，不导出训练数据')
    return {'state': request['state'], 'questions': to_laya_questions(request['questions']),
            'expected': sample['proposed_answers'] if draft else sample['reviewed_answers']}


def describe(samples):
    return {'samples': len(samples), 'decisions': sum(len(s['request']['questions']) for s in samples),
            'review': dict(Counter(s['review_status'] for s in samples)),
            'areas': dict(Counter(s['area'] for s in samples)),
            'splits': dict(Counter(s['split'] for s in samples))}


def training_coverage(samples):
    counts,labels = {},{}
    for sample in samples:
        if sample['review_status']=='rejected':
            continue
        answers = sample['reviewed_answers'] if sample['review_status']=='approved' else sample['proposed_answers']
        for question in sample['request']['questions']:
            qid,kind = question['id'],question['kind']
            if qid=='task_decision':
                continue  # opt0 等仅代表本题选项位置，不能当作跨场景类别。
            domain = {'false','true'} if kind['type']=='noul' else set(kind.get('criteria',kind.get('levels',[])))
            labels.setdefault(qid,set()).update(domain)
            counts.setdefault(qid,Counter())
            if sample['split']=='train':
                answer=answers[qid]
                counts[qid][str(answer).lower() if type(answer) is bool else answer]+=1
    return {qid:{'train_counts':dict(counts[qid]),'missing_train_labels':sorted(domain-set(counts[qid]))}
            for qid,domain in labels.items()}


def export(document, output):
    samples = validate(document)
    if any(s['review_status'] == 'pending' for s in samples):
        raise ValueError('仍有待审核标签；请先完成审核，不会使用 proposed_answers 训练')
    accepted = [s for s in samples if s['review_status'] == 'approved']
    if set(s['split'] for s in accepted) != set(SPLITS):
        raise ValueError('四个数据划分都必须含已审核样例')
    if output.exists():
        raise ValueError('输出目录已存在；使用新的版本目录，避免覆盖数据')
    output.parent.mkdir(parents=True, exist_ok=True)
    manifest = {'source_sha256': hashlib.sha256(canonical(document).encode()).hexdigest(),
                'summary': describe(samples), 'files': {}, 'samples': [
                    {k:s[k] for k in ('id','group_id','split','review_status')} for s in samples]}
    with tempfile.TemporaryDirectory(dir=output.parent, prefix='.laya-export-') as tmp:
        staged = Path(tmp) / 'dataset'
        staged.mkdir()
        for split in SPLITS:
            content = ''.join(canonical(row(s))+'\n' for s in accepted if s['split'] == split)
            (staged / f'{split}.jsonl').write_text(content, encoding='utf-8')
            manifest['files'][f'{split}.jsonl'] = hashlib.sha256(content.encode()).hexdigest()
        (staged / 'manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
        staged.rename(output)
    return manifest['summary']


def preflight(document, model):
    """仅用 tokenizer 检查候选格式；临时 JSONL 不作为训练导出物。"""
    from laya.train import TrainConfig, dry_run
    samples = validate(document)
    if not (model / 'rl_agent_config.json').is_file():
        raise ValueError('只能使用现有本地 checkpoint，不能隐式下载')
    with tempfile.TemporaryDirectory(prefix='yunxi-laya-preflight-') as tmp:
        path = Path(tmp) / 'draft.jsonl'
        path.write_text(''.join(canonical(row(s, draft=True))+'\n' for s in samples), encoding='utf-8')
        result = dry_run(str(path), str(model), TrainConfig(calib_frac=0, calib_max=0,
                        micro_batch=1, grad_accum=8, loss='soft-ce'))
    if result['skipped'] or result['valid_items'] != describe(samples)['decisions']:
        raise ValueError(f'上游编码器丢弃样例：{result["skipped"]}')
    return {k:result[k] for k in ('rows_read','valid_items','skipped','max_len','head_max_len')}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('check','export','preflight'))
    parser.add_argument('--data', type=Path, default=DEFAULT)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--model', type=Path, default=ROOT/'data/models/laya-multilingual')
    args = parser.parse_args()
    document = json.loads(args.data.read_text(encoding='utf-8'))
    if args.command == 'export':
        if args.output is None:
            parser.error('export 需要 --output 新目录')
        result = export(document, args.output)
    elif args.command == 'preflight':
        result = preflight(document, args.model)
    else:
        samples = validate(document)
        result = dict(describe(samples),training_coverage=training_coverage(samples))
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError) as exc:
        sys.exit(f'数据校验失败：{exc}')
