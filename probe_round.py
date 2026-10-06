# -*- coding: utf-8 -*-
"""只打印**这一轮**新写入的 s4 事件——排除历史干扰。"""
import json
import os
import subprocess
import sys

HOME = os.path.expandvars(r"%LOCALAPPDATA%\YunXiBot")
LEDGER = os.path.join(HOME, "ledger.jsonl")
TASK = "do-20261006-144736-这个项目的满"
WORK = os.path.expandvars(r"%USERPROFILE%\Desktop\yunxi-rt")


def s4_events():
    rows = []
    for line in open(LEDGER, encoding="utf-8"):
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        d = e.get("data") or {}
        if d.get("step") == "s4" or (
            e.get("kind") == "human_answered" and d.get("step") == "s4"
        ):
            rows.append(e.get("kind"))
    return rows


before = s4_events()
print(f"跑之前 s4 有 {len(before)} 条事件")

r = subprocess.run(
    [r"D:\YunXi Bot\target\release\yunxi-bot.exe", "resume", TASK,
     "--answer", "等于阈值时应当触发，用 >= 而不是 >。"],
    capture_output=True, text=True, encoding="utf-8", cwd=WORK, timeout=900,
    env=dict(os.environ, PYTHONIOENCODING="utf-8"),
)
out = (r.stdout or "") + (r.stderr or "")
print("--- resume 输出（尾部）---")
print("\n".join(out.strip().splitlines()[:6]))

after = s4_events()
new = after[len(before):]
print(f"\n这一轮新写入的 s4 事件： {new}")
