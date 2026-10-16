#!/usr/bin/env python3
"""真实 DeepSeek 的聊天、摘要、画像提炼与陪伴表达；决策用固定测试替身。"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

from e2e_common import ensure_fresh_binary, REPO
import mock_laya


def credentials(home):
    secret = home / 'secrets/deepseek.key'
    secret.parent.mkdir(parents=True, mode=0o700)
    secret.touch(mode=0o600)
    secret.write_bytes((REPO / 'data/secrets/deepseek.key').read_bytes())


def main():
    binary = ensure_fresh_binary()
    model = 'deepseek-flash'
    report = {'model': model, 'commands': [], 'checks': {}}
    output = REPO / 'data/benchmarks/task-chain/cloud-auxiliary.json'
    output.parent.mkdir(parents=True, exist_ok=True)
    mock_laya.SCRIPT.update(choice='speak', noul=0.99)
    server = ThreadingHTTPServer(('127.0.0.1', 0), mock_laya.Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-local-aux-') as tmp:
            base = Path(tmp)
            home = base / 'state'
            credentials(home)
            work = base / 'work'
            work.mkdir()
            env = {k: v for k, v in os.environ.items() if not k.endswith('_KEY')}
            # 旧环境变量即使残留，也不能改变本地客户端或成为其他服务的凭证。
            env.update(YUNXI_BOT_HOME=str(home), YUNXI_BOT_AGNES_KEY='retired-provider-test-only')

            def run(label, args, text='', state=home):
                start = time.monotonic()
                result = subprocess.run([str(binary), *args], cwd=work,
                    env=dict(env, YUNXI_BOT_HOME=str(state)), input=text,
                    text=True, capture_output=True, timeout=180)
                report['commands'].append(dict(case=label, returncode=result.returncode,
                    seconds=round(time.monotonic()-start, 2), stdout=result.stdout, stderr=result.stderr))
                print(f'{label}: exit={result.returncode}', flush=True)
                return result

            r = run('think', ['think', '只回答：连接正常', '--thinking', 'off'])
            report['checks']['think_uses_deepseek'] = r.returncode == 0 and model in r.stdout
            report['checks']['think_reports_requested_mode'] = '思考模式  : 关' in r.stdout
            r = run('reject_retired_provider', ['think', '你好', '--provider', 'agnes'])
            report['checks']['agnes_provider_rejected'] = r.returncode == 2
            r = run('no_credential_crosstalk', ['think', '你好', '--provider', 'deepseek'], state=base/'no-key')
            report['checks']['agnes_env_cannot_authenticate_deepseek'] = r.returncode == 2 and 'deepseek.key' in r.stderr
            run('chat', ['chat', '--id', 'aux', '--thinking', 'off'], '我叫松林，我喜欢徒步。请记住我的名字。\n/exit\n')
            session_path = home / 'sessions/aux.json'
            report['checks']['chat_saved'] = session_path.exists()
            r = run('profile_learn', ['profile', '--learn'])
            events = [json.loads(line) for line in (home / 'ledger.jsonl').read_text().splitlines()]
            proposal_ids = [e['data']['id'] for e in events if e.get('kind') == 'profile_proposed']
            report['checks']['profile_proposals_have_unique_ids'] = bool(proposal_ids) and len(proposal_ids) == len(set(proposal_ids))
            report['checks']['profile_proposals_pending'] = r.returncode == 0 and any(
                e.get('kind') == 'profile_proposed' and any(word in e['data'].get('text', '') for word in ['松林', '徒步']) for e in events)
            report['checks']['profile_not_silently_accepted'] = not (home / 'profile.md').exists()

            session = json.loads(session_path.read_text())
            for i in range(10):
                session['layout']['history'] += [
                    {'role': 'user', 'content': f'第{i}轮闲聊：我叫松林，喜欢徒步。周末的散步让我放松。'},
                    {'role': 'assistant', 'content': '记住了，你叫松林，喜欢徒步。'}]
            # 人工降低预算锚点的余量，确定触发真实 CLI 压缩路径。
            session['layout']['usage'] = {'anchor_tokens': 3000, 'delta_chars': 0}
            session_path.write_text(json.dumps(session, ensure_ascii=False))
            run('resume_and_summarize', ['chat', '--resume', '--id', 'aux', '--context-window', '2048', '--thinking', 'off'], '你还记得我叫什么名字吗？\n/exit\n')
            events = [json.loads(line) for line in (home / 'ledger.jsonl').read_text().splitlines()]
            report['checks']['model_summary_completed'] = any(e.get('kind') == 'context_compacted' and e['data'].get('summarized') for e in events)
            calls = [e['data'] for e in events if e.get('kind') == 'model_called']
            report['checks']['all_generation_used_deepseek'] = bool(calls) and all(c['provider'] == 'deepseek' for c in calls)

            agent_home = base / 'agent'
            credentials(agent_home)
            event = dict(seq=1, at=int(time.time()*1000), kind='job_failed', job='备份',
                         data={'error': '测试情景：备份目录不存在，请检查路径'})
            (agent_home / 'ledger.jsonl').write_text(json.dumps({'yunxi_bot_ledger': 1})+'\n'+json.dumps(event, ensure_ascii=False)+'\n')
            r = run('companion_expression', ['agent', '--hour', '12', '--endpoint',
                f'http://127.0.0.1:{server.server_port}/decide'], state=agent_home)
            report['checks']['companion_spoke_with_deepseek'] = r.returncode == 0 and model in r.stdout and '--- 它开口了 ---' in r.stdout
    finally:
        server.shutdown()
        server.server_close()
        output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps(report['checks'], ensure_ascii=False), flush=True)
    return 0 if all(report['checks'].values()) else 1


if __name__ == '__main__':
    raise SystemExit(main())
