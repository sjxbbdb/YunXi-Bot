# -*- coding: utf-8 -*-
"""压力测试：同一个真实任务跑 N 次，把"偶尔失败"变成一个**比率**。

跑它：python sidecar/stress_taskrepeat.py [次数]

## 为什么要有这个

D76/D77 里同一个场景两次跑出两种结果：

| 运行 | 任务跑完了吗 | 文件改对了吗 |
|---|---|---|
| A | 是 | **否** |
| B | 是 | 是 |

**"跑完了但没做成"是最危险的一类失败**——报告说完成，实际什么都没做。

但**单次失败说明不了严重程度**：是 1/10 还是 1/2，处理方式完全不同。
而"再跑一次看看"是纪律禁止的做法——**不能拿第二次的成功盖第一次的失败**。

所以这里做的是：**跑 N 次，报通过率**。有数字才谈得上判断。

## 每次都是一套干净的工作区

不能复用同一个目录：第一次跑完文件已经改对了，第二次再看"改对了吗"
永远是"对"——**那测的是第一次的结果**，不是这一次的。

所以每次重建工作区。而 `YUNXI_BOT_HOME` 也每次新建，避免上一次的
会话/台账影响这一次。
"""
from __future__ import annotations

import json
import os
import re
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

GOAL = "这个项目的满减边界条件有问题。找出问题、修好它。"


def one_run(bin_path: str, real_home: Path, index: int) -> dict:
    """跑一轮，返回这一轮的结果。**每次都一套干净的工作区。**"""
    root = Path(tempfile.mkdtemp(prefix=f"yunxi-rep{index}-"))
    try:
        home = root / "home"
        (home / "secrets").mkdir(parents=True)
        for n in ("agnes.key", "deepseek.key", "bocha.key"):
            src = real_home / "secrets" / n
            if src.exists():
                shutil.copy(src, home / "secrets" / n)
        work = root / "proj"
        (work / "src").mkdir(parents=True)
        (work / "README.md").write_text(
            "# 项目说明\n\n计费逻辑在 src/billing.py。\n已知问题：满减的边界条件写错了。\n",
            encoding="utf-8")
        (work / "src" / "billing.py").write_text(BUGGY, encoding="utf-8")

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
        r = subprocess.run(
            [bin_path, "do", GOAL, "--yes"],
            capture_output=True, text=True, encoding="utf-8",
            env=env, cwd=str(work), timeout=2400,
        )
        out = (r.stdout or "") + (r.stderr or "")

        final = (work / "src" / "billing.py").read_text(encoding="utf-8")
        lines = len(final.splitlines())

        # **"改对了"这个判据原来是松的：只看那个字符串在不在。**
        #
        # 松判据的代价在真机上出现过：有一次跑出 **24 行**（原本 19），
        # 模型往 `billing.py` 里塞了别的东西，而判据照样报"✓"。
        # **那等于把"它顺手改坏了别的"算成了成功。**
        #
        # 现在四道一起过：
        #   1. 修复真的在（`>=` 那一处）
        #   2. **结构没被破坏**——两个函数签名都还在
        #   3. **没被塞进多余的东西**——行数不该比原来多
        #   4. 也没被砍掉（行数不该少太多）
        fixed = (
            ">= DISCOUNT_THRESHOLD" in final
            and "def subtotal(plan: str, seats: int) -> int:" in final
            and "def total(plan: str, seats: int) -> int:" in final
            and lines <= 21
            and lines >= 15
        )
        # "毁掉"的判据也收紧：不只"变得很短"，**结构丢了**也算
        destroyed = lines < 15 or "def total(" not in final

        # 状态：任务跑到终态了吗
        m = re.search(r"yunxi-bot resume ([^\s`]+)", out)
        state = "未知"
        if m:
            rt = subprocess.run([bin_path, "tasks", m.group(1)],
                                capture_output=True, text=True, encoding="utf-8",
                                env=env, cwd=str(work), timeout=300)
            so = (rt.stdout or "") + (rt.stderr or "")
            state = next((x.split(":")[-1].strip() for x in so.splitlines() if "状态" in x), "?")
        else:
            state = "完成（没停下来问人）"

        # **把失败的性质也抓出来——只报一个数字不够。**
        #
        # "40% 变 60%" 说明不了什么；"失败的**原因换了一种**"才是真信息：
        # 那说明上一个原因修掉了、而下面是另一个。
        failures = []
        # **工具循环打转时要看得见它那 8 轮干了什么。**
        #
        # 报"跑了 8 轮仍未结束"只说了一个结论——而"每一轮调了什么工具、
        # 拿到什么结果、为什么没收敛"才是能改的东西。
        # 台账里有 `tool_called`，把它按步归拢起来。
        tool_rounds: dict[str, list[str]] = {}
        ledger = home / "ledger.jsonl"
        if ledger.exists():
            for line in ledger.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("kind") == "tool_called":
                    d = e.get("data") or {}
                    # **台账里没有 step 字段**——只有 tool / decision /
                    # executed / outcome。所以按顺序收，不做"哪一步"的归类：
                    # 对一个"原地打转"的问题，"它连续调了什么"就够了。
                    tool_rounds.setdefault("all", []).append(
                        f"{d.get('tool')}[{d.get('decision')}]"
                    )
                    continue
                if e.get("kind") != "step_failed":
                    continue
                d = e.get("data") or {}
                reason = (d.get("error") or d.get("reason") or "")
                failures.append(f"{d.get('step')}: {reason.replace(chr(10), ' ')}")

        return {
            "fixed": fixed,
            "destroyed": destroyed,
            "state": state,
            "rate_limited": "超出速率限制" in out,
            "lines": len(final.splitlines()),
            "failures": failures,
            "tool_rounds": tool_rounds,
        }
    finally:
        shutil.rmtree(root, ignore_errors=True)


