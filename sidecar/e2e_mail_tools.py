#!/usr/bin/env python3
"""只用随机命名的本机合成邮件/草稿验收。没有发送真实邮件的路径。"""
import argparse
import datetime
import json
import mailbox
import os
from pathlib import Path
import subprocess
import tempfile
import uuid
from email.message import EmailMessage
from email.policy import SMTP

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--jxa', action='store_true', help='开发期直接测试共享Mail.js；正式验收省略此项走.app')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    prefix = 'yunxi-mail-test-' + uuid.uuid4().hex[:10]
    report = dict(prefix=prefix, transport='jxa' if args.jxa else 'native_app', checks={}, real_email_sent=False)
    with tempfile.TemporaryDirectory(prefix=prefix) as tmp:
        base = Path(tmp)

        def jxa(source):
            script = base / 'run.js'
            script.touch(mode=0o600)
            script.write_text(source)
            result = subprocess.run(['/usr/bin/osascript', '-l', 'JavaScript', str(script)], capture_output=True, text=True, timeout=40)
            if result.returncode:
                raise RuntimeError('JXA测试失败：' + result.stderr[:500])
            return json.loads(result.stdout)

        def call(action, params, **internal):
            if action == 'mac_mail_send':
                raise AssertionError('验收严禁真实发送')
            if not args.jxa:
                from e2e_agenda import native
                return native(action, params)
            request = dict(action=action, args=params, state_path=str(base),
                           mail_pid=int(subprocess.check_output(['pgrep', '-x', 'Mail']).strip()), **internal)
            source = (ROOT / 'native/macos/Mail.js').read_text()
            result = jxa(source + '\nconst req=' + json.dumps(request) + ';JSON.stringify(runRequest(req));')
            if result.get('ok') is not True:
                raise RuntimeError(result.get('message', '无有效回执'))
            return result['result']

        def check(name, value):
            report['checks'][name] = bool(value)
            if not value:
                raise AssertionError(name)
            print('PASS ' + name, flush=True)

        before = jxa('const m=Application("com.apple.mail");JSON.stringify(m.mailboxes().map(b=>String(b.name())));')
        folder = base / (prefix + '.mbox'); folder.mkdir()
        box = mailbox.mbox(folder / 'mbox', create=True)
        for i in range(3):
            m = EmailMessage(policy=SMTP)
            m['From'] = 'sender@example.com'; m['To'] = 'recipient@example.com'; m['Subject'] = prefix
            m['Message-ID'] = f'<{prefix}-{i}@example.com>'
            m['Date'] = 'Fri, 16 Oct 2026 09:00:00 +0800'
            if i == 1:
                m['References'] = f'<{prefix}-0@example.com>'; m['In-Reply-To'] = m['References']
            m.set_content(f'合成正文-{i}\n' + ('连续正文'*2200 if i == 0 else '同主题不等于同线程。'))
            if i == 0:
                m.add_attachment('云熙附件验收口令-青竹'.encode(), maintype='text', subtype='plain', filename='fixture.txt')
            box.add(m)
        box.flush(); box.close()
        archive = prefix + '-archive'
        try:
            jxa('const m=Application("com.apple.mail");m.importMailMailbox({at:Path(' + json.dumps(str(folder)) + ')});'
                'm.mailboxes.push(m.Mailbox({name:' + json.dumps(archive) + '}));JSON.stringify(true);')
            accounts = call('mac_mail_accounts', {})['accounts']
            local = next(a for a in accounts if a['id'] == 'local')
            path = next(b['path'] for b in local['mailboxes'] if b['path'][-1] == prefix)
            scope = dict(account_id='local', mailbox_path=path)
            found = call('mac_mail_search', scope | dict(limit=1))
            check('search_paginated', found['total_matches'] == 3 and found['next_offset'] == 1)
            all_items = call('mac_mail_search', scope)['items']
            anchor = next(m for m in all_items if (prefix+'-0@') in m['message_id'])
            locator = scope | dict(id=anchor['id'])
            read = call('mac_mail_read', locator)
            check('body_and_attachment', read['next_offset'] is not None and len(read['attachments']) == 1)
            tail = call('mac_mail_read', locator | dict(offset=read['next_offset']))
            check('read_complete_without_marking', tail['next_offset'] is None and tail['read'] == anchor['read'])
            thread = call('mac_mail_thread', locator)
            check('thread_uses_headers_not_subject', len(thread['items']) == 2 and thread['complete_in_account'])
            save = locator | dict(expected_revision=read['revision'], attachment_id=read['attachments'][0]['id'], path=str(base/'download.txt'))
            if args.jxa:
                call('mac_mail_attachment_save', save, attachment_staging=save['path'])
            else:
                check('attachment_receipt', call('mac_mail_attachment_save', save)['saved'])
            check('attachment_bytes', Path(save['path']).read_text() == '云熙附件验收口令-青竹')
            marked = call('mac_mail_mark_read', locator | dict(expected_revision=read['revision'], read=not read['read']))
            check('mark_read_verified', marked['read'] is not read['read'])
            moved = call('mac_mail_archive', locator | dict(expected_revision=marked['revision'], archive_path=[archive]))
            check('archive_verified', moved['archived'] and not call('mac_mail_search',scope | dict(query=prefix))['total_matches'] == 3)
            moved_locator = {k:moved['item'][k] for k in ('account_id','mailbox_path','id')}
            original = call('mac_mail_read', moved_locator)
            source = moved_locator | dict(expected_revision=original['revision'])
            sender = next(a['addresses'][0] for a in accounts if a['addresses'])
            for mode in ('new','reply','forward'):
                draft_args = dict(mode=mode, sender=sender, to=['yunxi-test@example.com'], subject=prefix+'-'+mode,
                                  body='仅用于合成验收；不会发送。', request_id=prefix+'-'+mode)
                if mode != 'new': draft_args['source'] = source
                if mode == 'new': draft_args['attachments'] = [save['path']]
                draft = call('mac_mail_draft', draft_args)
                check(mode+'_draft_not_sent', draft['sent'] is False and draft['to'] == ['yunxi-test@example.com'])
                check(mode+'_replay_same_draft', call('mac_mail_draft',draft_args)['draft_id'] == draft['draft_id'])
                reread = call('mac_mail_draft_get',dict(draft_id=draft['draft_id']))
                check(mode+'_revision_matches', reread['revision'] == draft['revision'])
                updated = call('mac_mail_draft_update',dict(draft_id=draft['draft_id'],expected_revision=draft['revision'],subject=prefix+'-'+mode+'-updated'))
                check(mode+'_update_preserves_attachments', updated['attachments'] == draft['attachments'])
                try:
                    call('mac_mail_draft_update',dict(draft_id=draft['draft_id'],expected_revision=draft['revision'],subject='stale'))
                    check(mode+'_stale_rejected',False)
                except RuntimeError: check(mode+'_stale_rejected',True)
                check(mode+'_discard',call('mac_mail_draft_discard',dict(draft_id=draft['draft_id'],expected_revision=updated['revision']))['discarded'])
        except Exception as e:
            report['error'] = str(e)
            print('FAIL ' + str(e), flush=True)
        finally:
            # 只清理随机前缀精确标识的测试记录。不会删除其他邮件或已有导入目录。
            source = 'const m=Application("com.apple.mail"),prefix='+json.dumps(prefix)+',before='+json.dumps(before)+';'
            source += '''
m.outgoingMessages().forEach(d=>{if(String(d.subject()).startsWith(prefix))m.delete(d);});
function clean(parent){parent.mailboxes().forEach(b=>{try{const name=String(b.name());if(name===prefix||name===prefix+'-archive')m.delete(b);else clean(b);}catch(e){}});}
clean(m);
m.mailboxes().forEach(b=>{if(before.indexOf(String(b.name()))<0&&String(b.name()).startsWith('导入')&&!b.mailboxes().length&&!b.messages().length)m.delete(b);});
let remains=[];function scan(parent){parent.mailboxes().forEach(b=>{if(String(b.name()).startsWith(prefix))remains.push(String(b.name()));scan(b);});}scan(m);
JSON.stringify({remaining:remains,drafts:m.outgoingMessages().filter(d=>String(d.subject()).startsWith(prefix)).length});'''
            try:
                clean = jxa(source)
                report['cleanup'] = not clean['remaining'] and clean['drafts'] == 0
            except Exception as e: report['cleanup'] = False; report['cleanup_error'] = str(e)
    report['passed'] = bool(report['checks']) and all(report['checks'].values()) and 'error' not in report and report.get('cleanup') is True
    args.out.parent.mkdir(parents=True,exist_ok=True); args.out.touch(mode=0o600); args.out.write_text(json.dumps(report,ensure_ascii=False,indent=2))
    print(json.dumps({'passed':report['passed'],'checks':len(report['checks']),'cleanup':report.get('cleanup')},ensure_ascii=False),flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
