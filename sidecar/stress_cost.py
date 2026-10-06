# -*- coding: utf-8 -*-
"""压力测试（四）：成本与缓存命中——**跑完之后看账，确认没有失控**。

跑它：python sidecar/stress_cost.py

## 为什么缓存命中率是这个项目最该盯的数字

缓存命中比未命中**便宜 50 倍**（DeepSeek）。而命中要求
**前缀逐字节稳定**——这个前提下有好几处会悄悄破坏它：

- 人格/规则/工具定义每轮重新拼接，只要顺序或空白差一点就废
- 压缩历史（D34）天然会改前缀
- 会话中途 cwd 变了（D35 的规则加载）

**而它的失效是静默的**：不报错、不变慢，只表现为**账单变贵**。
所以必须有一个能定期看的数字。

## Agnes 不返回缓存字段

Agnes 的 usage 里没有 `cache_hit_tokens` / `cache_miss_tokens`，
所以那条链路测不了命中率。**这不是 bug，是如实记录**——
而且它免费，所以也不重要。真正要盯的是走 DeepSeek 的复杂任务。

## 阈值定在哪

项目早先实测过 88.3%。这里要求 **≥ 60%** 算合格：
留足余量，因为任务形态不同命中率本来就不同（短任务前缀占比低、
压缩之后要重建一次）。**但它不该低到二三十** —— 那说明前缀在每轮变。
"""
from __future__ import annotations

import json
import os
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

MIN_RATE = 60.0


def main() -> int:
    home = Path(os.environ.get("LOCALAPPDATA", "")) / "YunXiBot"
    ledger = home / "ledger.jsonl"
    if not ledger.exists():
        print(f"没有台账：{ledger}", file=sys.stderr)
        return 2

    agg = defaultdict(lambda: {"calls": 0, "prompt": 0, "hit": 0, "miss": 0})
    for line in ledger.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("kind") != "model_called":
            continue
        d = e.get("data") or {}
        u = d.get("usage") or {}
        key = f"{d.get('provider')}/{d.get('model')}"
        a = agg[key]
        a["calls"] += 1
        a["prompt"] += u.get("prompt_tokens") or 0
        a["hit"] += u.get("cache_hit_tokens") or 0
        a["miss"] += u.get("cache_miss_tokens") or 0

    if not agg:
        print("台账里一条模型调用都没有——先跑点东西再来看。")
        return 1

    ok = True
    print(f"台账：{ledger}")
    for key, a in sorted(agg.items()):
        total = a["hit"] + a["miss"]
        if total:
            rate = a["hit"] / total * 100
            verdict = "✓" if rate >= MIN_RATE else "✗"
            print(
                f"  {verdict} {key:26s} {a['calls']:4d} 次  "
                f"prompt {a['prompt']:>8d}  命中 {a['hit']:>7d} / 未命中 {a['miss']:>6d}  "
                f"命中率 {rate:5.1f}%"
            )
            if rate < MIN_RATE:
                ok = False
        else:
            print(
                f"  ·  {key:26s} {a['calls']:4d} 次  "
                f"prompt {a['prompt']:>8d}  **没有缓存字段**（这个 provider 不返回）"
            )

    print()
    print(f"判据：有缓存字段的 provider 命中率要 ≥ {MIN_RATE:.0f}%")
    print("成本统计：" + ("合格" if ok else "**命中率偏低——前缀可能在每轮变**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
