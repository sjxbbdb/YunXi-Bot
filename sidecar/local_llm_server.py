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


# ---------------------------------------------------------------- 工具调用
#
# ## 这一节是整条本地路上最容易"静默失败"的一环
#
# 在此之前 `do_POST` **从来不读 `req["tools"]`**，`_encode()` 也从不把
# `tools=` 传给 `apply_chat_template`。实测：同一个请求带上 3922 字节的工具
# 清单和不带，编出来的提示词**一模一样**（prompt_tokens 都是 22），模型看到的
# 就是一段"没有工具可用"的提示词，于是它用散文回答。
#
# 后果和 D128（本地请求发给了 Agnes）是同一个形状：**HTTP 200、台账写着
# `provider: "local"`、终端标签也对，而这一步其实什么都没做。**
# 一个需要工具的步骤被路由到本地槽位之后，拿到的是一个"自信但什么都没干"
# 的答案——**那比报错贵得多**：报错会有人去查，散文不会。
#
# 所以这里有三件事必须一起成立，缺一个就退回静默：
#   1. 工具清单要**真的进提示词**（`_render_prompt` 渲染完还要回头查一遍）
#   2. 模型的输出要**真的被解析成 tool_calls**（`_split_tool_calls`）
#   3. 解析不了要**大声报错**，不许把标记当正文交出去
_TOOL_OPEN = "<tool_call>"
_TOOL_CLOSE = "</tool_call>"

# 带工具的请求，最后补一条提醒。**这段文字是量出来的，不是想出来的。**
#
# ## 不加会怎样（实测，同一个真实请求打 17872）
#
# | 条件 | 模型调工具 |
# |---|---|
# | 产品原样（10 个工具、系统提示词要求"第一行必须以 OK: 开头"） | **0/6** |
# | 只给 1 个工具 | 4/4 |
# | 去掉"第一行必须 OK:"那条要求（10 个工具） | 4/4 |
# | 10 个工具 + 这条提醒 | **6/6** |
#
# 0/6 那六次的正文是「OK: 今天是星期二。／星期三。／星期四。」——**每次都不一样，
# 全是编的**。这正是 D128 之后这一轮要堵的东西：HTTP 200、台账、
# 终端标签全都正常，而这一步什么都没做，还给了个假事实。
#
# ## 为什么是这个形状
#
# 冲突在"产品要求第一行是 `OK:`"和"工具调用必须以 `<tool_call>` 开头"之间，
# 4B 模型在两者之间摇摆；工具清单越长（10 个工具的签名有 2 千多 token），
# 它越倾向去满足那句离它更近、说得更硬的 `OK:` 要求。
# 提醒放在**消息最末尾**（离生成点最近的地方）就把天平扳回来了——
# 而它没有改动调用方发来的任何一条消息。
#
# ## 措辞是第二遍量出来的（闲聊那条路差得很远）
#
# 第一版写的是"如果需要工具……就直接调用工具"，够用于 `do` 的步骤提示词
# （6/6），但**在 `chat` 那条路上是 0/6**：`chat` 的系统提示词更长
# （3880 字，里面还有"简单的事直接做"），模型六次全都回了「五」
# ——一个凭印象编的、而且每次都一样的错答案。
# 把话说到"**必须先调用工具**""凭印象说等于给了一个假答案"就变成 6/6。
# 顺带量到：这件事**和采样温度无关**（0.7 / 0.3 / 0.0 都是 0/6），
# 所以不是"多试几次就好"，必须从提示词上解决。
#
# ## 为什么可以这么干，以及它的代价
#
# 这不是"猜一个咒语"：它只加给**带工具的请求**（不带工具的闲聊一字不动），
# 而本地槽位存在的意义就是替远端跑那些要工具的简单步骤。
# 代价是本地槽位看到的提示词比其他 provider 多一句——所以它必须写在这里、
# 写清楚，而不是藏在某个模板里。
#
# 实测它对"不该调工具"的场景没有副作用（各 4 次）：
#   - 闲聊："你好呀" → 0 次工具调用（不加提醒时反而会**编一个日期**出来）
#   - 不需要工具的改写步骤 → 0 次工具调用，正文照旧 `OK: ...`
#   - 「什么是二分查找」→ 0 次工具调用，正常作答
#   - 读文件 / 查日期 → 4/4、6/6 真的去调
_TOOL_REMINDER = (
    "提醒：要回答这个问题，如果你需要的事实只能通过工具拿到"
    "（当前日期时间、文件内容、网页内容、命令输出等），**必须先调用工具**，"
    "拿到结果再回答。**凭印象说一个日期/时间/文件内容等于给了一个假答案**，"
    "那比说「我不知道」更糟。"
)


