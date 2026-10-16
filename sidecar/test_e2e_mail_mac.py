import json
from pathlib import Path
import tempfile
import unittest

from e2e_mail_mac import authorized_test_mail, expected_terms_present, observed_notification


class ObservedNotificationTests(unittest.TestCase):
    def test_authorized_sender_must_match_address_not_display_name_or_substring(self):
        expected = 'sender@example.com'
        self.assertTrue(authorized_test_mail({'subject': '测试', 'sender': '测试者 <SENDER@example.com>'}, expected))
        for sender in ('other@example.com', 'sender@example.com.evil.test', 'sender@example.com <other@example.com>'):
            self.assertFalse(authorized_test_mail({'subject': '测试', 'sender': sender}, expected))
        self.assertFalse(authorized_test_mail({'subject': '非测试', 'sender': expected}, expected))
        self.assertFalse(authorized_test_mail({'subject': '测试', 'sender': ''}, ''))

    def test_synonyms_do_not_skip_other_facts_or_accept_empty_terms(self):
        terms = ['面试|复试', '简历', '17:00']
        self.assertTrue(expected_terms_present('复试需要简历，17:00前回复', terms))
        self.assertFalse(expected_terms_present('复试，17:00前回复', terms))
        self.assertFalse(expected_terms_present('复试', ['面试|']))
        self.assertFalse(expected_terms_present('复试', []))

    def test_only_one_confirmed_real_notification_can_start_the_chain(self):
        location = dict(account_id='test', mailbox_path=['INBOX'], id='1')
        identifier = json.dumps(location)
        entry = dict(head=dict(source='mac_mail', id=identifier), status='notified', analysis={'label': '人事回复'})
        notice = dict(kind='notice_sent', data=dict(source='mac_mail', id=identifier, dry_run=False, confirmed=True))
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            state_path = home/'mail-review-state.json'
            state_path.write_text(json.dumps({'entries': {'selected': entry}}))
            ledger_path = home/'ledger.jsonl'
            for notices in ([], [notice, notice], [dict(kind='notice_sent', data=notice['data'] | {'dry_run': True})],
                            [dict(kind='notice_sent', data=notice['data'] | {'confirmed': False})]):
                ledger_path.write_text(''.join(json.dumps(n)+'\n' for n in notices))
                with self.assertRaises(ValueError):
                    observed_notification(home, location)
            ledger_path.write_text(json.dumps(notice)+'\n')
            state, actual = observed_notification(home, location)
            self.assertEqual(actual, notice)
            self.assertEqual(state['entries']['selected'], entry)
            entry['status'] = 'held'
            state_path.write_text(json.dumps({'entries': {'selected': entry}}))
            with self.assertRaises(ValueError):
                observed_notification(home, location)


if __name__ == '__main__':
    unittest.main()
