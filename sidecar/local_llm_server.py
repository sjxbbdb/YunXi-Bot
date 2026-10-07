#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""本地小模型服务：OpenAI 兼容的 `/v1/chat/completions`。

## 为什么做成 OpenAI 兼容的端点

`crates/yunxi-bot-core/src/think/agnes.rs` 里的 `OpenAiThinker` **已经会说
这个协议**（Agnes 和 DeepSeek 都走它）。所以本地模型只要长得像它们，
**Rust 侧几乎一行不用改**——加一个 `ThinkerConfig` 就够了。

另起一套协议的话，Rust 侧要加一个 `Thinker` 实现，那是没有必要的分叉。

## 从 Verdict 那两次故障学到的三件事（D104 / D106）

1. **D104：`ThreadingHTTPServer` + 每请求开线程 → 线程耗尽死掉。**
   这里用一个**全局锁**串行化生成：一块 GPU 本来就同时只能跑一个推理，
   排队比并发更诚实。**并发的那些请求不会更快，只会一起变慢然后崩。**

2. **D106：`OMP_NUM_THREADS=1` 会让 torch 加载不了模型。**
   所以这里**一个线程相关的环境变量都不设**——要限制就让
   `torch.set_num_threads()` 去限，那个只影响计算、不影响加载。

3. **模型加载是本进程最慢的一步（几 GB，几十秒）。**
   所以 `/health` 要如实报 `loading`，而且**加载失败要把原因留住**，
   不能只留一句"没加载"——D106 那次查了很久就是因为看不出为什么。

## 用法

    python sidecar/local_llm_server.py                       # 默认 Qwen3-1.7B
    python sidecar/local_llm_server.py --model Qwen3-4B
    python sidecar/local_llm_server.py --port 17872 --device cuda
