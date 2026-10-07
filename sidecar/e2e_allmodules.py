#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""端到端真实任务验证：一个任务里同时压到各功能模块。

## 为什么要有这个脚本

前面各模块都是**一条一条单独验**的：人格验人格、Workspace 验 Workspace。
**从来没有一次"多个模块在同一个任务里同时正确"的验证。**
D113 里我把这一条明确列为"该做而没做的"。

这个脚本补的就是它。

## 设计原则

1. **每个模块都要有独立的、可观测的判据。**
   只报"任务成功了"是不够的——那可能是别的模块在起作用，
   也可能某个模块**整个没参与**而没人发现。
2. **判据取的是外部可观测的东西**：台账事件、诊断输出、文件内容，
   不是"我觉得它在工作"。
3. **失败要能定位到模块**，不是笼统一个 FAIL。

## 覆盖的模块与判据

| 模块 | 判据 |
|---|---|
| 人格 | 前缀里出现自定义人格的名字与特征；改人格后指纹变一次 |
| 画像 | `profile --pending/--accept` 链路；画像正文进稳定前缀 |
| 常驻记忆 | 事实类记忆进前缀（`# 关于使用者（记忆 v…）`） |
| 动态召回（RAG） | `[记忆]` 诊断里 lexical/dense 候选数 > 0 且选中 |
| 召回门控 | 通用问题判 `none`（不白跑召回） |
| 分层记忆（Workspace） | 在项目目录召回、换目录不召回 |
| 决策模型 | 台账里出现决策相关事件，或路由留痕里点名决策模型 |
| 任务引擎 | 三步以上任务跑完且**文件真的被改对** |
| 脱敏诊断 | 诊断输出里不含记忆正文（只出 id） |
| 工具层 | 台账里有 `tool_called` 且 `outcome=ok` |
"""

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
    home_untouched,
    real_home_dir,
    snapshot_home,
)

REPO = Path(__file__).resolve().parent.parent
BIN = REPO / "target" / "release" / ("yunxi-bot.exe" if os.name == "nt" else "yunxi-bot")

# 待修的项目：和 stress_taskrepeat 用的是同一类真实场景
BUGGY = '''"""计费。"""

RATES = {"basic": 10, "pro": 30, "team": 80}
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

README_MD = """# 计费模块

满 100 减 20。

