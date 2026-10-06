# -*- coding: utf-8 -*-
"""压力测试（一）：并发写同一本台账，看会不会互相踩。

跑它：python sidecar/stress_concurrent.py

## 为什么先测这个

台账是整个系统的**唯一真相来源**——审批、成本、任务状态全靠它。
而它是**追加式的 JSONL 文件**，进程内靠 `seen_len` + `sync_seq_from_disk`
维持序号一致（D30 那一族的问题）。

多写入者是它最脆弱的地方：
- 两个进程同时读到同一个文件长度 → 算出一样的序号 → **序号重复**
- 一行写一半被另一个进程读到 → **解析失败**（而项目规定"无法解析的行视为损坏"）

这两件事都不会让程序当场崩，只会让**事后追查失去依据**——
而那是这个项目最在乎的东西。

## 每一条结论都要有数字
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

ROUNDS = 3          # 同时跑的实例数
PER_INSTANCE = 3    # 每个实例写几条


def read_ledger(path: Path):
    """返回 (总行数, 坏行数, 序号列表)。"""
    if not path.exists():
        return 0, 0, []
    total = bad = 0
    seqs = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s:
            continue
        total += 1
        try:
            e = json.loads(s)
        except json.JSONDecodeError:
            bad += 1
            continue
        if "seq" in e:
            seqs.append(e["seq"])
    return total, bad, seqs


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    real_home = Path(os.environ.get("LOCALAPPDATA", "")) / "YunXiBot"
    if not (real_home / "secrets" / "agnes.key").exists():
        print("没有 agnes.key，这个验证需要真模型", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="yunxi-conc-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        for n in ("agnes.key", "deepseek.key", "bocha.key"):
            src = real_home / "secrets" / n
            if src.exists():
                shutil.copy(src, home / "secrets" / n)
        work = home / "work"
        work.mkdir()
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
        ledger = home / "ledger.jsonl"

        print(f"=== {ROUNDS} 个实例同时跑，每个 {PER_INSTANCE} 次写台账 ===")
        # 每个实例跑 `think`：它会产生 ModelCalled 事件（写台账），
        # 而且不碰别的状态——**把变量压到只剩"并发写"这一件事**。
        procs = []
        t0 = time.monotonic()
        for i in range(ROUNDS):
            for j in range(PER_INSTANCE):
                p = subprocess.Popen(
                    [str(BIN), "think", f"回答这一个字：{i}{j}", "--thinking", "off"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    env=env, cwd=str(work),
                )
                procs.append(p)
        for p in procs:
            p.wait(timeout=600)
        elapsed = time.monotonic() - t0
        codes = [p.returncode for p in procs]
        print(f"      共 {len(procs)} 个进程，耗时 {elapsed:.1f} 秒")
        print(f"      退出码：{codes}")

        check("**所有实例都跑完了**", all(c == 0 for c in codes),
              f"有非零退出码：{codes}")

        print()
        print("=== 台账完整性 ===")
        total, bad, seqs = read_ledger(ledger)
        print(f"      台账 {total} 行；坏行 {bad} 行；带序号的 {len(seqs)} 条")
        check("**没有写坏的行**", bad == 0,
              f"{bad} 行解析不了——那一行的内容就永远查不回来了")
        check("**序号没有重复**", len(seqs) == len(set(seqs)),
              f"{len(seqs)} 条序号里只有 {len(set(seqs))} 个不同的")
        check("序号是递增的", seqs == sorted(seqs), "乱序说明有两本账在各自计数")

        # 每个进程至少该留一条 ModelCalled
        model_calls = 0
        for line in ledger.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                continue
            if e.get("kind") == "model_called":
                model_calls += 1
        print(f"      model_called 事件 {model_calls} 条")
        check("**每次调用都留痕了**", model_calls >= len(procs),
              f"{len(procs)} 次调用只留了 {model_calls} 条——有调用没记账")

    print()
    print("并发压力测试：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
