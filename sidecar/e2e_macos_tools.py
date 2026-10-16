#!/usr/bin/env python3
"""真实模型入口验收：权限、查询、通知，以及日历/待办写入生命周期。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

from e2e_common import read_ledger, ensure_fresh_binary, real_home_dir

ROOT = Path(__file__).resolve().parents[1]
LIFECYCLE = {'mac_calendar_create', 'mac_calendar_update', 'mac_calendar_delete',
             'mac_reminder_create', 'mac_reminder_update', 'mac_reminder_delete'}


def cleanup_created(records):
    """只清理由本轮原生工具实际创建的记录，模型中途退出也不留下合成待办。"""
    remaining = {}
    for record in records:
        if not record.get('executed') or record.get('outcome') != 'ok':
            continue
        output = json.loads(record['output'])
        if record['tool'].endswith('_create') and output.get('created'):
            item = output['item']
            remaining[item['id']] = (record['tool'].replace('_create', '_delete'), item['revision'])
        elif record['tool'].endswith('_update') and output['item']['id'] in remaining:
            item = output['item']
            remaining[item['id']] = (remaining[item['id']][0], item['revision'])
        elif record['tool'].endswith('_delete'):
            remaining.pop(output.get('deleted_id'), None)
    for identifier, (action, revision) in remaining.items():
        with tempfile.TemporaryDirectory(prefix='yunxi-mac-cleanup-') as tmp:
            base = Path(tmp)
            for name in ('in.json', 'out.json', 'stderr'):
                (base / name).touch(mode=0o600)
            (base / 'in.json').write_text(json.dumps({'action': action, 'args': {'id': identifier, 'expected_revision': revision}}))
            subprocess.run(['/usr/bin/open','-W','-n','-g','-a', str(ROOT / 'target/macos/云熙本机助理.app'),
                            '-i',str(base/'in.json'),'-o',str(base/'out.json'),'--stderr',str(base/'stderr')], check=True, timeout=35)
            response = json.loads((base/'out.json').read_text())
            if not response.get('ok') or not response['result'].get('verified_absent'):
                raise RuntimeError('合成记录清理失败，需按报告中的记录 ID 检查；未处理其他记录')
    return not remaining  # True 表示模型本身已完成清理。


def lifecycle_checks(records):
    checks = {'all_six_tools': {r.get('tool') for r in records} == LIFECYCLE,
              'all_succeeded': bool(records) and all(r.get('executed') and r.get('outcome') == 'ok' for r in records)}
    outputs = {r['tool']: json.loads(r['output']) for r in records if r.get('executed') and r.get('outcome') == 'ok'}
    for kind in ('calendar', 'reminder'):
        created = outputs.get(f'mac_{kind}_create', {}).get('item', {})
        updated = outputs.get(f'mac_{kind}_update', {}).get('item', {})
        deleted = outputs.get(f'mac_{kind}_delete', {})
        checks[kind + '_same_record'] = bool(created.get('id')) and created.get('id') == updated.get('id') == deleted.get('deleted_id')
        checks[kind + '_correct_store'] = created.get('account') == 'iCloud' and created.get('calendar') == '云熙'
        checks[kind + '_updated'] = updated.get('title') == '云熙工具验收：已调整' and (kind == 'calendar' or updated.get('completed') is True)
        checks[kind + '_deleted'] = deleted.get('verified_absent') is True
    return checks


def checks_for(records, expected):
    calls = [r for r in records if r.get('tool') == expected]
    checks = {'actual_call': bool(calls), 'only_expected': bool(records) and all(r.get('tool') == expected for r in records),
              'all_succeeded': bool(calls) and all(r.get('executed') and r.get('outcome') == 'ok' for r in calls)}
    outputs = [json.loads(r['output']) for r in calls if r.get('outcome') == 'ok']
    def valid_receipt(output):
        if expected == 'mac_status':
            return output.get('write_scope') == 'icloud_yunxi_only' and all(
                output.get(k) == 'full_access' for k in ('calendar', 'reminders'))
        if expected in ('mac_calendar_list', 'mac_reminders_list'):
            return (isinstance(output.get('items'), list) and output.get('truncated') is False
                    and output.get('read_scope') == 'mac_event_store_including_synced_accounts')
        return expected == 'mac_notify' and ((output.get('kind') == 'handed_off' and bool(output.get('note')))
            or (output.get('kind') == 'confirmed' and bool(output.get('evidence'))))
    checks['native_receipt'] = bool(outputs) and all(map(valid_receipt, outputs))
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--lifecycle-only', action='store_true', help='仅测真实模型创建、修改、删除合成日程和待办')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('请使用新的报告路径，保留失败记录')
    binary, real_home = ensure_fresh_binary(), real_home_dir()
    cases = []
    with tempfile.TemporaryDirectory(prefix='yunxi-mac-e2e-') as tmp:
        base = Path(tmp)
        scenarios = [
            ('mac_status', '只调用 mac_status，报告日历和提醒事项的真实权限状态。不要申请权限或操作实际事项。'),
            ('mac_calendar_list', '只调用 mac_calendar_list，读取2031年4月22日00:00:00+08:00至2031年4月23日00:00:00+08:00的日历。按实际结果报告，空列表也如实报告，不创建记录。'),
            ('mac_reminders_list', f'只调用 mac_reminders_list，include_completed=true，notes_prefix="云熙查询验收{uuid.uuid4().hex}"。按实际结果报告，空列表也如实报告，不创建记录。'),
            ('mac_notify', '调用 mac_notify 提交一条测试通知，标题“云熙工具验收”，正文“DeepSeek 到 Mac 通知工具链路测试”。如实说明提交与用户看到的区别。')]
        if args.lifecycle_only:
            token = uuid.uuid4().hex
            scenarios = [('lifecycle',
                '执行已授权的合成工具验收，严格完成以下三轮，不能只描述计划：'
                f'第一轮用 mac_calendar_create 创建标题“云熙工具验收”、start=2030-01-10T10:00:00+08:00、end=2030-01-10T11:00:00+08:00、request_id=test-calendar-{token}；'
                f'用 mac_reminder_create 创建同标题待办，due_at=2030-01-10T09:00:00+08:00、request_id=test-reminder-{token}。'
                '第二轮分别用 mac_calendar_update 和 mac_reminder_update，使用真实返回的id和revision作为expected_revision，把两个标题都改成“云熙工具验收：已调整”，待办还要completed=true。'
                '第三轮使用更新后返回的revision，分别调用 mac_calendar_delete 和 mac_reminder_delete 清理刚创建的两条合成记录。'
                '只能使用这六个工具，不读取其他事项，不操作其他记录。最后据实际回执报告完成情况。')]
        for tool, prompt in scenarios:
            home = base / tool
            home.mkdir(mode=0o700)
            (home / 'models').symlink_to(real_home / 'models', target_is_directory=True)
            source = real_home / 'secrets/deepseek.key'
            if source.exists():
                (home / 'secrets').mkdir(mode=0o700)
                key = home / 'secrets/deepseek.key'
                key.touch(mode=0o600)
                key.write_bytes(source.read_bytes())
            # 所有其他工具均明确禁止，不能用命令或联网工具绕过原生工具。
            names = ['now','read_file','list_dir','search_files','write_file','edit_file','run_command','ask_user','knowledge_search','web_fetch','web_search',
                     'mac_status','mac_calendar_list','mac_calendar_create','mac_calendar_update','mac_calendar_delete',
                     'mac_reminders_list','mac_reminder_create','mac_reminder_update','mac_reminder_delete','mac_notify',
                     'mac_agenda_plan','mac_agenda_apply']
            names += [t['name'] for t in json.loads((ROOT / 'native/macos/assistant-tools.json').read_text())]
            names += ['mac_trip_plan', 'mac_trip_apply']
            argv = [str(binary), 'chat', '--id', 'native-acceptance', '--thinking', 'off']
            allowed = LIFECYCLE if tool == 'lifecycle' else {tool}
            for name in names:
                argv += ['--allow' if name in allowed else '--deny', name]
            result = subprocess.run(argv, cwd=home, env=dict(os.environ, YUNXI_BOT_HOME=str(home)),
                                    input='执行这个工具验收任务，必须实际调用工具：' + prompt + '\n/exit\n',
                                    text=True, capture_output=True, timeout=180)
            ledger = home / 'ledger.jsonl'
            events = read_ledger(ledger)
            records = [e['data'] for e in events if e.get('kind') == 'tool_called']
            checks = lifecycle_checks(records) if tool == 'lifecycle' else checks_for(records, tool)
            if tool == 'lifecycle':
                checks['model_cleaned_fixtures'] = cleanup_created(records)
            checks['completed'] = result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败','这一轮未执行'))
            models = [e['data'] for e in events if e.get('kind') == 'model_called']
            checks['deepseek'] = bool(models) and all(m['provider'] == 'deepseek' for m in models)
            cases.append(dict(tool=tool, checks=checks, records=records, stdout=result.stdout, stderr=result.stderr))
            print(tool, checks, flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(cases, ensure_ascii=False, indent=2))
    return 0 if all(all(case['checks'].values()) for case in cases) else 1


if __name__ == '__main__':
    assert not all(checks_for([], 'mac_status').values()), '没有实际调用不能通过验收'
    assert not all(checks_for([dict(tool='mac_notify', executed=True, outcome='ok', output='{"kind":"confirmed"}')], 'mac_notify').values()), '不得把通知提交伪装成已读'
    assert not all(lifecycle_checks([]).values()), '完整操作不能凭空通过'
    assert not all(checks_for([dict(tool='mac_calendar_list', executed=True, outcome='ok', output='{}')], 'mac_calendar_list').values()), '缺少查询回执不能通过'
    raise SystemExit(main())