def _assert_tools_in_prompt(text: str, names: list[str]) -> None:
    """确认工具清单真的渲染进提示词了。**没有就抛错。**

    ## 为什么渲染完还要回头查

    `apply_chat_template` 对**不认识的变量是静默忽略**的：模板里没有
    `tools` 时，传进去的工具清单既不报错也不生效。所以"调用没抛异常"
    证明不了"工具进去了"——那正是原来那个 bug 能瞒这么久的原因。

    查的是**每个工具名在不在提示词里**：Qwen3 的模板会把每个工具的
    JSON 签名（含 `name`）原样写进 `<tools>` 段，名字出现即说明渲染生效。
    这比"检查某个固定的 `<tools>` 字符串"更可靠——它认的是内容，不是格式。

    **提醒那条也一起查**：模板要是把末尾的 system 吞掉（有些模板只认第一条），
    工具可靠性就退回实测的 0/6，而那种退化**从外面完全看不出来**——
    所以宁可在这里报错，也不放它过去。
    """
    missing = [n for n in names if n not in text]
    if missing:
        raise RuntimeError(
            f"这个模板没有把 {len(missing)} 个工具渲染进提示词"
            f"（例如 {missing[0]}）——模型看到的是一段**没有任何工具说明**的"
            f"提示词，它只会用散文回答，而请求本身看起来完全正常。"
            f"本地槽位拒绝按'没有工具'回答这种请求："
            f"要么换一个支持 tools 的模板，要么别把带工具的任务路由到本地。"
        )
    head = _TOOL_REMINDER[:12]
    if head not in text:
        raise RuntimeError(
            f"这个模板把末尾那条工具提醒吞掉了（提示词里找不到「{head}…」）。"
            f"没有它，实测模型在 10 个工具面前 0/6 次会去调工具、"
            f"而是直接编一个答案（六次里日期有四种写法）——"
            f"那正是要堵的静默失败，所以这里直接报错，不降级。"
        )


def _render_prompt(tok, messages: list[dict], tools: list[dict] | None) -> str:
    """把消息（以及工具清单）渲染成提示词。**流式与非流式共用。**

    两条路的输入必须一模一样，否则"流式"和"不流式"会得到不同结果——
    而那种差异极难排查（同一个问题换个前端就变了个人）。

    **思考模式关掉**：Qwen3 默认会先"想"再答，那对闲聊是纯浪费——
    延迟翻几倍，而闲聊不需要想。模板不支持这个参数时忽略（`TypeError`，
    这一层容错原来就有，保留）。

    ## 工具为什么不走这条容错

    `enable_thinking` 不支持，退一步的后果只是"多想了"——**慢，但对**。
    而 `tools` 不支持，退一步的后果是"模型不知道有工具"——**答案是错的，
    而且看不出来**。两者代价不对称，所以：工具传不进去时**不降级，直接报错**。

    ## 提醒为什么在这里加

    `_TOOL_REMINDER` 只在有工具时追加，而且**加在这一层**——
    流式和非流式共用它，所以两条路的提示词仍然一模一样（这是本文件的前提）。
    加在调用方（`_stream` / `generate`）上就有两份，早晚会漂。
    """
    if tools:
        # **不改调用方发来的任何一条消息**，只在末尾补一条。
        messages = list(messages) + [{"role": "system", "content": _TOOL_REMINDER}]
    names = [n for n in ((t.get("function") or {}).get("name") for t in (tools or [])) if n]
    if tools and len(names) != len(tools):
        # 有工具没名字——那不是模板的问题，是请求本身残缺。
        # 静默少给一个工具，模型就永远调不到它。
        raise RuntimeError(
            f"请求里 {len(tools)} 个工具有 {len(tools) - len(names)} 个没带 "
            f"function.name，无法渲染成工具清单"
        )
    if tools:
        for kwargs in ({"enable_thinking": False, "tools": tools}, {"tools": tools}):
            try:
                text = tok.apply_chat_template(
                    messages, tokenize=False, add_generation_prompt=True, **kwargs,
                )
            except TypeError:
                # 模板不认这个关键字（`enable_thinking` 很常见）。
                # 换下一个组合再试；两个都试完还是不行，就是模板不支持 tools。
                continue
            _assert_tools_in_prompt(text, names)
            return text
        raise RuntimeError(
            "这个模板不接受 `tools=` 参数（试过带/不带 enable_thinking 两种写法）——"
            "本地槽位不能在没有工具说明的情况下回答一个带工具的请求。"
        )
    try:
        return tok.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True,
            enable_thinking=False,
        )
    except TypeError:
        return tok.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True,
        )


