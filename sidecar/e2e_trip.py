#!/usr/bin/env python3
"""第三批真实模型验收：公开资料查询、预订提取、连续对话及两日 EventKit 生命周期。"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid
from datetime import datetime, timedelta

from e2e_agenda import NAMES, native
from e2e_common import ensure_fresh_binary, read_ledger, real_home_dir

DATES = ['2031-05-18', '2031-05-19']
READ = {'now', 'ask_user', 'mac_trip_plan', 'mac_document_read', 'read_file'}


def same_time(actual, expected):
    return isinstance(actual, str) and datetime.fromisoformat(actual) == datetime.fromisoformat(expected)


def fixture_items(prefix):
    items = []
    for date in DATES:
        end = (datetime.fromisoformat(date) + timedelta(days=1)).date().isoformat()
        for kind, action, args in (
            ('calendar', 'mac_calendar_list', dict(start=date+'T00:00:00+08:00', end=end+'T00:00:00+08:00')),
            ('reminder', 'mac_reminders_list', dict(include_completed=True, notes_prefix=f'云熙日程 v1\n{{"date":"{date}",')),
        ):
            reply = native(action, args)
            if reply.get('truncated') is not False:
                raise RuntimeError('系统记录不完整，无法验收和确定清理范围')
            items.extend((kind, row) for row in reply['items'] if row.get('managed') is True and row.get('title', '').startswith(prefix))
    return items


def cleanup(prefix):
    for kind, row in fixture_items(prefix):
        reply = native(f'mac_{kind}_delete', dict(id=row['id'], expected_revision=row['revision']))
        if reply.get('verified_absent') is not True:
            raise RuntimeError('测试记录清理未核实')
    return not fixture_items(prefix)


def tool_results(history):
    names = {}
    results = {}
    for message in history:
        for call in message.get('tool_calls', []):
            names[call['id']] = call['function']['name']
        name = names.get(message.get('tool_call_id'))
        if name:
            try:
                results.setdefault(name, []).append(json.loads(message['content']))
            except (ValueError, KeyError):
                pass
    return results


def plan_checks(plan, prefix):
    days = plan.get('days', [])
    tasks = {e['task']['title']: e for d in days for e in d.get('entries', [])}
    expected = {'列车', '出发准备', '酒店入住', '酒店退房', '活动'}
    train = tasks.get(prefix+'列车', {}).get('task', {})
    prep = tasks.get(prefix+'出发准备', {}).get('task', {})
    return {
        'two_days_read_back': [d.get('date') for d in days] == DATES,
        'all_five_items': set(tasks) == {prefix+s for s in expected},
        'nothing_unscheduled': plan.get('unscheduled_count') == 0,
        'departure_linked': bool(train.get('id')) and prep.get('travel_for') == train['id'] and prep.get('duration_minutes') == 30,
        'booking_details': all('合成预订.txt' in tasks.get(prefix+s, {}).get('task', {}).get('details', '') for s in ('列车', '酒店入住', '酒店退房')),
        'hotel_is_checkpoints': all(tasks.get(prefix+s, {}).get('task', {}).get('duration_minutes') == 30 for s in ('酒店入住', '酒店退房')),
        'no_supplier_action': plan.get('booking_actions_performed') is False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--research-only', action='store_true', help='仅复验公开交通与酒店查询，不写入系统记录')
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请换路径保留失败证据')
    binary = ensure_fresh_binary()
    prefix = '云熙行程验收'+uuid.uuid4().hex[:8]+'：'
    report = dict(prefix=prefix, cases=[], cleaned=False)
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-trip-e2e-') as tmp:
            home = Path(tmp)
            real_home = real_home_dir()
            (home/'models').symlink_to(real_home/'models', target_is_directory=True)
            (home/'secrets').mkdir(mode=0o700)
            for name in ('deepseek.key', 'bocha.key'):
                source = real_home/'secrets'/name
                if source.exists():
                    target = home/'secrets'/name
                    target.touch(mode=0o600)
                    target.write_bytes(source.read_bytes())
            (home/'合成预订.txt').write_text(
                f'以下均为虚构测试资料，非真实预订。用户确认仅用于日程测试。时区上海。\n'
                f'2031-05-18 11:00乘测试列车TTEST，12:00到达；事项标题{prefix}列车。\n'
                f'出发准备及到站共30分钟，事项标题{prefix}出发准备，跟随列车。\n'
                f'酒店：虚构青竹酒店，虚构青竹路18号。5月18日15:00入住，办理30分钟，标题{prefix}酒店入住；'
                f'5月19日10:00退房，办理30分钟，标题{prefix}酒店退房。\n'
                f'5月19日14:00活动，60分钟，标题{prefix}活动。\n'
                '价格、库存和退改条款未核实；酒店整晚不占白天工作时段。来源就是合成预订.txt。')
            definitions = [
                ('missing-information', 'clarify', READ, set(), '我准备出去玩两天，也要住一晚酒店，先帮我规划，不要保存。'),
                ('booking-plan', 'trip', READ, {'mac_trip_plan'},
                 '读取当前目录的合成预订.txt，整理这两天的行程，只预排不要保存。标题完整保留；每个交通和酒店事项带来源文件名、具体预订资料与虚构测试状态。采用默认可安排时段和10分钟间隔。'),
                ('save-two-days', 'trip', READ | {'mac_trip_apply'}, {'mac_trip_apply'},
                 '就按刚才这两天的计划，全部同步到Mac日历和提醒事项。'),
                ('change-second-day', 'trip', READ | {'mac_trip_apply'}, {'mac_trip_plan', 'mac_trip_apply'},
                 '第二天的活动改到下午四点，时长不变，其余保留，更新到Mac。'),
                ('read-new-session', 'fresh', READ, {'mac_trip_plan'},
                 f'读取2031年5月18日和19日已经保存的行程，告诉我标题以“{prefix}”开头的安排，包括时间、状态及酒店和列车详情，只查看，不保存。'),
                ('cancel-departure', 'trip', READ | {'mac_trip_apply'}, {'mac_trip_plan', 'mac_trip_apply'},
                 '第一天列车日程取消，关联的出发准备也一起取消，其余保留并更新到Mac。这只是取消日历日程，不操作车票订单。'),
                ('public-travel-research', 'research', {'now', 'web_search', 'web_fetch', 'mac_places_search', 'mac_route'}, {'web_search', 'mac_places_search', 'mac_route'},
                 '我想比较上海虹桥站到静安寺的驾车和公共交通方案，按明天下午两点到达查询，并找静安寺附近两家不同的酒店作为候选。请查地图和公开网页给出名称、地址、交通耗时和来源；无需实时房价，不订票订房，也不写日程。查不到的价格、可订状态、退改条款明确写未核实。'),
            ]
            if args.research_only:
                definitions = definitions[-1:]
            seen = set()
            original_ids = {}
            original_details = {}
            for label, session_id, allowed, required, prompt in definitions:
                path = home/'sessions'/f'{session_id}.json'
                history_before = len(json.loads(path.read_text())['layout']['history']) if path.exists() else 0
                before = len(read_ledger(home/'ledger.jsonl'))
                argv = [str(binary), 'chat', '--resume' if session_id in seen else '--id', session_id, '--thinking', 'off']
                for name in NAMES:
                    argv += ['--allow' if name in allowed else '--deny', name]
                result = subprocess.run(argv, cwd=home, env=dict(os.environ, YUNXI_BOT_HOME=str(home)),
                                        input=prompt+'\n/exit\n', text=True, capture_output=True, timeout=300)
                seen.add(session_id)
                history = json.loads(path.read_text())['layout']['history'][history_before:] if path.exists() else []
                results = tool_results(history)
                events = read_ledger(home/'ledger.jsonl')[before:]
                records = [e['data'] for e in events if e.get('kind') == 'tool_called']
                models = [e['data'] for e in events if e.get('kind') == 'model_called']
                checks = {
                    'process_ok': result.returncode == 0 and not any(s in result.stderr for s in ('这一轮失败', '这一轮未执行')),
                    'deepseek': bool(models) and all(m['provider'] == 'deepseek' for m in models),
                    'required_tools': required.issubset({r['tool'] for r in records if r.get('executed') is True and r.get('outcome') == 'ok'}),
                    'only_allowed': all(r['tool'] in allowed for r in records),
                    'all_tools_succeeded': all(r.get('executed') is True and r.get('outcome') == 'ok' for r in records),
                }
                if label == 'missing-information':
                    checks['asks_before_inventing'] = bool(re.search('哪里|哪[天里]|目的地|日期|什么时候', result.stdout)) and 'mac_trip_plan' not in results
                elif label == 'booking-plan':
                    checks.update(plan_checks(results.get('mac_trip_plan', [{}])[-1], prefix))
                    checks['read_real_document'] = any(r['tool'] in {'read_file', 'mac_document_read'} and r.get('outcome') == 'ok' for r in records) and any(m.get('role') == 'tool' and 'TTEST' in m.get('content', '') for m in history)
                    checks['not_saved'] = not fixture_items(prefix) and 'mac_trip_apply' not in results
                elif label == 'public-travel-research':
                    routes = results.get('mac_route', [])
                    checks['two_transport_modes'] = {r.get('mode') for r in routes} == {'driving', 'transit'} and all(r.get('travel_minutes', 0) > 0 for r in routes)
                    checks['price_availability_terms_unknown'] = all(s in result.stdout for s in ('未核实', '可订', '退改')) and bool(re.search('价格|房价', result.stdout))
                    checks['provides_sources'] = 'https://' in result.stdout
                else:
                    writes = 'mac_trip_apply' in allowed
                    if writes:
                        receipts = results.get('mac_trip_apply', [])
                        checks['verified_all_days'] = bool(receipts) and all(p.get('applied') is True and p.get('degraded') is False and p.get('unscheduled_count') == 0 for p in receipts)
                    else:
                        checks.update(plan_checks(results.get('mac_trip_plan', [{}])[-1], prefix))
                    items = fixture_items(prefix)
                    maps = {kind: {r['title']: r for k, r in items if k == kind} for kind in ('reminder', 'calendar')}
                    cancelled = label == 'cancel-departure'
                    checks['exact_system_counts'] = len(maps['reminder']) == 5 and len(maps['calendar']) == (3 if cancelled else 5) and len(items) == (8 if cancelled else 10)
                    saved = {title: json.loads(row['notes'].split('\n', 1)[1])['task'] for title, row in maps['reminder'].items()}
                    if not original_ids:
                        original_ids = {(kind, row['title']): row['id'] for kind, row in items}
                        original_details = {t: s.get('details', '') for t, s in saved.items() if t != prefix+'活动'}
                    checks['original_ids_retained'] = bool(original_ids) and all(original_ids.get((kind, row['title'])) == row['id'] for kind, row in items)
                    checks['booking_details_retained'] = all(saved.get(t, {}).get('details', '') == d for t, d in original_details.items())
                    checks['changed_activity_keeps_source'] = '合成预订.txt' in saved.get(prefix+'活动', {}).get('details', '')
                    activity = maps['calendar'].get(prefix+'活动', {})
                    hour = '14:00' if label == 'save-two-days' else '16:00'
                    checks['correct_activity_time'] = same_time(activity.get('start'), DATES[1]+'T'+hour+':00+08:00')
                    if cancelled:
                        checks['linked_departure_cancelled'] = all(saved.get(prefix+s, {}).get('status') == 'cancelled' and prefix+s not in maps['calendar'] and maps['reminder'].get(prefix+s, {}).get('completed') is True for s in ('列车', '出发准备'))
                    else:
                        checks['departure_alarm'] = same_time(maps['reminder'].get(prefix+'出发准备', {}).get('due_at'), DATES[0]+'T10:20:00+08:00')
                    report['last_items'] = items
                report['cases'].append(dict(name=label, checks=checks, records=records, history=history, stdout=result.stdout, stderr=result.stderr))
                print(label, checks, flush=True)
                if not all(checks.values()):
                    raise RuntimeError(label+'验收失败')
    except Exception as error:
        report['error'] = str(error)
    finally:
        try:
            report['cleaned'] = True if args.research_only else cleanup(prefix)
        except Exception as error:
            report['cleanup_error'] = str(error)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.touch(mode=0o600)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print('cleaned:', report['cleaned'], 'error:', report.get('error'), flush=True)
    return 0 if report['cleaned'] and len(report['cases']) == (1 if args.research_only else 7) and all(all(c['checks'].values()) for c in report['cases']) else 1


if __name__ == '__main__':
    assert not all(plan_checks({}, 'test').values()), '空回执不能通过'
    assert tool_results([{'role': 'assistant', 'content': '已保存'}]) == {}, '文字承诺不算工具结果'
    assert same_time('2031-05-18T02:20:00Z', '2031-05-18T10:20:00+08:00')
    assert not same_time('2031-05-18T10:20:00Z', '2031-05-18T10:20:00+08:00')
    raise SystemExit(main())
