#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「交互式会话」——通用 agent 的核心体验。

## 它验什么

1. **连续对话**：三轮里第三轮能引用第一轮说过的内容（这就是"上下文"）
2. **退出再进来**：`--resume` 之后它还记得
3. **每轮落盘**：不是退出时才存——一次崩溃不该丢掉整场对话
4. **斜杠命令不进模型**：`/history` 不该花掉一次调用

## 为什么这是"通用 agent"的关键

别的命令都是"做一件事就退"。对话的价值在延续：你说了"这个文件"，
下一句的"它"才有指代对象。没有这个，就只是"发命令—等结果"。

## 需要真模型

用的是 Agnes（免费档）。三轮对话 + 可能的工具调用，
10 RPM 下会有限流等待——脚本超时给足。

跑：python sidecar/e2e_chat.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

# 第一轮埋的东西。后面两轮要能把它说出来——**这就是"记住了"**。
SECRET_WORD = "青绿色"


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    real_home = Path(os.environ.get("LOCALAPPDATA", "")) / "YunXiBot"
    key = real_home / "secrets" / "agnes.key"
    if not key.exists():
        print(f"没有 agnes.key（{key}），这个验证需要真模型", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="yunxi-chat-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        sid = "e2e-memory"

        def run_chat(lines: list[str], *extra: str, timeout: int = 300):
            inp = "\n".join(lines) + "\n"
            return subprocess.run(
                [str(BIN), "chat", "--id", sid, *extra],
                input=inp, capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=timeout, cwd=str(home),
            )

        # ---- 第 1 步：三轮对话，第一轮埋一个词 ----
        print("=== 一轮会话里聊三轮 ===")
        r1 = run_chat([
            f"请记住：我最喜欢的颜色是{SECRET_WORD}。只回复：「记住了」。",
            "我刚才说我喜欢的颜色是什么？只回复那个颜色名。",
            "再说一遍那个颜色。只回复颜色名。",
            "/exit",
        ])
        out1 = (r1.stdout or "") + (r1.stderr or "")
        print(out1[-1200:])

        check("退出码为 0", r1.returncode == 0, f"实际 {r1.returncode}")
        check("**第三轮还记得第一轮说的颜色**", out1.count(SECRET_WORD) >= 2,
              f"出现 {out1.count(SECRET_WORD)} 次（第一轮埋的 + 后两轮答的）")

        # ---- 第 2 步：会话文件落盘了 ----
        print()
        print("=== 会话落盘 ===")
        sp = home / "sessions" / f"{sid}.json"
        check("会话文件已生成", sp.exists(), str(sp))
        turns = 0
        if sp.exists():
            data = json.loads(sp.read_text(encoding="utf-8"))
            turns = data.get("turns", 0)
            check("记了三轮", turns == 3, f"turns={turns}")
            hist = (data.get("layout") or {}).get("history") or []
            check("历史里有多条消息", len(hist) >= 4, f"{len(hist)} 条")
            check("有稳定前缀（缓存命中的依据）",
                  bool((data.get("layout") or {}).get("stable")))
            check("有指纹", "fingerprint" in data)

        # ---- 第 3 步：--resume 之后还记得 ----
        print()
        print("=== 退出再进来（--resume）===")
        r2 = run_chat([
            "我喜欢的颜色是什么？只回复颜色名。",
            "/exit",
        ], "--resume")
        out2 = (r2.stdout or "") + (r2.stderr or "")
        print(out2[-900:])

        check("退出码为 0", r2.returncode == 0, f"实际 {r2.returncode}")
        check("说了「接着上次聊」", "接着上次聊" in out2)
        check("**新进程里还记得那个颜色**", SECRET_WORD in out2,
              "这就是跨进程的上下文")

        # ---- 第 4 步：斜杠命令不进模型 ----
        print()
        print("=== 斜杠命令 ===")
        r3 = run_chat(["/history", "/exit"], "--resume")
        out3 = (r3.stdout or "") + (r3.stderr or "")
        check("/history 有输出", "轮" in out3)
        # 它不该花一次模型调用——表现是不出现路由标记
        check("斜杠命令没触发模型调用", "[" not in out3.split("›")[-1][:20] if "›" in out3 else True,
              "")

        # ---- 第 5 步：会话能列出来 ----
        print()
        print("=== chat list ===")
        r4 = subprocess.run(
            [str(BIN), "chat", "list"],
            capture_output=True, text=True, encoding="utf-8", env=env, timeout=60,
        )
        out4 = (r4.stdout or "") + (r4.stderr or "")
        print(out4.strip())
        check("列出了会话", sid in out4)
        check("显示了轮数", "轮" in out4)

    print()
    print("交互式会话端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