def _partial_marker_len(s: str, marker: str) -> int:
    """`s` 的末尾是不是 `marker` 的一个**前缀**；返回该留住不发的长度。

    ## 为什么流式非要这个

    模型吐的是 token 流，`<tool_call>` 完全可能被切成 `<tool` + `_call>`。
    没有这段预留，`<tool` 会先被当成正文发出去——于是终端上冒出一截 XML，
    而工具调用本身照样被识别（后面的标记还在），看起来只是"多了点乱码"。
    真正糟的是反过来：只切剩 `<tool` 时我们**发了正文**，
    下一批才拼出完整标记——正文已经交出去了，收不回来。
    """
    n = min(len(s), len(marker) - 1)
    for k in range(n, 0, -1):
        if s.endswith(marker[:k]):
            return k
    return 0


def _parse_tool_call_block(raw: str) -> list[tuple[str, str]]:
    """解析一对 `<tool_call></tool_call>` 之间的内容，返回 `[(工具名, 参数JSON字符串)]`。

    ## 模型实际吐的形状（实测，不是照文档猜的）

    ```text
    <tool_call>
    {"name": "now", "arguments": {}}
    </tool_call>
    ```

    也就是 Qwen3 模板里写死的那句 "return a json object with function name
    and arguments within `<tool_call></tool_call>` XML tags"。

    ## 为什么容错，以及容错到哪为止

    容错的部分是**外壳**：`json` 围栏、前后空白、一次给一个数组
    （Qwen 偶尔把多个调用放进一个 `[ ]` 里）。这些形状变了意思没变。

    不容错的部分是**内容**：解析不出来就抛错。理由和上面那条一样——
    把一段解析不了的 `<tool_call>` 当正文交出去，等于给使用者看一段 JSON，
    而那正是"自信但什么都没干"的另一种长相。
    """
    text = raw.strip()
    if text.startswith("```"):
        text = text[3:]
        if text[:4].lower() == "json":
            text = text[4:]
        text = text.rsplit("```", 1)[0].strip()
    try:
        obj = json.loads(text)
    except Exception as e:  # noqa: BLE001
        raise RuntimeError(
            f"工具调用里的 JSON 解析不了（{type(e).__name__}: {e}）；原文：{raw[:300]!r}"
        ) from e
    items = obj if isinstance(obj, list) else [obj]
    out: list[tuple[str, str]] = []
    for it in items:
        if not isinstance(it, dict) or not it.get("name"):
            raise RuntimeError(f"工具调用里没有 name 字段：{raw[:300]!r}")
        args = it.get("arguments", {})
        # **参数必须是 JSON 字符串**：OpenAI 协议如此，Rust 侧
        # （`tool/runner.rs::parse_tool_calls`）就是按字符串取的。
        # 给它一个对象，那边会走 `Some(v) => v.clone()` 那条路——
        # 看着也能用，但流的形状和非流式就不一样了，而"两条路必须一样"
        # 是这个文件反复强调的前提。
        args_text = args if isinstance(args, str) else json.dumps(args, ensure_ascii=False)
        out.append((str(it["name"]), args_text))
    return out


def _wire_tool_call(idx: int, name: str, arguments: str) -> dict:
    """拼成 OpenAI 的 `tool_calls` 元素。

    `id` 是必须的：Rust 把它回灌成工具结果的 `tool_call_id`
    （`Message::tool_result`）。给空串的话，某些服务端会报
    "missing field tool_call_id"——本地这条路现在不会，但把
    历史带去别的 provider 时就会。
    """
    return {
        "id": f"call_{idx}",
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    }


