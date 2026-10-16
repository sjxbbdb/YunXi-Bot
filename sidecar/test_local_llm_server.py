#!/usr/bin/env python3
"""设备适配回归：通过实际加载/卸载入口，模拟硬件；不下载权重、不依赖 torch。"""
import sys
import unittest
import io
import json
from types import SimpleNamespace
from unittest import mock

import local_llm_server as server


class RequestTests(unittest.TestCase):
    def test_chat_only_model_rejects_tools_before_loading(self):
        handler = server.Handler.__new__(server.Handler)
        handler.path = "/v1/chat/completions"
        body = json.dumps({"messages": [{"role": "user", "content": "读文件"}],
                           "tools": [{"function": {"name": "read_file"}}]}).encode()
        handler.headers = {"Content-Length": str(len(body))}
        handler.rfile = io.BytesIO(body)
        handler._send = mock.Mock()
        with mock.patch.object(server, "_model_name", "Qwen3.5-9B-4bit"), \
             mock.patch.object(server, "load") as load:
            handler.do_POST()
            load.assert_not_called()
        self.assertEqual(handler._send.call_args.args[0], 400)
        self.assertIn("仅提供聊天", handler._send.call_args.args[1]["error"])

    def test_zero_temperature_reaches_generator_unchanged(self):
        handler = server.Handler.__new__(server.Handler)
        handler.path = "/v1/chat/completions"
        body = json.dumps({"messages": [{"role": "user", "content": "你好"}],
                           "temperature": 0, "stream": True}).encode()
        handler.headers = {"Content-Length": str(len(body))}
        handler.rfile = io.BytesIO(body)
        handler._stream = mock.Mock()
        with mock.patch.object(server, "_model", object()):
            handler.do_POST()
        self.assertEqual(handler._stream.call_args.args[2], 0)

    def test_invalid_requests_never_start_model_loading(self):
        valid = {"messages": [{"role": "user", "content": "你好"}]}
        cases = [{**valid, "temperature": float("nan")}, {**valid, "max_tokens": 0},
                 {**valid, "max_tokens": 9000}, {**valid, "model": "wrong-model"},
                 {**valid, "thinking": {"type": "guess"}}, {**valid, "tools": [None]},
                 {**valid, "tools": {}},
                 {"messages": []}]
        for request in cases:
            with self.subTest(request=request):
                handler = server.Handler.__new__(server.Handler)
                handler.path = "/v1/chat/completions"
                body = json.dumps(request).encode()
                handler.headers = {"Content-Length": str(len(body))}
                handler.rfile = io.BytesIO(body)
                handler._send = mock.Mock()
                with mock.patch.object(server, "load") as load:
                    handler.do_POST()
                    load.assert_not_called()
                self.assertEqual(handler._send.call_args.args[0], 400)

    def test_mlx_generation_error_does_not_leave_generation_lock_held(self):
        with mock.patch.object(server, "_device", "mlx"), \
             mock.patch.object(server, "_model", object()), \
             mock.patch.object(server, "_tokenizer", object()), \
             mock.patch("mlx_backend.generate_events", side_effect=RuntimeError("failed")):
            with self.assertRaises(RuntimeError):
                list(server.stream_generate([], 10, 0))
        acquired = server._generate_lock.acquire(blocking=False)
        self.assertTrue(acquired)
        if acquired:
            server._generate_lock.release()


class DeviceTests(unittest.TestCase):
    def setUp(self):
        for name, value in {
            "_model": None, "_tokenizer": None, "_model_error": None,
            "_loading": False, "_ever_loaded": False, "_device": None,
        }.items():
            patcher = mock.patch.object(server, name, value, create=True)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.torch = SimpleNamespace(
            cuda=SimpleNamespace(
                is_available=mock.Mock(return_value=False),
                is_bf16_supported=mock.Mock(return_value=True),
                empty_cache=mock.Mock(),
                memory_reserved=mock.Mock(return_value=0),
            ),
            backends=SimpleNamespace(mps=SimpleNamespace(
                is_available=mock.Mock(return_value=False),
            )),
            mps=SimpleNamespace(empty_cache=mock.Mock()),
            set_num_threads=mock.Mock(),
            bfloat16="bf16", float16="fp16", float32="fp32",
        )
        self.model = mock.Mock()
        self.model.to.return_value = self.model
        self.loader = mock.Mock(return_value=self.model)
        self.tokenizer = mock.Mock()
        transformers = SimpleNamespace(
            AutoModelForCausalLM=SimpleNamespace(from_pretrained=self.loader),
            AutoTokenizer=SimpleNamespace(from_pretrained=self.tokenizer),
        )
        patcher = mock.patch.dict(sys.modules, torch=self.torch, transformers=transformers)
        patcher.start()
        self.addCleanup(patcher.stop)

    def assert_loaded(self, requested, device, dtype):
        server.load("test-model-without-weights", requested)
        self.assertIsNone(server._model_error)
        self.assertIs(server._model, self.model)
        self.model.to.assert_called_once_with(device)
        self.assertEqual(self.loader.call_args.kwargs["dtype"], dtype)
        self.assertEqual(server._device, device)
        handler = server.Handler.__new__(server.Handler)
        handler.path = "/health"
        handler._send = mock.Mock()
        handler.do_GET()
        self.assertEqual(handler._send.call_args.args[1]["device"], device)

    def test_auto_loads_on_apple_gpu_without_cuda(self):
        self.torch.backends.mps.is_available.return_value = True
        self.assert_loaded("auto", "mps", "fp16")

    def test_auto_preserves_cuda_on_supported_windows_hardware(self):
        self.torch.cuda.is_available.return_value = True
        self.assert_loaded("auto", "cuda", "bf16")

    def test_cuda_without_bfloat16_uses_float16(self):
        self.torch.cuda.is_available.return_value = True
        self.torch.cuda.is_bf16_supported.return_value = False
        self.assert_loaded("auto", "cuda", "fp16")

    def test_auto_works_without_any_gpu(self):
        self.assert_loaded("auto", "cpu", "fp32")

    def test_explicit_cpu_is_respected_even_with_a_gpu(self):
        self.torch.backends.mps.is_available.return_value = True
        self.assert_loaded("cpu", "cpu", "fp32")

    def test_unavailable_explicit_gpu_fails_before_loading_weights(self):
        for device in ["cuda", "mps"]:
            with self.subTest(device=device):
                server._model_error = None
                server.load("test-model-without-weights", device)
                self.assertIsNone(server._model)
                self.assertIn(device, server._model_error)
                self.loader.assert_not_called()
                self.tokenizer.assert_not_called()

    def test_mps_unload_releases_the_correct_allocator(self):
        self.torch.backends.mps.is_available.return_value = True
        server._model = self.model
        server._ever_loaded = True
        server._device = "mps"
        self.assertTrue(server.unload())
        self.assertIsNone(server._model)
        self.torch.mps.empty_cache.assert_called()
        self.torch.cuda.empty_cache.assert_not_called()

    def test_failed_mlx_load_reports_original_error_without_trying_pytorch(self):
        with mock.patch("mlx_backend.load_model", side_effect=FileNotFoundError("missing weights")):
            server.load("missing-gemma", "mlx")
        self.loader.assert_not_called()
        with self.assertRaisesRegex(RuntimeError, "missing weights"):
            server.generate([{"role": "user", "content": "你好"}], 10, 0)


if __name__ == "__main__":
    unittest.main()
