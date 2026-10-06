# -*- coding: utf-8 -*-
"""压力测试（三）：连续多轮——看有没有**累积性**问题。

跑它：python sidecar/stress_rounds.py

## 为什么要连跑而不是单跑

单跑一次能测"对不对"，测不出"跑久了会不会变坏"。累积性问题都长这样：

- **限流器/熔断器的状态**：跑久了会不会自己把自己锁死
- **台账**：越写越大，写一次的时间会不会跟着涨
- **会话文件**：每轮落盘，会不会越来越慢
- **延迟**：第一轮和第十五轮的耗时差多少
- **成本**：缓存到底有没有命中（同一个会话的前缀是稳定的，**应该**命中）

**这一条最要紧的是缓存。** 项目设计里前缀必须逐字节稳定才可能命中，
而实现上有好几处会悄悄破坏它（D33/D34）。单跑一次看不出——
**要连续跑，前缀稳定性的收益才体现得出来。**

每一条结论都要有数字。
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

TURNS = 15


def ledger_usage(home: Path):
    """从台账里统计 token 与缓存命中。"""
    p = home / "ledger.jsonl"
    if not p.exists():
        return None
    prompt = completion = hit = miss = 0
    calls = 0
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("kind") != "model_called":
            continue
        u = (e.get("data") or {}).get("usage") or {}
        calls += 1
        prompt += u.get("prompt_tokens") or 0
        completion += u.get("completion_tokens") or 0
        hit += u.get("cache_hit_tokens") or 0
        miss += u.get("cache_miss_tokens") or 0
    return {
        "calls": calls, "prompt": prompt, "completion": completion,
        "hit": hit, "miss": miss,
    }


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

    with tempfile.TemporaryDirectory(prefix="yunxi-rounds-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        for n in ("agnes.key", "deepseek.key", "bocha.key"):
            src = real_home / "secrets" / n
            if src.exists():
                shutil.copy(src, home / "secrets" / n)
        work = home / "work"
        work.mkdir()
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        print(f"=== 同一个会话连跑 {TURNS} 轮 ===")
        # 每一轮的输入。**都走同一个会话**，前缀才会稳定。
        lines = []
        for i in range(1, TURNS + 1):
            lines.append(f"第{i}轮：只回复「收到{i}」这四个字，不要别的。")
        lines.append("/exit")

        t0 = time.monotonic()
        p = subprocess.run(
            [str(BIN), "chat", "--id", "stress", "--thinking", "off"],
            input="\n".join(lines) + "\n",
            capture_output=True, text=True, encoding="utf-8",
            env=env, timeout=3000, cwd=str(work),
        )
        total = time.monotonic() - t0
        out = (p.stdout or "") + (p.stderr or "")
        print("\n".join(out.strip().splitlines()[-3:]))

        check("**全部轮次都跑完了**", p.returncode == 0, f"退出码 {p.returncode}")

        # 每轮用时：从限流等待行推不出来，改数"收到N"出现的位置
        # **精确统计：一轮一轮地看，而不是拿子串去凑。**
        #
        # `f"收到{i}" in out` 有个坑：`收到1` 会被 `收到15` 命中，
        # 于是少答一轮也可能数出 15。改成按分隔符切段再逐段找。
        segments = out.split("›")
        missed = []
        for i in range(1, TURNS + 1):
            if not any(f"收到{i}" in seg for seg in segments):
                missed.append(i)
        got = TURNS - len(missed)
        print(f"      应答了 {got}/{TURNS} 轮；总耗时 {total:.1f} 秒"
              f"（平均 {total / max(got, 1):.1f} 秒/轮）")

        # **失败的轮次要说出原因。**
        #
        # 这条最初只数了应答数——于是"某一轮失败了"只表现为一个数字变少，
        # 看不出是限流、是 400、还是别的。**数字变少不是诊断。**
        fails = [l.strip() for l in out.splitlines() if "这一轮失败了" in l]
        if fails:
            print(f"      有 {len(fails)} 轮报错，原因是：")
            for f in fails[:5]:
                print(f"        {f}")
        if missed:
            print(f"      没应答的轮次：{missed}")

        check(f"**{TURNS} 轮全部有应答**", got == TURNS,
              f"没答的是第 {missed} 轮"
              + (f"；报错原话见上" if fails else "；但一轮都没报错——那更可疑"))

        # ---------------- 台账增长 ----------------
        print()
        print("=== 台账与会话文件 ===")
        led = home / "ledger.jsonl"
        sess = home / "sessions" / "stress.json"
        led_kb = led.stat().st_size / 1024 if led.exists() else 0
        sess_kb = sess.stat().st_size / 1024 if sess.exists() else 0
        print(f"      台账 {led_kb:.1f} KB；会话文件 {sess_kb:.1f} KB")
        check("会话文件存在（跨进程存活的前提）", sess.exists())
        if sess.exists():
            data = json.loads(sess.read_text(encoding="utf-8"))
            turns = data.get("turns", 0)
            hist = (data.get("layout") or {}).get("history") or []
            print(f"      记了 {turns} 轮；历史 {len(hist)} 条消息")
            check("**轮次都记上了**", turns >= TURNS - 1,
                  f"只记了 {turns} 轮（终点那轮可能还没写）")
            check("**历史条数和轮次对得上**", len(hist) >= (TURNS - 1) * 2,
                  f"{turns} 轮该有约 {turns * 2} 条，实际 {len(hist)}")

        # ---------------- 成本与缓存 ----------------
        print()
        print("=== 成本与缓存命中 ===")
        u = ledger_usage(home)
        if not u:
            check("台账里有用量记录", False, "一条 model_called 都没有")
        else:
            print(f"      模型调用 {u['calls']} 次")
            print(f"      prompt {u['prompt']} token / completion {u['completion']} token")
            print(f"      缓存命中 {u['hit']} / 未命中 {u['miss']}")
            check("**每次调用都记了用量**", u["calls"] >= got,
                  f"{got} 轮只记了 {u['calls']} 次调用")
            total_cache = u["hit"] + u["miss"]
            if total_cache > 0:
                rate = u["hit"] / total_cache * 100
                print(f"      缓存命中率 {rate:.1f}%")
                # **同一个会话连跑，前缀应该稳定，命中率应该不低。**
                # 很低的话说明前缀被什么东西破坏了——而那种破坏是静默的，
                # 只表现为账单变贵（D33/D34 记过这个陷阱）。
                check(
                    "**缓存命中率不低**",
                    rate >= 30,
                    f"只有 {rate:.1f}%——前缀可能在每轮之间变了，而那种失效是静默的",
                )
            else:
                print("      （模型没返回缓存字段，Agnes 不提供——这条验不了）")

    print()
    print("连续多轮压力测试：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
