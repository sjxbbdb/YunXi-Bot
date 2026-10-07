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

# **加载串行化。** 见 `load()`：第二个调用者要**等**第一个加载完，
# 而不是掉头就走——启动那几十秒正是使用者最可能说话的时候。
_load_lock = threading.Lock()


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
    """加载模型。**并发调用会排队**：后到的等前面那个加载完，然后直接返回。

    ## 为什么要排队，而不是"有人在加载就掉头走"

    原来是"有人在加载就直接 return"。而配合 `--warmup` 启动时，
    端口是在 `load()` **之前**就 bind 好的（先建 `ThreadingHTTPServer`、
    再加载、最后才 `serve_forever()`）。于是启动后那几十秒里进来的请求
    会一路走到 `generate()`，拿到一句"模型未加载"——
    **而那几十秒正是使用者刚打完第一句话的时候。**
    表现就是"第一次说话必然失败，再说一次就好了"。

    排队之后同一个请求只是**慢**，不会失败——而慢本来就是这条路的预期。
    """
    with _load_lock:
        _load_once(model, device)


def _load_once(model: str, device: str) -> None:
    """真正加载。**调用方必须已经持有 `_load_lock`。**"""
    global _model, _tokenizer, _model_name, _model_error, _loading

    with _lock:
        if _model is not None or _model_error is not None:
            # 已经加载好了，或者**上一次已经失败过**。
            # 失败过就不再重试：一次加载要几十秒，而原因已经留在
            # `_model_error` 里（`/health` 会报出来）。每个请求都重试
            # 只会让每一次都白等几十秒，还把真正的原因埋掉。
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


def _encode(tok, mdl, messages: list[dict]):
    """把消息编成模型输入。**流式与非流式共用。**

    两条路的输入必须一模一样，否则"流式"和"不流式"会得到不同结果——
    而那种差异极难排查（同一个问题换个前端就变了个人）。

    **思考模式关掉**：Qwen3 默认会先"想"再答，那对闲聊是纯浪费——
    延迟翻几倍，而闲聊不需要想。模板不支持这个参数时忽略。
    """
    try:
        text = tok.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True,
            enable_thinking=False,
        )
    except TypeError:
        text = tok.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True,
        )
    return tok([text], return_tensors="pt").to(mdl.device)


def _sample_kwargs(tok, inputs, max_tokens: int, temperature: float) -> dict:
    """采样参数。**同样两条路共用**，理由同上。"""
    return dict(
        **inputs,
        max_new_tokens=max_tokens,
        temperature=max(temperature, 1e-5),
        do_sample=temperature > 0,
        pad_token_id=tok.pad_token_id or tok.eos_token_id,
    )


def generate(messages: list[dict], max_tokens: int, temperature: float) -> dict:
    """跑一次生成。**全局串行**——一块 GPU 本来同时只能跑一个。"""
    import torch

    with _generate_lock:
        with _lock:
            tok, mdl = _tokenizer, _model
        if tok is None or mdl is None:
            raise RuntimeError(_model_error or "模型未加载")

        inputs = _encode(tok, mdl, messages)
        with torch.no_grad():
            out = mdl.generate(**_sample_kwargs(tok, inputs, max_tokens, temperature))
        gen = out[0][inputs["input_ids"].shape[1]:]
        content = tok.decode(gen, skip_special_tokens=True).strip()
        return {
            "content": content,
            "prompt_tokens": int(inputs["input_ids"].shape[1]),
            "completion_tokens": int(gen.shape[0]),
        }


