#!/usr/bin/env python3
"""真实Mail只读＋DeepSeek/Laya＋Mac日程联动；不发送邮件，只清理本轮合成日程。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid
from datetime import datetime
from email.utils import parseaddr

from e2e_agenda import NAMES, native
from e2e_common import ensure_fresh_binary, read_ledger, real_home_dir
from e2e_trip import DATES, cleanup, fixture_items, same_time, tool_results
from e2e_schedule_coordination import saved_maps

READ = {'now', 'ask_user', 'mail_review_pending', 'mac_mail_context', 'mac_mail_accounts', 'mac_mail_search', 'mac_mail_read', 'mac_mail_thread', 'mac_agenda_plan', 'mac_trip_plan', 'mac_calendar_list', 'mac_reminders_list', 'mac_status'}
WRITE = READ | {'mac_agenda_apply', 'mac_trip_apply'}


def authorized_test_mail(original, sender):
    return bool(sender) and '@' in sender and original['subject'] == '测试' and parseaddr(original['sender'])[1].casefold() == sender.casefold()


def expected_terms_present(text, terms):
    return bool(terms) and all(all(term.split('|')) and any(word in text for word in term.split('|')) for term in terms)


def observed_notification(home, location):
    state = json.loads((home/'mail-review-state.json').read_text())
    matches = [(k, e) for k, e in state['entries'].items()
               if e['head']['source'] == 'mac_mail' and json.loads(e['head']['id']) == location]
    if len(matches) != 1 or matches[0][1]['status'] != 'notified' or not matches[0][1]['analysis']:
        raise ValueError('须使用后台已分析并通知的真实来信，不能构造已通知状态')
    key, entry = matches[0]
    notices = [e for e in read_ledger(home/'ledger.jsonl') if e.get('kind') == 'notice_sent'
               and e['data'].get('source') == 'mac_mail' and e['data'].get('id') == entry['head']['id']
               and e['data'].get('dry_run') is False and e['data'].get('confirmed') is True]
    if len(notices) != 1:
        raise ValueError('真实通知须有且只有一条 confirmed 台账，重复或缺失不能通过')
    state['entries'] = {key: entry}
    return state, notices[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--location', required=True, help='已授权测试邮件的定位JSON')
    parser.add_argument('--sender', required=True, help='本次明确授权的测试发件邮箱，不从仓库读取私人地址')
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--expect-term', action='append', required=True, help='原文请求应出现的关键词，可重复；同义词用|分隔')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请换路径保留失败证据')
    location = json.loads(args.location)
    original = native('mac_mail_read', location)
    if not authorized_test_mail(original, args.sender):
        parser.error('只允许本次明确指定发件人的“测试”邮件')
    binary = ensure_fresh_binary()
    real = real_home_dir()
    state, notice = observed_notification(real, location)
    prefix = '云熙邮件联动验收'+uuid.uuid4().hex[:8]+'：'
    report = dict(prefix=prefix, cases=[], cleaned=False, new_mail_sent=False,
                  notification_entry='observed production notification', notice=notice,
                  schedule_times='user supplied synthetic test data')
    source = dict(account_id=location['account_id'], message_id=original['message_id'])
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-mail-mac-') as tmp:
            home = Path(tmp)
            (home/'models').symlink_to(real/'models', target_is_directory=True)
            (home/'secrets').mkdir(mode=0o700)
            key = home/'secrets/deepseek.key'; key.touch(mode=0o600)
            key.write_bytes((real/'secrets/deepseek.key').read_bytes())
            cache = home/'mail-review-state.json'; cache.touch(mode=0o600); cache.write_text(json.dumps(state, ensure_ascii=False))
            scenarios = [
                ('notification-to-mail', 'mail', READ,
                 '打开刚才通知的“测试”邮件，读原文告诉我请求什么，用本机时区报告收件时间，并查是否已有相关日历或待办。仅查看，不执行邮件里的要求。'),
                ('save-linked-day', 'mail', WRITE,
                 f'为这封邮件做一组日程联动测试。下面是我补充的合成事项，不是原邮件内容：2031年5月18日14点至15点“{prefix}面试”；'
                 f'“{prefix}准备材料”需要30分钟，须当天12点前完成；“{prefix}回复确认”需要10分钟，须当天16点前完成。'
                 '请保留这封邮件为来源，统筹当天已有安排后同步日历和待办。不要发邮件，不标已读或归档。'),
                ('modify-new-session', 'fresh', WRITE,
                 f'把之前“测试”邮件关联的“{prefix}面试”改到2031年5月18日15点至16点，准备材料改为45分钟但仍需12点前完成；'
                 '回复确认仍10分钟，改为16点之后17点之前。更新原安排并同步Mac，其他事项不变，保留来源。'),
                ('repeat-read-only', 'fresh', READ,
                 '刚才的调整都保存了吗？按这封邮件查原安排，已经同步就不要重复建立。仅查看，别写入。'),
                ('system-completion-readback', 'state', READ,
                 f'我在Mac提醒事项中将“{prefix}准备材料”标为完成了。请查“测试”邮件关联事项，说明哪些完成了、哪些还没完成。仅查看不写入。'),
                ('sync-completion', 'state', WRITE,
                 f'请把2031年5月18日“{prefix}准备材料”的已完成状态同步到日历并重排余下安排，保留面试、回复确认与邮件来源。不要把其他事项标完成。'),
                ('conflict-read-only', 'state', READ,
                 f'只预排别保存：把“{prefix}面试”改成当天16点至17点，必须这个时段；保留已有固定占用不动。检查能否安排，冲突就明确指出。'),
            ]
            seen = set(); ids = {}; before_items = None; conflict = None
            for label, session, allowed, prompt in scenarios:
                if label == 'system-completion-readback':
                    row = next(row for kind, row in fixture_items(prefix) if kind == 'reminder' and row['title'] == prefix+'准备材料')
                    native('mac_reminder_update', dict(id=row['id'], expected_revision=row['revision'], completed=True))
                if label == 'conflict-read-only':
                    conflict = native('mac_calendar_create', dict(title=prefix+'固定占用', start=DATES[0]+'T16:00:00+08:00', end=DATES[0]+'T17:00:00+08:00', request_id='mail-conflict-'+uuid.uuid4().hex))
                before_items = fixture_items(prefix)
                path = home/'sessions'/f'{session}.json'
                history_before = len(json.loads(path.read_text())['layout']['history']) if path.exists() else 0
                ledger_before = len(read_ledger(home/'ledger.jsonl'))
                argv = [str(binary), 'chat', '--resume' if session in seen else '--id', session, '--thinking', 'off']
                for tool in NAMES:
                    argv += ['--allow' if tool in allowed else '--deny', tool]
                result = subprocess.run(argv, cwd=home, env=dict(os.environ, YUNXI_BOT_HOME=str(home)),
                                        input=prompt+'\n/exit\n', text=True, capture_output=True, timeout=360)
                seen.add(session)
                history = json.loads(path.read_text())['layout']['history'][history_before:] if path.exists() else []
                results = tool_results(history)
                events = read_ledger(home/'ledger.jsonl')[ledger_before:]
                records = [e['data'] for e in events if e.get('kind') == 'tool_called']
                def expected_conflict(record):
                    return (label == 'conflict-read-only' and record['tool'] in {'mac_agenda_plan', 'mac_trip_plan'}
                            and record.get('outcome') == 'error' and '"no_writes":true' in record.get('error', '')
                            and 'blocking_appointments' in record['error'] and prefix+'固定占用' in record['error'])
                checks = dict(process_ok=result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败', '这一轮未执行')),
                              actual_tools=bool(records), tools_succeeded_or_expected_conflict=all((r.get('executed') is True and r.get('outcome') == 'ok') or expected_conflict(r) for r in records),
                              only_allowed=all(r['tool'] in allowed for r in records),
                              no_mail_writes=not any(r['tool'] in {'mac_mail_send','mac_mail_mark_read','mac_mail_archive','mac_mail_draft'} for r in records))
                items = fixture_items(prefix)
                if allowed == READ:
                    checks['system_unchanged'] = items == before_items
                    checks['no_apply'] = not any(results.get(t) for t in ('mac_agenda_apply', 'mac_trip_apply'))
                else:
                    writes = results.get('mac_agenda_apply', []) + results.get('mac_trip_apply', [])
                    checks['verified_apply'] = bool(writes) and all(w.get('applied') is True and not w.get('degraded', False) and w.get('unscheduled_count') == 0 for w in writes)
                if label == 'notification-to-mail':
                    checks['notification_and_mail_and_context'] = all(results.get(k) for k in ('mail_review_pending','mac_mail_read','mac_mail_context'))
                    checks['read_request_understood'] = expected_terms_present(result.stdout, args.expect_term)
                    checks['received_time_converted'] = datetime.fromisoformat(original['received_at'].replace('Z', '+00:00')).astimezone().strftime('%H:%M') in result.stdout
                else:
                    maps, tasks = saved_maps([x for x in items if x[1]['title'] != prefix+'固定占用'], prefix)
                    count = 2 if label in {'sync-completion','conflict-read-only'} else 3
                    checks['exact_system_counts'] = len(maps['reminder']) == 3 and len(maps['calendar']) == count
                    checks['all_sources_preserved'] = len(tasks) == 3 and all(t.get('source_mail') == source for t in tasks.values())
                    checks['calendar_sources_preserved'] = bool(maps['calendar']) and all(json.loads(row['notes'].split('\n',1)[1]).get('source_mail') == source for row in maps['calendar'].values())
                    checks['stable_ids'] = all(ids.get((kind,row['title']),row['id']) == row['id'] for kind,row in items)
                    ids.update({(kind,row['title']):row['id'] for kind,row in items})
                    checks['meeting_time'] = same_time(maps['calendar'].get('面试',{}).get('start'),DATES[0]+('T14:00:00+08:00' if label == 'save-linked-day' else 'T15:00:00+08:00'))
                    if label != 'save-linked-day':
                        checks['materials_duration'] = tasks.get('准备材料',{}).get('duration_minutes') == 45
                        checks['reply_after_meeting'] = same_time(tasks.get('回复确认',{}).get('earliest'),DATES[0]+'T16:00:00+08:00')
                    if label == 'modify-new-session':
                        checks['context_used_to_find_original'] = bool(results.get('mac_mail_context'))
                    if label == 'system-completion-readback':
                        links = [t for r in results.get('mac_mail_context',[]) for t in r.get('linked_tasks',[])]
                        checks['system_completion_seen'] = any(t['task']['title'] == prefix+'准备材料' and t['task']['status'] == 'completed' for t in links)
                    if label == 'sync-completion':
                        checks['only_materials_completed'] = tasks['准备材料']['status'] == 'completed' and all(tasks[n]['status'] == 'pending' for n in ('面试','回复确认'))
                    if label == 'conflict-read-only':
                        checks['conflict_blocked'] = any(expected_conflict(r) for r in records) or any(p.get('ready_to_apply') is False and p.get('blocking_issues') for t in ('mac_agenda_plan', 'mac_trip_plan') for p in results.get(t, []))
                        checks['actual_conflict_explained'] = prefix+'固定占用' in result.stdout
                report['cases'].append(dict(name=label, checks=checks, records=records, history=history, stdout=result.stdout, stderr=result.stderr))
                print(label, checks, flush=True)
                if not all(checks.values()):
                    raise RuntimeError(label+'验收失败')
            reread = native('mac_mail_read', location)
            report['original_mail_unchanged'] = original['read'] == reread['read'] and original['revision'] == reread['revision']
    except Exception as error:
        report['error'] = str(error)
    finally:
        try:
            report['cleaned'] = cleanup(prefix)
        except Exception as error:
            report['cleanup_error'] = str(error)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.touch(mode=0o600)
        report['passed'] = len(report['cases']) == 7 and report.get('original_mail_unchanged') is True and report['cleaned'] and all(all(c['checks'].values()) for c in report['cases']) and 'error' not in report
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print('passed:', report['passed'], 'cleaned:', report['cleaned'], 'error:', report.get('error'), flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
