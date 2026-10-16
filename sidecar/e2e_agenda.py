#!/usr/bin/env python3
"""真实 DeepSeek + Mac 日程链路：首次安排、追加、完成、新会话恢复；只清理本轮合成记录。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid
from datetime import datetime, timedelta
from e2e_common import read_ledger, ensure_fresh_binary, real_home_dir

ROOT = Path(__file__).resolve().parents[1]
DATE = '2031-04-18'
NAMES = ('now web_fetch web_search knowledge_search read_file list_dir search_files write_file edit_file run_command ask_user '
         'mac_status mac_calendar_list mac_calendar_create mac_calendar_update mac_calendar_delete mac_reminders_list '
         'mac_reminder_create mac_reminder_update mac_reminder_delete mac_notify mac_agenda_plan mac_agenda_apply').split()
NAMES += [t['name'] for t in json.loads((ROOT / 'native/macos/assistant-tools.json').read_text())]
NAMES += ['mac_trip_plan', 'mac_trip_apply']
NAMES += ['mail_review_pending', 'mac_mail_context', 'mail_reply_pending', 'mail_reply_prepare']


def native(action, args):
    with tempfile.TemporaryDirectory(prefix='yunxi-agenda-rpc-') as tmp:
        base = Path(tmp)
        for name in ('request', 'response', 'stderr'):
            (base / name).touch(mode=0o600)
        (base / 'request').write_text(json.dumps(dict(action=action, args=args)))
        subprocess.run(['/usr/bin/open', '-W', '-n', '-g', '-a', str(ROOT / 'target/macos/云熙本机助理.app'),
                        '-i', str(base / 'request'), '-o', str(base / 'response'), '--stderr', str(base / 'stderr')], check=True, timeout=35)
        reply = json.loads((base / 'response').read_text())
        if reply.get('ok') is not True:
            raise RuntimeError(reply.get('message', '原生接口失败'))
        return reply['result']


def fixture_items(prefix):
    result = []
    for kind, action, args in (
        ('calendar', 'mac_calendar_list', dict(start=DATE+'T00:00:00+08:00', end='2031-04-19T00:00:00+08:00')),
        ('reminder', 'mac_reminders_list', dict(include_completed=True, notes_prefix=f'云熙日程 v1\n{{"date":"{DATE}",')),
    ):
        response = native(action, args)
        if response.get('truncated') is not False:
            raise RuntimeError('列表不完整，不能确定本轮测试记录范围')
        result += [(kind, item) for item in response['items'] if item.get('managed') is True and item.get('title', '').startswith(prefix)]
    return result


def cleanup(prefix):
    for kind, item in fixture_items(prefix):
        result = native(f'mac_{kind}_delete', dict(id=item['id'], expected_revision=item['revision']))
        if result.get('verified_absent') is not True:
            raise RuntimeError('测试记录删除未核实')
    return not fixture_items(prefix)


def checks_for(records, writes):
    plans = [r for r in records if r.get('tool') == 'mac_agenda_plan']
    applied = [r for r in records if r.get('tool') == 'mac_agenda_apply']
    ok = lambda r: r.get('executed') is True and r.get('outcome') == 'ok'
    def applied_receipt(r):
        if not ok(r):
            return False
        # 台账只保留前2000字符；applied是序列化结果的首字段，另以EventKit回读验证真实状态。
        if r.get('output_clipped') is True:
            return r.get('output', '').startswith('{"applied":true,')
        return json.loads(r['output']).get('applied') is True
    return dict(plan_called=bool(plans) and all(map(ok, plans)),
                expected_write=(bool(applied) and all(map(applied_receipt, applied))) if writes else not applied,
                all_tools_succeeded=bool(records) and all(map(ok, records)),
                only_agenda_tools=bool(records) and all(r.get('tool') in {'mac_agenda_plan', 'mac_agenda_apply', 'now'} for r in records))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请使用新路径保留失败证据')
    binary = ensure_fresh_binary()
    prefix = '云熙日程链路验收' + uuid.uuid4().hex[:8] + '：'
    report = dict(prefix=prefix, cases=[], cleaned=False)
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-agenda-e2e-') as tmp:
            home = Path(tmp)
            (home / 'models').symlink_to(real_home_dir() / 'models', target_is_directory=True)
            source = real_home_dir() / 'secrets/deepseek.key'
            if source.exists():
                (home / 'secrets').mkdir(mode=0o700)
                key = home / 'secrets/deepseek.key'
                key.touch(mode=0o600)
                key.write_bytes(source.read_bytes())
            scenarios = [
                ('plan-day', 'day', True, f'请帮我把2031年4月18日安排好并同步到Mac日历和提醒事项：{prefix}报告需要90分钟，{prefix}练习需要60分钟。采用09:00–12:00、13:00–18:00时段和10分钟间隔。事项标题完整保留。请实际执行。'),
                ('append', 'day', True, f'2031年4月18日临时多一件事：{prefix}紧急确认，需要30分钟，必须当天10:00之前完成。请保留原有事项，重新安排余下时间并同步到Mac。'),
                ('complete-new-session', 'fresh-session', True, f'2031年4月18日的“{prefix}报告”已完成，请读取已有日程，标记完成并重排剩余安排，同步到Mac。保留其他事项。'),
                ('read-new-session', 'another-session', False, '请读取2031年4月18日已经保存的安排，报告每件事项的状态和时间。本次仅查看，不写入。'),
            ]
            prior_ids = {}
            prior_calendar_ids = {}
            for name, session, writes, prompt in scenarios:
                argv = [str(binary), 'chat', '--resume' if name == 'append' else '--id', session, '--thinking', 'off']
                allowed = {'mac_agenda_plan', 'now'} | ({'mac_agenda_apply'} if writes else set())
                for tool in NAMES:
                    argv += ['--allow' if tool in allowed else '--deny', tool]
                ledger = home / 'ledger.jsonl'
                before = len(read_ledger(ledger))
                result = subprocess.run(argv, input=prompt+'\n/exit\n', cwd=home, env=dict(os.environ, YUNXI_BOT_HOME=str(home)),
                                        text=True, capture_output=True, timeout=240)
                events = read_ledger(ledger)[before:]
                records = [e['data'] for e in events if e.get('kind') == 'tool_called']
                checks = checks_for(records, writes)
                checks['process_ok'] = result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败', '这一轮未执行'))
                models = [e['data'] for e in events if e.get('kind') == 'model_called']
                checks['deepseek'] = bool(models) and all(m['provider'] == 'deepseek' for m in models)
                items = fixture_items(prefix)
                reminders = {r['title']: r for kind, r in items if kind == 'reminder'}
                calendars = {r['title']: r for kind, r in items if kind == 'calendar'}
                checks['expected_count'] = len(reminders) == (2 if name == 'plan-day' else 3) and len(calendars) == (3 if name == 'append' else 2)
                checks['no_duplicate_titles'] = len(items) == len(reminders) + len(calendars)
                if name == 'plan-day':
                    prior_ids = {title:r['id'] for title,r in reminders.items()}
                    prior_calendar_ids = {title:r['id'] for title,r in calendars.items()}
                else:
                    checks['retains_original_records'] = bool(prior_ids) and all(reminders.get(title, {}).get('id') == identifier for title, identifier in prior_ids.items())
                slots = sorted((datetime.fromisoformat(r['start']), datetime.fromisoformat(r['end'])) for r in calendars.values())
                checks['no_overlaps_and_buffer'] = bool(slots) and all(a[1] + timedelta(minutes=10) <= b[0] for a, b in zip(slots, slots[1:]))
                if name == 'append':
                    checks['calendar_ids_unchanged'] = bool(prior_calendar_ids) and all(calendars.get(title, {}).get('id') == identifier for title, identifier in prior_calendar_ids.items())
                    urgent = calendars.get(prefix+'紧急确认', {})
                    checks['urgent_meets_deadline'] = bool(urgent.get('end')) and datetime.fromisoformat(urgent['end']) <= datetime.fromisoformat(DATE+'T10:00:00+08:00')
                if name in {'complete-new-session', 'read-new-session'}:
                    checks['completed_persisted'] = reminders.get(prefix+'报告', {}).get('completed') is True and prefix+'报告' not in calendars
                report['cases'].append(dict(name=name, checks=checks, records=records, stdout=result.stdout, stderr=result.stderr))
                print(name, checks, flush=True)
                if not all(checks.values()):
                    raise RuntimeError(f'{name}验收失败')
    except Exception as error:
        report['error'] = str(error)
    finally:
        try:
            report['cleaned'] = cleanup(prefix)
        except Exception as error:
            report['cleanup_error'] = str(error)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.touch(mode=0o600)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print('cleaned:', report['cleaned'], 'error:', report.get('error'), flush=True)
    return 0 if report['cleaned'] and len(report['cases']) == 4 and all(all(c['checks'].values()) for c in report['cases']) else 1


if __name__ == '__main__':
    assert not all(checks_for([], True).values())
    assert not all(checks_for([dict(tool='mac_agenda_apply', executed=True, outcome='ok', output='{"applied":false}')], True).values())
    assert checks_for([dict(tool='mac_agenda_plan', executed=True, outcome='ok'), dict(tool='mac_agenda_apply', executed=True, outcome='ok', output_clipped=True, output='{"applied":true,"changes":[')], True)['expected_write']
    raise SystemExit(main())
