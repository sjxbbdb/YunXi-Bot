# -*- coding: utf-8 -*-
"""跑一次真实任务，把**每一步的最终状态和失败原因**打全。

## 为什么要这个

上一轮 e2e_realtask 有一次跑完停在"卡住"：人的答案生效了、文件也改对了，
但后面某个步骤失败了。而 `resume` 的输出被 `Select-Object -Last N` 截断，
看不全——**只看到结论，看不到是哪一步、为什么**。

排查要看到"哪一步、什么错"，不是"任务卡住了"。
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

BIN = r"D:\YunXi Bot\target\release\yunxi-bot.exe"
REAL_HOME = Path(os.environ["LOCALAPPDATA"]) / "YunXiBot"

BUGGY = '''"""计费逻辑。"""

RATES = {"basic": 10, "pro": 30, "team": 80}

# 满 100 减 20
DISCOUNT_THRESHOLD = 100
DISCOUNT_AMOUNT = 20


def subtotal(plan: str, seats: int) -> int:
    return RATES[plan] * seats


def total(plan: str, seats: int) -> int:
    s = subtotal(plan, seats)
    # BUG: 应该用 >= ，写成了 >
    if s > DISCOUNT_THRESHOLD:
        return s - DISCOUNT_AMOUNT
    return s
'''

ANSWER = "满 100（含）减 20，也就是用 >= 而不是 >。"


def show_task(env, cwd, task_id):
    r = subprocess.run([BIN, "tasks", task_id], capture_output=True, text=True,
                       encoding="utf-8", env=env, cwd=cwd, timeout=300)
    print((r.stdout or "") + (r.stderr or ""))


def main():
    tmp = tempfile.mkdtemp(prefix="yunxi-diag-")
    root = Path(tmp)
    home = root / "home"
    (home / "secrets").mkdir(parents=True)
    for n in ("agnes.key", "deepseek.key", "bocha.key"):
        src = REAL_HOME / "secrets" / n
        if src.exists():
            shutil.copy(src, home / "secrets" / n)
    work = root / "proj"
    (work / "src").mkdir(parents=True)
    (work / "notes").mkdir()
    (work / "README.md").write_text(
        "# 项目说明\n\n计费逻辑在 src/billing.py。\n已知问题：满减的边界条件写错了。\n",
        encoding="utf-8")
    (work / "src" / "billing.py").write_text(BUGGY, encoding="utf-8")
    (work / "notes" / "journal.md").write_text(
        "2026-10-01\n- pro 3 席 = 90，没打折，是对的\n\n2026-10-02\n- 边界那块感觉不对\n",
        encoding="utf-8")

    env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
    goal = "这个项目的满减边界条件有问题。找出问题、修好它、然后验证修复。"

    print("=== 第一次 do ===")
    r1 = subprocess.run([BIN, "do", goal, "--yes"], capture_output=True, text=True,
                        encoding="utf-8", env=env, cwd=str(work), timeout=2400)
    out1 = (r1.stdout or "") + (r1.stderr or "")
    print("\n".join(out1.strip().splitlines()[-4:]))

    m = re.search(r"yunxi-bot resume ([^\s`]+)", out1)
    if not m:
        print("\n没停，直接看结果：")
        print((work / "src" / "billing.py").read_text(encoding="utf-8"))
        return
    tid = m.group(1)

    print(f"\n=== resume（给答案）===")
    r2 = subprocess.run([BIN, "resume", tid, "--yes", "--answer", ANSWER],
                        capture_output=True, text=True, encoding="utf-8",
                        env=env, cwd=str(work), timeout=2400)
    out2 = (r2.stdout or "") + (r2.stderr or "")
    print("\n".join(out2.strip().splitlines()[-3:]))

    print("\n=== 最终步骤明细 ===")
    show_task(env, str(work), tid)

    print("\n=== 台账里失败/跳过的步骤 ===")
    led = home / "ledger.jsonl"
    for line in led.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        d = e.get("data") or {}
        if d.get("task") != tid:
            continue
        k = e.get("kind")
        if k in ("step_failed", "step_skipped", "step_succeeded"):
            detail = (d.get("error") or d.get("reason") or d.get("result") or "")
            print(f"  {k:16s} {d.get('step'):5s} {detail.replace(chr(10), ' ')}")

    # 出错时把临时目录留下，好进去翻台账原文
    if os.environ.get("KEEP"):
        print(f"\n临时目录保留在: {tmp}")
    else:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
