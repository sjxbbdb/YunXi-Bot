#!/usr/bin/env python3
"""真实模型与Mac日程统筹：既有待办、两日出行、衔接冲突、连续修正及路线天气预排。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

from e2e_agenda import NAMES
from e2e_common import ensure_fresh_binary, read_ledger, real_home_dir
from e2e_trip import DATES, cleanup, fixture_items, same_time, tool_results

READ = {'now', 'ask_user', 'mac_trip_plan', 'mac_agenda_plan'}
WRITE = READ | {'mac_trip_apply', 'mac_agenda_apply'}


def saved_maps(items, prefix):
    maps = {kind: {r['title'].removeprefix(prefix): r for k, r in items if k == kind}
            for kind in ('calendar', 'reminder')}
    tasks = {title: json.loads(row['notes'].split('\n', 1)[1])['task']
             for title, row in maps['reminder'].items()}
    return maps, tasks


def links_preserved(tasks):
    pairs = [('去酒店', '列车'), ('去活动', '退房')]
    travel = [('准备', '列车'), ('去酒店', '入住'), ('去活动', '活动')]
    return bool(tasks) and all(tasks.get(a, {}).get('after_task') == tasks.get(b, {}).get('id')
                              and tasks.get(b, {}).get('id') for a, b in pairs) and all(
        tasks.get(a, {}).get('travel_for') == tasks.get(b, {}).get('id')
        and tasks.get(b, {}).get('id') for a, b in travel)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请换路径保留失败证据')
    binary = ensure_fresh_binary()
    prefix = '云熙统筹验收'+uuid.uuid4().hex[:8]+'：'
    report = dict(prefix=prefix, cases=[], cleaned=False)
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-coordination-') as tmp:
            home = Path(tmp)
            real_home = real_home_dir()
            (home/'models').symlink_to(real_home/'models', target_is_directory=True)
            (home/'secrets').mkdir(mode=0o700)
            source = real_home/'secrets/deepseek.key'
            if source.exists():
                key = home/'secrets/deepseek.key'
                key.touch(mode=0o600)
                key.write_bytes(source.read_bytes())
            scenarios = [
                ('existing-day', 'coord', WRITE,
                 f'做虚构日程测试。2031年5月18日9点固定约会一小时，必须保留；另有90分钟弹性工作，帮我安排并同步Mac。'
                 f'标题分别是“{prefix}约会”和“{prefix}工作”。我确认这天可以连续安排09:00–18:00，中午也可以，事项间隔10分钟。'),
                ('add-two-day-trip', 'coord', WRITE,
                 f'再加一趟两天的虚构出行，沿用原有约会和工作，统筹后同步Mac：5月18日11点列车出发，12点到站；出发准备30分钟跟随列车倒排；'
                 f'14点酒店入住办理30分钟，去酒店40分钟跟随入住倒排，必须先乘车到站才能去酒店。'
                 f'5月19日10点退房30分钟，11点半活动60分钟，去活动30分钟跟随活动倒排，必须退房后才能出发。'
                 f'两天都可以连续安排09:00–18:00、间隔10分钟，所有出行和酒店事项都必须安排，不能只留作待办，只有工作时间可调整。'
                 f'新标题逐一用“{prefix}”加上列车、准备、入住、去酒店、退房、活动、去活动。时长是我为本次测试确认的，详情标为用户提供的虚构测试资料，不查真实路线、不订票订房。'),
                ('late-arrival-hypothesis', 'coord', READ,
                 '只做假设别保存：第一天列车改为16点出发17点到站，其他固定安排保持原时间，去酒店仍需先到站。检查这两天还能不能成立，清楚指出冲突，不取消或移动已确认的入住来凑可行计划。'),
                ('correct-from-saved-facts', 'coord', WRITE,
                 '撤回刚才列车晚点的假设，以Mac已保存的列车时间为准。真正的修改是第一天入住改为15点，办理仍30分钟；去酒店改为50分钟，我确认这个测试时长，仍要先到站。'
                 '保留固定约会、其余出行及先后关系，重新安排弹性工作，更新这两天到Mac，说明工作原来几点、现在几点。'),
                ('recover-new-session', 'fresh', READ,
                 f'读取2031年5月18日和19日已保存的日程，关注“{prefix}”开头的记录，告诉我车次、酒店、工作时间和出行先后关系，只查看不要保存。'),
                ('route-weather-coordination', 'route', READ | {'mac_route', 'mac_weather'},
                 '只预排明天，不保存：14点到14点半在上海(31.228827,121.475213)有会，16点到16点半在上海(31.2411,121.4905)有会，'
                 '两个会都必须安排，坐标是我确认的；第二个会要步行过去，并另留10分钟出发准备，还要提前10分钟到。请实际查询路线与天气，'
                 '第一场结束后才能出发，把路途加准备作为关联第二场的时间块；用默认日程时段，说明天气影响和预计耗时。'),
            ]
            seen = set()
            original_ids = {}
            before_hypothesis = None
            for label, session, allowed, prompt in scenarios:
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
                models = [e['data'] for e in events if e.get('kind') == 'model_called']
                plans = results.get('mac_trip_plan', []) + results.get('mac_agenda_plan', [])
                receipts = results.get('mac_trip_apply', []) + results.get('mac_agenda_apply', [])
                checks = dict(
                    process_ok=result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败', '这一轮未执行')),
                    deepseek=bool(models) and all(m['provider'] == 'deepseek' for m in models),
                    all_tools_succeeded=bool(records) and all(r.get('executed') is True and r.get('outcome') == 'ok' for r in records),
                    only_allowed=all(r['tool'] in allowed for r in records),
                    actual_plan=bool(plans),
                )
                if allowed == WRITE:
                    checks['verified_write'] = bool(receipts) and all(r.get('applied') is True and r.get('degraded') is not True
                        and r.get('ready_to_apply') is True and r.get('unscheduled_count') == 0 for r in receipts)
                else:
                    checks['read_only'] = not receipts and not any(r['tool'].endswith('_apply') for r in records)
                if label == 'route-weather-coordination':
                    entries = [e for p in plans for d in p.get('days', [p]) for e in d.get('entries', [])]
                    routes = results.get('mac_route', [])
                    weather = results.get('mac_weather', [])
                    by_id = {e['task']['id']: e for e in entries}
                    transfers = [e for e in entries if e['task'].get('travel_for') and e['task'].get('after_task')]
                    checks['real_route_and_weather'] = bool(routes) and bool(weather) and all(r.get('travel_minutes', 0) > 0 and r.get('mode') == 'walking' for r in routes)
                    checks['route_duration_in_plan'] = bool(transfers) and bool(routes) and all(e['task']['duration_minutes'] == routes[-1]['travel_minutes']+10 for e in transfers)
                    checks['arrival_and_departure_linked'] = bool(transfers) and all(e['task']['after_task'] in by_id and e['task']['travel_for'] in by_id and e.get('start') for e in transfers)
                    checks['plan_ready'] = bool(plans) and plans[-1].get('ready_to_apply') is True
                else:
                    items = fixture_items(prefix)
                    maps, tasks = saved_maps(items, prefix)
                    expected_count = 2 if label == 'existing-day' else 9
                    checks['exact_system_counts'] = len(items) == 2*expected_count and all(len(maps[k]) == expected_count for k in maps)
                    checks['stable_ids'] = all(original_ids.get((kind, row['title']), row['id']) == row['id'] for kind, row in items)
                    original_ids.update({(kind, row['title']): row['id'] for kind, row in items})
                    checks['fixed_appointment_unchanged'] = same_time(maps['calendar'].get('约会', {}).get('start'), DATES[0]+'T09:00:00+08:00')
                    work_time = '10:10' if label == 'existing-day' else '12:10' if label in {'correct-from-saved-facts', 'recover-new-session'} else '14:40'
                    checks['flexible_work_replanned'] = same_time(maps['calendar'].get('工作', {}).get('start'), DATES[0]+'T'+work_time+':00+08:00')
                    if label != 'existing-day':
                        checks['dependencies_persist'] = bool(links_preserved(tasks))
                        checks['required_items_persist'] = len(tasks) == 9 and all(t.get('must_schedule') is True for name, t in tasks.items() if name != '工作')
                        checks['original_train_time'] = same_time(maps['calendar'].get('列车', {}).get('start'), DATES[0]+'T11:00:00+08:00')
                        checks['departure_reminder'] = same_time(maps['reminder'].get('准备', {}).get('due_at'), DATES[0]+'T10:20:00+08:00')
                    if label == 'add-two-day-trip':
                        before_hypothesis = items
                    if label == 'late-arrival-hypothesis':
                        checks['conflict_reported'] = any(p.get('ready_to_apply') is False and p.get('blocking_issues') for p in plans)
                        checks['saved_facts_untouched'] = items == before_hypothesis
                    if label in {'correct-from-saved-facts', 'recover-new-session'}:
                        checks['hotel_and_transfer_updated'] = same_time(maps['calendar'].get('入住', {}).get('start'), DATES[0]+'T15:00:00+08:00') and same_time(maps['calendar'].get('去酒店', {}).get('start'), DATES[0]+'T14:00:00+08:00') and tasks.get('去酒店', {}).get('duration_minutes') == 50
                    if label == 'correct-from-saved-facts':
                        entries = [e for p in plans for d in p.get('days', [p]) for e in d.get('entries', [])]
                        checks['reports_previous_and_new_work_time'] = any(e['task']['title'] == prefix+'工作' and same_time(e.get('previous_start'), DATES[0]+'T14:40:00+08:00') and same_time(e.get('start'), DATES[0]+'T12:10:00+08:00') for e in entries)
                    report['last_items'] = items
                report['cases'].append(dict(name=label, checks=checks, records=records, history=history, stdout=result.stdout, stderr=result.stderr))
                print(label, checks, flush=True)
                if not all(checks.values()):
                    raise RuntimeError(label+'验收失败')
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
    return 0 if report['cleaned'] and len(report['cases']) == 6 and all(all(c['checks'].values()) for c in report['cases']) else 1


if __name__ == '__main__':
    assert not links_preserved({}), '空记录不能当成先后关系完整'
    assert not links_preserved({'去酒店': {'id': 'transfer'}}), '缺失前置事项不能通过'
    raise SystemExit(main())
