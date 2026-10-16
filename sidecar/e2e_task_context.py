#!/usr/bin/env python3
"""真实 DeepSeek + EmbeddingGemma 任务上下文验收；仅使用临时合成资料。"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

from e2e_common import read_ledger, ensure_fresh_binary

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / 'data/benchmarks/embeddings/task-context-deepseek.json'
GOAL = ('单步骤任务：按画像称呼我，并说出你自己的名字；你记得我们之前讨论的星桥项目验收日期吗？'
        '必须调用 knowledge_search 查询星桥项目验收口令，并在回答中列出日期、口令和知识片段编号。'
        '只回答这些信息，不写文件，不联网，不执行命令。')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--max-steps', type=int, default=4)
    parser.add_argument('--output', type=Path, default=OUTPUT)
    args = parser.parse_args()
    if not 1 <= args.max_steps <= 12:
        parser.error('--max-steps 必须在1至12之间')
    output = args.output
    binary = ensure_fresh_binary()
    report = {'settings': {'max_steps': args.max_steps, 'goal': GOAL}, 'checks': {}, 'commands': []}
    output.parent.mkdir(parents=True, exist_ok=True)

    def check(name, value):
        report['checks'][name] = bool(value)
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
        print(f'{name}: {bool(value)}', flush=True)

    with tempfile.TemporaryDirectory(prefix='yunxi-task-context-') as temp:
        root = Path(temp).resolve()
        home, work, other = root / 'home', root / 'work', root / 'other'
        for path in (home, work, other):
            path.mkdir()
        (work / '.git').mkdir()
        key = ROOT / 'data/secrets/deepseek.key'
        if key.is_file():
            (home / 'secrets').mkdir(mode=0o700)
            target = home / 'secrets/deepseek.key'
            target.touch(mode=0o600)
            target.write_bytes(key.read_bytes())
        (other / '.git').mkdir()
        (home / 'models').symlink_to(ROOT / 'data/models', target_is_directory=True)
        (home / 'persona.md').write_text('# 星禾\n你是简洁可靠的中文助理。')
        (home / 'profile.md').write_text('使用者的称呼：松林。')
        (work / 'AGENTS.md').write_text('测试项目约定：回答不使用感叹号。')
        source = root / '星桥验收.txt'
        source.write_text('星桥项目的验收口令为「松月4826」。核对资料时要保留知识片段来源编号。')
        common = ['--provider', 'deepseek', '--budget', '6', '--max-steps', str(args.max_steps),
                  '--allow', 'knowledge_search', '--deny', 'write_file', '--deny', 'edit_file',
                  '--deny', 'run_command', '--deny', 'web_fetch', '--deny', 'web_search']

        def run(argv, cwd=work, endpoint=None):
            env = dict(os.environ, YUNXI_BOT_HOME=str(home))
            if endpoint:
                env['YUNXI_BOT_EMBEDDING_ENDPOINT'] = endpoint
            else:
                env.pop('YUNXI_BOT_EMBEDDING_ENDPOINT', None)
            start = time.monotonic()
            stdout, stderr = root / 'stdout.log', root / 'stderr.log'
            with stdout.open('w') as out, stderr.open('w') as err:
                try:
                    p = subprocess.run([str(binary), *argv], cwd=cwd, env=env, input='',
                                       stdout=out, stderr=err, text=True, timeout=300)
                except subprocess.TimeoutExpired:
                    report['commands'].append(dict(argv=argv, code='timeout', stdout=stdout.read_text(), stderr=stderr.read_text()))
                    output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
                    raise
            p.stdout, p.stderr = stdout.read_text(), stderr.read_text()
            report['commands'].append(dict(argv=argv, code=p.returncode,
                seconds=round(time.monotonic()-start, 3), stdout=p.stdout, stderr=p.stderr))
            output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
            return p

        def events():
            return read_ledger(home / 'ledger.jsonl')

        def payloads(kind, all_events=None):
            return [e['data'] for e in (events() if all_events is None else all_events) if e.get('kind') == kind]

        def context_rows():
            return [json.loads(d['reason']) for d in payloads('decision_decided') if d['action'] == 'context_loaded']

        check('import', run(['knowledge', 'add', str(source)]).returncode == 0)
        p = run(['knowledge', 'search', '星桥项目验收口令是什么'])
        check('real_encoder_auto_start_and_search', p.returncode == 0 and '松月4826' in p.stdout)
        if p.returncode:
            return 1
        index = home / 'vectors/knowledge.json'
        stamp = index.stat().st_mtime_ns
        p = run(['knowledge', 'search', '星桥项目验收口令是什么'])
        check('restart_reuses_persisted_vectors', p.returncode == 0 and index.stat().st_mtime_ns == stamp)
        run(['remember', '我们之前讨论过：星桥项目的验收日期是2026年11月8日。', '--kind', 'event'])
        run(['remember', '星桥项目在本工作区验收时保留片段编号。', '--kind', 'workspace'])
        run(['remember', '另一个项目的隐私标记 OTHER_SCOPE_SECRET。', '--kind', 'workspace'], cwd=other)
        run(['remember', '使用者偏好简洁、直接的答复。', '--kind', 'preference'])

        print('START real context task', flush=True)
        p = run(['do', GOAL, '--id', 'context-ok', *common])
        results = '\n'.join(d.get('result', '') for d in payloads('step_succeeded'))
        check('task_completed', p.returncode == 0)
        check('answer_uses_persona_profile_memory_knowledge', all(s in results for s in ('星禾', '松林', '11', '8', '松月4826')))
        check('knowledge_tool_really_executed', any(d['tool'] == 'knowledge_search' and d['executed'] and '松月4826' in d['output'] for d in payloads('tool_called')))
        rows = [d for d in context_rows() if d['task_id'] == 'context-ok']
        check('planning_and_step_context_audited', {d['phase'] for d in rows} == {'Plan', 'Step'} and all(d['profile'] and d['resident'] and d['rules'] and d['memory'] and d['knowledge'] for d in rows))
        check('other_workspace_filtered_before_embedding', 'OTHER_SCOPE_SECRET' not in (home / 'vectors/memory.json').read_text())
        model_calls = payloads('model_called')
        check('model_calls_use_deepseek', bool(model_calls) and all(d['provider'] == 'deepseek' for d in model_calls))

        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            unavailable = f'http://127.0.0.1:{sock.getsockname()[1]}/embed'
            before = len(events())
            p = run(['do', GOAL, '--id', 'context-resume', *common], endpoint=unavailable)
            failed = events()[before:]
            check('encoder_failure_stops_before_generation', p.returncode == 3 and not payloads('model_called', failed) and not payloads('tool_called', failed))
            check('encoder_failure_is_audited', any(d['action'] == 'context_unavailable' and d['degraded'] for d in payloads('decision_decided', failed)))
        (home / 'profile.md').write_text('使用者的称呼：雪杉。')
        print('START recovered task', flush=True)
        before = len(events())
        p = run(['resume', 'context-resume', *common])
        results = '\n'.join(d.get('result', '') for d in payloads('step_succeeded', events()[before:]))
        check('recovery_replans_and_uses_updated_profile', p.returncode == 0 and '雪杉' in results and '松月4826' in results)

        old_id = payloads('memory_recorded')[0]['id']
        check('memory_vectors_exist_before_forget', (home / 'vectors/memory.json').exists())
        check('forget_clears_memory_vectors', run(['memory', '--forget', old_id]).returncode == 0 and not (home / 'vectors/memory.json').exists())
        index.write_text('corrupt index')
        before = len(events())
        p = run(['do', GOAL, '--id', 'bad-cache', *common])
        failed = events()[before:]
        check('corrupt_index_stops_before_generation', p.returncode == 3 and not payloads('model_called', failed))
        check('reindex_recovers_corrupt_cache', run(['knowledge', 'reindex']).returncode == 0 and run(['knowledge', 'search', '星桥项目验收口令']).returncode == 0)

        doc_id = json.loads(run(['knowledge', 'search', '星桥项目验收口令']).stdout)[0]['passage']['id'].split('#')[0]
        source.write_text('星桥项目的最新验收口令为「冬雨7193」。')
        run(['knowledge', 'add', str(source)])
        p = run(['knowledge', 'search', '星桥项目验收口令'])
        check('document_update_invalidates_old_vector', '冬雨7193' in p.stdout and '松月4826' not in index.read_text())
        p = run(['knowledge', 'remove', doc_id])
        check('remove_copy_preserves_source_and_prunes_cache', p.returncode == 0 and source.exists() and not index.exists() and run(['knowledge', 'search', '星桥项目验收口令']).stdout.strip() == '[]')
        check('ledger_reload', run(['tasks', 'context-ok']).returncode == 0)
        report['events'] = events()
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if all(report['checks'].values()) else 1


if __name__ == '__main__':
    raise SystemExit(main())
