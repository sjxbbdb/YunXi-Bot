#!/usr/bin/env python3
"""本地陪伴链路可重复基准：加载、首轮、热轮、prompt token。"""
import argparse
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
PORT = 17876
MODELS = {
    "gemma": "gemma-4-26B-A4B-it-qat-4bit",
    "qwen": "Qwen3.5-9B-4bit",
    "qwen38": "Qwen3.8-27B-4bit",
}


def health(timeout=120):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=2) as r:
                return json.load(r)
        except Exception:
            time.sleep(1)
    raise RuntimeError("local model did not become healthy")


def ask(model, messages, max_tokens=48):
    body = json.dumps(
        {
            "model": model,
            "messages": messages,
            "max_tokens": max_tokens,
            "stream": False,
            "thinking": {"type": "disabled"},
        },
        ensure_ascii=False,
    ).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{PORT}/v1/chat/completions",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    start = time.perf_counter()
    with urllib.request.urlopen(req, timeout=240) as r:
        data = json.load(r)
    elapsed = time.perf_counter() - start
    message = data["choices"][0]["message"]
    return elapsed, data.get("usage", {}), message.get("content") or ""


def system_prompt():
    prompts = ROOT / "crates/yunxi-bot-core/src/prompts"
    persona = (ROOT / "data/persona.md").read_text(encoding="utf-8")
    profile = ROOT / "data/profile.md"
    profile_text = profile.read_text(encoding="utf-8") if profile.exists() else ""
    return "\n\n".join(
        [
            (prompts / "base.md").read_text(encoding="utf-8"),
            persona,
            profile_text,
            (prompts / "local_chat.md").read_text(encoding="utf-8"),
        ]
    )


def bench(name):
    model = MODELS[name]
    env = dict(os.environ)
    env["YUNXI_BOT_HOME"] = str(ROOT / "data")
    log = open(f"/tmp/yunxi-bench-{name}.log", "w")
    started = time.perf_counter()
    proc = subprocess.Popen(
        [
            str(ROOT / ".venv-mlx/bin/python"),
            str(ROOT / "sidecar/local_llm_server.py"),
            "--model",
            model,
            "--device",
            "mlx",
            "--port",
            str(PORT),
            "--warmup",
            "--idle-unload",
            "0",
        ],
        cwd=ROOT,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=log,
    )
    try:
        h = health()
        load = time.perf_counter() - started
        system = system_prompt()
        first_user = "你好"
        first_elapsed, first_usage, first_text = ask(
            model, [{"role": "system", "content": system}, {"role": "user", "content": first_user}]
        )
        second_elapsed, second_usage, second_text = ask(
            model,
            [
                {"role": "system", "content": system},
                {"role": "user", "content": first_user},
                {"role": "assistant", "content": first_text},
                {"role": "user", "content": "今天有点累。"},
            ],
        )
        return {
            "model": model,
            "health": h,
            "system_chars": len(system),
            "load_seconds": round(load, 2),
            "first_seconds": round(first_elapsed, 2),
            "first_prompt_tokens": first_usage.get("prompt_tokens"),
            "second_seconds": round(second_elapsed, 2),
            "second_prompt_tokens": second_usage.get("prompt_tokens"),
            "second_completion_tokens": second_usage.get("completion_tokens"),
        }
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        log.close()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--model", choices=["gemma", "qwen", "qwen38", "both", "all"], default="both")
    args = ap.parse_args()
    names = ["gemma", "qwen"] if args.model == "both" else (["gemma", "qwen", "qwen38"] if args.model == "all" else [args.model])
    results = []
    for name in names:
        print(f"== {name} ==", flush=True)
        result = bench(name)
        print(json.dumps(result, ensure_ascii=False), flush=True)
        results.append(result)
    out = ROOT / "data/benchmarks" / f"companion-chain-{int(time.time())}.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"saved {out}")


if __name__ == "__main__":
    main()
