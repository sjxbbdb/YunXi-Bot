#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「审批提示里能看到 diff」——goal 第 5 条的验收项。

## 为什么要单独一个脚本

这条是我**手验过一次**的：跑一轮 chat，肉眼看审批框里有没有出现
`- 旧的这一行 / + 新的这一行`。手验的问题不是不可靠，而是**不会重现**——
下次谁改坏了审批框，没有任何东西会告诉他。

所以把它固化成脚本：跑一轮会让模型调 `edit_file` 的对话，
**把审批提示抓下来断言**。

## 怎么在脚本里应答审批

REPL 的输入流是：
1. 第一行：给模型的提示词
2. 第二行：审批框读的那一行（这里给 `n` ——**拒绝**）
3. 第三行：`/exit`

给 `n` 是有意的：**验证的是"提示里显示了什么"，不是"批了之后会怎样"**。
拒绝还能顺带确认"拒绝真的没执行"（文件内容一个字节没变）。

跑：python sidecar/e2e_preview.py
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

ORIGINAL = "第一行\n旧的这一行\n第三行\n"


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
        print("没有 agnes.key，这个验证需要真模型", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="yunxi-preview-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")
        work = home / "work"
        work.mkdir()
        target = work / "notes.txt"
        target.write_text(ORIGINAL, encoding="utf-8")

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        print("=== 一轮会让模型改文件的对话（审批时给 n）===")
        r = subprocess.run(
            [str(BIN), "chat", "--id", "pv", "--thinking", "off"],
            input=(
                "读一下 notes.txt，然后把里面「旧的这一行」改成「新的这一行」。\n"
                "n\n"
                "/exit\n"
            ),
            capture_output=True, text=True, encoding="utf-8",
            env=env, timeout=400, cwd=str(work),
        )
        out = (r.stdout or "") + (r.stderr or "")
        print(out[-1600:])

        check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")

        print()
        print("=== 审批提示的内容 ===")
        check("**出现了审批框**", "需要你确认" in out,
              "模型没走到写文件那一步——这次验不了（多半是它选择直接回答）")
        check("**有「会变成」那一段**", "会变成" in out,
              "没有它的话审批框里只有参数 JSON")
        check("**看得到要改的文件**", "动的是" in out or "notes.txt" in out)
        check("**看得到被删掉的行**", "- 旧的这一行" in out,
              "这是这一条的核心：被改掉的东西必须看得见")
        check("**看得到换成的行**", "+ 新的这一行" in out)
        check("看得到上下文", "第一行" in out or "第三行" in out,
              "没有上下文的话，改在哪儿看不出来")
        check("说了改了几行", "加 1 行" in out or "删 1 行" in out)
        check("参数也还在（想核对时有得看）",
              "old_string" in out or "参数" in out)

        print()
        print("=== 拒绝之后文件没被改 ===")
        check("**文件一个字节没变**",
              target.read_text(encoding="utf-8") == ORIGINAL,
              "给了 n 却还是改了")

    print()
    print("审批 diff 预览端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