def main() -> int:
    n = 5
    if len(sys.argv) > 1:
        n = int(sys.argv[1])
    BIN = ensure_fresh_binary()
    real_home = real_home_dir()
    # **跑之前记一份真实 home**：跑完再比一次，证明这次测试没写回去。
    before_home = snapshot_home(real_home)
    if not (real_home / "secrets" / "agnes.key").exists():
        print("没有 agnes.key", file=sys.stderr)
        return 2

    print(f"=== 同一个真实任务跑 {n} 次 ===")
    results = []
    for i in range(1, n + 1):
        r = one_run(str(BIN), real_home, i)
        results.append(r)
        mark = "✓" if r["fixed"] else "✗"
        extra = "  **文件被毁了**" if r["destroyed"] else ""
        rate = "  （这一轮限流过）" if r["rate_limited"] else ""
        print(f"  第 {i} 次 {mark}  状态={r['state']}  文件 {r['lines']} 行{extra}{rate}")
        for f in r["failures"]:
            print(f"          ✗ {f}")
            # 工具循环那类失败——把它调过什么打出来
            if "工具循环" in f or "同一个动作" in f:
                calls = r.get("tool_rounds", {}).get("all", [])
                if calls:
                    print(f"             这一轮共调工具 {len(calls)} 次，最后 12 次："
                          f"{' -> '.join(calls[-12:])}")

    ok = sum(1 for r in results if r["fixed"])
    destroyed = sum(1 for r in results if r["destroyed"])
    rate_limited = sum(1 for r in results if r["rate_limited"] and not r["fixed"])

    # **按"失败在哪一步、什么原因"归类。**
    # 这是这一轮最该看的东西：数字会变，而"换了个原因"能告诉你
    # 上一个原因是不是真的修掉了。
    from collections import Counter
    kinds = Counter()
    for r in results:
        for f in r["failures"]:
            kinds[f[:40]] += 1
    if kinds:
        print()
        print("=== 失败的性质（按条数）===")
        # **变量名不能再用 `n`。** 它在外面是"跑了几次"，
        # 这里一覆盖，下面的 `ok / n` 就变成了"7 / 1 = 700%"——
        # 我的测量脚本自己产出了一个假数字。
        for kind_text, count in kinds.most_common(6):
            print(f"  {count}× {kind_text}")
    else:
        print()
        print("=== 没有 step_failed 事件 —— 失败不在「某一步」这一层 ===")
        print("    （那说明原因在别处：拆解、预算、限流、或者根本没失败）")

    print()
    print(f"=== 结果 ===")
    print(f"  改对了      {ok}/{n}   （{ok / n * 100:.0f}%）")
    print(f"  文件被毁    {destroyed}/{n}")
    print(f"  因限流失败  {rate_limited}/{n}")

    # **隔离断言：真实 home 一个字节都不该动。**
    # 这条以前不存在——`real_home` 只被用来借密钥，没有任何东西证明
    # 这几轮没写回真实数据（见 `e2e_common.home_untouched`）。
    # **命名避开 `ok`/`n`**：那两个在上面有别的意思（见本文件里
    # "变量名不能再用 n" 那一段）。
    home_ok, home_why = home_untouched(before_home, real_home)
    print(f"  {'✓' if home_ok else '✗'} 没有碰真实 home   {home_why}")
    print()

    # **判据和措辞都要诚实。**
    #
    # 这不是"测试通过"那种二值判断——它是**一个比率**，而比率的用途是
    # 判断严重程度、以及改动前后有没有变好。所以这里只在**全挂**或
    # **有文件被毁**时判失败；部分失败时报数字、让人自己看。
    if not home_ok:
        return 1
    if destroyed > 0:
        print("**文件被工具毁过——这是数据损坏，必须查。**")
        return 1
    if ok == 0:
        print("**一次都没做成——不是'偶尔失败'，是坏透了。**")
        return 1
    print(f"通过率 {ok / n * 100:.0f}%。"
          f"低于 100% 说明存在间歇性，**而间歇性的程度就是上面这个数字**。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
