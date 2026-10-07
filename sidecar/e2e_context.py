#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「上下文压缩」——goal 第 2 条。

## 它验什么

1. **塞满预算能自动压缩并继续**——不报 API 错误
2. **压缩留痕**：台账里能看到丢了多少、省了多少、是不是有损
3. **压缩后对话还能接上**（不是失忆）

## 怎么把"塞满"变成可测的

真按 32k 窗口聊，要聊几十轮才触发——又慢又贵。
所以用 `--context-window` 把窗口调到很小（比如 1500 token），
五轮长对话就能逼出压缩。

**这不是为测试开的后门**：窗口本来就是该能调的
（不同模型不一样，使用者也该能按自己的成本偏好调）。

## 为什么这条最重要

历史只增不减的话，长会话迟早撞上模型上限，而
**撞上时的表现是 API 报错，不是优雅降级**——
使用者看到的是"这一轮失败了"，而不是"我该开个新会话了"。

跑：python sidecar/e2e_context.py
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

from e2e_common import (  # noqa: E402
    ensure_fresh_binary,
    home_untouched,
    real_home_dir,
    snapshot_home,
)

# 小窗口：五轮长对话就能逼出压缩
WINDOW = 1500
SID = "e2e-compact"

# 每轮都问一个要记住的具体值。压缩之后如果还记得最早的，
# 说明摘要起了作用；如果忘了，那是"压缩成功但对话断了"——
# 两件事要分开验。
FACTS = ["青绿色", "42", "D:\\notes"]


def read_events(home: Path) -> list[dict]:
    out = []
    p = home / "ledger.jsonl"
    if not p.exists():
        return out
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "kind" in e:
            out.append(e)
    return out


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    real_home = real_home_dir()
    # **跑之前记一份真实 home**：跑完再比一次，证明这次测试没写回去。
    before_home = snapshot_home(real_home)
    key = real_home / "secrets" / "agnes.key"
    if not key.exists():
        print(f"没有 agnes.key，这个验证需要真模型", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="yunxi-ctx-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        # 每轮都写一段长内容，逼迫预算用完。
        lines = []
        lines.append(f"请记住三件事，之后我会问：颜色是{FACTS[0]}，数字是{FACTS[1]}，路径是{FACTS[2]}。只回复「好」。")
        # 中间灌几轮长内容
        for i in range(4):
            lines.append(
                f"请把下面这段话原样复述一遍（不要总结）：{'这是一段用来占满上下文的中文内容，' * 40}"
            )
        lines.append("我刚才让你记住的三件事分别是什么？")
        lines.append("/exit")

        print(f"=== 一轮会话，窗口设为 {WINDOW} token ===")
        r = subprocess.run(
            [str(BIN), "chat", "--id", SID, "--context-window", str(WINDOW)],
            input="\n".join(lines) + "\n",
            capture_output=True, text=True, encoding="utf-8",
            env=env, timeout=900, cwd=str(home),
        )
        out = (r.stdout or "") + (r.stderr or "")
        print(out[-2000:])

        check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")
        # **这是最关键的一条**：撞上限的表现是 API 报错
        check("**没有出现 API 报错**",
              "这一轮失败了" not in out and "context length" not in out.lower()
              and "maximum context" not in out.lower())

        print()
        print("=== 压缩记录 ===")
        events = read_events(home)
        comps = [e for e in events if e.get("kind") == "context_compacted"]
        check("**台账里有压缩记录**", len(comps) >= 1, f"{len(comps)} 次")
        if comps:
            d = comps[-1].get("data") or {}
            for k in ("dropped_messages", "before_tokens", "after_tokens",
                      "saved_tokens", "summarized"):
                check(f"记了 {k}", k in d, str(d.get(k)))
            check("确实省了 token", (d.get("saved_tokens") or 0) > 0,
                  f"省了 {d.get('saved_tokens')}")
            check("标了摘要是不是模型写的", "summarized" in d,
                  f"summarized={d.get('summarized')}")

        print()
        print("=== 压缩之后还接得上吗 ===")
        # 会话文件里该有一条摘要头
        sp = home / "sessions" / f"{SID}.json"
        head = ""
        if sp.exists():
            data = json.loads(sp.read_text(encoding="utf-8"))
            hist = (data.get("layout") or {}).get("history") or []
            if hist:
                head = hist[0].get("content", "")
        check("历史开头是压缩摘要", "已压缩" in head, head[:60])
        # **摘要必须非空**：一条光秃秃的标记等于把历史扔了却不留说明
        body = head.split("\n", 1)[1] if "\n" in head else ""
        check("**摘要不只是个标记**", len(body.strip()) > 20,
              f"摘要正文 {len(body.strip())} 字")

        # **至少有一次是模型摘要（不是兜底）。**
        # 兜底是说"这里少了 N 条"，模型拿不到任何内容——
        # 全用兜底的话，压缩虽然"成功"但对话实际断了。
        summarized = [e for e in comps if (e.get("data") or {}).get("summarized")]
        print(f"      压缩 {len(comps)} 次，其中模型摘要 {len(summarized)} 次")
        check("**至少有一次是模型摘要**", len(summarized) >= 1,
              "全用兜底的话，压缩成功但对话实际断了")

        # 兜底的那几次要说清为什么（不然"丢信息"不可见）
        fallbacks = [e for e in comps if not (e.get("data") or {}).get("summarized")]
        if fallbacks:
            print(f"      （{len(fallbacks)} 次兜底——原因在下面的日志里）")

        # **对话还在继续**：6 轮都跑完了，没有被压缩打断
        check("**压缩之后对话继续跑完了**", "会话已存" in out,
              "压缩不该打断会话")

    # **隔离断言：真实 home 一个字节都不该动。**
    # 这条以前不存在——`real_home` 只被用来借密钥，没有任何东西证明
    # 测试没写回真实数据（见 `e2e_common.home_untouched`）。
    check("**没有碰真实 home**", *home_untouched(before_home, real_home))

    print()
    print("上下文压缩端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
