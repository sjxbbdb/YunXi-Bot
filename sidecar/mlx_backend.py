"""本地聊天的 MLX 推理；HTTP、串行锁与空闲卸载由 local_llm_server 复用。"""
import copy
import json
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

CONTEXT_LIMIT = 81920
_worker = ThreadPoolExecutor(max_workers=1, thread_name_prefix="yunxi-mlx")


def load_model(path):
    return _worker.submit(_load_model, path).result()


def _load_model(path):
    # 本地服务不在请求期间下载权重，也不把 MLX 量化文件交给 PyTorch。
    path = Path(path)
    if not path.is_dir():
        raise FileNotFoundError("本地权重缺失，请运行 安装Mac环境.command --local-model")
    model_type = json.loads((path / "config.json").read_text())["model_type"]
    if model_type not in ("gemma4", "qwen3_5"):
        raise ValueError("此 MLX 后端只接入已验收的 Gemma 4 / Qwen3.5")
    import mlx.core as mx
    from mlx_lm import load

    if not mx.metal.is_available():
        raise RuntimeError("MLX Metal 不可用，需要 Apple Silicon Mac")
    # 软限额不是整机隔离；仍只加载一个聊天模型、串行处理生成。
    mx.set_memory_limit((12 if model_type == "qwen3_5" else 24) * 1024**3)
    mx.set_cache_limit(256 * 1024**2)
    model, tokenizer = load(str(path), tokenizer_config={"trust_remote_code": False})
    mx.synchronize()
    return model, tokenizer


def render_prompt(tokenizer, messages, tools, thinking, reminder):
    messages = copy.deepcopy(messages)
    # OpenAI 历史里的 arguments 是 JSON 字符串；Gemma 模板要求参数对象，
    # 否则会把整段 JSON 再包进一对花括号，破坏下一轮工具结果的上下文。
    for message in messages:
        for call in message.get("tool_calls") or []:
            function = call["function"]
            if isinstance(function["arguments"], str):
                function["arguments"] = json.loads(function["arguments"])
            if not isinstance(function["arguments"], dict):
                raise ValueError("历史工具参数必须是 JSON 对象")
    if tools:
        if messages[0]["role"] == "system":
            messages[0]["content"] += "\n" + reminder
        else:
            messages.insert(0, {"role": "system", "content": reminder})
    prompt = tokenizer.apply_chat_template(
        messages, tools=tools, tokenize=False, add_generation_prompt=True,
        enable_thinking=thinking,
    )
    if tools and (reminder not in prompt or any(
        tool["function"]["name"] not in prompt for tool in tools
    )):
        raise ValueError("聊天模板未包含完整工具说明")
    return prompt


def parse_call(body, tools, family="gemma"):
    if not tools:
        raise ValueError("本轮没有工具权限，拒绝模型生成的调用")
    allowed = {tool["function"]["name"] for tool in tools or []}
    if family == "qwen":
        from mlx_lm.tool_parsers import qwen3_coder

        call = qwen3_coder.parse_tool_call(body.strip(), tools)
        if not isinstance(call, dict):
            raise ValueError("Qwen 工具调用不是 JSON 对象")
        name = call.get("name")
        args = call.get("arguments", {})
        if name not in allowed or not isinstance(args, dict):
            raise ValueError("Qwen 返回未知工具或非法参数")
        return [(name, json.dumps(args, ensure_ascii=False, allow_nan=False))]
    from mlx_lm.tool_parsers import gemma4

    # 上游解析器用 finditer，会忽略调用前后的坏数据；必须先要求完整匹配。
    if not gemma4._tool_call_regex.fullmatch(body.strip()):
        raise ValueError("Gemma 工具调用不完整或含多余内容")
    call = gemma4.parse_tool_call(body)
    if call["name"] not in allowed or not isinstance(call["arguments"], dict):
        raise ValueError("Gemma 返回未知工具或非法参数")
    return [(call["name"], json.dumps(call["arguments"], ensure_ascii=False, allow_nan=False))]


def _tool_family(tokenizer) -> str:
    name = str(getattr(tokenizer, "name_or_path", "")).lower()
    return "qwen" if "qwen" in name else "gemma"


