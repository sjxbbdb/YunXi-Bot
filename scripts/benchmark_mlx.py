"""固定题集的 Mac 文本模型实测；每个模型独立进程，不执行模型提出的工具。"""
import argparse
import hashlib
import json
import os
import platform
import re
import resource
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

from compare_local_models import CASES

ROOT = Path(__file__).resolve().parent.parent
SUITE = ROOT / "models/eval/local-chat-v2.json"
MANIFEST = ROOT / "models/local-benchmark-manifest.json"
OUTPUT = ROOT / "data/benchmarks/local-chat-v2"


def require_all_downloads(models, model_root):
    pending = []
    for entry in models:
        name = entry["repo_id"].split("/")[-1]
        dest = model_root / name
        try:
            marker = json.loads((dest / "benchmark-verified.json").read_text())
            ready = marker == {"revision": entry["revision"], "bytes": entry["bytes"]}
            ready = ready and not any(dest.rglob("*.aria2")) and all(
                (dest / file["path"]).stat().st_size == file["size"] for file in entry["files"])
        except (OSError, ValueError):
            ready = False
        if not ready:
            pending.append(name)
    if pending:
        raise ValueError("全部模型下载并校验完成后才能开始测试；尚未就绪：" + ", ".join(pending))


def parse_calls(text, tokenizer, tools):
    start, end = tokenizer.tool_call_start, tokenizer.tool_call_end
    if not start or start not in text:
        return []
    calls = []
    for part in text.split(start)[1:]:
        body = part.split(end, 1)[0] if end else part
        parsed = tokenizer.tool_parser(body.strip(), tools)
        calls.extend(parsed if isinstance(parsed, list) else [parsed])
    return calls


def split_thinking(text, prompt, start, end):
    # shortcut: 分离一个思考段；新增多段思考协议时改用原生状态机。
    if not start or not end:
        return text, ""
    if prompt.rfind(start) > prompt.rfind(end):
        reasoning = text
        prefix = ""
    elif start in text:
        prefix, reasoning = text.split(start, 1)
    elif start.startswith(text.lstrip()):
        return "", ""
    else:
        return text, ""
    if end not in reasoning:
        return prefix, reasoning
    reasoning, answer = reasoning.split(end, 1)
    return prefix + answer, reasoning


def check(case, text, calls):
    if "expected_calls" in case:
        canonical = lambda items: sorted(json.dumps(item, sort_keys=True, ensure_ascii=False) for item in items)
        if canonical(calls) != canonical(case["expected_calls"]):
            return False
        if "expected_text" in case:
            return text.strip() == case["expected_text"]
        return bool(calls or text.strip())
    if "expected" not in case:
        return None
    expected = case["expected"]
    if case["group"] == "tool" and isinstance(expected, dict):
        if len(calls) != 1:
            return False
        actual = calls[0]
    elif case["group"] == "tool":
        return not calls and text.strip() == expected
    else:
        try:
            actual = json.loads(text.strip())
        except (ValueError, TypeError):
            return False
    # 不把 0/1 误算成正确的布尔值。
    return json.dumps(actual, sort_keys=True, ensure_ascii=False) == json.dumps(
        expected, sort_keys=True, ensure_ascii=False)


def content_check(case, text):
    if case["group"] != "objective":
        return None
    # 格式错误单独计分；只移除包住整个答案的一个围栏，不搜索答案片段。
    fenced = re.fullmatch(r"```(?:json)?\s*\n(.*?)\n```", text.strip(), re.DOTALL | re.IGNORECASE)
    return check(case, fenced.group(1) if fenced else text, [])


