# -*- coding: utf-8 -*-
"""RAG 记忆召回的真实场景验证。

跑它：python sidecar/e2e_memory.py

## 验的是"用起来到底行不行"，不是"函数返回对不对"

单元测试验的是 `recall_for_prompt` 挑得准不准；这里验的是
**端到端**：记住的东西，下一次对话里**它真的能答出来**。

中间隔着好几段，任何一段断了单元测试都看不出来：
- 召回挑出来了，但没拼进易变尾
- 拼进去了，但模型没把它当"自己记得的事"
- 状态没落盘，换个进程就没了

## 一条踩过的坑

**真机测试必须用重新构建的二进制。** 接完线我只跑了
`cargo build --workspace`（debug），而真机用的是 `target/release/`——
于是验的是**没接线的旧版本**，还以为是产品坏了。
`ensure_fresh_binary()` 就是防这个的，**别绕过它直接跑 release**。
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402

# (正文, 类别, 问法, 答案里必须出现的词)
CASES = [
    ("使用者最喜欢的颜色是青绿色", "preference", "我最喜欢什么颜色？", "青绿"),
    # 这条问法的选择有讲究：一开始问的是「你该怎么称呼我比较合适？」，
    # 它答「我直接用"你"就行」——**那是对的行为**（它知道这条偏好，
    # 于是挑了个不冒犯的叫法），只是回答里不含"亲"这个字，被我判成了失败。
    # 改成直接问偏好本身，答案里就必然出现那个词。
    ("使用者不喜欢被叫亲", "preference", "我在称呼上说过什么讲究吗？", "亲"),
    ("使用者住在杭州", "fact", "我住在哪？", "杭州"),
    ("使用者养了一只叫豆豆的猫", "fact", "我养的宠物叫什么？", "豆豆"),
    ("上周和客户开了个预算会", "event", "上周我和客户干了什么？", "预算"),
    ("上个月换了台笔记本", "event", "上个月我换过什么设备？", "笔记本"),
    ("周末去爬了山", "event", "周末我去哪了？", "山"),
    ("和小李一起做过一个项目", "relationship", "我和小李一起做过什么？", "项目"),
]

# 再塞一批**互不相干、也不该被召回**的记忆，把库撑到 20 条以上。
# 这一条是验收要求的："不是全塞进上下文"——库大了召回还得挑得准。
NOISE = [
    ("使用者不吃香菜", "fact"),
    ("使用者用的是 Windows", "fact"),
    ("使用者每天七点起床", "fact"),
    ("使用者喜欢喝美式不加糖", "preference"),
    ("昨天把测试全部跑通了", "event"),
    ("前天读了本关于记忆的书", "event"),
    ("上上周末修了自行车", "event"),
    ("上季度把响应时间降了一半", "event"),
    ("去年学过一点日语", "event"),
    ("同事叫小李", "relationship"),
    ("妈妈住在南京", "relationship"),
    ("和爸爸关系很好", "relationship"),
    ("家里的路由器换过一次", "event"),
    ("下个月要出差去深圳", "event"),
]


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

    with tempfile.TemporaryDirectory(prefix="yunxi-mem-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        for n in ("agnes.key", "deepseek.key", "bocha.key"):
            src = real_home / "secrets" / n
            if src.exists():
                shutil.copy(src, home / "secrets" / n)
        work = home / "work"
        work.mkdir()
        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        # ---------- 记 ----------
        total = len(CASES) + len(NOISE)
        print(f"=== 记 {total} 条记忆（其中 {len(NOISE)} 条是干扰项）===")
        for item in CASES:
            text, kind = item[0], item[1]
            r = subprocess.run(
                [str(BIN), "remember", text, "--kind", kind],
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=120, cwd=str(work),
            )
            if r.returncode != 0:
                print(f"  记不进去：{text}", file=sys.stderr)
                return 2
        for text, kind in NOISE:
            r = subprocess.run(
                [str(BIN), "remember", text, "--kind", kind],
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=120, cwd=str(work),
            )
            if r.returncode != 0:
                print(f"  记不进去：{text}", file=sys.stderr)
                return 2

        r = subprocess.run([str(BIN), "memory"], capture_output=True, text=True,
                           encoding="utf-8", env=env, timeout=120, cwd=str(work))
        out = (r.stdout or "") + (r.stderr or "")
        first = next((l for l in out.splitlines() if "条记忆" in l), "")
        print(f"      {first.strip()}")
        check("**记忆都记下了**", f"共 {total} 条" in out, first)

        # ---------- 问 ----------
        print()
        print("=== 一句一句问（每句一个新进程）===")
        hit = 0
        judged = 0
        inconclusive = 0
        for i, (text, _kind, question, want) in enumerate(CASES):
            # 每句用不同的会话 id，**避免上一句的上下文替它答**
            r = subprocess.run(
                [str(BIN), "chat", "--id", f"memq{i}", "--thinking", "off"],
                input=f"{question}\n/exit\n",
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=600, cwd=str(work),
            )
            reply = (r.stdout or "") + (r.stderr or "")
            # 只取回答那几行，别把启动横幅算进去
            body = " ".join(
                l for l in reply.splitlines()
                if l.strip() and not l.startswith(("搜索", "模型", "限流", "超时", "提示词",
                                                   "Base", "思考", "人格", "项目", "前缀",
                                                   "规则", "加载", "新会话", "›", "会话已存",
                                                   "接着聊", "·", "└", "┌"))
            )
            # **区分"答错了"和"这次调用根本没成"。**
            #
            # 真机上踩到过：Agnes 是**账号级** 10 RPM（D57），连发八轮会
            # 打爆配额，本地 4 次重试用尽之后这一轮就整轮失败。那时回复里
            # 是「超出速率限制」，而不是"它想不起来"。
            #
            # 一次调用失败**不能拿来判召回准不准**——那是两件事。
            # 但也不能悄悄跳过：跳了几次要报出来。
            if "这一轮失败了" in body or "超出速率限制" in body:
                inconclusive += 1
                print(f"  ? 「{question}」→ **这次调用没成**（限流），不计入判定")
                continue
            got = want in body
            print(f"  {'✓' if got else '✗'} 「{question}」→ 期望含「{want}」")
            if not got:
                print(f"      实际回复：{body[:200]}")
            judged += 1
            hit += got

        print()
        print(f"      可判定 {judged} 条，命中 {hit} 条；"
              f"因限流无法判定 {inconclusive} 条")
        if inconclusive > 2:
            check("**环境可用（限流不能太多）**", False,
                  f"{inconclusive} 条因限流判不了——这个环境下测不出召回质量")
        # **门槛：可判定的里面 ≥75%。** 字符 n-gram 不是真 embedding，
        # 跨语义（宠物→猫）会有漏；而且模型偶尔不照记忆答。
        need = max(1, (judged * 3 + 3) // 4)
        check(
            f"**召回挑得准（≥{need}/{judged}）**",
            judged > 0 and hit >= need,
            f"实际 {hit}/{judged}",
        )

        # ---------- 跨进程还在 ----------
        print()
        print("=== 跨进程 ===")
        r = subprocess.run([str(BIN), "memory", "--search", "豆豆"],
                           capture_output=True, text=True, encoding="utf-8",
                           env=env, timeout=120, cwd=str(work))
        check("**记忆跨进程还在**", "豆豆" in ((r.stdout or "") + (r.stderr or "")))

        # ---------- 忘记之后不该再想起来 ----------
        print()
        print("=== 忘记 ===")
        r = subprocess.run([str(BIN), "memory", "--search", "豆豆"],
                           capture_output=True, text=True, encoding="utf-8",
                           env=env, timeout=120, cwd=str(work))
        import re
        m = re.search(r"(m\d+)", (r.stdout or "") + (r.stderr or ""))
        if m:
            subprocess.run([str(BIN), "memory", "--forget", m.group(1)],
                           capture_output=True, text=True, encoding="utf-8",
                           env=env, timeout=120, cwd=str(work))
            r = subprocess.run(
                [str(BIN), "chat", "--id", "forgot", "--thinking", "off"],
                input="我养的宠物叫什么？\n/exit\n",
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=600, cwd=str(work),
            )
            body = (r.stdout or "") + (r.stderr or "")
            check("**忘了之后就不该再答出来**", "豆豆" not in body,
                  "忘了还答得出来，说明忘记没真的生效")
        else:
            check("找到要忘的那条", False, "搜不到编号")

    print()
    print("记忆召回端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
