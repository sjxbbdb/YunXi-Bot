"""协议测试不加载权重；原生工具语法使用已固定版本的 MLX-LM 解析器。"""
import json
import unittest
import threading
from types import SimpleNamespace
from unittest.mock import Mock, patch

import mlx_backend as backend

TOOLS = [{"type": "function", "function": {"name": "read_file", "description": "读文件",
          "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}]
TOK = SimpleNamespace(tool_call_start="<|tool_call>", tool_call_end="<tool_call|>",
                      think_start="<|channel>thought", think_end="<channel|>")
CALL = '<|tool_call>call:read_file{path:<|"|>/tmp/中文 a.txt<|"|>}<tool_call|>'


class PromptTests(unittest.TestCase):
    def test_loading_generation_and_close_share_one_worker_across_http_threads(self):
        seen = []
        def load(path):
            seen.append(("load", threading.get_ident()))
            return object(), object()
        def events(*args):
            try:
                seen.append(("generate", threading.get_ident()))
                yield "delta", "你好"
            finally:
                seen.append(("close", threading.get_ident()))
        with patch.object(backend, "_load_model", load), patch.object(backend, "_generate_events", events):
            backend.load_model("test")
            def request():
                stream = backend.generate_events()
                next(stream)
                stream.close()
            thread = threading.Thread(target=request)
            thread.start()
            thread.join()
        self.assertEqual([kind for kind, _ in seen], ["load", "generate", "close"])
        self.assertEqual(len({ident for _, ident in seen}), 1)
        self.assertNotEqual(seen[0][1], threading.get_ident())

    def test_openai_history_becomes_native_arguments_without_mutating_input(self):
        tokenizer = SimpleNamespace(apply_chat_template=Mock(return_value="read_file 提醒"))
        original = [{"role": "assistant", "content": None, "tool_calls": [{
            "id": "call_0", "function": {"name": "read_file", "arguments": '{"path":"/tmp/a"}'}}]},
            {"role": "tool", "tool_call_id": "call_0", "content": "内容"},
            {"role": "user", "content": "总结"}]
        before = json.dumps(original)
        backend.render_prompt(tokenizer, original, TOOLS, True, "提醒")
        sent = tokenizer.apply_chat_template.call_args
        self.assertTrue(sent.kwargs["enable_thinking"])
        self.assertEqual(sent.args[0][1]["tool_calls"][0]["function"]["arguments"], {"path": "/tmp/a"})
        self.assertEqual(json.dumps(original), before)

    def test_template_must_keep_tools_and_reminder(self):
        tokenizer = SimpleNamespace(apply_chat_template=Mock(return_value="你好"))
        with self.assertRaises(ValueError):
            backend.render_prompt(tokenizer, [{"role": "user", "content": "读文件"}], TOOLS, False, "提醒")

    def test_missing_weights_fail_without_downloading(self):
        with self.assertRaises(FileNotFoundError):
            backend.load_model("/nonexistent/yunxi-gemma-test")


class StreamTests(unittest.TestCase):
    def test_qwen_chat_separates_thinking_and_rejects_unoffered_tools(self):
        tok = SimpleNamespace(tool_call_start="<tool_call>", tool_call_end="</tool_call>",
                              think_start="<think>", think_end="</think>")
        events = list(backend.split_stream(iter("想一想</think>今天辛苦了。"), tok, "<think>", None))
        self.assertEqual("".join(v for k, v in events if k == "delta"), "今天辛苦了。")
        self.assertEqual("".join(v for k, v in events if k == "reasoning"), "想一想")
        with self.assertRaisesRegex(ValueError, "没有工具权限"):
            list(backend.split_stream(iter('<tool_call>{"name":"read_file"}</tool_call>'), tok, "", None))

    @classmethod
    def setUpClass(cls):
        try:
            from mlx_lm.tool_parsers import gemma4
        except ImportError:
            raise unittest.SkipTest("请用 .venv-mlx 跑原生工具协议测试")

    def split(self, text, prompt=""):
        return list(backend.split_stream(iter(text), TOK, prompt, TOOLS))

    def test_every_character_can_be_a_fragment_without_leaking_protocol(self):
        events = self.split("先看看。" + CALL + "稍等。")
        self.assertEqual("".join(v for k, v in events if k == "delta"), "先看看。稍等。")
        calls = [v for k, v in events if k == "tool_call"]
        self.assertEqual(json.loads(calls[0][0][1]), {"path": "/tmp/中文 a.txt"})

    def test_thought_tool_text_is_never_executed_or_shown_as_answer(self):
        events = self.split("<|channel>thought" + CALL + "<channel|>回答")
        self.assertFalse(any(k == "tool_call" for k, _ in events))
        self.assertEqual("".join(v for k, v in events if k == "delta"), "回答")
        events = self.split("思考<channel|>答案", "<|channel>thought")
        self.assertEqual(''.join(v for k,v in events if k == 'reasoning'), '思考')

    def test_reasoning_streams_before_the_closing_marker(self):
        def pieces():
            yield '正在分析'
            raise AssertionError('首段应已经交给客户端，不能等待整个思考结束')
        stream = backend.split_stream(pieces(), TOK, TOK.think_start, TOOLS)
        self.assertEqual(next(stream), ('reasoning', '正在分析'))
        stream.close()

    def test_truncated_markers_and_call_bodies_fail_closed(self):
        for text in ["<|tool_", CALL[:-1], "<|channel>thought思考", "<|tool_call>call:read_file{"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                self.split(text)

    def test_unknown_or_malformed_calls_cannot_be_salvaged(self):
        for body in ['call:unknown{}', 'junk call:read_file{}', 'call:read_file{} junk',
                     'call:read_file{path:NaN}', 'call:read_file{}call:read_file{}']:
            with self.subTest(body=body), self.assertRaises(ValueError):
                backend.parse_call(body, TOOLS)

    def test_failed_or_truncated_generation_never_commits_earlier_calls(self):
        import mlx_lm
        tok = SimpleNamespace(**vars(TOK), encode=Mock(return_value=[1]))
        for ending, reason in [("<|tool_call>bad", "stop"), ("", "length")]:
            emitted = []
            def responses(*args, **kwargs):
                yield SimpleNamespace(text=CALL + ending, prompt_tokens=1,
                                      generation_tokens=20, finish_reason=reason)
            with patch.object(backend, "render_prompt", return_value="prompt"), \
                 patch.object(mlx_lm, "stream_generate", responses):
                with self.assertRaises(ValueError):
                    for event in backend.generate_events(object(), tok, [], TOOLS, 100, 0, False, ""):
                        emitted.append(event)
            self.assertFalse(any(k == "tool_call" for k, _ in emitted))


if __name__ == "__main__":
    unittest.main()