**注意：满减的边界条件写错了。**
"""


def run(args, cwd=None, env=None, timeout=900):
    e = dict(os.environ)
    if env:
        e.update(env)
    p = subprocess.run(
        [str(BIN)] + args, cwd=str(cwd) if cwd else None, env=e,
        capture_output=True, text=True, encoding="utf-8", errors="replace",
        timeout=timeout,
    )
    return p.returncode, (p.stdout or "") + (p.stderr or "")


_KEEP = [False]


class Report:
    def __init__(self):
        self.rows = []

    def add(self, module, ok, detail):
        self.rows.append((module, bool(ok), detail))

    def show(self):
        print()
        print("=" * 74)
        print("端到端验证结果（按模块）")
        print("=" * 74)
        width = max(len(m) for m, _, _ in self.rows)
        npass = 0
        for module, ok, detail in self.rows:
            mark = "✓" if ok else "✗"
            if ok:
                npass += 1
            print(f"  {mark} {module.ljust(width)}  {detail}")
        print()
        print(f"  {npass}/{len(self.rows)} 个模块通过")
        return npass == len(self.rows)


def main():
    # **报告里有 ✓/✗，而 Windows 上 Python 默认按 GBK 编码 stdout。**
    # 第一版没这一行，结果检查全跑完了、打印时才崩：
    #   UnicodeEncodeError: 'gbk' codec can't encode character '\u2713'
    # **一个连自己的报告都打不出来的工具，比没有工具更浪费时间**——
    # 那些检查其实都做了，却一条也看不到。
    try:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    except (AttributeError, OSError):
        pass

    rep = Report()
    home = Path(tempfile.mkdtemp(prefix="e2e-home-"))
    work = Path(tempfile.mkdtemp(prefix="e2e-proj-"))
    other = Path(tempfile.mkdtemp(prefix="e2e-other-"))
    env = {"YUNXI_BOT_HOME": str(home), "YUNXI_BOT_MEMORY_DEBUG": "1",
           "PYTHONIOENCODING": "utf-8"}

    # 密钥从真实 home 借（测试不碰真实数据）
    #
    # **这一行原来是写死的 `%LOCALAPPDATA%\YunXiBot`。** 运行目录搬家之后
    # 那个路径已经不在，于是密钥一个也借不到、而这里**没有那道
    # `if not key.exists(): return 2` 的门**——它会带着一个空的 secrets
    # 接着往下跑。所以路径必须和产品一样从 `YUNXI_BOT_HOME` 解析。
    real_home = real_home_dir()
    # **跑之前记一份真实 home**：跑完再比一次，证明这次测试没写回去。
    before_home = snapshot_home(real_home)
    (home / "secrets").mkdir(parents=True, exist_ok=True)
    for n in ("agnes.key", "deepseek.key", "bocha.key"):
        src = real_home / "secrets" / n
        if src.is_file():
            shutil.copy2(src, home / "secrets" / n)

    (work / "src").mkdir(parents=True)
    (work / "src" / "billing.py").write_text(BUGGY, encoding="utf-8")
    (work / "README.md").write_text(README_MD, encoding="utf-8")

    try:
        # ---------- 准备阶段：把各模块的输入摆好 ----------
        persona = "# 云熙\n\n说话直接、简短，先给结论。"
        (home / "persona.md").write_text(persona, encoding="utf-8")

        rc, out = run(["remember", "使用者偏好简洁的回答，不喜欢客套"], env=env)
        rep.add("记忆写入", rc == 0, "remember 一条偏好" if rc == 0 else out[:60])

        rc, out = run(["remember", "这个项目用 pytest 不用 unittest", "--kind", "workspace"],
                      cwd=work, env=env)
        rep.add("分层记忆写入", rc == 0 and "作用域" in out,
                "workspace 记忆带上了 cwd 作用域" if "作用域" in out else out[:60])

        rc, out = run(["profile", "--propose", "使用者是后端工程师"], env=env)
        rc2, out2 = run(["profile", "--pending"], env=env)
        rep.add("画像链路", rc == 0 and "后端工程师" in out2,
                "propose → pending 可见" if "后端工程师" in out2 else out2[:60])
        m = re.search(r"\[(pp\d+)\]", out2)
        if m:
            run(["profile", "--accept", m.group(1)], env=env)
        prof = (home / "profile.md").read_text(encoding="utf-8") if (home / "profile.md").is_file() else ""
        rep.add("画像落盘", "后端工程师" in prof or "使用者是后端工程师" in prof,
                "接受后写进 profile.md" if prof else "profile.md 没生成")

        # ---------- 1) 对话：召回 + 门控 + 诊断 ----------
        # chat 是交互式的，用管道喂输入
        p = subprocess.run(
            [str(BIN), "chat", "--id", "e2e", "--thinking", "off"],
            cwd=str(work), env={**os.environ, **env},
            input="这个项目用 pytest 还是 unittest？\n使用者喜欢什么样的回答？\n什么是 HashMap？\n/exit\n",
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=900,
        )
        chat_out = (p.stdout or "") + (p.stderr or "")
        diag = [l for l in chat_out.splitlines() if l.startswith("[记忆]")]

        # 1a) Workspace 记忆在它自己的目录里被召回
        own = [l for l in diag if "candidates" in l]
        got_own = any("lexical_candidates 0 / dense_candidates 0" not in l for l in own[:1])
        rep.add("动态召回（RAG）", got_own,
                f"本目录召回候选：{own[0].split('] ')[-1] if own else '无诊断输出'}")

        # 1b) 门控：通用问题该判 none
        gate_none = any("memory_decision: none" in l for l in diag)
        rep.add("召回门控", gate_none, "通用问题判 none，没白跑召回" if gate_none
                else "没看到 none 判定")

        # 1c) 脱敏：诊断里不能出现记忆正文
        leaked = [l for l in diag if "pytest" in l or "简洁" in l]
        rep.add("脱敏诊断", not leaked,
                "诊断里只有 id 没有正文" if not leaked else f"泄漏了正文：{leaked[0][:60]}")

        # 1d) 人格进了前缀（通过 persona 命令间接确认）
        rc, out = run(["persona"], env=env)
        rep.add("人格加载", "云熙" in out and str(home) in out,
                "人格文件被读到" if "云熙" in out else out[:60])

        # ---------- 2) 换目录：分层记忆不该给 ----------
        p2 = subprocess.run(
            [str(BIN), "chat", "--id", "e2e2", "--thinking", "off"],
            cwd=str(other), env={**os.environ, **env},
            input="这个项目用 pytest 还是 unittest？\n/exit\n",
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=900,
        )
        d2 = [l for l in ((p2.stdout or "") + (p2.stderr or "")).splitlines()
              if l.startswith("[记忆]") and "candidates" in l]
        not_leaked = bool(d2) and "lexical_candidates 0 / dense_candidates 0" in d2[0]
        rep.add("分层记忆（按 cwd 筛）", not_leaked,
                "换目录后候选为 0" if not_leaked else (d2[0] if d2 else "无诊断"))

        # ---------- 3) 真实任务：引擎 + 工具 + 决策 ----------
        rc, out = run(["do", "计费模块的满减边界条件写错了，请修好它", "--yes"],
                      cwd=work, env=env, timeout=1800)
        final = (work / "src" / "billing.py").read_text(encoding="utf-8")
        lines = len(final.splitlines())
        fixed = (">= DISCOUNT_THRESHOLD" in final
                 and "def total(plan: str, seats: int) -> int:" in final
                 and 15 <= lines <= 21)
        rep.add("任务引擎（端到端）", fixed,
                f"文件 {lines} 行，修复{'在' if fixed else '不在'}")
        if not fixed:
            # **失败时把两样东西打出来**：任务说了什么、文件变成了什么。
            # 只有"17 行"这一个数字的话，什么也查不了。
            print()
            print("---- 任务输出（失败现场）----")
            for ln in out.splitlines()[-25:]:
                print("   " + ln)
            print("---- 最终文件 ----")
            for i, ln in enumerate(final.splitlines(), 1):
                print(f"   {i:3} {ln}")
            print("----")

        ledger = home / "ledger.jsonl"
        events = []
        if ledger.is_file():
            for ln in ledger.read_text(encoding="utf-8").splitlines():
                ln = ln.strip()
                if ln:
                    try:
                        events.append(json.loads(ln))
                    except json.JSONDecodeError:
                        pass
        kinds = [e.get("kind") for e in events]

        rep.add("工具层", kinds.count("tool_called") > 0,
                f"台账里 tool_called {kinds.count('tool_called')} 条")
        rep.add("决策模型参与",
                kinds.count("decision_asked") > 0 or kinds.count("decision_decided") > 0
                or "决策模型" in out,
                "台账有决策事件" if kinds.count("decision_asked") else
                ("路由留痕点名决策模型" if "决策模型" in out else "没看到决策模型参与"))
        rep.add("任务终态", "完成" in out or "卡住" in out or "等人工" in out,
                "任务有明确终态")

        # **隔离断言：真实 home 一个字节都不该动。**
        # 这条以前不存在——`real_home` 只被用来借密钥，没有任何东西证明
        # 这次测试没写回真实数据（见 `e2e_common.home_untouched`）。
        rep.add("测试隔离", *home_untouched(before_home, real_home))

        ok = rep.show()
        _KEEP[0] = not ok
        print()
        print(f"  工作目录 : {work}")
        print(f"  测试 home: {home}")
        return 0 if ok else 1
    finally:
        # **失败时保留现场。** 第一版无脑删，结果"文件被砍到 17 行"
        # 这个发现只剩下一个数字，看不到它到底变成了什么——
        # **删掉证据等于把一次真实故障变成一个传闻。**
        if _KEEP[0]:
            print()
            print("  保留现场（失败时）")
            for d in (home, work, other):
                print(f"    {d}")
        else:
            for d in (home, work, other):
                shutil.rmtree(d, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
