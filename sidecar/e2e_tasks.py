#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「任务推进」——goal 开头点名的那条断链：

> 任务卡住后**没有任何东西自动推进它**

## 怎么算证明

1. 造一个**没人管的任务**：处于 `Running`、且已经安静了很久
2. 起 daemon，**不给它任何命令**
3. 等它自己那一轮到点
4. 检查：它自己接手了、推进了、留了台账

## 三条必须同时成立的性质

- **接手了**：安静够久的 Running 任务被推进
- **不抢**：刚刚还在动的任务不动（否则会和终端里的使用者同时执行）
- **不越权**：`AwaitingHuman` 的任务**绝不被续跑**——那等于自动批准

跑：python sidecar/e2e_tasks.py
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

LEDGER_HEADER = {"yunxi_bot_ledger": 1}


def seq_events(events: list[dict]) -> list[str]:
    """把事件加上 seq 与表头，凑成一份合法台账。"""
    lines = [json.dumps(LEDGER_HEADER, ensure_ascii=False)]
    for i, e in enumerate(events, start=1):
        e = dict(e)
        e.setdefault("seq", i)
        e.setdefault("data", {})
        lines.append(json.dumps(e, ensure_ascii=False))
    return lines


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


def kind_of(e: dict) -> str | None:
    return e.get("kind")


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    with tempfile.TemporaryDirectory(prefix="yunxi-tasks-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        os.environ["YUNXI_BOT_HOME"] = str(home)

        # 造三个任务，覆盖三种该被区别对待的情况。
        # 时间戳都设在很久以前 —— "已经安静了"是这一轮的判定依据。
        long_ago = int(time.time() * 1000) - 3_600_000  # 一小时前
        just_now = int(time.time() * 1000)              # 刚刚

        events = [
            # ① 被放弃的：Running 且安静了一小时 -> 该被接手
            {"at": long_ago, "kind": "task_created",
             "data": {"task": "abandoned", "goal": "数一下一到十"}},
            {"at": long_ago, "kind": "task_state_changed",
             "data": {"task": "abandoned", "state": "running"}},

            # ② 正在被跑的：Running 但刚刚还有活动 -> **不该被抢**
            {"at": just_now, "kind": "task_created",
             "data": {"task": "busy", "goal": "正在终端里跑的任务"}},
            {"at": just_now, "kind": "task_state_changed",
             "data": {"task": "busy", "state": "running"}},

            # ③ 等人的：**绝不该被自动续跑**——那等于自动批准
            {"at": long_ago, "kind": "task_created",
             "data": {"task": "waiting", "goal": "需要你批准才能继续的任务"}},
            {"at": long_ago, "kind": "task_state_changed",
             "data": {"task": "waiting", "state": "awaiting_human"}},
        ]

        (home / "ledger.jsonl").write_text(
            "\n".join(seq_events(events)) + "\n", encoding="utf-8"
        )

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        print("=== 起守护（不给任何命令，看它会不会自己接手）===")
        proc = subprocess.Popen(
            [str(BIN), "daemon",
             "--interval", "500",
             "--assistant-interval", "0",     # 关掉邮件，这轮只看任务
             "--task-interval", "1",          # 一秒看一眼
             "--task-idle", "60",             # 安静 60 秒以上的才接手
             "--console"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", env=env,
        )

        def wait_for(pred, timeout=40.0):
            t0 = time.time()
            while time.time() - t0 < timeout:
                if pred():
                    return True, time.time() - t0
                time.sleep(0.4)
            return False, time.time() - t0

        # ---- 1. 它自己接手了被放弃的任务 ----
        got, secs = wait_for(lambda: any(
            kind_of(e) in ("step_running", "task_planning", "task_state_changed")
            and (e.get("data") or {}).get("task") == "abandoned"
            and e.get("at", 0) > just_now
            for e in read_events(home)
        ))
        check("**它自己接手了被放弃的任务**", got,
              f"{secs:.1f} 秒" if got else "超时")

        # ---- 2. 等人的任务被报出来了 ----
        got2, _ = wait_for(lambda: any(
            kind_of(e) == "task_attention_notified"
            and (e.get("data") or {}).get("task") == "waiting"
            for e in read_events(home)
        ))
        check("**等人的任务被主动报出来**（而不是静静躺着）", got2)

        # ---- 3. 不被抢、不越权 ----
        time.sleep(6)  # 再跑几轮，看有没有越界
        events2 = read_events(home)

        busy_touched = [
            e for e in events2
            if (e.get("data") or {}).get("task") == "busy" and e.get("at", 0) > just_now
        ]
        check("**刚刚还在动的任务没被抢**", not busy_touched,
              f"{len(busy_touched)} 条越界事件" if busy_touched else "")

        wait_touched = [
            e for e in events2
            if (e.get("data") or {}).get("task") == "waiting"
            and kind_of(e) in ("step_running", "task_planning", "task_state_changed")
            and e.get("at", 0) > just_now
        ]
        check("**等人的任务绝没被自动续跑**（那等于自动批准）", not wait_touched,
              f"{len(wait_touched)} 条越权事件" if wait_touched else "")

        # 只提醒一次
        notices = [
            e for e in events2
            if kind_of(e) == "task_attention_notified"
            and (e.get("data") or {}).get("task") == "waiting"
        ]
        check("同一个任务只提醒一次（不刷屏）", len(notices) == 1,
              f"{len(notices)} 次")

        # 诊断：abandoned 的状态事件序列
        print()
        print("=== abandoned 的状态事件 ===")
        for e in events2:
            d = e.get("data") or {}
            if d.get("task") == "abandoned" and kind_of(e) in ("task_state_changed", "task_finished", "task_planned"):
                print("    at=%s %s state=%s" % (e.get("at"), kind_of(e), d.get("state")))

        proc.terminate()
        try:
            out, _ = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            out, _ = proc.communicate()

        print()
        print("=== 守护输出（尾部）===")
        for line in (out or "").strip().splitlines()[-12:]:
            print("  " + line)

        joined = out or ""
        check("启动时报告了任务通道", "任务" in joined)
        check("明确说了审批者是一律拒绝",
              "一律拒绝" in joined, "无人值守不获得新权限")

    print()
    print("任务推进端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
