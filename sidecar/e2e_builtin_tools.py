#!/usr/bin/env python3
"""逐项验收真实 DeepSeek → 内置工具 → 结果回传；只操作临时合成资料。"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid
from urllib.parse import urlparse

from e2e_common import read_ledger, ensure_fresh_binary, real_home_dir, snapshot_home

TOOLS = ('now', 'read_file', 'list_dir', 'search_files', 'write_file', 'edit_file',
         'run_command', 'ask_user', 'knowledge_search', 'web_fetch', 'web_search')
ROOT = Path(__file__).resolve().parents[1]


def verify(tool, records, stdout, work, marker, history=()):
    """核对真实工具返回及独立产物，不能只相信助手最后一句话。"""
    calls = [r for r in records if r.get('tool') == tool]
    good = [r for r in calls if r.get('executed') and r.get('outcome') == 'ok']
    text = '\n'.join(r.get('output', '') for r in good)
    checks = {'called': bool(calls), 'succeeded': bool(good),
              'no_tool_errors': all(r.get('outcome') == 'ok' for r in records)}
    if tool == 'now':
        checks['evidence'] = 'Unix 时间戳' in text and '日期时间' in text
    elif tool in ('read_file', 'search_files', 'knowledge_search'):
        checks['evidence'] = marker in text and marker in stdout
    elif tool == 'list_dir':
        checks['evidence'] = '输入 中文.txt' in text and 'notes.txt' in text
    elif tool in ('write_file', 'edit_file', 'run_command'):
        name = {'write_file': '新建/结果.txt', 'edit_file': 'notes.txt',
                'run_command': 'command-proof.txt'}[tool]
        path = work / name
        expected = f'首行\n{marker}\n末行\n' if tool == 'edit_file' else marker
        checks['evidence'] = path.is_file() and path.read_text() == expected
        if tool == 'run_command':
            checks['stdout_returned'] = marker in text and marker in stdout
    elif tool == 'ask_user':
        # D143：提问只交回会话循环，用户回答必须在后续 user 消息中，不能伪造为工具结果。
        checks['evidence'] = False
        if len(good) == 1 and not good[0].get('output_clipped'):
            try:
                pending = json.loads(good[0]['output'])
            except (ValueError, TypeError):
                pending = {}
            if not isinstance(pending, dict):
                pending = {}
            if pending.get('waiting_for_user') is True and pending.get('options') == ['白鹭', '青竹']:
                for i, message in enumerate(history):
                    if message.get('role') == 'tool' and message.get('content') == good[0]['output']:
                        later = history[i + 1:]
                        checks['evidence'] = any(
                            answer.get('role') == 'user' and answer.get('content', '').splitlines()[-1:] == ['2']
                            and reply.get('role') == 'assistant' and not reply.get('tool_calls')
                            and '青竹' in reply.get('content', '') and '白鹭' not in reply.get('content', '')
                            for answer, reply in zip(later, later[1:]))
    elif tool == 'web_fetch':
        checks['evidence'] = 'Example Domain' in text and 'example.com' in text
    elif tool == 'web_search':
        links = [line.strip() for line in text.splitlines() if line.strip().startswith(('https://', 'http://'))]
        checks['evidence'] = any(
            (urlparse(url).hostname == 'rust-lang.org' or (urlparse(url).hostname or '').endswith('.rust-lang.org'))
            and url in stdout for url in links)
    else:
        raise ValueError(f'未定义验收判据：{tool}')
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rounds', type=int, default=2)
    parser.add_argument('--only', nargs='+', choices=TOOLS)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.rounds <= 5:
        parser.error('--rounds 必须为 1 至 5')
    if args.output.exists():
        parser.error('输出已存在，请换路径保留之前的失败证据')
    selected = args.only or list(TOOLS)
    binary = ensure_fresh_binary()
    real_home = real_home_dir()
    before = snapshot_home(real_home)
    report = {'selected': selected, 'rounds': args.rounds, 'cases': [], 'checks': {}}
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))

    with tempfile.TemporaryDirectory(prefix='yunxi-builtins-') as tmp:
        root = Path(tmp).resolve()
        for round_no in range(1, args.rounds + 1):
            for tool in selected:
                label = f'{round_no}-{tool}'
                folder = root / label
                home, work = folder / 'home', folder / 'work'
                home.mkdir(parents=True)
                work.mkdir()
                (work / '.git').mkdir()
                (home / 'models').symlink_to(real_home / 'models', target_is_directory=True)
                secret_names = ['deepseek.key'] + (['bocha.key'] if tool == 'web_search' else [])
                for name in secret_names:
                    source = real_home / 'secrets' / name
                    if source.is_file():
                        (home / 'secrets').mkdir(mode=0o700, exist_ok=True)
                        key = home / 'secrets' / name
                        key.touch(mode=0o600)
                        key.write_bytes(source.read_bytes())
                marker = '验收' + uuid.uuid4().hex[:12]
                (work / '输入 中文.txt').write_text(f'独立验收标记：{marker}\n')
                (work / 'notes.txt').write_text('首行\n旧内容\n末行\n')
                env = dict(os.environ, YUNXI_BOT_HOME=str(home))
                prompts = {
                    'now': '调用 now 获取当前时间，并转述它返回的日期和 Unix 时间戳。',
                    'read_file': '调用 read_file 读取「输入 中文.txt」，原样转述其中的验收标记。',
                    'list_dir': '调用 list_dir 查看当前目录，列出文件名。',
                    'search_files': '调用 search_files 搜索当前目录里含「独立验收标记」的行，原样转述标记。',
                    'write_file': f'调用 write_file 新建「新建/结果.txt」，完整内容为「{marker}」，不要添加换行。',
                    'edit_file': f'先 read_file 读取 notes.txt，再调用 edit_file 将「旧内容」替换成「{marker}」，保留其他文字和换行。',
                    'run_command': '调用 run_command，command 参数为 ' + json.dumps([
                        str(ROOT / '.venv/bin/python'), '-c',
                        f'from pathlib import Path; Path("command-proof.txt").write_text("{marker}"); print("{marker}")'
                    ], ensure_ascii=False) + '，转述命令的真实输出。',
                    'ask_user': '调用 ask_user 问我选择哪个代号，options 必须依次是「白鹭」「青竹」。获得回答后原样转述，不再追问。',
                    'knowledge_search': '调用 knowledge_search 查询星桥项目的验收口令，并原样转述口令和来源编号。',
                    'web_fetch': '调用 web_fetch 获取 https://example.com 的正文，返回它的标题和网页用途。',
                    'web_search': '调用 web_search 搜索「Rust programming language official website」，最多3条，转述真实返回的官网链接。',
                }
                case = {'case': label, 'tool': tool, 'checks': {}}
                if tool == 'knowledge_search':
                    source = folder / '星桥.txt'
                    source.write_text(f'星桥项目的验收口令为 {marker}。')
                    imported = subprocess.run([str(binary), 'knowledge', 'add', str(source)],
                        cwd=work, env=env, capture_output=True, text=True, timeout=60)
                    case['checks']['knowledge_imported'] = imported.returncode == 0
                allowed = {tool, 'read_file'} if tool == 'edit_file' else {tool}
                argv = [str(binary), 'chat', '--id', label, '--thinking', 'off']
                from e2e_agenda import NAMES
                for name in NAMES:
                    argv += ['--allow' if name in allowed else '--deny', name]
                prompt = '请执行这个工具验收任务，必须实际调用指定工具；失败就如实报告，不能换其他工具：' + prompts[tool]
                feed = prompt + '\n' + ('2\n' if tool == 'ask_user' else '') + '/exit\n'
                print(f'START {label}', flush=True)
                started = time.monotonic()
                try:
                    result = subprocess.run(argv, input=feed, cwd=work, env=env, capture_output=True,
                                            text=True, timeout=240)
                    case.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
                    case['checks']['completed'] = result.returncode == 0 and not any(
                        s in result.stderr for s in ('这一轮失败', '这一轮未执行'))
                except subprocess.TimeoutExpired:
                    case.update(returncode='timeout', stdout='', stderr='240 秒超时')
                    case['checks']['completed'] = False
                ledger = home / 'ledger.jsonl'
                events = read_ledger(ledger)
                records = [e['data'] for e in events if e.get('kind') == 'tool_called']
                models = [e['data'] for e in events if e.get('kind') == 'model_called']
                case['tools'] = records
                session = home / 'sessions' / f'{label}.json'
                history = json.loads(session.read_text())['layout']['history'] if session.exists() else []
                if tool == 'ask_user':
                    case['question_history'] = history
                case['checks'].update(verify(tool, records, case['stdout'], work, marker, history))
                case['checks']['deepseek_used'] = bool(models) and all(m['provider'] == 'deepseek' for m in models)
                case['checks']['only_expected_tools'] = all(r['tool'] in allowed for r in records)
                with socket.socket() as sock:
                    sock.settimeout(.3)
                    case['checks']['no_local_generator'] = sock.connect_ex(('127.0.0.1', 17872)) != 0
                case['seconds'] = round(time.monotonic() - started, 3)
                case['passed'] = all(case['checks'].values())
                report['cases'].append(case)
                save()
                print(f'END {label}: {case["passed"]}; {case["seconds"]}s; failed=' +
                      ','.join(k for k,v in case['checks'].items() if not v), flush=True)

    # 报告是唯一有意写入真实 home 的文件；其他真实资料只比元数据，不读取正文。
    after = snapshot_home(real_home)
    try:
        report_key = str(args.output.resolve().relative_to(real_home.resolve()))
    except ValueError:
        report_key = None
    before.pop(report_key, None)
    after.pop(report_key, None)
    report['checks']['real_home_untouched'] = before == after
    report['checks']['all_selected_attempted'] = len(report['cases']) == args.rounds * len(selected)
    report['checks']['all_cases_passed'] = all(c['passed'] for c in report['cases'])
    save()
    print('SUMMARY', report['checks'], flush=True)
    return 0 if all(report['checks'].values()) else 1


if __name__ == '__main__':
    raise SystemExit(main())
