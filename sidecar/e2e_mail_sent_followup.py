#!/usr/bin/env python3
"""复用已发送测试信，验收发件记录→来源日程→Mac通知；没有发送权限。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

from e2e_agenda import NAMES, native
from e2e_common import ensure_fresh_binary, read_ledger, real_home_dir
from e2e_schedule_coordination import saved_maps
from e2e_trip import cleanup, fixture_items, same_time, tool_results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sent-location', required=True, help='已获授权的测试信在Mac已发送中的定位JSON')
    parser.add_argument('--draft-id', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error('报告已存在，请换路径保留失败证据')
    location = json.loads(args.sent_location)
    before = native('mac_mail_read', location)
    receipt = native('mac_mail_outbox', dict(draft_id=args.draft_id))
    # 与生产Mail.js相同的行分隔符规范化；不能抹掉实际文字或内部换行差异。
    import re
    normalize = lambda s: re.sub(r'^[ \t]+$', '', s.replace('\u2028','\n'), flags=re.M).strip()
    assert before['next_offset'] is None and receipt['next_offset'] is None
    assert receipt['status']=='handed_off' and before['to']==receipt['to'] and before['subject']==receipt['subject']
    assert normalize(before['body'])==normalize(receipt['body'])
    assert not before['attachments'] and not receipt['attachments'], '此验收仅复用先前无附件的测试回信'
    source = dict(account_id=location['account_id'],message_id=before['message_id'])
    binary = ensure_fresh_binary()
    prefix = '云熙发件跟进验收'+uuid.uuid4().hex[:8]+'：'
    report = dict(prefix=prefix, cases=[], cleaned=False, new_mail_sent=False, original=before, outbox=receipt)
    allowed = {'now','ask_user','mac_mail_outbox','mac_mail_read','mac_mail_context','mac_mail_thread',
               'mac_agenda_plan','mac_agenda_apply','mac_trip_plan','mac_trip_apply','mac_calendar_list','mac_reminders_list','mac_notify'}
    try:
        with tempfile.TemporaryDirectory(prefix='yunxi-sent-followup-') as tmp:
            home = Path(tmp);real = real_home_dir()
            (home/'models').symlink_to(real/'models',target_is_directory=True)
            (home/'secrets').mkdir(mode=0o700)
            key=home/'secrets/deepseek.key';key.touch(mode=0o600);key.write_bytes((real/'secrets/deepseek.key').read_bytes())
            scenarios = [
                ('verify-and-followup',f'做发件后的合成联动测试，不发新邮件。先查本机发件记录{args.draft_id}，再读Mac已发送测试信，定位为{json.dumps(location,ensure_ascii=False)}。'
                 f'核对正文和收件人一致后，按真实邮件来源关联一个合成事项“{prefix}跟进答复”：2031年5月19日09:00至09:10，固定10分钟，同步到Mac日历和待办，保留其他安排。'
                 '成功后通过Mac通知告诉我测试信已在本机已发送中核实、合成跟进已保存。标题用“云熙 · 邮件｜发送状态”，正文明确标注“合成联动验收”，不能说刚发送新邮件或对方已经收到。'),
                ('recover-linked-followup',f'读取已发送测试邮件{json.dumps(location,ensure_ascii=False)}关联的“{prefix}跟进答复”，核对Mac当前日历和待办是否保存、何时跟进、是否完成。只查，不重复建、不通知、不发送。'),
            ]
            prior_ids = None
            for label,prompt in scenarios:
                permitted=allowed if prior_ids is None else allowed-{'mac_agenda_apply','mac_trip_apply','mac_notify'}
                start=len(read_ledger(home/'ledger.jsonl'))
                argv=[str(binary),'chat','--id',label,'--thinking','off']
                for name in NAMES:argv+=['--allow' if name in permitted else '--deny',name]
                p=subprocess.run(argv,cwd=home,env=dict(os.environ,YUNXI_BOT_HOME=str(home)),input=prompt+'\n/exit\n',text=True,capture_output=True,timeout=300)
                history=json.loads((home/'sessions'/f'{label}.json').read_text())['layout']['history']
                results=tool_results(history)
                records=[e['data'] for e in read_ledger(home/'ledger.jsonl')[start:] if e.get('kind')=='tool_called']
                items=fixture_items(prefix);maps,tasks=saved_maps(items,prefix)
                ids={(kind,row['title']):row['id'] for kind,row in items}
                checks=dict(process_ok=p.returncode==0 and '这一轮失败' not in p.stderr,
                            all_tools_ok=bool(records) and all(r.get('executed') and r.get('outcome')=='ok' for r in records),
                            only_scoped_tools=all(r['tool'] in permitted for r in records),
                            no_mail_writes=all(r['tool'] not in {'mac_mail_send','mac_mail_draft','mac_mail_mark_read','mac_mail_archive'} for r in records),
                            context_read=bool(results.get('mac_mail_context')),
                            exact_system_counts=len(maps['calendar'])==1 and len(maps['reminder'])==1,
                            source_preserved=bool(tasks) and all(t.get('source_mail')==source for t in tasks.values()),
                            calendar_source_preserved=bool(maps['calendar']) and all(json.loads(row['notes'].split('\n',1)[1]).get('source_mail')==source for row in maps['calendar'].values()),
                            correct_start=same_time(maps['calendar'].get('跟进答复',{}).get('start'),'2031-05-19T09:00:00+08:00'),
                            correct_end=same_time(maps['calendar'].get('跟进答复',{}).get('end'),'2031-05-19T09:10:00+08:00'),
                            unfinished=maps['reminder'].get('跟进答复',{}).get('completed') is False)
                if prior_ids is None:
                    checks.update(outbox_read=bool(results.get('mac_mail_outbox')), sent_read=bool(results.get('mac_mail_read')),
                                  saved=any(r.get('applied') is True for tool in ('mac_agenda_apply','mac_trip_apply') for r in results.get(tool,[])),
                                  notification_confirmed=any(r.get('kind')=='confirmed' for r in results.get('mac_notify',[])))
                else:
                    checks.update(stable_ids=ids==prior_ids, no_writes=all(r['tool'] not in {'mac_agenda_apply','mac_trip_apply','mac_notify'} for r in records))
                prior_ids=ids
                report['cases'].append(dict(case=label,checks=checks,records=records,history=history,stdout=p.stdout,stderr=p.stderr,system_items=items))
                print(label,checks,flush=True)
                if not all(checks.values()):raise AssertionError('场景未通过：'+label)
            report['original_mail_unchanged']=native('mac_mail_read',location)['revision']==before['revision']
    except Exception as error:
        report['error']=str(error)
    finally:
        try:report['cleaned']=cleanup(prefix)
        except Exception as error:report['cleanup_error']=str(error)
        report['passed']=len(report['cases'])==2 and all(all(c['checks'].values()) for c in report['cases']) and report['cleaned'] and report.get('original_mail_unchanged',False) and 'error' not in report
        args.output.parent.mkdir(parents=True,exist_ok=True);args.output.touch(mode=0o600);args.output.write_text(json.dumps(report,ensure_ascii=False,indent=2))
    print(json.dumps({k:report.get(k) for k in ('passed','cleaned','error','cleanup_error')}),flush=True)
    return 0 if report['passed'] else 1


if __name__=='__main__':raise SystemExit(main())