def save(path, result):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def worker(entry):
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    import mlx.core as mx
    import mlx_lm
    from mlx_lm import load, stream_generate
    from mlx_lm.sample_utils import make_sampler

    # 降低 MLX 的分配建议限额；这是软限额，主要隔离仍依靠单模型进程。
    mx.set_memory_limit(40 * 1024**3)
    mx.set_cache_limit(512 * 1024**2)

    name = entry["repo_id"].split("/")[-1]
    model_dir = ROOT / "data/models" / name
    verified = json.loads((model_dir / "benchmark-verified.json").read_text())
    if verified["revision"] != entry["revision"]:
        raise ValueError("模型版本与下载清单不符")
    suite = json.loads(SUITE.read_text())
    OUTPUT.mkdir(parents=True, exist_ok=True)
    path = OUTPUT / (name + ".json")
    result = {"model": entry["repo_id"], "revision": entry["revision"],
              "mode": "all_downloaded_serial_v2", "mlx_memory_limit_gib": 40,
              "system_prompt": suite["system"],
              "started_at": datetime.now(timezone.utc).isoformat(), "python": platform.python_version(),
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "companion_cases_sha256": hashlib.sha256(json.dumps(CASES, ensure_ascii=False).encode()).hexdigest(),
              "suite_sha256": hashlib.sha256(SUITE.read_bytes()).hexdigest(),
              "platform": platform.platform(), "mlx": mx.__version__, "mlx_lm": mlx_lm.__version__,
              "thinking": "baseline off; paired off/on", "kv_cache": "fresh per request, unquantized", "runs": []}
    started = time.perf_counter()
    model, tokenizer = load(str(model_dir), tokenizer_config={"trust_remote_code": False})
    mx.synchronize()
    result["load_seconds"] = time.perf_counter() - started

    def generate(case, repeat, temperature, max_tokens=384, thinking=False):
        messages = [{"role": "system", "content": suite["system"]}] + case["messages"]
        mock_tools = suite["tools"] if case.get("use_tools") else case.get("tools")
        input_sha256 = hashlib.sha256(json.dumps(
            {"messages": messages, "tools": mock_tools}, sort_keys=True, ensure_ascii=False).encode()).hexdigest()
        kwargs = {"tools": mock_tools} if mock_tools else {}
        prompt = tokenizer.apply_chat_template(messages, tokenize=False, add_generation_prompt=True,
                                               enable_thinking=thinking, **kwargs)
        mx.random.seed(20261015 + repeat)
        mx.reset_peak_memory()
        start = time.perf_counter()
        first_token = first_text = first_answer = None
        text, generated_tokens = "", []
        last = None
        for response in stream_generate(model, tokenizer, prompt, max_tokens=max_tokens,
                                        sampler=make_sampler(temp=temperature, top_p=.8),
                                        prefill_step_size=512):
            if first_token is None:
                first_token = time.perf_counter() - start
            if response.text and first_text is None:
                first_text = time.perf_counter() - start
            text += response.text
            generated_tokens.append(response.token)
            answer, reasoning = split_thinking(text, prompt, tokenizer.think_start, tokenizer.think_end)
            if answer.strip() and first_answer is None:
                first_answer = time.perf_counter() - start
            last = response
        mx.synchronize()
        elapsed = time.perf_counter() - start
        if last is None:
            raise RuntimeError("模型没有生成任何响应")
        calls, parse_error = [], None
        if mock_tools:
            try:
                calls = parse_calls(answer, tokenizer, mock_tools)
            except Exception as exc:
                parse_error = f"{type(exc).__name__}: {exc}"
        passed = check(case, answer, calls)
        content_passed = content_check(case, answer)
        reasoning_tokens = 0
        if reasoning and tokenizer.has_thinking:
            begin = tokenizer.find_think_start(generated_tokens)
            begin = begin + len(tokenizer.think_start_tokens) if begin >= 0 else 0
            end = tokenizer.find_think_end(generated_tokens)
            reasoning_tokens = (end if end >= 0 else len(generated_tokens)) - begin
        if parse_error or last.finish_reason == "length":
            passed = False if passed is not None else None
            content_passed = False if content_passed is not None else None
        return {"id": case["id"], "group": case["group"], "repeat": repeat,
                "category": case.get("category"), "scenario": case.get("scenario"),
                "input_sha256": input_sha256, "seed": 20261015 + repeat,
                "thinking": thinking,
                "temperature": temperature, "top_p": .8, "max_tokens": max_tokens,
                "text": answer, "raw_text": text, "reasoning_tokens": reasoning_tokens,
                "tool_calls": calls, "tool_parse_error": parse_error,
                "passed": passed, "content_passed": content_passed, "finish_reason": last.finish_reason,
                "ttft_seconds": first_token, "first_text_seconds": first_text,
                "first_answer_seconds": first_answer,
                "elapsed_seconds": elapsed, "prompt_tokens": last.prompt_tokens,
                "generation_tokens": last.generation_tokens,
                "prompt_tokens_per_second": last.prompt_tps,
                "generation_tokens_per_second": last.generation_tps,
                "characters_per_second": len(answer) / elapsed,
                "mlx_peak_gb": mx.get_peak_memory() / 1e9,
                "process_peak_rss_gb": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1e9}

    warmup = {"id": "warmup", "group": "warmup", "messages": [{"role": "user", "content": "你好"}]}
    result["warmup"] = generate(warmup, 0, 0, 8)
    save(path, result)
    cases = []
    for index, (scenario, tone, messages) in enumerate(CASES):
        for repeat in range(2):
            cases.append(({"id": f"companion_{index + 1:02}", "group": "companion",
                           "scenario": scenario, "tone": tone, "messages": messages}, repeat, .7))
    cases += [(case, 0, case.get("temperature", 0)) for case in suite["cases"]]
    perf = {"id": "performance", "group": "performance", "messages": [{"role": "user", "content":
        "用中文说明怎样把一个 Windows 开发的 Python 项目迁移到 Mac。写一段约 250 字的连续说明，涵盖路径、虚拟环境、依赖和测试。"}]}
    cases += [(perf, repeat, 0) for repeat in range(3)]
    requests = [(case, repeat, temperature, False,
                 256 if case["group"] == "performance" else 512 if case["id"] == "companion_12" else 384,
                 "baseline") for case, repeat, temperature in cases]
    paired = [case for case in suite["cases"] if case["id"] in suite["thinking_pair_ids"]]
    for index, case in enumerate(paired):
        order = (False, True) if index % 2 == 0 else (True, False)
        requests += [(case, 0, 0, thinking, 1536, "thinking_pair") for thinking in order]
    for case, repeat, temperature, thinking, max_tokens, phase in requests:
        try:
            row = generate(case, repeat, temperature, max_tokens, thinking)
        except Exception as exc:
            row = {"id": case["id"], "group": case["group"], "repeat": repeat,
                   "error": f"{type(exc).__name__}: {exc}",
                   "passed": False if "expected" in case or "expected_calls" in case else None}
        row.update(phase=phase, thinking=thinking)
        result["runs"].append(row)
        save(path, result)
        print(name, phase, case["id"], repeat, f"thinking={thinking}", "error" if "error" in row else
              f"{row['elapsed_seconds']:.2f}s {row['generation_tokens_per_second']:.1f}tok/s pass={row['passed']}", flush=True)
    result["complete"] = True
    save(path, result)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--model", help="清单里的模型短名；仍须全部下载完成；省略时逐个测试")
    args = ap.parse_args()
    models = json.loads(MANIFEST.read_text())["models"]
    try:
        require_all_downloads(models, ROOT / "data/models")
    except ValueError as exc:
        ap.error(str(exc))
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if args.model:
        entry = next((m for m in models if m["repo_id"].split("/")[-1] == args.model), None)
        if entry is None:
            ap.error("未知模型")
        import fcntl
        # 即使误开第二个测试命令，也不允许同时加载两份模型。
        with (OUTPUT.parent / "mlx-inference.lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                ap.error("已有模型测试进程运行，拒绝同时加载")
            worker(entry)
        return 0
    for ready in sorted(models, key=lambda m: m["bytes"]):
        name = ready["repo_id"].split("/")[-1]
        print("BENCHMARK", name, flush=True)
        with (OUTPUT / (name + ".log")).open("a") as log:
            try:
                command = [sys.executable, str(Path(__file__).resolve()), "--model", name]
                process = subprocess.run(command,
                                         stdout=log, stderr=subprocess.STDOUT, timeout=3600)
                print("FINISHED", name, process.returncode, flush=True)
                if process.returncode:
                    return process.returncode
            except subprocess.TimeoutExpired:
                print("TIMEOUT", name, flush=True)
                return 1
        time.sleep(3)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
