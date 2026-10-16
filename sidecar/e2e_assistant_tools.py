#!/usr/bin/env python3
"""真实 DeepSeek/Laya → 第二批工具。合成联系人/文档/本机草稿，发送始终deny。"""
import argparse
import datetime
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid
from e2e_common import read_ledger, ensure_fresh_binary, real_home_dir
from e2e_agenda import NAMES, native


def clarified_before_draft(history):
    users=[i for i,m in enumerate(history) if m.get('role')=='user']
    drafts=[i for i,m in enumerate(history) if any(c.get('function',{}).get('name')=='mac_mail_draft' for c in m.get('tool_calls',[]))]
    if len(users)<2 or not drafts or min(drafts)<=users[1]:return False
    return any(m.get('role')=='assistant' and ('？' in m.get('content','') or '?' in m.get('content','')) for m in history[users[0]+1:users[1]]) or any(c.get('function',{}).get('name')=='ask_user' for m in history[users[0]+1:users[1]] for c in m.get('tool_calls',[]))


def main():
    parser=argparse.ArgumentParser();parser.add_argument('--out',type=Path,required=True);args=parser.parse_args()
    binary=ensure_fresh_binary();real_home=real_home_dir();cases=[]
    tomorrow=(datetime.date.today()+datetime.timedelta(days=1)).isoformat()
    token=uuid.uuid4().hex[:10]
    definitions=[
        ('contacts',{'mac_contacts_search'},f'请在Mac通讯录查找“云熙不存在联系人{token}”，如实说明结果。',{'mac_contacts_search'}),
        ('document',{'mac_document_read'},'用mac_document_read读取当前目录的附件.txt，告诉我验收口令。',{'mac_document_read'}),
        ('places',{'mac_places_search','web_search','web_fetch'},'查询上海博物馆人民广场馆的准确地址和开放时间。营业时间请核对博物馆官网，不能凭记忆。',{'mac_places_search','web_search'}),
        ('travel',{'now','mac_route','mac_weather','mac_agenda_plan'},
         f'只做预排，不保存：{tomorrow}15点开会一小时；起点31.228827,121.475213，终点31.2411,121.4905，步行。出发准备15分钟。查真实路线和天气，把准备加路途放到同日行程里，和会议绑定。请保留默认时段及间隔。',{'mac_route','mac_weather','mac_agenda_plan'}),
        ('recipient_followup',{'ask_user','mac_mail_accounts','mac_mail_draft'},
         f'做一个不发送的合成邮件测试。这里有两位张经理：研发部zhang.a@example.com、采购部zhang.b@example.com。给张经理起草一封邮件，主题“云熙收件人验收-{token}”，正文“明天开会”。用Mail账户的发件地址。\n研发那位',{'mac_mail_accounts','mac_mail_draft'}),
    ]
    with tempfile.TemporaryDirectory(prefix='yunxi-assistant-e2e-') as temp:
        for label,allowed,prompt,required in definitions:
            home=Path(temp)/label;home.mkdir(mode=0o700)
            (home/'models').symlink_to(real_home/'models',target_is_directory=True)
            (home/'secrets').mkdir(mode=0o700)
            for name in ('deepseek.key','bocha.key'):
                source=real_home/'secrets'/name
                if source.exists():
                    target=home/'secrets'/name;target.touch(mode=0o600);target.write_bytes(source.read_bytes())
            (home/'附件.txt').write_text('验收口令：青竹-'+token)
            argv=[str(binary),'chat','--id',label,'--thinking','off']
            for tool in NAMES:argv+=['--allow' if tool in allowed else '--deny',tool]
            result=subprocess.run(argv,cwd=home,env=dict(os.environ,YUNXI_BOT_HOME=str(home)),input=prompt+'\n/exit\n',text=True,capture_output=True,timeout=240)
            events=read_ledger(home/'ledger.jsonl')
            records=[e['data'] for e in events if e.get('kind')=='tool_called']
            successful=[r for r in records if r.get('executed') and r.get('outcome')=='ok']
            # 台账有意截短工具正文，证据从保存的完整工具会话读取，仍核对台账执行状态。
            session=json.loads((home/'sessions'/f'{label}.json').read_text())
            history=session['layout']['history'];call_names={};by_name={}
            for message in history:
                for call in message.get('tool_calls',[]):call_names[call['id']]=call['function']['name']
                name=call_names.get(message.get('tool_call_id'))
                if name and name.startswith('mac_'):
                    try:by_name[name]=json.loads(message['content'])
                    except (ValueError,KeyError):pass
            checks={'all_required':required.issubset({r['tool'] for r in successful}),'only_allowed':all(r['tool'] in allowed for r in records),
                    'no_tool_error':bool(records) and all(r.get('outcome')=='ok' for r in records),
                    'completed':result.returncode==0 and '这一轮失败' not in result.stderr and '这一轮未执行' not in result.stderr}
            if label=='contacts':checks['empty_is_honest']=by_name.get('mac_contacts_search',{}).get('items')==[]
            if label=='document':checks['actual_text']=('青竹-'+token) in result.stdout and by_name.get('mac_document_read',{}).get('next_offset') is None
            if label=='places':checks['address_returned']=bool(by_name.get('mac_places_search',{}).get('items'))
            if label=='travel':
                plan=by_name.get('mac_agenda_plan',{});route=by_name.get('mac_route',{})
                linked=[e for e in plan.get('entries',[]) if e.get('task',{}).get('travel_for')]
                checks['travel_bound_and_sized']=len(linked)==1 and linked[0]['task']['duration_minutes']==route.get('travel_minutes',-100)+15 and linked[0].get('start') is not None
                checks['not_saved']=plan.get('applied') is False
            if label=='recipient_followup':
                draft=by_name.get('mac_mail_draft',{});checks['correct_recipient']=draft.get('to')==['zhang.a@example.com'] and draft.get('sent') is False
                checks['asked_before_draft']=clarified_before_draft(history)
                if draft.get('draft_id'):
                    cleanup=native('mac_mail_draft_discard',{'draft_id':draft['draft_id'],'expected_revision':draft['revision']})
                    checks['draft_cleaned']=cleanup.get('discarded') is True and cleanup.get('degraded') is False
            cases.append(dict(case=label,checks=checks,records=records,history=history,stdout=result.stdout,stderr=result.stderr))
            args.out.parent.mkdir(parents=True,exist_ok=True);args.out.touch(mode=0o600);args.out.write_text(json.dumps(cases,ensure_ascii=False,indent=2))
            print(label,checks,flush=True)
    return 0 if all(all(c['checks'].values()) for c in cases) else 1


if __name__=='__main__':raise SystemExit(main())
