#!/usr/bin/env python3
"""借一封已读的云熙测试邮件验收巡检，结束时恢复已读；不发送邮件、不建立测试邮箱。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from e2e_agenda import native
from e2e_common import read_ledger

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--account', required=True)
    parser.add_argument('--mailbox', required=True, help='JSON邮箱路径数组')
    parser.add_argument('--id', required=True)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    scope = dict(account_id=args.account, mailbox_path=json.loads(args.mailbox))
    location = scope | dict(id=args.id)
    report = dict(checks={}, real_email_sent=False)

    def check(name, ok):
        report['checks'][name] = bool(ok)
        if not ok:
            raise AssertionError(name)
        print('PASS ' + name, flush=True)

    original = native('mac_mail_read', location)
    check('only_existing_read_test_message', original['read'] is True
          and '云熙' in original['subject'] and '测试' in original['subject'])
    check('no_unrelated_unread_mail', native('mac_mail_search', scope | dict(unread_only=True, limit=1))['total_matches'] == 0)
    try:
        native('mac_mail_mark_read', location | dict(expected_revision=original['revision'], read=False))
        with tempfile.TemporaryDirectory(prefix='yunxi-notification-e2e-') as tmp:
            home = Path(tmp)
            (home/'mac-mail.json').write_text(json.dumps([scope]))
            env = dict(os.environ, YUNXI_BOT_HOME=str(home))

            def run(*argv):
                p = subprocess.run([str(ROOT/'target/debug/yunxi-bot'), *argv], env=env,
                                   capture_output=True, text=True, timeout=120)
                if p.returncode:
                    raise RuntimeError('CLI失败: ' + p.stderr[-500:])

            def events(kind):
                rows = read_ledger(home/'ledger.jsonl')
                return [r['data'] for r in rows if r.get('kind') == kind]

            run('check', '--mail-source', 'mac', '--dry-run')
            triage = events('info_triaged')
            check('real_laya_reads_message_without_degradation', len(triage) == 1
                  and triage[0]['used_model'] is True and triage[0]['degraded'] is False)
            report['real_laya_action'] = triage[0]['action']
            check('inspection_does_not_mark_mail_read', native('mac_mail_read', location)['read'] is False)

            # 固定规则单独验证通知出口，不把模型恰巧说Speak当成基础设施通过。
            sender = triage[0]['from']
            (home/'triage.json').write_text(json.dumps(dict(block_senders=[sender])))
            run('check', '--mail-source', 'mac')
            check('blocked_sender_silent', events('info_triaged')[-1]['action'] == 'Quiet'
                  and not [e for e in events('notice_sent') if not e['dry_run']])
            (home/'triage.json').write_text(json.dumps(dict(allow_senders=[sender])))
            run('check', '--mail-source', 'mac')
            sent = [e for e in events('notice_sent') if not e['dry_run']]
            check('native_notification_confirmed_and_audited', len(sent) == 1 and sent[0]['confirmed'] is True)
            run('check', '--mail-source', 'mac')
            check('new_process_does_not_repeat_notice', len([e for e in events('notice_sent') if not e['dry_run']]) == 1)
            run('daemon', '--mail-source', 'mac', '--interval', '1000', '--assistant-interval', '1',
                '--max-ticks', '2', '--task-interval', '0', '--judge-every', '0',
                '--endpoint', 'http://127.0.0.1:17870/decide')
            check('daemon_reuses_persisted_dedup', len([e for e in events('notice_sent') if not e['dry_run']]) == 1
                  and len(events('info_fetched')) >= 6)
            check('still_unread_after_daemon', native('mac_mail_read', location)['read'] is False)
    except Exception as e:
        report['error'] = str(e)
    finally:
        try:
            current = native('mac_mail_read', location)
            if current['read'] is False:
                native('mac_mail_mark_read', location | dict(expected_revision=current['revision'], read=True))
            check('original_read_state_restored', native('mac_mail_read', location)['read'] is True)
        except Exception as e:
            report['cleanup_error'] = str(e)
        report['passed'] = all(report['checks'].values()) and not any(k in report for k in ('error', 'cleanup_error'))
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.touch(mode=0o600)
        args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
