#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端脚本的公共部分。

## 为什么有一个"先重建二进制"的函数

端到端脚本调的是 `target/debug/yunxi-bot.exe`。而 **`cargo clippy` 和
`cargo test` 都不会刷新那个文件**——它们各自产出自己的中间物。

于是有一个很难发现的陷阱：改完代码跑 `cargo clippy`（干净）+
`cargo test`（全绿），接着跑端到端，**实际验的是上一次 build 的旧二进制**。
结果是要么假通过，要么报一个已经修掉的 bug——而两种都会让人往错的方向查。

这个坑真踩过一次：`NoticeSent` 缺字段的 bug 明明修了，
端到端却还在报旧行为。所以这里**主动重建**。多花十几秒，
比一个假结果便宜得多。
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def binary_path() -> Path:
    p = REPO / "target" / "debug" / "yunxi-bot.exe"
    return p if p.exists() else REPO / "target" / "debug" / "yunxi-bot"


def ensure_fresh_binary(quiet: bool = False) -> Path:
    """重建并返回二进制路径。**失败直接退出**，绝不退回用旧的。"""
    if not quiet:
        print("重建二进制（避免验到旧版本）…", flush=True)
    r = subprocess.run(
        ["cargo", "build", "--workspace"],
        cwd=str(REPO), capture_output=True, text=True, encoding="utf-8",
    )
    if r.returncode != 0:
        print("cargo build 失败，端到端没法跑：", file=sys.stderr)
        print((r.stderr or "")[-3000:], file=sys.stderr)
        raise SystemExit(2)
    b = binary_path()
    if not b.exists():
        print(f"构建成功但找不到二进制 {b}", file=sys.stderr)
        raise SystemExit(2)
    return b
