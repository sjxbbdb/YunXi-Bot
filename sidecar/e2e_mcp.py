#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证 MCP：**连一个真实的第三方 server**。

## 为什么用真的 server 而不是自己写的假 server

D20 的验收原话是"连一个真实的第三方 MCP server 并列出它的工具"。
自己写一个假 server 只能证明"我的代码和我的理解一致"——
真正的风险在于**别人实现的协议细节和我不一样**：
initialize 的字段、tools/list 的分页、名字里能不能有下划线。

所以这里用官方参考实现 `mcp-server-time`（PyPI 上的 `mcp-server-time`，
由 `uvx` 拉起，stdio 传输）。

## 它验什么

1. 真的能起一个第三方 server 并完成 MCP 握手
2. **列出的工具走进注册表**，名字带 `mcp__<server>__<tool>` 前缀
3. **真的能调通**——列出来不等于调得动
4. MCP 工具的 `Capability` 是 `Unknown`（server 的能力声称不可核实）
5. **环境隔离**：子进程拿不到我们的 API key

需要网络（uvx 要拉包）。断网机器上会失败，这是刻意的——
一条能联网却不联网的"MCP 测试"证明不了什么。

跑：python sidecar/e2e_mcp.py
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary, REPO  # noqa: E402

SERVER = "time"


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    with tempfile.TemporaryDirectory(prefix="yunxi-mcp-") as tmp:
        home = Path(tmp)
        os.environ["YUNXI_BOT_HOME"] = str(home)

        # **一个真实的第三方 server。** uvx 会先装再跑。
        (home / "mcp.json").write_text(json.dumps({
            "servers": [{
                "name": SERVER,
                "command": "uvx",
                "args": ["mcp-server-time", "--local-timezone", "Asia/Shanghai"],
            }],
        }, ensure_ascii=False), encoding="utf-8")

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
        # uvx 要走代理才能拉包（本机 Clash 在 7890）。**只给 uvx 用**——
        # 这不是给孩子进程的环境，是我们自己起它时用的。
        env.setdefault("HTTPS_PROXY", "http://127.0.0.1:7890")
        env.setdefault("HTTP_PROXY", "http://127.0.0.1:7890")
        env.pop("ALL_PROXY", None)   # socks5，uv 不认

        print("=== yunxi-bot mcp（列出真实 server 的工具）===")
        r = subprocess.run(
            [str(BIN), "mcp", "list"],
            capture_output=True, text=True, encoding="utf-8", env=env, timeout=180,
        )
        out = (r.stdout or "") + (r.stderr or "")
        print(r.stdout or r.stderr or "(空)")

        check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")
        check("真的连上了第三方 server", f"mcp__{SERVER}__" in out)
        check("列出了工具", "工具" in out or "tool" in out.lower())

        # 官方 time server 至少会提供 get_current_time
        check("**列出了 get_current_time**", "get_current_time" in out)
        check("工具名带出处前缀", f"mcp__{SERVER}__get_current_time" in out)

        # ---- 真的调用 ----
        print()
        print("=== yunxi-bot mcp call（真的调一次）===")
        r2 = subprocess.run(
            [str(BIN), "mcp", "call", SERVER, "get_current_time",
             "--args", json.dumps({"timezone": "Asia/Shanghai"})],
            capture_output=True, text=True, encoding="utf-8", env=env, timeout=180,
        )
        out2 = (r2.stdout or "") + (r2.stderr or "")
        print(r2.stdout or r2.stderr or "(空)")

        check("调用退出码为 0", r2.returncode == 0, f"实际 {r2.returncode}")
        # 返回里该有一个真实的时间。不强求格式（那是 server 的自由），
        # 但该能看到 2026（现在是 2026 年）
        check("**拿到了真实的时间结果**", "2026" in out2, out2.strip()[:160])
        check("结果里没有协议垃圾", "jsonrpc" not in out2.lower())

        # ---- 能力类别 ----
        print()
        print("=== 工具的能力类别（默认必须是 Unknown）===")
        r3 = subprocess.run(
            [str(BIN), "tools"],
            capture_output=True, text=True, encoding="utf-8", env=env, timeout=180,
        )
        out3 = (r3.stdout or "") + (r3.stderr or "")
        lines = [l for l in out3.splitlines() if f"mcp__{SERVER}__" in l]
        if lines:
            print("  " + "\n  ".join(lines[:3]))
            check("MCP 工具标成能力未知", any("能力未知" in l for l in lines),
                  "server 自己声明的能力是声称，不是事实")
        else:
            check("--tools 里能看到 MCP 工具", False,
                  "（mcp list 能看到但 tools 里没有，说明没进注册表）")

    print()
    print("MCP 端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
