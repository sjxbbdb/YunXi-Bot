# -*- coding: utf-8 -*-
"""真实任务端到端：难题 → 停下问人 → 给答案 → 授权写入 → 跑完。

跑它：python sidecar/e2e_realtask.py

## 为什么这个脚本值得存在

D44（人工介入是死路）是**手动跑了好几轮才发现**的：任务停在 `decide:` 步骤等人，
`resume` 却接不住人的答案，于是反复问、反复弃权，最后连状态都从
「等人工」变成了「卡住」。

手验过的东西不会重现——下次谁改坏了，没有任何东西会告诉他。所以固化成脚本。
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

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

ANSWER = "满 100（含）减 20，也就是用 >= 而不是 >。折扣后不允许为负，但按现有单价算不出负数。"


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

    with tempfile.TemporaryDirectory(prefix="yunxi-rt-") as tmp:
        root = Path(tmp)
        home = root / "home"
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        # **密钥要备齐。**
        #
        # 复杂任务会路由到 DeepSeek（多轮任务省 RPM），而第一次跑的时候
        # 这里只复制了 agnes.key——任务连拆解都没开始就报
        # "无法初始化 deepseek 的客户端"。
        #
        # 那**不是产品的问题**（报错清楚，指了缺什么），是脚本没准备齐。
        # 所以按"有哪些就复制哪些"来，缺的那个仍然会被如实报出来。
        copied = []
        for name in ("agnes.key", "deepseek.key", "bocha.key"):
            src = real_home / "secrets" / name
            if src.exists():
                shutil.copy(src, home / "secrets" / name)
                copied.append(name)
        print(f"  已准备密钥：{copied}")
        if "deepseek.key" not in copied:
            print("  注意：没有 deepseek.key，复杂任务的路由会失败")
        assert (home / "secrets" / "agnes.key").exists()
        work = root / "proj"
        (work / "src").mkdir(parents=True)
        (work / "notes").mkdir()
        (work / "README.md").write_text(
            "# 项目说明\n\n计费逻辑在 src/billing.py。\n已知问题：满减的边界条件写错了。\n",
            encoding="utf-8",
        )
        (work / "src" / "billing.py").write_text(BUGGY, encoding="utf-8")
        (work / "notes" / "journal.md").write_text(
            "2026-10-01\n- pro 3 席 = 90，没打折，是对的\n- team 1 席 = 80，也没打折\n\n"
            "2026-10-02\n- 又看了一遍，感觉边界那块不对\n",
            encoding="utf-8",
        )
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        # ================= 第一次：跑到决策步骤停下 =================
        print("=== 第一次运行（预期：停在决策步骤等人）===")
        r1 = subprocess.run(
            [str(BIN), "do", "这个项目的满减边界条件有问题。找出问题、修好它、然后验证修复。",
             "--yes"],
            capture_output=True, text=True, encoding="utf-8",
            env=env, timeout=1800, cwd=str(work),
        )
        out1 = (r1.stdout or "") + (r1.stderr or "")
        print("\n".join(out1.strip().splitlines()[-6:]))

        # 提示语形如 `处理完之后用 \`yunxi-bot resume <id>\` 续跑。`
        # ——id 后面紧跟一个反引号，`\S+` 会把它一起吃掉，
        # 于是后面拿这个 id 去 resume 必然"找不到任务"。
        m = re.search(r"yunxi-bot resume ([^\s`]+)", out1) if out1 else None
        # **两条路都算通过。**
        #
        # 一开始我要求"必须打印出任务 id"，它挂了——而那不是产品的问题：
        # 拆解出来有没有 `decide:` 步骤，是**模型按任务决定的**。
        # 没有决策步骤时任务会一路跑完，自然不会打印续跑的提示。
        #
        # 用测试去规定模型该怎么拆解，是把测试写成了产品规格。
        # 所以这里分成两条：
        #   A. 停下来了 → 验人工答案那条路（下面继续）
        #   B. 自己跑完了 → 验它把活干成了没有（文件改对、没被毁）
        #
        # 人工答案那套逻辑本身由单元测试盯着
        # （`a_human_answer_settles_the_step_without_calling_any_model`
        # 和 `a_human_answer_revives_a_step_whose_retries_ran_out`），
        # 不靠这一条端到端来保证。
        planned = "拆解路由" in out1 or "拆解估算" in out1
        check("**拆解跑起来了**", planned, "连拆解都没有——那才是真失败")
        # **不要断言"这时文件还没被改"。**
        #
        # 一开始我写了这条，它挂了——而产品是对的：计划里完全可能
        # 先改文件、后面才遇到决策步骤。**断言写宽了会把正确行为判成错的**
        # （同一个坑在 human_answer 那条测试上刚踩过一次）。

        if not m:
            # 分支 B：它自己跑完了
            print()
            print("=== 没有决策步骤，任务自己跑完了 ===")
            final = (work / "src" / "billing.py").read_text(encoding="utf-8")
            print(f"      billing.py 现在 {len(final.splitlines())} 行")
            check("**文件没被截断**", len(final.splitlines()) >= 10)
            check("**判定被改对了**", ">= DISCOUNT_THRESHOLD" in final)
            print()
            print("真实任务端到端："
                  + ("全部通过" if ok else "**有失败项**")
                  + "（这一轮没有决策步骤，人工答案那条路见单元测试）")
            return 0 if ok else 1
        task_id = m.group(1)

        # ================= 关键断言：状态是「等人工」不是「卡住」 =================
        print()
        print("=== 任务状态 ===")
        rt = subprocess.run([str(BIN), "tasks", task_id],
                            capture_output=True, text=True, encoding="utf-8",
                            env=env, timeout=300, cwd=str(work))
        state_out = (rt.stdout or "") + (rt.stderr or "")
        state_line = next((x for x in state_out.splitlines() if "状态" in x), "")
        print(f"      {state_line.strip()}")
        check("**停在「等人工」而不是「卡住」**", "等人工" in state_out,
              "变「卡住」的话人就再也答不上了——那正是 D44 的坑")

        # ================= 给答案 + 授权写入 =================
        print()
        print("=== 给出人的决定，让它跑完 ===")
        #
        # **一个任务可能有多个决策点**，而 `resume --answer` 一次答一个。
        #
        # 真机上第一次跑就是栽在这儿：答完第一个决策点，任务继续，
        # 然后又停在第二个（模型在动风险代码之前先问"要不要动手、
        # 怎么回滚"——**那是好行为**）。脚本却以为答一次就完事，于是判了失败。
        #
        # 所以要**循环答**，直到它不再是"等人工"。
        # 上限是防死循环：真出问题时要停下来报，而不是转到天荒地老。
        out2 = ""
        answered = 0
        for round_no in range(1, 6):
            rt = subprocess.run([str(BIN), "tasks", task_id],
                                capture_output=True, text=True, encoding="utf-8",
                                env=env, timeout=300, cwd=str(work))
            state_now = (rt.stdout or "") + (rt.stderr or "")
            if "等人工" not in state_now:
                break
            r2 = subprocess.run(
                [str(BIN), "resume", task_id, "--yes", "--answer", ANSWER],
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=1800, cwd=str(work),
            )
            out2 = (r2.stdout or "") + (r2.stderr or "")
            if "已记下你对步骤" not in out2:
                # 没答上（没有待决的决策步骤了，或者状态不让插手）——别再转
                break
            answered += 1
            print(f"      第 {round_no} 轮：答了一个决策点")
        print(f"      共答了 {answered} 个决策点")
        print("\n".join(out2.strip().splitlines()[-4:]))

        check("**人的决定被记下了**", "已记下你对步骤" in out2,
              "没这句话说明 --answer 根本没落到步骤上")

        # ================= 结果 =================
        print()
        print("=== 结果 ===")
        final = (work / "src" / "billing.py").read_text(encoding="utf-8")
        lines = len(final.splitlines())
        print(f"      billing.py 现在 {lines} 行")
        check("**文件没被截断**", lines >= 10,
              f"只剩 {lines} 行——被工具毁过（真机上出过）")
        check("**判定被改对了**", ">= DISCOUNT_THRESHOLD" in final,
              "还是 > 的话说明修复步骤没成功")
        check("**人的决定进了产物**",
              "source" in out2 or "人工" in out2 or ">= " in out2)

        # **不要断言"一定有个测试文件"。**
        #
        # 一开始我这么写了，它挂了。而产品没错：拆解出来的步骤里
        # 有没有"写测试"这一步、写成什么文件名，都是模型按任务决定的。
        # 断言一个具体的文件名，是在用测试去规定模型该怎么拆解。
        #
        # 该断言的是**任务到底做成了没有**——那才是使用者在乎的。
        py_files = sorted(
            str(p.relative_to(work)) for p in work.rglob("*.py")
        )
        print(f"      目录里的 python 文件：{py_files}")

        # 任务自己怎么说
        rt2 = subprocess.run([str(BIN), "tasks", task_id],
                             capture_output=True, text=True, encoding="utf-8",
                             env=env, timeout=300, cwd=str(work))
        final_state = (rt2.stdout or "") + (rt2.stderr or "")
        state_now = next((x for x in final_state.splitlines() if "状态" in x), "")
        print(f"      任务最终状态：{state_now.strip()}")
        check(
            "**任务跑到了终态（不再卡着等人）**",
            "等人工" not in state_now and "卡住" not in state_now,
            "还停在等人/卡住的话，说明闭环没走完",
        )

    print()
    print("真实任务端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
