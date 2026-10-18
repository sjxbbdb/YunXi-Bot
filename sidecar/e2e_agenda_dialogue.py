#!/usr/bin/env python3
"""真实 Laya + DeepSeek 连续追问验收；每轮重启恢复会话，所有写入明确拒绝。"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid
from datetime import datetime, timedelta
from types import SimpleNamespace

from e2e_agenda import NAMES, native, checks_for as agenda_checks
from e2e_common import read_ledger, ensure_fresh_binary, real_home_dir

READ_ONLY = {'now', 'ask_user', 'mac_agenda_plan', 'mac_calendar_list', 'mac_reminders_list', 'mac_status'}
LIVE_DATE = '2031-04-20'


def fixture_items(prefix):
    items = []
    for kind, action, arguments in (
        ('calendar', 'mac_calendar_list', dict(start=LIVE_DATE+'T00:00:00+08:00', end='2031-04-21T00:00:00+08:00')),
        ('reminder', 'mac_reminders_list', dict(include_completed=True, notes_prefix=f'云熙日程 v1\n{{"date":"{LIVE_DATE}",')),
    ):
        response = native(action, arguments)
        if response.get('truncated') is not False:
            raise RuntimeError('系统列表不完整，不能核对合成记录')
        items.extend((kind, item) for item in response['items'] if item.get('managed') is True and item.get('title', '').startswith(prefix))
    return items


def cleanup(prefix):
    for kind, item in fixture_items(prefix):
        result = native(f'mac_{kind}_delete', dict(id=item['id'], expected_revision=item['revision']))
        if result.get('verified_absent') is not True:
            raise RuntimeError('合成记录清理未核实')
    return not fixture_items(prefix)

def checks_for(case, result, events):
    records = [e['data'] for e in events if e.get('kind') == 'tool_called']
    plans = [r for r in records if r.get('tool') == 'mac_agenda_plan']
    arguments = [json.loads(r['arguments']) for r in plans if not r.get('arguments_clipped')]
    tasks = [(a, t) for a in arguments for t in a.get('tasks', [])]
    checks = {
        'process_ok': result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败', '这一轮未执行')),
        'only_expected_tools': all(r.get('tool') in READ_ONLY | ({'mac_agenda_apply'} if case.get('writes') else set()) for r in records),
        'tools_succeeded': all(r.get('executed') is True and r.get('outcome') == 'ok' for r in records),
        'arguments_complete': all(r.get('arguments_clipped') is False for r in plans),
        'no_tool_markup_in_reply': 'DSML' not in result.stdout and '<tool_call' not in result.stdout,
    }
    if not case.get('live'):
        checks['no_false_saved_claim'] = not re.search('已同步|已写入|已保存到', result.stdout)
    if case.get('writes'):
        checks['verified_apply_receipt'] = agenda_checks(records, True)['expected_write']
    if case.get('ask_duration'):
        checks['asks_missing_duration'] = bool(re.search('多久|多长|多少分钟', result.stdout)) or (
            case.get('past_clock') and bool(re.search('过去|补记|哪天|哪个日期|别的日期|哪种', result.stdout)))
    if case.get('clock'):
        checks['retains_corrected_clock'] = any(t in result.stdout for t in case['clock'])
    if case.get('drafts'):
        expected = case['drafts']
        checks['all_draft_items_preserved'] = bool(arguments) and {
            t['title']: (t.get('fixed_start', '')[11:16], t.get('duration_minutes'))
            for t in arguments[-1].get('tasks', [])} == expected
        checks['draft_date_retained'] = bool(arguments) and arguments[-1].get('date') == '2031-04-21'
    elif case.get('plan'):
        date, start, minutes = case['plan']
        checks['correct_plan_after_followup'] = bool(tasks) and all(
            args.get('date') == date and t.get('duration_minutes') == minutes
            and t.get('fixed_start', '').startswith(date + 'T' + start)
            for args, t in tasks)
    else:
        checks['does_not_invent_or_resume_cancelled_task'] = not tasks
    if case.get('clarify_target'):
        checks['asks_which_meeting'] = bool(re.search('哪一个|哪一场|哪个|哪场|哪一件|哪件', result.stdout))
    return checks, records


def self_check():
    case = {'plan': ('2031-04-18', '16:00', 60)}
    reply = SimpleNamespace(returncode=0, stdout='', stderr='')
    assert not all(checks_for(case, reply, [])[0].values()), '没有工具证据不能通过'
    record = dict(tool='mac_agenda_plan', arguments=json.dumps(dict(date='2031-04-18', tasks=[
        dict(duration_minutes=60, fixed_start='2031-04-18T16:00:00+08:00')])), arguments_clipped=False,
        outcome='ok', executed=True)
    events = [dict(kind='tool_called', data=record)]
    assert all(checks_for(case, reply, events)[0].values())
    assert not all(checks_for({'plan': ('2031-04-19', '16:00', 60)}, reply, events)[0].values())
    record['arguments_clipped'] = True
    assert not all(checks_for(case, reply, events)[0].values())


def main():
    self_check()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--thinking', choices=('on', 'off'), default='on')
    parser.add_argument('--live', action='store_true', help='改为真实日历保存/修改/取消验收，结束清理合成记录')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请换新路径保留失败证据')
    binary = ensure_fresh_binary()
    today = datetime.now().astimezone().date()
    cases = [
        dict(name='implicit-today', session='today', text='我今天下午三点有个会', ask_duration=True, past_clock=datetime.now().hour >= 15),
        dict(name='ack-is-not-duration', session='today', text='好', ask_duration=True, past_clock=datetime.now().hour >= 15),
        dict(name='implicit-future', session='meeting', text='我2031年4月18日下午三点有个会，叫连续追问验收会议', ask_duration=True),
        dict(name='correct-time-before-duration', session='meeting', text='改到四点', ask_duration=True, clock=['16:00', '16点', '四点', '4点']),
        dict(name='supply-duration-after-restart', session='meeting', text='一个小时', plan=('2031-04-18', '16:00', 60)),
        dict(name='correct-date-before-saving', session='meeting', text='还是2031年4月19日吧', plan=('2031-04-19', '16:00', 60)),
        dict(name='cancel-and-change-topic', session='meeting', text='算了，这场会先不安排了，陪我聊聊猫吧'),
        dict(name='short-answer-after-cancel', session='meeting', text='一个小时'),
        dict(name='quoted-meeting', session='quote', text='小说里写着“我今天下午三点有个会”，这句话自然吗？'),
        dict(name='past-event', session='past', text='我昨天下午三点开了个会，真累，只想聊聊'),
        dict(name='two-unsaved-meetings', session='two', text='2031年4月21日，下午三点项目会，四点招聘会，各半小时，先预排不要保存。', drafts={'项目会': ('15:00', 30), '招聘会': ('16:00', 30)}),
        dict(name='second-meeting-only', session='two', text='第二个改到五点', drafts={'项目会': ('15:00', 30), '招聘会': ('17:00', 30)}),
        dict(name='ambiguous-which-meeting', session='two', text='其中一场改成一个小时', clarify_target=True),
        dict(name='answer-which-meeting', session='two', text='招聘那个', drafts={'项目会': ('15:00', 30), '招聘会': ('17:00', 60)}),
        dict(name='relative-tomorrow', session='relative', text='我明天下午三点有个面试', ask_duration=True),
        dict(name='half-hour-answer', session='relative', text='半小时', plan=((today+timedelta(days=1)).isoformat(), '15:00', 30)),
        dict(name='relative-day-correction', session='relative', text='还是后天吧', plan=((today+timedelta(days=2)).isoformat(), '15:00', 30)),
    ]
    prefix = '云熙连续追问验收' + uuid.uuid4().hex[:8] + '：'
    if args.live:
        cases = [
            dict(name='save-natural-meeting', text=f'我2031年4月20日下午三点有个会，叫“{prefix}会议”，一个小时，帮我同步到Mac。标题请完整保留。', plan=(LIVE_DATE, '15:00', 60), writes=True),
            dict(name='correct-saved-time', text='改到四点', plan=(LIVE_DATE, '16:00', 60), writes=True),
            dict(name='correct-saved-duration', text='半小时就够了', plan=(LIVE_DATE, '16:00', 30), writes=True),
            dict(name='read-saved-meeting', text='看一下刚才那个会议现在安排成什么样了，不要修改', writes=False),
            dict(name='cancel-saved-meeting', text='取消刚才那个会', plan=(LIVE_DATE, '16:00', 30), writes=True, cancelled=True),
        ]
        for case in cases:
            case.update(session='saved', live=True)
    report = {'cases': [], 'read_only': not args.live, 'each_turn_resumes_from_disk': True, 'prefix': prefix}
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-agenda-dialogue-') as tmp:
            home = Path(tmp)
            (home / 'models').symlink_to(real_home_dir() / 'models', target_is_directory=True)
            source = real_home_dir() / 'secrets/deepseek.key'
            if source.exists():
                (home / 'secrets').mkdir(mode=0o700)
                key = home / 'secrets/deepseek.key'
                key.touch(mode=0o600)
                key.write_bytes(source.read_bytes())
            seen = set()
            original_ids = {}
            for case in cases:
                session = case['session']
                argv = [str(binary), 'chat', '--resume' if session in seen else '--id', session, '--thinking', args.thinking]
                for tool in NAMES:
                    allowed = tool in READ_ONLY or (tool == 'mac_agenda_apply' and case.get('writes'))
                    argv += ['--allow' if allowed else '--deny', tool]
                ledger = home / 'ledger.jsonl'
                before = len(read_ledger(ledger))
                result = subprocess.run(argv, input=case['text']+'\n/exit\n', cwd=home,
                                        env=dict(os.environ, YUNXI_BOT_HOME=str(home)),
                                        text=True, capture_output=True, timeout=180)
                seen.add(session)
                events = read_ledger(ledger)[before:]
                checks, records = checks_for(case, result, events)
                if args.live:
                    items = fixture_items(prefix)
                    calendars = [v for k, v in items if k == 'calendar']
                    reminders = [v for k, v in items if k == 'reminder']
                    checks['system_record_count'] = len(reminders) == 1 and len(calendars) == (0 if case.get('cancelled') else 1)
                    if not original_ids and calendars and reminders:
                        original_ids = {'calendar': calendars[0]['id'], 'reminder': reminders[0]['id']}
                    checks['same_record_ids'] = bool(original_ids) and all(original_ids.get(k) == v['id'] for k, v in items)
                    checks['completion_state'] = bool(reminders) and reminders[0].get('completed') is bool(case.get('cancelled'))
                    if not case.get('cancelled'):
                        start = '15:00' if case['name'] == 'save-natural-meeting' else '16:00'
                        minutes = 60 if case['name'] in {'save-natural-meeting', 'correct-saved-time'} else 30
                        expected = datetime.fromisoformat(LIVE_DATE+'T'+start+':00+08:00')
                        checks['system_time_matches'] = bool(calendars) and datetime.fromisoformat(calendars[0]['start']) == expected and datetime.fromisoformat(calendars[0]['end']) == expected + timedelta(minutes=minutes)
                    report['last_items'] = items
                classifications = [e['data'] for e in events if e.get('kind') == 'decision_decided' and e['data'].get('class') == 'classify' and e['data'].get('action') in {'task', 'chat', 'unavailable'}]
                report['cases'].append(dict(case, checks=checks, records=records, classifications=classifications,
                                            stdout=result.stdout, stderr=result.stderr))
                print(case['name'], checks, flush=True)
                if args.live and not all(checks.values()):
                    raise RuntimeError('真实写入验收未通过，停止并清理合成记录')
    except Exception as exc:
        report['error'] = str(exc)
    finally:
        if args.live:
            try:
                report['cleaned'] = cleanup(prefix)
            except Exception as exc:
                report['cleanup_error'] = str(exc)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.touch(mode=0o600)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if len(report['cases']) == len(cases) and all(all(c['checks'].values()) for c in report['cases']) and (not args.live or report.get('cleaned')) else 1


if __name__ == '__main__':
    raise SystemExit(main())