def stream_generate(messages: list[dict], max_tokens: int, temperature: float):
    """边生成边产出正文片段，最后产出一次用量。

    ## 为什么本地模型也必须支持流式

    这一条是实测出来的：Rust 侧的工具循环**只走流式**
    （`tool/runner.rs` 一律调 `think_stream`），而这个服务原来直接回
    `400 stream=true is not supported`。后果是——**路由修好、请求真的
    发到本地之后，这一轮依然是失败的**，只是错误从"发给了错的模型"
    换成了"本地模型不支持流式"。少一环，整条路还是不通。

    而且流式在这条路上不只是协议对齐：4B 模型在这台机器上约 20 token/秒，
    **不流式的话使用者要盯着一个不动的光标等十几秒**，
    那正是"延迟感"的来源。

    ## 产出形状

    先若干次 `("delta", 片段)`，最后 `("usage", {...})` 一次。
    错误若发生在**第一个片段之前**，调用方还没发响应头，
    就能正常回一个 500——所以调用方必须先取第一个片段再发头。
    """
    import torch
    from transformers import TextIteratorStreamer

    with _generate_lock:
        with _lock:
            tok, mdl = _tokenizer, _model
        if tok is None or mdl is None:
            raise RuntimeError(_model_error or "模型未加载")

        inputs = _encode(tok, mdl, messages)
        prompt_tokens = int(inputs["input_ids"].shape[1])
        streamer = TextIteratorStreamer(tok, skip_prompt=True, skip_special_tokens=True)
        kwargs = _sample_kwargs(tok, inputs, max_tokens, temperature)
        kwargs["streamer"] = streamer

        # `generate` 是阻塞的，所以放线程里跑，主线程从 `streamer` 取片段。
        # **这不是为了并发**：`_generate_lock` 保证同时只有一个生成在跑。
        err_box: list = []

        def _run() -> None:
            try:
                with torch.no_grad():
                    mdl.generate(**kwargs)
            except BaseException as e:  # noqa: BLE001
                err_box.append(e)
            finally:
                # **成功失败都要收尾。** 少这一句，主线程就会永远等下去
                # ——而且看起来只是"模型很慢"。
                streamer.end()

        threading.Thread(target=_run, daemon=True).start()
        pieces: list[str] = []
        for piece in streamer:
            if piece:
                pieces.append(piece)
                yield "delta", piece
        if err_box:
            raise err_box[0]
        # **生成量自己数，不问 `generate` 的返回值要。**
        #
        # 这是实测出来的：传了 `streamer` 之后，`generate` 返回的张量长度
        # 是**提问那一侧**（13 个 token 的提问，返回长度也是 13），
        # 于是 `completion_tokens` 恒为 0。而台账的成本和上下文锚点都吃
        # 这个数——"0" 会让一段长回答在账上像没发生过。
        #
        # 数法是**把真正流出去的正文重新编码一次**：和 `generate` 那条路
        # 数的是同一个东西（token，不是字符），而且不依赖 transformers
        # 在流式下到底返回什么。代价是边界处可能与真实生成量差一两个
        # token——**这是估算，不是精确值**；本地模型免费，这点误差不进账。
        completion_tokens = 0
        if pieces:
            completion_tokens = int(len(tok("".join(pieces))["input_ids"]))
        yield "usage", {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
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

    def _sse(self, obj: dict) -> None:
        """发一个 SSE 事件。**每条都立刻 flush**——攒着发就等于没流式。"""
        data = json.dumps(obj, ensure_ascii=False)
        self.wfile.write(f"data: {data}\n\n".encode("utf-8"))
        self.wfile.flush()

    def _chunk(self, text: str) -> dict:
        """正文片段。**形状要和 `agnes.rs::read_sse` 认的那个一致**：
        它只认 `data:` 行，正文从 `choices[0].delta.content` 取。"""
        return {
            "id": "local-1",
            "object": "chat.completion.chunk",
            "model": _model_name,
            "choices": [{
                "index": 0,
                "delta": {"content": text},
                "finish_reason": None,
            }],
            "usage": None,
        }

    def _stream(self, messages: list[dict], max_tokens: int, temperature: float) -> None:
        """流式回一轮。"""
        gen = stream_generate(messages, max_tokens, temperature)
        # **先取第一段，再发响应头。**
        # 加载失败、权重不全这类错误都发生在第一个片段之前；那时候还没发头，
        # 就能老老实实回一个 500。一旦把头发出去，剩下的报错手段只有断开连接，
        # 于是"加载失败"会伪装成"模型没回内容"——**而 Rust 那边正是靠
        # 这个区分来判断是配置问题还是模型问题**。
        try:
            first_kind, first_val = next(gen)
        except StopIteration:
            first_kind, first_val = None, None
        except Exception as e:  # noqa: BLE001
            self._send(500, {"error": f"{type(e).__name__}: {e}"})
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        # **必须显式说 close。** 事件流的长度事先不知道，所以给不了
        # Content-Length；而协议是 HTTP/1.1，默认长连接——不说这一句，
        # 客户端会一直等一个永远不会来的 Content-Length。
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

        def _events():
            if first_kind is not None:
                yield first_kind, first_val
            yield from gen

        try:
            for kind, val in _events():
                if kind == "delta":
                    self._sse(self._chunk(val))
                    continue
                # 收尾两块，形状照 OpenAI 的样子来：
                # 先一个只带 finish_reason 的，再一个只带 usage 的。
                # `usage` 单独一块是因为 Rust 那边认"任何一块的非空 usage"，
                # 而它靠这个数算上下文锚点。
                self._sse({
                    "id": "local-1",
                    "object": "chat.completion.chunk",
                    "model": _model_name,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": None,
                })
                self._sse({
                    "id": "local-1",
                    "object": "chat.completion.chunk",
                    "model": _model_name,
                    "choices": [],
                    "usage": {
                        "prompt_tokens": val["prompt_tokens"],
                        "completion_tokens": val["completion_tokens"],
                        "total_tokens": val["prompt_tokens"] + val["completion_tokens"],
                    },
                })
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            # 客户端走了（按了 Ctrl-C、关掉终端）。**这不是错误**，
            # 更不该在这里刷一段栈——那只会让人以为服务坏了。
            gen.close()

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
        # 没加载就先加载（第一个请求会慢，这是有意的：**空闲时不占显存**）
        if _model is None:
            load(_model_name, self.server.device)  # type: ignore[attr-defined]
        messages = req.get("messages") or []
        max_tokens = int(req.get("max_tokens") or 512)
        temperature = float(req.get("temperature") or 0.7)
        if req.get("stream"):
            # **流式是必须支持的**：Rust 侧的工具循环只走流式
            # （`tool/runner.rs` 一律调 `think_stream`）。
            # 这里原来回 400 "not supported"，于是路由修好之后
            # 本地这条路依然一次也走不通。
            self._stream(messages, max_tokens, temperature)
            return
        try:
            r = generate(messages, max_tokens, temperature)
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