def _split_tool_calls(text: str) -> tuple[str, list[dict]]:
    """把整段生成切成 `(正文, 工具调用)`。**非流式那条路用。**

    正文里不留 `<tool_call>` 标记：它本来就不是给使用者看的东西，
    而留着它会让一段纯工具调用显示成一坨 JSON。
    """
    parts: list[str] = []
    calls: list[dict] = []
    rest = text
    while True:
        a = rest.find(_TOOL_OPEN)
        if a < 0:
            parts.append(rest)
            break
        b = rest.find(_TOOL_CLOSE, a + len(_TOOL_OPEN))
        if b < 0:
            # 有头无尾。最常见的成因是 max_tokens 用尽把参数截断在中间——
            # 而**一条截断的工具调用不能当成完整的用**（runner.rs 里
            # 那段长注释记着同一个教训）。所以这里报错，不猜。
            raise RuntimeError(
                f"模型吐出了 {_TOOL_OPEN} 但没有 {_TOOL_CLOSE}"
                f"（多半是被 max_tokens 截断）：{rest[a:a + 300]!r}"
            )
        parts.append(rest[:a])
        for name, args in _parse_tool_call_block(rest[a + len(_TOOL_OPEN):b]):
            calls.append(_wire_tool_call(len(calls), name, args))
        rest = rest[b + len(_TOOL_CLOSE):]
    return "".join(parts).strip(), calls


def _split_stream(pieces):
    """把模型的文本流切成 `("delta", 正文)` 和 `("tool_call", [调用])`。

    ## 为什么不能"先攒完再切"

    流式的全部意义就是**边生成边显示**（实测 4B 在这台机器上约 20 token/秒，
    不流式要等十几秒才见第一个字）。攒完再切等于把流式关掉。

    ## 工具调用那段要**扣住不发**

    `<tool_call>...</tool_call>` 之间的内容是参数 JSON，属于协议不属于回答。
    把它当正文发出去，使用者会看到一坨 JSON；而 Rust 那边正文和工具调用
    是两条独立的通道（`delta.content` / `delta.tool_calls`），
    混着发等于两边都说了一半。

    ## 截断要报错

    流结束时如果还停在 `<tool_call>` 里面，说明参数被 max_tokens 截断了。
    那种 JSON 拼不完整，**不能猜**——抛错，让调用方如实失败。
    """
    buf = ""
    in_call = False
    # `</tool_call>` 后面那个换行是标记的一部分，不是正文。
    # 不吞掉的话，一轮纯工具调用的回答会打出一个空行。
    swallow_nl = False

    def clean(t: str) -> str:
        nonlocal swallow_nl
        if swallow_nl:
            swallow_nl = False
            if t.startswith("\n"):
                t = t[1:]
        return t

    for piece in pieces:
        if not piece:
            continue
        buf += piece
        # 一批里可能同时有正文、工具调用、正文——所以是 while 不是 if
        while True:
            if in_call:
                j = buf.find(_TOOL_CLOSE)
                if j < 0:
                    break  # 还没闭合，继续攒
                raw = buf[:j]
                buf = buf[j + len(_TOOL_CLOSE):]
                in_call = False
                swallow_nl = True
                # **返回的是 `(名字, 参数)` 对，不是拼好的 wire 元素**：
                # `id` 和 `index` 必须由调用方**跨整条流**统一编号。
                # 在这里 `enumerate` 的话，模型分两次吐两个工具调用就会
                # 得到两个 `call_0`——而 Rust 拿 `id` 去回灌工具结果
                # （`tool_call_id`），重号意味着两条结果指向同一个调用。
                yield "tool_call", _parse_tool_call_block(raw)
                continue
            i = buf.find(_TOOL_OPEN)
            if i >= 0:
                head = clean(buf[:i])
                buf = buf[i + len(_TOOL_OPEN):]
                in_call = True
                # 纯空白的提前语不发：它只会显示成一个空行
                if head.strip():
                    yield "delta", head
                continue
            # 没有完整标记：只发**确定不是标记前缀**的那部分
            hold = _partial_marker_len(buf, _TOOL_OPEN)
            out = clean(buf[:len(buf) - hold]) if hold < len(buf) else ""
            buf = buf[len(buf) - hold:]
            if out:
                yield "delta", out
                continue
            break

    if in_call:
        raise RuntimeError(
            f"流在 {_TOOL_OPEN} 中间就结束了（多半是被 max_tokens 截断）——"
            f"参数拼不完整，不能猜：{buf[:300]!r}"
        )
    tail = clean(buf)
    if tail:
        yield "delta", tail


def _encode(tok, mdl, messages: list[dict], tools: list[dict] | None):
    """把消息（和工具）编成模型输入。**流式与非流式共用。**"""
    text = _render_prompt(tok, messages, tools)
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


