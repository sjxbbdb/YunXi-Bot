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

# ---- 按需加载 / 空闲释放。见 `unload()` 和 `_idle_watchdog()`。----
#
# 这个进程是**分离进程**（`DETACHED_PROCESS`，见
# `crates/yunxi-bot-cli/src/main.rs`）：使用者关掉 `yunxi-bot chat` 之后
# 它还活着，于是那 8 GB 显存一直占着，直到重启机器。而 `nvidia-smi` 上
# 只看得到"卡被占着"，看不出是谁占的——**这不是"有点浪费"，是"卡用不了了
# 还不知道为什么"**。所以权重必须能放掉，而且要**没人用的时候自己放掉**。
_last_use = time.monotonic()   # **单调钟**，理由见 `_idle_watchdog()`
_ever_loaded = False           # 曾经加载成功过——`/health` 报 `idle` 的前提
_idle_unload_seconds = 600.0   # <=0 表示不自动释放（留着手动/基准测试用）
_warmup_pending = False        # 后台已经在加载了，别为每次 /warmup 再开一个线程


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
    global _ever_loaded, _last_use

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
            _ever_loaded = True
            # **加载完成也算一次"在用"。** 不然刚装好就被看门狗按"上次用的
            # 时间"（可能已经是几分钟前，比如空闲释放之后隔了很久才重新加载）
            # 误判成空闲，白装一次——而那一次是十几秒。
            _last_use = time.monotonic()
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


def _flush_cuda_cache() -> None:
    """把分配器缓存的显存还给驱动。**要看着它真的降下去，不能只叫一次。**

    ## 为什么一次 `empty_cache()` 不够（这是实测出来的）

    卸载是**和请求线程赛跑**的。生成锁放开的那一瞬间，`stream_generate()`
    那个生成器才开始拆自己的栈帧，而权重正好挂在它的局部变量上
    （`tok, mdl, inputs`）——也就是说"锁已经放了"和"权重真的没人要了"
    之间**有一条缝**，流式响应最后那一小段正好落在这条缝里。

    实测：在缝里叫 `empty_cache()` **一点都回收不到**（权重还活着，
    分配器眼里那些块是"在用"不是"缓存"）；等权重真放手时，块已经静静地
    躺回分配器的缓存里，**而没有人会再叫一次**。表现就是 `/health` 说
    `idle`、`nvidia-smi` 纹丝不动——和"这个功能根本没做"长得一模一样。

    所以这里循环几轮：每轮先 `gc.collect()` 再 `empty_cache()`，然后看
    `memory_reserved()` 归零没有。第一轮扑空不要紧，等 0.25 秒那条缝就合上了，
    第二轮通常就归零。**上限四轮**：抓不到的引用等多久都抓不到，
    有界才不会把卸载挂死。

    （调用方保证此刻没有生成在跑，所以这零点几秒没人等。）
    """
    try:
        import gc

        import torch

        if not torch.cuda.is_available():
            return
        for _ in range(4):
            gc.collect()
            torch.cuda.empty_cache()
            if int(torch.cuda.memory_reserved()) == 0:
                return
            time.sleep(0.25)
    except Exception as e:  # noqa: BLE001
        # 引用已经掉了，权重迟早会还；这里只是没还干净。
        # **不要把异常吞掉不说**——那正是"卸载看起来成功了但显存没降"的来源。
        sys.stderr.write(f"[local-llm] 释放显存缓存失败：{type(e).__name__}: {e}\n")


