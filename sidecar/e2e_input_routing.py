#!/usr/bin/env python3
"""Laya 输入分类 + DeepSeek 聊天与任务 + 本地仅辅助模型；仅用临时合成数据。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import socket
import tempfile
import time
import urllib.request

from e2e_common import ensure_fresh_binary

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / 'data/benchmarks/input-routing/acceptance.json'


def http(path, body=None, port=17872):
    data = None if body is None else json.dumps(body, ensure_ascii=False).encode()
    with urllib.request.urlopen(urllib.request.Request(f'http://127.0.0.1:{port}{path}', data,
            {'Content-Type': 'application/json'}), timeout=30) as response:
        return json.load(response)


def generation_stopped():
    with socket.socket() as sock:
        sock.settimeout(.3)
        return sock.connect_ex(('127.0.0.1', 17872)) != 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=17870, help='独立候选端口，默认使用生产决策服务')
    parser.add_argument('--output', type=Path, default=OUTPUT)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error('端口必须在1至65535之间')
    endpoint = f'http://127.0.0.1:{args.port}/decide'
    binary = ensure_fresh_binary()
    suite = json.loads((ROOT / 'models/eval/input-routing-v1.json').read_text())
    # 判据必须与生产逐字相同，防止只测了一份效果更好的测试提示词。
    source = (ROOT / 'crates/yunxi-bot-core/src/think/router.rs').read_text()
    assert all(json.dumps(text, ensure_ascii=False) in source for text in suite['criteria'].values())
    report = {'classification': [], 'commands': [], 'checks': {}}

    def save():
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))

    def check(name, passed):
        report['checks'][name] = bool(passed)
        print(name, bool(passed), flush=True)
        save()

    for sample in suite['samples']:
        request = {'state': {'input': sample['input']}, 'questions': [{'id': 'input_kind',
            'kind': {'type': 'choice', 'criteria': dict(sorted(suite['criteria'].items()))},
            'instructions': '这句话是要我办事，还是随口聊聊？'}]}
        response = http('/decide', request, args.port)
        chosen = response['answers']['input_kind']['choice']
        report['classification'].append(dict(sample, response=response, chosen=chosen,
                                              passed=chosen == sample['expected']))
    report['classification_score'] = {group: {'correct': sum(s['passed'] for s in report['classification'] if s.get('group') == group),
        'total': sum(s.get('group') == group for s in report['classification'])} for group in ['development', 'validation']}
    print('classification:', report['classification_score'], flush=True)
    save()

    with tempfile.TemporaryDirectory(prefix='yunxi-input-routing-') as tmp:
        root = Path(tmp)
        work = root / 'work'; work.mkdir()
        (work / '输入.txt').write_text('本轮验收暗号为：竹影七三九。\n')
        for name, text, mode in [
            ('task', '请调用 read_file 读取当前目录的输入.txt，告诉我里面的暗号。', 'task'),
            ('mixed', '我有点心烦，不过先帮我调用 read_file 读一下输入.txt，告诉我暗号。', 'task'),
            ('chat', '今天有点累，不想解决什么事情，只想有人陪我随便聊两句。', 'chat'),
            ('quoted', '“帮我删除文件”是我刚才看到的一句台词，我不是让你做，随便聊聊。', 'chat'),
        ]:
            assert generation_stopped(), '先关闭本地生成服务再验证'
            home = root / name; home.mkdir()
            (home / 'secrets').mkdir(mode=0o700)
            key = home / 'secrets/deepseek.key'; key.touch(mode=0o600)
            key.write_bytes((ROOT / 'data/secrets/deepseek.key').read_bytes())
            (home / 'persona.md').write_text('# 云熙\n你是温暖自然的中文聊天伙伴，认真倾听，简洁回应。')
            env = dict(os.environ, YUNXI_BOT_HOME=str(home))
            start = time.monotonic()
            result = subprocess.run([str(binary), 'chat', '--id', name, '--endpoint', endpoint, '--deny', 'write_file',
                '--deny', 'edit_file', '--deny', 'run_command', '--deny', 'web_fetch', '--deny', 'web_search'],
                input=text+'\n/exit\n', text=True, capture_output=True, cwd=work, env=env, timeout=240)
            stopped = generation_stopped()
            events = [json.loads(line) for line in (home/'ledger.jsonl').read_text().splitlines()]
            models = [e['data'] for e in events if e.get('kind') == 'model_called']
            tools = [e for e in events if e.get('kind') == 'tool_called']
            report['commands'].append(dict(case=name, seconds=round(time.monotonic()-start, 3),
                returncode=result.returncode, stdout=result.stdout, stderr=result.stderr,
                models=models, tools=tools, local_generation_stopped=stopped))
            check(name+'_provider', bool(models) and all(m['provider'] == 'deepseek' for m in models))
            check(name+'_completed', result.returncode == 0 and '这一轮失败' not in result.stderr and '这一轮未执行' not in result.stderr)
            check(name+'_no_local_generation', stopped)
            if mode == 'task':
                check(name+'_real_file_evidence', '竹影七三九' in result.stdout and bool(tools))
            else:
                check(name+'_no_tools', not tools)
                check(name+'_plain_chat', 'tool_call' not in result.stdout and '```json' not in result.stdout)
        # Same-session mode changes must preserve context and the per-turn tool boundary.
        result = subprocess.run([str(binary), 'chat', '--id', 'mixed-turns', '--endpoint', endpoint,
            '--deny', 'write_file', '--deny', 'run_command'],
            input='你好，我喜欢小猫，陪我聊两句。\n请用 read_file 读取输入.txt 里的暗号。\n谢谢，先不做任何事了，陪我聊聊小猫吧。\n/exit\n',
            text=True, capture_output=True, cwd=work, env=env, timeout=240)
        report['commands'].append(dict(case='mixed-turns', returncode=result.returncode, stdout=result.stdout, stderr=result.stderr))
        check('chat_task_chat_history', result.returncode == 0 and '竹影七三九' in result.stdout and
              result.stdout.count('[deepseek-flash·') == 3 and generation_stopped() and '这一轮失败' not in result.stderr)
        with socket.socket() as unavailable:
            unavailable.bind(('127.0.0.1', 0))
            bad = f'http://127.0.0.1:{unavailable.getsockname()[1]}/decide'
            r = subprocess.run([str(binary), 'chat', '--id', 'classify-failure', '--endpoint', bad],
                input='你好\n/exit\n', text=True, capture_output=True, cwd=work, env=env, timeout=30)
            check('classification_failure_waits_without_generation', '这一轮未执行' in r.stderr and generation_stopped())
        no_key = root / 'no-key'
        no_key_env = {k:v for k,v in env.items() if k != 'YUNXI_BOT_DEEPSEEK_KEY'}
        no_key_env['YUNXI_BOT_HOME'] = str(no_key)
        r = subprocess.run([str(binary), 'chat', '--id', 'no-key', '--endpoint', endpoint], input='你好\n/exit\n',
            text=True, capture_output=True, cwd=work, env=no_key_env, timeout=30)
        check('missing_cloud_key_fails_without_local_fallback', '这一轮失败' in r.stderr and 'deepseek.key' in r.stderr and generation_stopped())
    save()
    return 0 if all(report['checks'].values()) else 1


if __name__ == '__main__':
    raise SystemExit(main())
