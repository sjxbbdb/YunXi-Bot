#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「并行工具调用」——goal 第 4 条。

## 耗时证据怎么取

本地文件读得太快（几毫秒），光看总耗时**看不出并行的痕迹**——
串行 3 个 5ms 的读是 15ms，并行也是 5ms 出头，噪声比信号大。

所以证据取的是**区间重叠**：台账现在记录每次调用的
`started_at_ms` 和 `duration_ms`，两个调用的区间相交就是并行。

```text
r1  |--------|
r2  |--------|      <- 三个区间两两相交 = 同时跑的
r3  |--------|
```

这比"总耗时变短"硬得多：串行时区间**永远不可能**重叠。

## 另一半：有副作用的必须串行

同一个台账里，如果有写操作，它们的区间必须**两两不相交**。
否则就是"并行写"——结果取决于调度，事后无法复现。

跑：python sidecar/e2e_parallel.py
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

# 造几个文件让它读
FILES = ["alpha.txt", "beta.txt", "gamma.txt"]


def intervals_overlap(a: dict, b: dict) -> bool:
    """两个 [start, start+duration] 区间是否相交。"""
    a0, a1 = a["started_at_ms"], a["started_at_ms"] + a["duration_ms"]
    b0, b1 = b["started_at_ms"], b["started_at_ms"] + b["duration_ms"]
    return a0 < b1 and b0 < a1


def read_tool_calls(home: Path) -> list[dict]:
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
        if e.get("kind") == "tool_called":
            out.append(e.get("data") or {})
    return out


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

    with tempfile.TemporaryDirectory(prefix="yunxi-par-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")

        work = home / "work"
        work.mkdir()
        for f in FILES:
            (work / f).write_text(f"{f} 的内容\n" * 20, encoding="utf-8")

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        # ---- 三个独立读操作 ----
        print("=== 让模型一次读三个文件 ===")
        r = subprocess.run(
            [str(BIN), "chat", "--id", "par", "--thinking", "off"],
            input=(
                f"把这三个文件内容各读一遍，然后只说「读完了」："
                f"{', '.join(FILES)}。请在一次里同时发出这三个读取。\n/exit\n"
            ),
            capture_output=True, text=True, encoding="utf-8",
            env=env, timeout=400, cwd=str(work),
        )
        out = (r.stdout or "") + (r.stderr or "")
        print(out[-600:])

        check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")

        calls = read_tool_calls(home)
        reads = [c for c in calls if c.get("tool") == "read_file"]
        print(f"      台账里 {len(calls)} 次工具调用，其中读文件 {len(reads)} 次")

        check("**模型确实一次发了多个读**", len(reads) >= 2,
              f"只有 {len(reads)} 次读——模型没批量发，这次验不了并行")
        check("记了 started_at_ms", all("started_at_ms" in c for c in calls),
              "没记时序就没法证明并行")
        check("记了 duration_ms", all("duration_ms" in c for c in calls))

        if len(reads) >= 2:
            # **要找的是"同一批"。** 模型可能分几轮发，
            # 所以按开始时刻聚簇：开始时刻差在 1 秒内的算一批。
            reads_sorted = sorted(reads, key=lambda c: c["started_at_ms"])
            base = reads_sorted[0]["started_at_ms"]
            batch = [c for c in reads_sorted if c["started_at_ms"] - base < 1000]

            pairs = 0
            overlapped = 0
            for i in range(len(batch)):
                for j in range(i + 1, len(batch)):
                    pairs += 1
                    if intervals_overlap(batch[i], batch[j]):
                        overlapped += 1
            print(f"      同一批 {len(batch)} 个读，{overlapped}/{pairs} 对区间相交")
            for c in batch:
                print(f"        {c.get('tool')}  +{c['started_at_ms'] - base}ms  "
                      f"耗时 {c['duration_ms']}ms")
            check("**同一批的读区间相交（真并行）**",
                  overlapped > 0,
                  "串行时区间永远不可能重叠")

        # ---- 写入必须串行 ----
        print()
        print("=== 写操作的区间必须两两不相交 ===")
        #
        # REPL 里写操作要问人，而脚本把 stdin 占着——所以"写入串行"
        # 这一层由单元测试覆盖：`write_calls_never_run_concurrently`
        # 直接断言并发峰值恒为 1，比在这儿绕路更硬。
        #
        # 这里只对**台账里实际出现过的**写调用做交叉检查。
        writes = [c for c in calls if c.get("tool") in ("write_file", "edit_file")]
        if writes:
            bad = 0
            for i in range(len(writes)):
                for j in range(i + 1, len(writes)):
                    if intervals_overlap(writes[i], writes[j]):
                        bad += 1
            check("**写操作区间不相交（没并行）**", bad == 0,
                  f"{bad} 对写操作重叠了")
        else:
            print("      （这一轮没有写操作，写入串行由单元测试覆盖）")
            print("      见 write_calls_never_run_concurrently")

        # ---- 单元测试确实覆盖了写入串行 ----
        print()
        print("=== 单元测试覆盖（写入串行 + 并发峰值）===")
        r3 = subprocess.run(
            ["cargo", "test", "-p", "yunxi-bot-core", "--lib", "parallel_tests",
             "--", "--nocapture"],
            capture_output=True, text=True, encoding="utf-8",
            cwd=str(Path(BIN).parent.parent.parent),
            timeout=600,
        )
        t = (r3.stdout or "") + (r3.stderr or "")
        check("并行单元测试全过", "test result: ok" in t and "0 failed" in t,
              t.strip().splitlines()[-1] if t.strip() else "")
        check("覆盖了写入不并行",
              "write_calls_never_run_concurrently" in t)

    print()
    print("并行工具调用端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
