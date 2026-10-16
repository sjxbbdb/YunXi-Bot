import tempfile
import json
from pathlib import Path
import unittest

from e2e_builtin_tools import verify


class EvidenceTests(unittest.TestCase):
    def test_search_requires_an_actual_official_link_and_its_return_to_the_user(self):
        record = {'tool': 'web_search', 'executed': True, 'outcome': 'ok',
                  'output': '官网\n   https://evil.test/rust-lang.org\n限定域名：rust-lang.org'}
        with tempfile.TemporaryDirectory() as tmp:
            self.assertFalse(verify('web_search', [record], record['output'], Path(tmp), '')['evidence'])
            record['output'] = '官网\n   https://www.rust-lang.org/zh-CN/learn\n摘要'
            self.assertFalse(verify('web_search', [record], '没找到', Path(tmp), '')['evidence'])
            self.assertTrue(verify('web_search', [record], 'https://www.rust-lang.org/zh-CN/learn', Path(tmp), '')['evidence'])

    def test_chat_question_requires_a_pending_receipt_and_a_later_human_answer(self):
        record = {'tool': 'ask_user', 'executed': True, 'outcome': 'ok',
                  'output': json.dumps({'waiting_for_user': True, 'question': '选择哪个代号？',
                                        'options': ['白鹭', '青竹'], 'display': '选择哪个代号？\n1. 白鹭\n2. 青竹'})}
        history = [{'role': 'user', 'content': '提问'},
                   {'role': 'tool', 'content': record['output']},
                   {'role': 'assistant', 'content': '选择哪个代号？\n1. 白鹭\n2. 青竹'},
                   {'role': 'user', 'content': '【本轮本机时间：2026-10-16T12:00:00+08:00】\n2'},
                   {'role': 'assistant', 'content': '你选的是青竹。'}]
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            self.assertTrue(verify('ask_user', [record], '青竹', work, '', history)['evidence'])
            self.assertFalse(verify('ask_user', [record], '青竹', work, '', history[:3])['evidence'])
            self.assertFalse(verify('ask_user', [record], '青竹', work, '', history[:4])['evidence'])
            wrong = history[:-1] + [{'role': 'assistant', 'content': '你选的是白鹭。'}]
            self.assertFalse(verify('ask_user', [record], '青竹', work, '', wrong)['evidence'])
            for output in ['invalid JSON', '[]', '{}', '{"waiting_for_user":false}']:
                self.assertFalse(verify('ask_user', [dict(record, output=output)], '青竹', work, '', history)['evidence'])

    def test_model_claim_without_tool_call_never_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            checks = verify('read_file', [], '暗号已读取', Path(tmp), '暗号')
        self.assertFalse(all(checks.values()))

    def test_successful_write_claim_requires_actual_correct_file(self):
        record = {'tool': 'write_file', 'executed': True, 'outcome': 'ok', 'output': '已写入'}
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            self.assertFalse(verify('write_file', [record], '', work, '证据')['evidence'])
            (work / '新建').mkdir()
            (work / '新建/结果.txt').write_text('证据')
            self.assertTrue(all(verify('write_file', [record], '', work, '证据').values()))

    def test_recovered_error_is_still_counted_as_instability(self):
        records = [{'tool': 'read_file', 'executed': False, 'outcome': 'error'},
                   {'tool': 'read_file', 'executed': True, 'outcome': 'ok', 'output': '暗号'}]
        with tempfile.TemporaryDirectory() as tmp:
            checks = verify('read_file', records, '暗号', Path(tmp), '暗号')
        self.assertTrue(checks['evidence'])
        self.assertFalse(checks['no_tool_errors'])


if __name__ == '__main__':
    unittest.main()