def _drop_weights() -> bool:
    """把权重真的放掉。返回"是否放掉了东西"。

    **调用方必须已经持有 `_generate_lock`**（`unload()` 和 `_idle_watchdog()`
    都是这么进来的）。

    ## 为什么光掉引用不够

    掉 Python 引用只是让模型对象**可回收**；torch 的缓存分配器仍然把显存
    攥在自己手里，`nvidia-smi` **一点变化都不会有**——看起来和"这次卸载
    根本没生效"一模一样。所以必须把缓存**还**给驱动，见 `_flush_cuda_cache()`。

    `gc.collect()` 是同一个道理的补刀：模型对象偶尔处在引用环里，
    光把全局引用置空不一定立刻析构，先收一遍垃圾才有东西可以还。
    """
    global _model, _tokenizer

    with _lock:
        had = _model is not None or _tokenizer is not None
        _model = None
        _tokenizer = None
        ever = _ever_loaded
    if not had and not ever:
        # 这个进程从来没装过模型：**连 torch 都不用碰**——为一个空操作
        # 去把几百兆的 torch 拖进内存没有意义。
        return False
    # `had` 为假也要冲一次：上一次卸载可能正好落在上面那条缝里，
    # 缓存还攥着那几 GB，而重复的 /unload 是唯一会再来看一眼的人。
    _flush_cuda_cache()
    return had


def unload() -> bool:
    """释放权重，**进程留着**。返回是否真的释放了。

    ## 为什么必须先拿生成锁

    卸载和生成是**对同一份权重**的两件事。不拿锁就卸，等于把权重从一次
    正在跑的推理脚下抽走；而且 `generate()` / `stream_generate()` 各自拿着
    `_model` 的一个局部引用，权重那时其实还活着——表现就是"卸载了，
    但显存没降"，然后下一句话再撞一句"模型未加载"。

    先拿 `_generate_lock`，卸载就变成"**等这一轮说完再卸**"：这正是想要的，
    而且这时候 `empty_cache()` 才真的能还出一大块。

    （`empty_cache()` 也留在锁里：它会短暂卡一下，但反正此刻没有生成在跑。）
    """
    with _generate_lock:
        return _drop_weights()


def _warmup_background(model: str, device: str) -> None:
    """后台加载。`/warmup` **不等它**——理由见 `Handler._warmup()`。

    `load()` 自己会排队（`_load_lock`）、自己会把失败原因留在 `_model_error`，
    所以这里不需要再包一层错误处理。
    """
    global _warmup_pending

    try:
        load(model, device)
    finally:
        # **无论成败都要清标记**，否则一次失败的加载会把 `/warmup` 永久
        # 锁死在"warming"，而 `/health` 那边其实早就说出原因了。
        with _lock:
            _warmup_pending = False