def generate(
    messages: list[dict], max_tokens: int, temperature: float,
    tools: list[dict] | None = None,
) -> dict:
    """跑一次生成。**全局串行**——一块 GPU 本来同时只能跑一个。

    返回值里的 `tool_calls` 是解析出来的工具调用（没调就是空列表）。
    **工具调用不是正文的一部分**：它必须和 `content` 分开交出去，
    否则 Rust 那边只会拿到一段写着 `<tool_call>` 的文字，然后把它当成回答。
    """
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

        inputs = _encode(tok, mdl, messages, tools)
        with torch.no_grad():
            out = mdl.generate(**_sample_kwargs(tok, inputs, max_tokens, temperature))
        gen = out[0][inputs["input_ids"].shape[1]:]
        raw = tok.decode(gen, skip_special_tokens=True).strip()
        prompt_tokens = int(inputs["input_ids"].shape[1])
        completion_tokens = int(gen.shape[0])
        # **先解析再放锁。** 解析失败要抛出去（调用方回 500），
        # 而那属于"这一轮的结果"，不属于"生成还在跑"。
        content, tool_calls = _split_tool_calls(raw)
        with _lock:
            # 生成**结束**也记一次，理由见 `stream_generate()` 里那段。
            _last_use = time.monotonic()
        if tools:
            sys.stderr.write(
                f"[local-llm] 本次带 {len(tools)} 个工具，提示词 {prompt_tokens} token；"
                f"模型回了 {len(tool_calls)} 个工具调用\n"
            )
        return {
            "content": content,
            "tool_calls": tool_calls,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
        }


