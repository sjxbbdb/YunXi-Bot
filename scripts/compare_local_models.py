#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""两个本地模型的多场景 · 多语气 · 多次采样对比。

## 为什么要这么铺用例

上一版只测了 4 题、每题一次，**而一次输出不代表模型性格**——采样方差很大。
而且"陪伴"不是只有"安慰"一种：***分享好消息***、***吐槽***、***无聊***、
***自我怀疑*** 要的是**完全不同的回应**。一个说教型模型在"安慰"那题露馅，
但在"教你做事"那题可能反而合适——**所以要把谱系铺开。**

## 三类语气

同一个场景用不同语气问，看它会不会**跟着语气走**：

- `低能量`：短、没标点、不想说话
- `外放`：长、带感叹号、情绪浓
- `克制`：完整句子但不带情绪词

**跟不上的表现是"无论你怎么说，它都用同一种腔调回"**——那是最伤的。

## 全文写文件

60+ 段输出刷屏没有意义，而且我会看不过来。
**全过程写进 `对比报告.md`**，终端只打汇总和逐题并列。
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PY = REPO / ".venv" / "Scripts" / "python.exe"
SERVER = REPO / "sidecar" / "local_llm_server.py"
PORT = 17875
OUT = Path(r"D:\DevTools\yunxi-compare")

# (场景, 语气, 消息)
CASES = [
    ("疲惫·不想说话", "低能量", [{"role": "user", "content": "累"}]),
    ("疲惫·不想说话", "外放", [{"role": "user", "content": "今天真的累死了！！！一点都不想动！！！"}]),
    ("受委屈·需要安慰", "克制", [{"role": "user", "content": "今天开会的时候被领导当众批评了。"}]),
    ("受委屈·需要安慰", "外放", [{"role": "user", "content": "气死我了！！凭什么当着所有人面说我！！"}]),
    ("分享好消息", "外放", [{"role": "user", "content": "我升职了！！！！！！"}]),
    ("分享好消息", "克制", [{"role": "user", "content": "今天项目上线了，还算顺利。"}]),
    ("吐槽第三方", "外放", [{"role": "user", "content": "那个同事又把锅甩给我，烦死了"}]),
    ("焦虑·睡不着", "低能量", [{"role": "user", "content": "睡不着"}]),
    ("自我怀疑", "低能量", [{"role": "user", "content": "我是不是挺没用的"}]),
    ("无聊·闲聊", "低能量", [{"role": "user", "content": "好无聊啊"}]),
    ("多轮指代", "克制", [
        {"role": "user", "content": "我下周要去南京出差"},
        {"role": "assistant", "content": "南京不错，夫子庙那边晚上挺热闹。"},
        {"role": "user", "content": "那边现在天气怎么样？"},
    ]),
    ("不知道的事", "克制", [{"role": "user", "content": "我这只猫今天吐了三次，怎么办"}]),
    ("简单任务", "克制", [{"role": "user", "content": "帮我写一句给同事的生日祝福，简短点"}]),
]


def wait_health(timeout: float) -> bool:
    t0 = time.time()
    while time.time() - t0 < timeout:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/health", timeout=3) as r:
                if json.loads(r.read()).get("status") == "ok":
                    return True
        except Exception:  # noqa: BLE001
            pass
        time.sleep(2)
    return False


def ask(messages, max_tokens=300, temperature=0.7):
    body = json.dumps({"messages": messages, "max_tokens": max_tokens,
                       "temperature": temperature}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}/v1/chat/completions",
                                 data=body, headers={"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=300) as r:
        d = json.loads(r.read())
    dt = time.time() - t0
    u = d["usage"]
    return d["choices"][0]["message"]["content"], dt, u["completion_tokens"]


def run_model(model: str, repeat: int) -> dict:
    """跑一个模型，返回 {场景: [(text, dt, tokens), ...]}"""
    proc = subprocess.Popen(
        [str(PY), "-u", str(SERVER), "--model", model, "--port", str(PORT),
         "--device", "cuda", "--warmup"],
        cwd=str(REPO), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    res: dict = {}
    try:
        if not wait_health(300):
            print(f"  ✗ {model} 没能就绪")
            return res
        try:
            ask([{"role": "user", "content": "你好"}], max_tokens=8)
        except Exception:  # noqa: BLE001
            pass  # 首调热身，失败不影响后面
        for name, tone, msgs in CASES:
            key = f"{name}｜{tone}"
            res[key] = []
            for i in range(repeat):
                try:
                    text, dt, n = ask(msgs)
                    res[key].append((text, dt, n))
                except Exception as e:  # noqa: BLE001
                    res[key].append((f"<失败 {e}>", 0.0, 0))
            tps = [n / dt for _, dt, n in res[key] if dt > 0 and n > 0]
            avg = sum(tps) / len(tps) if tps else 0
            print(f"  {key:<28} {avg:5.1f} tok/s  ×{repeat}")
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
        # **等显存真的还回去**，否则下一个模型 OOM，而那个 OOM
        # 看起来像"这个模型不行"。
        time.sleep(8)
    return res


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", nargs="+",
                    default=["MiniCPM5-2B", "Qwen3-4B-Instruct-2507"])
    ap.add_argument("--repeat", type=int, default=3)
    args = ap.parse_args()

    OUT.mkdir(parents=True, exist_ok=True)
    allres = {}
    for m in args.models:
        print(f"\n=== {m} ===")
        allres[m] = run_model(m, args.repeat)

    md = ["# 本地模型对比：多场景 × 多语气 × 多次采样", "",
          f"每格 {args.repeat} 次采样，temperature=0.7，max_tokens=300。", ""]
    for key in allres[args.models[0]]:
        md.append(f"\n## {key}\n")
        for m in args.models:
            runs = allres[m].get(key, [])
            tps = [n / dt for _, dt, n in runs if dt > 0 and n > 0]
            avg = sum(tps) / len(tps) if tps else 0
            md.append(f"### {m}  （{avg:.1f} tok/s）\n")
            for i, (text, dt, n) in enumerate(runs, 1):
                md.append(f"**第 {i} 次**（{dt:.1f}s / {n} tok）\n")
                md.append(f"> {text}\n")
    report = OUT / "对比报告.md"
    report.write_text("\n".join(md), encoding="utf-8")
    print(f"\n全文报告：{report}")
    print(f"（{len(CASES)} 个用例 × {args.repeat} 次 × {len(args.models)} 个模型"
          f" = {len(CASES) * args.repeat * len(args.models)} 次生成）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