"""

import argparse
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# ---- 全局状态。用锁保护，因为 HTTP 线程会并发读。----
_lock = threading.Lock()
_model = None
_tokenizer = None
_model_name = ""
_model_error: str | None = None
_loading = False
# **生成串行化**：见文件头 D104 那条。
_generate_lock = threading.Lock()


def resolve_model_dir(model: str) -> str:
    """找模型目录。

    **和 `verdict_server.py:119` 同一套解析**——`YUNXI_BOT_HOME` 优先。
    两份解析一旦漂移，表现就是"便携版读不到盘上的模型"。
    """
    if os.path.isdir(model):
        return model
    home = os.environ.get("YUNXI_BOT_HOME")
    if not home:
        local = os.environ.get("LOCALAPPDATA")
        home = str(Path(local) / "YunXiBot") if local else str(Path.home() / ".yunxi-bot")
    candidate = Path(home) / "models" / model
    if (candidate / "config.json").is_file():
        return str(candidate)
    return model  # 交给 transformers 按仓库名去拉（联网时能用）


def load(model: str, device: str) -> None:
    global _model, _tokenizer, _model_name, _model_error, _loading

    with _lock:
        if _model is not None or _loading:
            return
        _loading = True
        _model_name = model
    path = resolve_model_dir(model)
    try:
        import torch
        from transformers import AutoModelForCausalLM, AutoTokenizer

        # **线程数用 torch 限，不用环境变量。** D106 那次就是环境变量
        # 把模型加载弄挂了——环境变量影响的是整个进程（包括加载），
        # 而 `set_num_threads` 只影响计算。
        torch.set_num_threads(min(8, os.cpu_count() or 4))

        sys.stderr.write(f"[local-llm] 加载 {path}（device={device}）\n")
        t0 = time.time()
        tok = AutoTokenizer.from_pretrained(path, trust_remote_code=True)
        # **不用 `device_map`。** 它要求额外的 `accelerate` 包，而为了
        # "把模型搬到显卡上"引一个新依赖不值得——**手动 `.to()` 就够了**。
        # 参数名用 `dtype`：`torch_dtype` 在 transformers 5.x 已经废弃，
        # 传旧名会打一行警告（而警告多了就没人看了）。
        mdl = AutoModelForCausalLM.from_pretrained(
            path,
            trust_remote_code=True,
            # **bf16**：5060 Ti 支持，比 fp32 省一半显存且更快。
            dtype=torch.bfloat16,
        )
        mdl = mdl.to(device)
        mdl.eval()
        with _lock:
            _tokenizer, _model = tok, mdl
        sys.stderr.write(f"[local-llm] 加载完成，用了 {time.time() - t0:.1f} 秒\n")
    except Exception as e:  # noqa: BLE001
        # **留住原因。** D106 那次"模型没加载"查了很久，
        # 就是因为只看到一句 `model: unloaded`，看不出为什么。
        with _lock:
            _model_error = f"{type(e).__name__}: {e}"
        sys.stderr.write(f"[local-llm] 加载失败：{_model_error}\n")
    finally:
        with _lock:
            _loading = False


def generate(messages: list[dict], max_tokens: int, temperature: float) -> dict:
    """跑一次生成。**全局串行**——一块 GPU 本来同时只能跑一个。"""
    import torch

    with _generate_lock:
        with _lock:
            tok, mdl = _tokenizer, _model
        if tok is None or mdl is None:
            raise RuntimeError(_model_error or "模型未加载")

        # **思考模式关掉。** Qwen3 默认会先"想"再答，那对闲聊是纯浪费——
        # 延迟翻几倍，而闲聊不需要想。模板不支持这个参数时忽略。
        try:
            text = tok.apply_chat_template(
                messages, tokenize=False, add_generation_prompt=True,
                enable_thinking=False,
            )
        except TypeError:
            text = tok.apply_chat_template(
                messages, tokenize=False, add_generation_prompt=True,
            )

        inputs = tok([text], return_tensors="pt").to(mdl.device)
        with torch.no_grad():
            out = mdl.generate(
                **inputs,
                max_new_tokens=max_tokens,
                temperature=max(temperature, 1e-5),
                do_sample=temperature > 0,
                pad_token_id=tok.pad_token_id or tok.eos_token_id,
            )
        gen = out[0][inputs["input_ids"].shape[1]:]
        content = tok.decode(gen, skip_special_tokens=True).strip()
        return {
            "content": content,
            "prompt_tokens": int(inputs["input_ids"].shape[1]),
            "completion_tokens": int(gen.shape[0]),
        }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # noqa: A003
        sys.stderr.write("[local-llm] " + (fmt % args) + "\n")

    def _send(self, code: int, obj: dict) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path.startswith("/health"):
            with _lock:
                if _model is not None:
                    self._send(200, {"status": "ok", "model": _model_name})
                elif _model_error:
                    # **把原因报出来**，不要只说 unloaded。
                    self._send(200, {"status": "degraded", "model": "unloaded",
                                     "detail": _model_error})
                else:
                    self._send(200, {"status": "loading", "model": _model_name})
            return
        if self.path.startswith("/v1/models"):
            self._send(200, {"object": "list", "data": [{"id": _model_name, "object": "model"}]})
            return
        self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if not self.path.startswith("/v1/chat/completions"):
            self._send(404, {"error": "not found"})
            return
        try:
            n = int(self.headers.get("Content-Length") or 0)
            req = json.loads(self.rfile.read(n) or b"{}")
        except Exception as e:  # noqa: BLE001
            self._send(400, {"error": f"bad request: {e}"})
            return
        if req.get("stream"):
            # **不假装支持流式。** 假装的话客户端会一直等 SSE 帧，
            # 而它一条也等不到——那种卡死比明确报错难查得多。
            self._send(400, {"error": "stream=true is not supported by the local model server"})
            return
        # 没加载就先加载（第一个请求会慢，这是有意的：**空闲时不占显存**）
        if _model is None:
            load(_model_name, self.server.device)  # type: ignore[attr-defined]
        try:
            r = generate(
                req.get("messages") or [],
                int(req.get("max_tokens") or 512),
                float(req.get("temperature") or 0.7),
            )
        except Exception as e:  # noqa: BLE001
            self._send(500, {"error": f"{type(e).__name__}: {e}"})
            return
        self._send(200, {
            "id": "local-1",
            "object": "chat.completion",
            "model": _model_name,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": r["content"]},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": r["prompt_tokens"],
                "completion_tokens": r["completion_tokens"],
                "total_tokens": r["prompt_tokens"] + r["completion_tokens"],
            },
        })


def main() -> int:
    # **必须声明 global。** 少了这一行，`_model_name = args.model` 会建一个
    # 局部变量，模块全局那个一直是 ""——表现就是 /health 和 /v1/models
    # 都报空模型名，而服务看起来一切正常。
    global _model_name

    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="Qwen3-1.7B")
    ap.add_argument("--port", type=int, default=17872)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--device", default="cuda")
    ap.add_argument("--warmup", action="store_true",
                    help="启动时就加载（默认是第一个请求才加载）")
    args = ap.parse_args()

    if args.host not in ("127.0.0.1", "localhost", "::1"):
        sys.stderr.write("[local-llm] 只监听回环地址——这是在跑一个能读取器的人。\n")
        return 2

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    srv.device = args.device  # type: ignore[attr-defined]
    with _lock:
        _model_name = args.model
    sys.stderr.write(
        f"[local-llm] 监听 http://{args.host}:{args.port}  "
        f"/v1/chat/completions  /health\n"
    )
    if args.warmup:
        load(args.model, args.device)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