def stream_generate(
    messages: list[dict], max_tokens: int, temperature: float,
    tools: list[dict] | None = None,
):
    """边生成边产出正文片段与工具调用，最后产出一次用量。

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

    若干次 `("delta", 片段)` / `("tool_call", [调用])`，
    最后 `("usage", {...})` 一次。

    **工具调用走单独一种事件，不混进 `delta`**：这两样东西在 Rust 那边是
    两条通道（`delta.content` / `delta.tool_calls`），而且正文会被打印给使用者、
    工具调用不会。混在一起的话，终端上会出现一坨参数 JSON。

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

        inputs = _encode(tok, mdl, messages, tools)
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

        # **一边收一边切。** 片段除了交给下面切成事件，还要留一份：
        # 生成量靠它数（见函数末尾那段），而工具调用那段正文正好是
        # 不会作为正文发出去的那些——不留一份就漏计了。
        pieces: list[str] = []

        def _pieces():
            for piece in streamer:
                if piece:
                    pieces.append(piece)
                    yield piece

        tool_call_count = 0
        for kind, val in _split_stream(_pieces()):
            if kind == "tool_call":
                tool_call_count += len(val)
                for name, args in val:
                    sys.stderr.write(
                        f"[local-llm] 模型请求调用工具：{name}({args[:200]})\n"
                    )
            yield kind, val
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
        if tools:
            sys.stderr.write(
                f"[local-llm] 本次带 {len(tools)} 个工具，提示词 {prompt_tokens} token；"
                f"模型回了 {tool_call_count} 个工具调用\n"
            )
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

    def _chunk_tool_calls(self, fragments: list[dict]) -> dict:
        """工具调用的分片。**形状同样是 `agnes.rs` 定死的**：

        - 每一片都在 `choices[0].delta.tool_calls` 里
        - 每片必须带 `index`——那边是 `BTreeMap<u64, ToolCallAccum>` 按它攒的，
          **分片不保证按顺序到达**，`index` 是它们唯一的身份
        - `id` / `function.name` 只出现在第一片；后面的片只带 `arguments`，
          而空值**不会覆盖**已经拿到的值（`ToolCallAccum::absorb`）
        - `arguments` 是**字符串**，会被逐片拼接
        """
        return {
            "id": "local-1",
            "object": "chat.completion.chunk",
            "model": _model_name,
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": fragments},
                "finish_reason": None,
            }],
            "usage": None,
        }

    def _stream(
        self, messages: list[dict], max_tokens: int, temperature: float,
        tools: list[dict] | None = None,
    ) -> None:
        """流式回一轮。"""
        gen = stream_generate(messages, max_tokens, temperature, tools)
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

        # 工具调用的 `index` 是**整条流共用的**，不是每批从 0 重新数：
        # 模型一次吐两个 `<tool_call>` 就是两批事件，各自从 0 开始的话
        # 第二组会和第一组**撞 index**，在 `BTreeMap` 里被并成同一个调用
        # ——名字对、参数是两份拼起来的，且不报错。
        tc_index = 0
        finish = "stop"
        try:
            for kind, val in _events():
                if kind == "delta":
                    self._sse(self._chunk(val))
                    continue
                if kind == "tool_call":
                    finish = "tool_calls"
                    for name, args in val:
                        c = _wire_tool_call(tc_index, name, args)
                        # **参数故意切成两片发。**
                        #
                        # 这不是为了省事，是为了让"拼装"这条路**每次都真的走一遍**：
                        # Rust 侧是按 index 累加 `arguments` 字符串的，只在长参数时
                        # 才分片的话，这条路平时根本不跑，坏掉也没人知道
                        # （而坏掉的表现是"参数少了半截"——JSON 解析失败，
                        # 工具报一句看不懂的错）。
                        half = len(args) // 2
                        self._sse(self._chunk_tool_calls([{
                            "index": tc_index,
                            "id": c["id"],
                            "type": "function",
                            "function": {
                                "name": c["function"]["name"],
                                "arguments": args[:half],
                            },
                        }]))
                        if args[half:]:
                            self._sse(self._chunk_tool_calls([{
                                "index": tc_index,
                                "function": {"arguments": args[half:]},
                            }]))
                        tc_index += 1
                    continue
                # 收尾两块，形状照 OpenAI 的样子来：
                # 先一个只带 finish_reason 的，再一个只带 usage 的。
                # `usage` 单独一块是因为 Rust 那边认"任何一块的非空 usage"，
                # 而它靠这个数算上下文锚点。
                self._sse({
                    "id": "local-1",
                    "object": "chat.completion.chunk",
                    "model": _model_name,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
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
        except Exception as e:  # noqa: BLE001
            # **头已经发出去了，HTTP 状态码这条退路没有了。**
            #
            # 还能做的只有一件：把原因塞进流里，让对面**报出来**。
            # 这里发的是一段**JSON 字符串**（不是对象）——`agnes.rs` 的
            # `StreamChunk` 反序列化不了它，于是会把它当"解析不了的块"记下来，
            # 并在"既没有正文也没有工具调用"时**把原文带进错误信息**。
            #
            # 反过来，如果发一个 `{"error": ...}` 对象：那是能解析成功的
            # （`StreamChunk` 每个字段都有 `serde(default)`），会被静静跳过——
            # 于是"模型吐了个坏的工具调用"就退化成"模型没回内容"，
            # 而这正是这一整轮要消灭的那种静默。
            sys.stderr.write(f"[local-llm] 流中途失败：{type(e).__name__}: {e}\n")
            try:
                self.wfile.write(
                    b"data: " + json.dumps(
                        f"本地模型这一轮的输出不能用：{type(e).__name__}: {e}"
                    ).encode("utf-8") + b"\n\n"
                )
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError, OSError):
                pass

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
        # **`tools` 必须读进来。** 这一行以前不存在，于是请求带着工具清单
        # 进来、提示词里一个工具都没有，模型用散文回答，而 HTTP 200。
        # 见本文件顶部"工具调用"那一节。
        tools = req.get("tools") or None
        if tools is not None and not isinstance(tools, list):
            self._send(400, {"error": f"tools 必须是数组，收到 {type(tools).__name__}"})
            return
        if req.get("stream"):
            # **流式是必须支持的**：Rust 侧的工具循环只走流式
            # （`tool/runner.rs` 一律调 `think_stream`）。
            # 这里原来回 400 "not supported"，于是路由修好之后
            # 本地这条路依然一次也走不通。
            self._stream(messages, max_tokens, temperature, tools)
            return
        try:
            r = generate(messages, max_tokens, temperature, tools)
        except Exception as e:  # noqa: BLE001
            # **工具解析不了要如实报 500，不能退回一段正文。**
            # 退回去等于把"这一步什么都没做"包装成一个正常回答。
            self._send(500, {"error": f"{type(e).__name__}: {e}"})
            return
        # `content` 只在真有正文时给字符串。**纯工具调用时给 `null`**——
        # OpenAI 协议如此，而 Rust 那边也认（`WireMessage.content` 是 `Option`，
        # 注释写明"只返回工具调用时 content 就是 null"）。
        # 给空串的话，有些实现会把这条消息当成"带内容的助手消息"。
        message: dict = {
            "role": "assistant",
            "content": r["content"] or None,
        }
        if r["tool_calls"]:
            message["tool_calls"] = r["tool_calls"]
        self._send(200, {
            "id": "local-1",
            "object": "chat.completion",
            "model": _model_name,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": "tool_calls" if r["tool_calls"] else "stop",
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