def _idle_watchdog() -> None:
    """空闲到点就把权重放掉。**这才是真正堵住那 8 GB 的东西。**

    ## 为什么轮询，而不是算一个精确的 `sleep(剩余时间)`

    "上一次用是什么时候"是外部事件改的（一个请求进来就变了），睡一个
    长觉就必然错过"刚好又有人说话"。所以按固定间隔醒来看一眼，
    间隔取 `min(5, 窗口/4)`：默认 600 秒时每 5 秒看一眼，
    到点后几秒内就能发现；窗口很短（测试用）时也不会太钝。

    **这不是忙等**：每次醒来只做几次赋值就接着睡，不烧 CPU——
    这个特性的全部意义就是省资源，自己先烧一个核就说不过去了。

    ## 为什么用单调钟

    `time.time()` 会被对时、夏令时、休眠唤醒改掉，量出来的"空闲了多久"
    可以是负数或者几小时。`time.monotonic()` 只会往前走。
    """
    while True:
        window = _idle_unload_seconds
        time.sleep(max(1.0, min(5.0, window / 4.0)))
        with _lock:
            if _model is None:
                continue  # 没加载：没有可放的，也没必要看时间
            idle_for = time.monotonic() - _last_use
        if idle_for < window:
            continue
        # 到点了。**拿生成锁之后再确认一次**：生成开始和结束都会更新
        # `_last_use`，而生成也必须先拿到这把锁——所以锁里看到的空闲时长
        # 是准的，不会出现"人家刚开口就把它卸了"。生成在跑的时候这里会**等**，
        # 等它跑完再看时间（那时 `_last_use` 已经是新的，于是这轮就跳过）。
        with _generate_lock:
            with _lock:
                stale = _model is not None and time.monotonic() - _last_use >= window
            if stale and _drop_weights():
                sys.stderr.write(
                    f"[local-llm] 空闲 {window:.0f} 秒没有请求，已释放显存"
                    f"（进程还在，下一个请求会重新加载）\n"
                )


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
    global _last_use
    import torch

    with _generate_lock:
        with _lock:
            tok, mdl = _tokenizer, _model
            # **进门就记一次"在用"。** 看门狗只看这个时间戳，所以它必须在
            # 拿到生成锁之后、真正开跑之前更新——否则一次很长的生成会被
            # 误判成"早就没人用了"。
            _last_use = time.monotonic()
        if tok is None or mdl is None:
            raise RuntimeError(_model_error or "模型未加载")

        inputs = _encode(tok, mdl, messages)
        with torch.no_grad():
            out = mdl.generate(**_sample_kwargs(tok, inputs, max_tokens, temperature))
        gen = out[0][inputs["input_ids"].shape[1]:]
        content = tok.decode(gen, skip_special_tokens=True).strip()
        with _lock:
            # 生成**结束**也记一次，理由见 `stream_generate()` 里那段。
            _last_use = time.monotonic()
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
    # **必须声明 global。** 这个函数体里给 `_last_use` 赋值，不声明的话
    # Python 会把它当成本函数的局部变量：赋值照做，模块全局那个纹丝不动，
    # 而看门狗看的就是全局那个——表现是"流式走完照样被当成空闲"。
    global _last_use
    import torch
    from transformers import TextIteratorStreamer

    with _generate_lock:
        with _lock:
            tok, mdl = _tokenizer, _model
            # 同 `generate()`：**开跑之间**就记时间，理由见看门狗那段。
            _last_use = time.monotonic()
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
        # **生成结束也要记一次。** 只记开始那一下是不够的："空闲 N 秒"会被一次
        # 比 N 还长的生成整个吃掉——看门狗睡醒时窗口早就过了，它会等在生成锁上，
        # 等生成一结束就把权重卸掉：**说完话的下一瞬间**模型就没了，而使用者还在
        # 同一个对话里，下一个问题又得等十几秒装回来。
        # 这不是推测，是实测：一次 24 秒的生成，`--idle-unload 20` 下
        # 释放发生在生成结束后 0.0 秒。
        with _lock:
            _last_use = time.monotonic()
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

    def _warmup(self) -> None:
        """`/warmup`：**立刻返回**，加载扔给后台线程。

        Rust 侧是 fire-and-forget 地叫这一下的（"我要说话了，你去把模型热上"），
        不是在这里等十几秒。所以**不能等 `load()` 返回**——等的话就等于把
        "加载慢"从后台搬到了对话的第一句话上，而那正是 D106 那条要躲的东西。
        """
        global _warmup_pending

        with _lock:
            loaded = _model is not None
            err = _model_error
            pending = _warmup_pending
            if not loaded and not err and not pending:
                # **在锁里就占坑**：两次 /warmup 前后脚进来时，
                # 第二个看到 pending 就不会再开一个线程（那个线程只会堵在
                # `_load_lock` 上，然后什么都不干地退出）。
                _warmup_pending = True
        if loaded:
            self._send(200, {"status": "ok"})
            return
        if err:
            # **失败过就不再试**，和 `load()` 里那条规矩一致：一次加载几十秒，
            # 原因已经留在 `_model_error` 里（`/health` 报得出来），
            # 每个请求都重试只会让每一次都白等，还把真正的原因埋掉。
            self._send(200, {"status": "degraded", "detail": err})
            return
        if not pending:
            threading.Thread(
                target=_warmup_background,
                args=(_model_name, self.server.device),  # type: ignore[attr-defined]
                daemon=True,
            ).start()
        # **202**：接下了，但还没做完。客户端不该在这里等。
        self._send(202, {"status": "warming"})

    def do_GET(self) -> None:  # noqa: N802
        if self.path.startswith("/health"):
            with _lock:
                if _model is not None:
                    self._send(200, {"status": "ok", "model": _model_name})
                elif _model_error:
                    # **把原因报出来**，不要只说 unloaded。
                    #
                    # **`degraded` 必须压过 `idle`**：加载失败过的模型不能被
                    # 说成"只是闲着"——那是把一个真故障藏在一个看着很乖的
                    # 状态后面，而 D106 那次就是因为状态太乖才查了很久。
                    self._send(200, {"status": "degraded", "model": "unloaded",
                                     "detail": _model_error})
                elif _loading:
                    # 正在往显存里搬权重。**这一条要排在 `idle` 前面**：
                    # 空闲释放之后被重新叫醒时还报 `idle`，会让人以为
                    # 不用等——而那时候它恰恰正在等。
                    self._send(200, {"status": "loading", "model": _model_name})
                elif _ever_loaded:
                    # 曾经加载好、现在权重被空闲释放了。**这不是错误，
                    # 也不是"在加载"**：下一个请求会自己把它拉回来，
                    # 使用者什么都不用做。
                    self._send(200, {"status": "idle", "model": _model_name})
                else:
                    self._send(200, {"status": "loading", "model": _model_name})
            return
        if self.path.startswith("/v1/models"):
            self._send(200, {"object": "list", "data": [{"id": _model_name, "object": "model"}]})
            return
        self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        # `/warmup`：叫一声就走，不等加载（理由见 `_warmup()`）。
        if self.path.startswith("/warmup"):
            self._warmup()
            return
        # `/unload`：放掉权重、进程留着——关掉 `yunxi-bot chat` 之后
        # 还占着的那 8 GB 就是从这里还回去的。
        if self.path.startswith("/unload"):
            freed = unload()
            self._send(200, {"status": "unloaded" if freed else "already_unloaded"})
            return
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
    global _model_name, _idle_unload_seconds

    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="Qwen3-1.7B")
    ap.add_argument("--port", type=int, default=17872)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--device", default="cuda")
    ap.add_argument("--warmup", action="store_true",
                    help="启动时就加载（默认是第一个请求才加载）")
    ap.add_argument("--idle-unload", type=float, default=600.0, metavar="SECONDS",
                    help="空闲这么多秒之后释放显存（默认 600；0 = 关掉）")
    args = ap.parse_args()

    if args.host not in ("127.0.0.1", "localhost", "::1"):
        sys.stderr.write("[local-llm] 只监听回环地址——这是在跑一个能读取器的人。\n")
        return 2

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    srv.device = args.device  # type: ignore[attr-defined]
    with _lock:
        _model_name = args.model
        _idle_unload_seconds = args.idle_unload
    sys.stderr.write(
        f"[local-llm] 监听 http://{args.host}:{args.port}  "
        f"/v1/chat/completions  /health  /warmup  /unload\n"
    )
    if args.idle_unload > 0:
        # **看门狗是 daemon 线程**：主线程一退（Ctrl-C、被关掉），它跟着走，
        # 不会变成第二个"进程还在、显存还占着"的东西。
        threading.Thread(target=_idle_watchdog, daemon=True).start()
        sys.stderr.write(
            f"[local-llm] 空闲 {args.idle_unload:.0f} 秒没有请求就释放显存"
            f"（--idle-unload 0 可关掉）\n"
        )
    else:
        # **关掉的时候要说出来。** 不说的话，"空闲了显存却一直不降"
        # 会看起来像这个特性坏了，而不是被人为关了。
        sys.stderr.write("[local-llm] 空闲释放已关闭（--idle-unload 0）\n")
    if args.warmup:
        load(args.model, args.device)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