def split_stream(pieces, tokenizer, prompt, tools, thinking=True):
    """隔离思考和工具协议；跨 token 的标记必须完整识别后才发出。"""
    markers = {tokenizer.tool_call_start: ("tool_call", tokenizer.tool_call_end),
               tokenizer.think_start: ("reasoning", tokenizer.think_end)}
    state, end, buffer = "delta", None, ""
    # 只有“开启思考且提示词正好以 think_start 结尾”才延续思考态。
    # 不能因为提示词正文里出现过 think_start 就把整轮当思考——人格/记忆里
    # 一旦出现这个字面量，正文会被全部吞掉，客户端只看到“没有正文”。
    if thinking and prompt.rstrip().endswith(tokenizer.think_start):
        state, end = markers[tokenizer.think_start]
    for piece in pieces:
        buffer += piece
        while buffer:
            if state != "delta":
                pos = buffer.find(end)
                if pos < 0:
                    if state == "reasoning":
                        # 长思考也要及时发响应；只缓冲可能属于结束标记的尾部。
                        hold = max([0] + [n for n in range(1, len(end)) if buffer.endswith(end[:n])])
                        if len(buffer) > hold:
                            yield "reasoning", buffer[:len(buffer) - hold]
                        buffer = buffer[-hold:] if hold else ""
                    break
                body, buffer = buffer[:pos], buffer[pos + len(end):]
                yield state, parse_call(body, tools, _tool_family(tokenizer)) if state == "tool_call" else body
                state, end = "delta", None
                continue
            found = [(buffer.find(m), m) for m in markers if m in buffer]
            if found:
                pos, marker = min(found)
                if pos:
                    yield "delta", buffer[:pos]
                buffer = buffer[pos + len(marker):]
                state, end = markers[marker]
                continue
            hold = max([0] + [n for m in markers for n in range(1, len(m))
                              if buffer.endswith(m[:n])])
            if len(buffer) > hold:
                yield "delta", buffer[:len(buffer) - hold]
            buffer = buffer[-hold:] if hold else ""
            break
    if state != "delta" or buffer:
        raise ValueError("Gemma 思考或工具协议被截断，拒绝作为完整输出")


def generate_events(*args):
    # 加载、KV 缓存和生成必须归属同一线程；仅串行锁不足以满足 Metal 流归属。
    # 按需取下一片，无后台队列；客户端断开时关闭生成器，不继续白跑。
    events = _generate_events(*args)
    try:
        while (event := _worker.submit(next, events, None).result()) is not None:
            yield event
    finally:
        _worker.submit(events.close).result()


def clear_cache():
    def clear():
        import gc
        import mlx.core as mx

        gc.collect()
        mx.synchronize()
        mx.clear_cache()
    _worker.submit(clear).result()


def _generate_events(model, tokenizer, messages, tools, max_tokens, temperature, thinking, reminder):
    import mlx.core as mx
    from mlx_lm import stream_generate
    from mlx_lm.sample_utils import make_sampler

    prompt = render_prompt(tokenizer, messages, tools, thinking, reminder)
    tokens = tokenizer.encode(prompt, add_special_tokens=False)
    if len(tokens) + max_tokens > CONTEXT_LIMIT:
        raise ValueError(f"本地上下文与输出预算合计超过 {CONTEXT_LIMIT} token，请先压缩会话")
    responses = stream_generate(model, tokenizer, tokens, max_tokens=max_tokens,
                                sampler=make_sampler(temp=temperature, top_p=.8),
                                # Metal 流归属线程，不能复用加载线程创建的流。
                                stream=mx.default_stream(mx.default_device()),
                                prefill_step_size=512)
    last = None
    def pieces():
        nonlocal last
        for response in responses:
            last = response
            yield response.text

    # 纯聊天：不思考、无工具时不做思考/工具协议解析。
    # 这条路径只要正文，解析器反而会把提示词里的字面量误判成协议。
    if not thinking and not tools:
        for response in responses:
            last = response
            if response.text:
                yield "delta", response.text
        if last is None:
            raise RuntimeError("Gemma 没有产生输出")
        yield "usage", {"prompt_tokens": last.prompt_tokens,
                        "completion_tokens": last.generation_tokens,
                        "finish_reason": last.finish_reason or "stop"}
        return

    calls = []
    try:
        for kind, value in split_stream(pieces(), tokenizer, prompt, tools, thinking):
            if kind == "tool_call":
                calls.extend(value)
            else:
                yield kind, value
        if last is None:
            raise RuntimeError("Gemma 没有产生输出")
        if calls and last.finish_reason == "length":
            raise ValueError("Gemma 工具响应耗尽输出预算，拒绝执行部分调用")
        # 整轮解析成功才提交调用，避免前一个有效、后一个损坏时执行半个计划。
        if calls:
            yield "tool_call", calls
        yield "usage", {"prompt_tokens": last.prompt_tokens,
                        "completion_tokens": last.generation_tokens,
                        "finish_reason": "tool_calls" if calls else last.finish_reason or "stop"}
    finally:
        responses.close()
        mx.synchronize()
