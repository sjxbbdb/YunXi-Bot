import unittest
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace

from benchmark_mlx import check, content_check, parse_calls, require_all_downloads, split_thinking


class EvaluationTests(unittest.TestCase):
    def test_reasoning_is_excluded_even_when_template_prefills_its_start(self):
        self.assertEqual(split_thinking('推理</think> {"x":1}', '<think>\n', '<think>', '</think>'),
                         (' {"x":1}', '推理'))
        self.assertEqual(split_thinking('<think>推理</think>答案', '', '<think>', '</think>'),
                         ('答案', '推理'))
        self.assertEqual(split_thinking('还没想完', '<think>', '<think>', '</think>'), ('', '还没想完'))
        self.assertEqual(split_thinking('直接回答', '<think></think>', '<think>', '</think>'), ('直接回答', ''))
        self.assertEqual(split_thinking('<thi', '', '<think>', '</think>'), ('', ''))

    def test_json_checks_reject_wrong_types_and_extra_prose(self):
        case = {"group": "objective", "expected": {"approved": False}}
        self.assertTrue(check(case, '{"approved":false}', []))
        self.assertFalse(check(case, '{"approved":0}', []))
        self.assertFalse(check(case, '结果是 {"approved":false}', []))

    def test_tool_checks_require_exact_function_and_arguments(self):
        expected = {"name": "read_file", "arguments": {"path": "/x", "offset": 21}}
        case = {"group": "tool", "expected": expected}
        self.assertTrue(check(case, "", [expected]))
        self.assertFalse(check(case, "我已经读好了", []))
        self.assertFalse(check(case, "", [expected, expected]))

    def test_native_parser_receives_content_without_transport_markers(self):
        tokenizer = SimpleNamespace(tool_call_start="<call>", tool_call_end="</call>",
                                    tool_parser=lambda body, tools: {"name": body})
        self.assertEqual(parse_calls("<call>read_file</call>", tokenizer, []), [{"name": "read_file"}])
        self.assertEqual(parse_calls("普通回复", tokenizer, []), [])

    def test_tool_arguments_do_not_accept_bool_as_integer(self):
        case = {"group": "tool", "expected": {"name": "read_file", "arguments": {"offset": 1}}}
        self.assertFalse(check(case, "", [{"name": "read_file", "arguments": {"offset": True}}]))

    def test_independent_calls_ignore_order_but_not_duplicates(self):
        calls = [{"name": "get_weather", "arguments": {"city": city}} for city in ["南京", "上海"]]
        case = {"group": "tool", "expected_calls": calls}
        self.assertTrue(check(case, "", calls[::-1]))
        self.assertFalse(check(case, "", [calls[0], calls[0]]))

    def test_no_call_requires_a_reply_and_respects_exact_text(self):
        case = {"group": "tool", "expected_calls": [], "expected_text": "收到"}
        self.assertTrue(check(case, "收到", []))
        self.assertFalse(check(case, "", []))
        self.assertFalse(check(case, "收到", [{"name": "delete_file"}]))

    def test_code_fences_count_as_content_correct_but_format_wrong(self):
        case = {"group": "objective", "expected": {"approved": False}}
        text = '```json\n{"approved":false}\n```'
        self.assertFalse(check(case, text, []))
        self.assertTrue(content_check(case, text))
        self.assertTrue(content_check(case, text.replace("```json", "```JSON")))
        self.assertFalse(content_check(case, '结果是 '+text))
        self.assertFalse(content_check(case, '{"approved":0}'))

    def test_no_model_can_start_until_every_download_is_ready(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            models = [{"repo_id": "local/"+name, "revision": "fixed", "bytes": 1,
                       "files": [{"path": "weights", "size": 1}]} for name in ["a", "b"]]
            for model in models:
                dest = root / model["repo_id"].split("/")[-1]
                dest.mkdir()
                (dest / "weights").write_bytes(b"x")
            marker = {"revision": "fixed", "bytes": 1}
            (root / "a/benchmark-verified.json").write_text(json.dumps(marker))
            with self.assertRaisesRegex(ValueError, "b"):
                require_all_downloads(models, root)
            (root / "b/benchmark-verified.json").write_text(json.dumps(marker))
            require_all_downloads(models, root)
            (root / "b/weights.aria2").touch()
            with self.assertRaises(ValueError):
                require_all_downloads(models, root)
            (root / "b/weights.aria2").unlink()
            (root / "b/weights").write_bytes(b"")
            with self.assertRaises(ValueError):
                require_all_downloads(models, root)
