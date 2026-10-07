#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「项目规则加载」——goal 第 3 条。

## 两层断言，分开验

**第一层：接线（确定性的）**
规则有没有真的进稳定前缀——这一层由程序断言，不看模型脸色。
它是有理由的：调试时踩过一个坑——规则加载正常、`/rules` 列得好好的，
**而模型说"系统提示词里没有这一节"**。那次差一点就去查模型了，
其实是"加载了"和"进去了"是两件事。

**第二层：模型用不用（概率性的）**
在一个只有一个规则文件的干净项目里，问一条**靠常识绝不可能猜到**的约定。
答得出来说明规则真的影响了它的行为。

**分两层的理由**：把"接线对不对"和"模型听不听话"混在一条断言里，
失败时你不知道该修哪边。

跑：python sidecar/e2e_rules.py
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import (  # noqa: E402
    ensure_fresh_binary,
    home_untouched,
    real_home_dir,
    snapshot_home,
)

# **故意编一条不可能猜到的约定。**
SECRET_BUILD = "make -f custom.mk relwithdebinfo"
OUTSIDE_RULE = "npm run build:legacy"


def main() -> int:
    BIN = ensure_fresh_binary()
    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    real_home = real_home_dir()
    # **跑之前记一份真实 home**：跑完再比一次，证明这次测试没写回去。
    before_home = snapshot_home(real_home)
    key = real_home / "secrets" / "agnes.key"
    if not key.exists():
        print("没有 agnes.key，这个验证需要真模型", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="yunxi-rules-") as tmp:
        root = Path(tmp)
        home = root / "home"
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        def chat(lines: list[str], cwd: Path, sid: str, *extra: str, timeout: int = 300):
            return subprocess.run(
                [str(BIN), "chat", "--id", sid, *extra],
                input="\n".join(lines) + "\n",
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=timeout, cwd=str(cwd),
            )

        # ================= 干净的单文件项目 =================
        clean = root / "clean"
        (clean / ".git").mkdir(parents=True)
        (clean / "AGENTS.md").write_text(
            f"# 本项目约定\n\n- 构建命令是：{SECRET_BUILD}\n"
            f"- 代码风格：禁止使用 unwrap\n",
            encoding="utf-8",
        )

        print("=== 第一层：接线（确定性）===")
        r1 = chat(["/rules", "/exit"], clean, "r-wire")
        out1 = (r1.stdout or "") + (r1.stderr or "")
        print("\n".join(l for l in out1.splitlines() if "前缀" in l or "规则" in l)[:600])

        check("**规则真的进了稳定前缀**", "规则已进前缀：是" in out1,
              "光'加载了'不够——接线断了也看不出来")
        check("列了来源路径", "AGENTS.md" in out1)
        check("标了层级", "项目" in out1)

        print()
        print("=== 第二层：模型用不用（概率性）===")
        r2 = chat(
            ["这个项目用什么命令构建？只回复那条命令，不要解释。", "/exit"],
            clean, "r-use",
        )
        out2 = (r2.stdout or "") + (r2.stderr or "")
        print(out2[-500:])
        check("**答得出编造的构建命令**", SECRET_BUILD in out2,
              "靠常识猜不到，只能是读到了规则")

        # 一个和 clean 有不同规则的目录，用来验"换目录前缀就变"
        nested_preview = root / "preview"
        (nested_preview / ".git").mkdir(parents=True)
        (nested_preview / "AGENTS.md").write_text("- 另一套约定：用 pytest\n", encoding="utf-8")

        # ================= 前缀稳定 =================
        print()
        print("=== 稳定前缀跨进程一致 ===")

        def fingerprint_of(cwd: Path, sid: str) -> str | None:
            """从 `/rules` 里取指纹。

            **不依赖模型调用。** 最初的写法是读会话文件里的指纹——
            而那个文件只在"有过成功的一轮"之后才写。于是模型一失败
            （限流、超时），指纹就是 None，而那被误报成"前缀不一致"。
            **把确定性的断言挂在概率性的东西上，就是在制造假失败。**
            """
            r = chat(["/rules", "/exit"], cwd, sid)
            text = (r.stdout or "") + (r.stderr or "")
            for line in text.splitlines():
                if "前缀指纹" in line:
                    return line.split("：", 1)[-1].strip()
            return None

        fp1 = fingerprint_of(clean, "r-fp1")
        fp2 = fingerprint_of(clean, "r-fp2")
        check("两次都拿到了指纹", fp1 is not None and fp2 is not None, f"{fp1} / {fp2}")
        check("**同一目录前缀一致**", fp1 == fp2, "不一致的话每次启动都缓存未命中")

        # 换个目录（规则不同）指纹就该不同
        fp3 = fingerprint_of(nested_preview, "r-fp3")
        check("**换了目录前缀就变**", fp3 is not None and fp3 != fp1,
              "规则不同 → 前缀不同，这是对的（会换来一次缓存未命中）")

        # ================= 项目边界 =================
        print()
        print("=== 项目边界 ===")
        outer = root / "outer"
        outer.mkdir()
        (outer / "AGENTS.md").write_text(
            f"这个项目用 {OUTSIDE_RULE}（**这份不该被读到**）", encoding="utf-8")
        nested = outer / "nested"
        (nested / ".git").mkdir(parents=True)
        (nested / "AGENTS.md").write_text("- 测试命令是 cargo test --all\n", encoding="utf-8")
        sub = nested / "crates" / "core"
        sub.mkdir(parents=True)
        (sub / "AGENTS.md").write_text("- 子目录测试：cargo test -p yunxi-bot-core\n", encoding="utf-8")

        r3 = chat(["/rules", "/exit"], sub, "r-boundary")
        out3 = (r3.stdout or "") + (r3.stderr or "")
        check("**项目外的规则没被读进来**", OUTSIDE_RULE not in out3,
              "项目边界（.git）该挡住它")
        check("项目根和子目录都读到了",
              "nested" in out3 and "core" in out3, "两份都该在")

        # 顺序：远的先读、近的后读
        rs_lines = [l for l in out3.splitlines() if "AGENTS.md" in l and "[" in l]
        if len(rs_lines) >= 2:
            far = next((i for i, l in enumerate(rs_lines) if l.rstrip().endswith("nested\\AGENTS.md")
                        or l.rstrip().endswith("nested/AGENTS.md")), None)
            near = next((i for i, l in enumerate(rs_lines) if "core" in l), None)
            check("**远的先读、近的后读**",
                  far is not None and near is not None and far < near,
                  "顺序反了的话泛泛的规则会盖过具体的")

        # ================= 没有规则的地方 =================
        print()
        print("=== 没有规则的地方 ===")
        bare = root / "bare"
        bare.mkdir()
        r4 = chat(["/rules", "/exit"], bare, "r-bare")
        out4 = (r4.stdout or "") + (r4.stderr or "")
        check("说了没有找到", "没有找到" in out4)
        check("说了怎么加", "AGENTS.md" in out4)
        check("退出码为 0", r4.returncode == 0)

    # **隔离断言：真实 home 一个字节都不该动。**
    # 这条以前不存在——`real_home` 只被用来借密钥，没有任何东西证明
    # 测试没写回真实数据（见 `e2e_common.home_untouched`）。
    check("**没有碰真实 home**", *home_untouched(before_home, real_home))

    print()
    print("项目规则端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
