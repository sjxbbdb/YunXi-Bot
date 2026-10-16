#!/usr/bin/env python3
"""真实 DeepSeek + Laya 任务验收；仅操作临时文件，保留原始台账和断言结果。"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

from e2e_common import ensure_fresh_binary

ROOT = Path(__file__).resolve().parents[1]
GOAL = (
    '请完成文件筛选任务，严格拆成4步并依次依赖：'
    's1 用 read_file 读取当前目录的 输入.txt 并给出原文；'
    's2 必须为 decide: 步骤，依据前一步规则在保留红色或保留蓝色之间选择；'
    's3 依据决策，用 write_file 创建 结果.txt，只写保留条目的 名称=数量，'
    '每行一个，末行 合计=数量总和；'
    's4 用 read_file 重新读取 结果.txt，核对筛选和数量是否正确。'
    '不要使用命令、联网或改动其他文件。'
)
INPUT = '规则：只保留红色条目。\n红色 苹果 3\n蓝色 水杯 2\n红色 草莓 4\n'
EXPECTED = '苹果=3\n草莓=4\n合计=7'


def events_at(home):
    path = home / 'ledger.jsonl'
    if not path.exists():
        return []
    # 坏行必须使验收失败，不能跳过它继续声称台账完整。
    return [json.loads(line) for line in path.read_text().splitlines()]


def payloads(events, kind):
    return [e['data'] for e in events if e.get('kind') == kind]


def decision_checks():
    """只测决策模型：交换选项顺序，核对它是否依原始证据选择。"""
    cases = []
    samples = [
        ('规则：只保留红色条目。', ['保留红色条目', '保留蓝色条目'], '保留红色条目'),
        ('规则：只保留蓝色条目。', ['保留红色条目', '保留蓝色条目'], '保留蓝色条目'),
        ('业务规则：订单总额大于等于100元才减20元。当前订单总额恰好100元。',
         ['本单减20元', '本单不减免'], '本单减20元'),
    ]
    for evidence, options, expected in samples:
        for ordered in [options, options[::-1]]:
            criteria = {f'opt{i}': option for i, option in enumerate(ordered)}
            question = '根据原始证据，应该选择哪个处理方式？'
            request = {'state': {'question': question, 'options': list(criteria), 'criteria': criteria,
                       'context': {'goal': '严格按规则处理', 'inputs': [['s1', evidence]]}},
                       'questions': [{'id': 'task_decision', 'kind': {'type': 'choice', 'criteria': criteria},
                                      'instructions': question}]}
            body = json.dumps(request, ensure_ascii=False).encode()
            with urllib.request.urlopen(urllib.request.Request('http://127.0.0.1:17870/decide', body,
                    {'Content-Type': 'application/json'}), timeout=10) as response:
                result = json.load(response)
            chosen = criteria.get(result['answers']['task_decision'].get('choice'))
            cases.append(dict(request=request, response=result, expected=expected, chosen=chosen, passed=chosen == expected))
    return dict(case='decision_evidence_matrix', checks={'six_evidence_choices_correct': all(c['passed'] for c in cases)}, samples=cases)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'data/benchmarks/task-chain/acceptance.json')
    parser.add_argument('--case', action='append', choices=['happy', 'decision_unavailable_resume', 'tool_denied', 'tool_missing_file', 'tool_gate_laya'])
    args = parser.parse_args()
    binary = ensure_fresh_binary()
    for port, model in [(17870, 'laya-multilingual')]:
        with urllib.request.urlopen(f'http://127.0.0.1:{port}/health', timeout=3) as response:
            health = json.load(response)
        assert health['model'] == model and health['status'] in ('ok', 'idle'), health

    reports = []
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(reports, ensure_ascii=False, indent=2))

    def run(home, cwd, argv):
        start = time.monotonic()
        result = subprocess.run(
            [str(binary), *argv], cwd=cwd,
            env=dict(os.environ, YUNXI_BOT_HOME=str(home)), input='',
            capture_output=True, text=True, timeout=300,
        )
        return dict(argv=argv, returncode=result.returncode, seconds=round(time.monotonic() - start, 3),
                    stdout=result.stdout, stderr=result.stderr)

    with tempfile.TemporaryDirectory(prefix='yunxi-local-task-') as tmp:
        base = Path(tmp).resolve()
        # 用未监听但仍占用的端口模拟失联，避免误连别人的服务；不停止真实 Laya。
        with socket.socket() as unavailable:
            unavailable.bind(('127.0.0.1', 0))
            bad_endpoint = f'http://127.0.0.1:{unavailable.getsockname()[1]}/decide'
            for name in ['happy', 'decision_unavailable_resume', 'tool_denied', 'tool_missing_file', 'tool_gate_laya']:
                if args.case and name not in args.case:
                    continue
                print(f'START {name}', flush=True)
                directory = base / name
                work = directory / 'work'; work.mkdir(parents=True)
                home = directory / 'state'; home.mkdir()
                key = ROOT / 'data/secrets/deepseek.key'
                if key.is_file():
                    (home / 'secrets').mkdir(mode=0o700)
                    target = home / 'secrets/deepseek.key'
                    target.touch(mode=0o600)
                    target.write_bytes(key.read_bytes())
                (work / '输入.txt').write_text(INPUT)
                output = work / '结果.txt'
                common = ['--provider', 'deepseek', '--budget', '12', '--deny', 'run_command',
                          '--deny', 'web_fetch', '--deny', 'web_search', '--deny', 'edit_file']
                if name in ('happy', 'decision_unavailable_resume'):
                    goal = GOAL
                    common += ['--max-steps', '4', '--allow', f'write_file:{output}']
                else:
                    common += ['--max-steps', '1', '--deny', 'write_file']
                    goal = {
                        'tool_denied': '单步骤任务：使用 write_file 新建 结果.txt，内容为 测试写入；不要改用其他工具绕过写入失败。',
                        'tool_missing_file': '单步骤任务：使用 read_file 读取当前目录的 不存在.txt 并报告其中内容，文件不存在时不要编造或创建。',
                        'tool_gate_laya': f'单步骤任务：使用 read_file 读取 {directory / "外部只读样例.txt"} 并报告里面的暗号，不做其他操作。',
                    }[name]
                    (directory / '外部只读样例.txt').write_text('暗号：春雨2468')
                extra = ['--endpoint', bad_endpoint] if name == 'decision_unavailable_resume' else []
                commands = [run(home, work, ['do', goal, '--id', name, *common, *extra])]
                initial = events_at(home)
                checks = {}
                if name == 'decision_unavailable_resume':
                    checks['unavailable_pauses_without_write'] = commands[0]['returncode'] == 3 and not output.exists()
                    checks['degradation_recorded'] = any(d.get('degraded') and d.get('model') is None for d in payloads(initial, 'decision_decided'))
                    # --answer 是测试人员给出的固定业务答案；不是模型替人批准工具。
                    commands.append(run(home, work, ['resume', name, '--answer', '仅保留红色条目', *common]))
                events = events_at(home)
                tools = payloads(events, 'tool_called')
                models = payloads(events, 'model_called')
                decisions = payloads(events, 'decision_decided')
                checks['only_deepseek_generation'] = bool(models) and all(m['provider'] == 'deepseek' and m['model'] == 'deepseek-flash' for m in models)
                checks['no_forbidden_tools_executed'] = not any(t['executed'] and t['tool'] in ['run_command', 'web_fetch', 'web_search', 'edit_file'] for t in tools)
                checks['planning_thinks'] = bool(models) and models[0]['thinking'] is True
                checks['input_unchanged'] = (work / '输入.txt').read_text() == INPUT
                if name in ('happy', 'decision_unavailable_resume'):
                    checks['completed_with_correct_file'] = commands[-1]['returncode'] == 0 and output.exists() and output.read_text().strip() == EXPECTED
                    checks['write_and_readback_executed'] = any(t['tool'] == 'write_file' and t['executed'] for t in tools) and sum(t['tool'] == 'read_file' and t['executed'] for t in tools) >= 2
                    if name == 'happy':
                        checks['laya_chose_red'] = any(d.get('model') == 'laya-multilingual' and not d['degraded'] and '红色' in d['action'] for d in decisions)
                        checks['no_false_health_warning'] = '本地决策模型连不上' not in commands[0]['stdout']
                    else:
                        checks['human_answer_recorded'] = bool(payloads(events, 'human_answered'))
                elif name == 'tool_denied':
                    checks['denied_without_write'] = not output.exists() and any(t['tool'] == 'write_file' and t['decision'] == 'deny' and not t['executed'] for t in tools)
                    checks['not_reported_success'] = commands[0]['returncode'] != 0 and bool(payloads(events, 'step_failed'))
                elif name == 'tool_missing_file':
                    checks['read_error_recorded'] = any(t['tool'] == 'read_file' and t['outcome'] == 'error' for t in tools)
                    checks['not_reported_success'] = commands[0]['returncode'] != 0 and bool(payloads(events, 'step_failed'))
                    checks['missing_file_not_fabricated'] = not (work / '不存在.txt').exists()
                else:
                    checks['laya_tool_gate_called'] = any('needs_human' in d.get('questions', []) for d in payloads(events, 'decision_asked')) and any(d.get('model') == 'laya-multilingual' and not d['degraded'] for d in decisions)
                    checks['gate_result_respected'] = any(t['tool'] == 'read_file' and ((t['executed'] and '春雨2468' in t['output']) or (not t['executed'] and t['outcome'] == 'not_run')) for t in tools)
                replay = run(home, work, ['tasks', name])
                checks['ledger_reloads'] = replay['returncode'] == 0
                reports.append(dict(case=name, checks=checks, commands=commands, replay=replay, events=events,
                                    files={p.name: p.read_text() for p in work.glob('*.txt')}))
                save()
                print(json.dumps(dict(case=name, seconds=sum(c['seconds'] for c in commands), checks=checks), ensure_ascii=False), flush=True)
    if not args.case:
        reports.append(decision_checks())
        save()
        print(json.dumps({k: v for k, v in reports[-1].items() if k != 'samples'}, ensure_ascii=False), flush=True)
    return 0 if all(all(r['checks'].values()) for r in reports) else 1


if __name__ == '__main__':
    raise SystemExit(main())
