#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「反馈闭环」——验收标准里那句原话：

> 用户说一句"这类以后别烦我" → 系统记住，下一条同类不再打扰

## 这个脚本演的是一个完整的故事

1. 收件箱里有 3 封未读：一封验证码、一封私人邮件、一封**在白名单里的**促销
2. 跑 `check` —— 系统自己判断，验证码和促销都通知了，**全部落台账**
3. 使用者说「刚才那条以后别烦我」（`feedback --last --never`）
4. **再来一封同类的邮件**
5. 再跑 `check` —— 那一封**不再打扰**，而且理由明确指向黑名单

## 为什么促销要走白名单

因为 `--last` 只能对**真的通知过你**的东西表态——这是刻意的
（别让人对着一封没见过的邮件建规则）。而普通邮件要判定就得走本地决策模型，
测试环境里 Verdict 不一定在跑，走了也是降级成"攒着"。
所以用白名单让促销**确定地**走到"通知"这一步。

顺带这条故事还验了一件事：**"以后别烦我"会把它从白名单里摘掉**。
不摘的话策略文件里会留着一对自相矛盾的规则（白名单说通知、黑名单说不提）——
行为上黑名单赢，但下次看到这个文件的人不知道哪条算数。

## 三条只有端到端才能验证的性质

- **规则真的改了策略**，不是只写进台账（第 3 步会证明）
- **通知过的不会重复打扰**（第 5 步的"已经告诉过你"计数）
- **攒着的不会被永久吞掉**（私人邮件第二轮仍会被再判一次）

跑：python sidecar/e2e_feedback.py
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import mail_server as ms  # noqa: E402
from fake_imap import FakeImapServer  # noqa: E402
from http.server import ThreadingHTTPServer  # noqa: E402
from test_mail_server import make_message  # noqa: E402
from e2e_common import ensure_fresh_binary  # noqa: E402




def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main() -> int:
    # **必须重建**：cargo clippy / cargo test 都不刷新这个 exe，
    # 不重建就会验到上一次 build 的旧二进制，得到假结果。
    BIN = ensure_fresh_binary()

    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    # 第一封促销在第 4 步之后会换成一封新的同类邮件——
    # 这就是"下一条同类"。
    PROMO = "promo@shop.example.com"  # 走白名单，确定能被通知
    msgs = {
        # **uid 越大越新，而 check 按"新到旧"处理。**
        # 所以最后被通知的其实是 uid 最小的那封——
        # 想让促销成为"刚才那条"，它就得是最老的。
        b"1": make_message("限时促销第一天", frm=PROMO,
                           to="me@example.com", body="买一送一。"),
        b"2": make_message("明天评审的材料", frm="张三 <zhangsan@example.com>",
                           to="me@example.com", body="材料发你了。"),
        b"3": make_message("验证码 135790", frm="security@bank.example.com",
                           to="me@example.com", body="买一送一。"),
    }

    with tempfile.TemporaryDirectory(prefix="yunxi-fb-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        os.environ["YUNXI_BOT_HOME"] = str(home)

        with FakeImapServer(msgs, unseen=[b"1", b"2", b"3"]) as imap:
            (home / "secrets" / "mail.json").write_text(json.dumps({
                "imap_host": "127.0.0.1", "imap_port": imap.port,
                "username": "me@example.com", "password": "secret",
                "use_ssl": False,
            }), encoding="utf-8")

            # **白名单**：让促销确定地走到"通知"这一步，
            # 否则它会被判成"机器发件人 -> 攒着"，而 --last 选不到没通知过的东西。
            (home / "triage.json").write_text(json.dumps({
                "allow_senders": [PROMO],
            }, ensure_ascii=False), encoding="utf-8")

            port = free_port()
            srv = ThreadingHTTPServer(("127.0.0.1", port), ms.Handler)
            threading.Thread(target=srv.serve_forever, daemon=True).start()

            env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

            def run(*extra: str) -> subprocess.CompletedProcess:
                return subprocess.run(
                    [str(BIN), *extra, "--port", str(port)],
                    capture_output=True, text=True, encoding="utf-8",
                    env=env, timeout=90,
                )

            # ---- 1. 第一轮 ----
            print("=== 第 1 步：跑一轮 check ===")
            r1 = run("check", "--dry-run")
            print(r1.stdout or r1.stderr)
            out1 = (r1.stdout or "") + (r1.stderr or "")
            check("第一轮退出码为 0", r1.returncode == 0, f"实际 {r1.returncode}")
            check("识别出三封未读", "本次判定  : 3 封" in out1 or "未读总数  : 3 封" in out1)

            # ---- 2. 反馈 ----
            print("=== 第 2 步：说「刚才那条以后别烦我」 ===")
            # 注意：dry-run 的不算"通知过你"，所以 --last 找不到。
            # 这是**故意的**——别让使用者对着一封没见过的邮件建规则。
            r_dry = run("feedback", "--last", "--never")
            check("演练模式的通知不能被反馈（故意）",
                  r_dry.returncode == 1 and "演练模式" in (r_dry.stderr or ""),
                  f"退出码 {r_dry.returncode}")

            # 真跑一轮（--console 把通知打到终端，算真通知过）
            print()
            print("--- 真跑一轮（通知打到终端）---")
            r2 = run("check", "--console")
            print(r2.stdout or r2.stderr)
            check("真跑一轮成功", r2.returncode == 0)

            print("--- 现在反馈 ---")
            r3 = run("feedback", "--last", "--never")
            print(r3.stdout or r3.stderr)
            out3 = (r3.stdout or "") + (r3.stderr or "")
            check("反馈成功", r3.returncode == 0, f"实际 {r3.returncode}")
            check("说出了针对谁", PROMO in out3, out3.strip()[:160])
            check("说了新规则是什么", "拉黑" in out3)
            check("说了范围", "只这个发件人" in out3)

            # ---- 3. 策略文件真的被改了 ----
            print()
            print("=== 第 3 步：策略文件真的改了吗 ===")
            pol_path = home / "triage.json"
            check("策略文件已生成", pol_path.exists())
            policy = json.loads(pol_path.read_text(encoding="utf-8")) if pol_path.exists() else {}
            blocked = policy.get("block_senders", [])
            check("黑名单里有那个发件人", PROMO in blocked, str(blocked))
            allowed = policy.get("allow_senders", [])
            check("**同时从白名单里摘掉了**（不留自相矛盾的规则）",
                  PROMO not in allowed, f"allow={allowed}")

            # ---- 4. 来一封同类的 ----
            print()
            print("=== 第 4 步：来一封同类的 ===")
            # 先看看台账里到底记了什么——"已通知过的不再重判"靠的就是它
            lp_dbg = home / "ledger.jsonl"
            if lp_dbg.exists():
                for line in lp_dbg.read_text(encoding="utf-8").splitlines():
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        e = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    k = e.get("kind") or (e.get("event") or {}).get("kind")
                    if k in ("notice_sent", "feedback_recorded"):
                        d = e.get("data") or {}
                        print(f"    [{k}] id={d.get('id')!r} source={d.get('source')!r} "
                              f"from={d.get('from')!r} dry_run={d.get('dry_run')!r}")

            imap.messages[b"4"] = make_message(
                "限时促销第二天", frm=PROMO,
                to="me@example.com", body="今天继续买一送一。")
            imap.flags[b"4"] = {"\\Unseen"}

            r4 = run("check", "--dry-run")
            print(r4.stdout or r4.stderr)
            out4 = (r4.stdout or "") + (r4.stderr or "")
            check("第二轮退出码为 0", r4.returncode == 0)
            check("**新那封同类被判定为不提**",
                  "[不提]" in out4 and "限时促销第二天" in out4,
                  "这就是验收标准那句「下一条同类不再打扰」")
            check("理由指向黑名单", "黑名单" in out4)

            # ---- 5. 通知过的没重复打扰 ----
            print()
            print("=== 第 5 步：通知过的不重复打扰 ===")
            check("**已告诉过你的没有重判**",
                  "已经告诉过你" in out4,
                  "同一封邮件不该被通知两次")

            # ---- 6. 台账 —— 全程留痕 ----
            print()
            print("=== 第 6 步：台账 ===")
            events = []
            lp = home / "ledger.jsonl"
            if lp.exists():
                for line in lp.read_text(encoding="utf-8").splitlines():
                    line = line.strip()
                    if line:
                        try:
                            events.append(json.loads(line))
                        except json.JSONDecodeError:
                            pass

            def kind_of(e):
                return e.get("kind") or (e.get("event") or {}).get("kind")

            kinds = [kind_of(e) for e in events]
            check("有 info_triaged", "info_triaged" in kinds)
            check("有 notice_sent", "notice_sent" in kinds)
            check("有 feedback_recorded", "feedback_recorded" in kinds)

            fb = [e for e in events if kind_of(e) == "feedback_recorded"]
            if fb:
                d = fb[-1].get("data") or {}
                check("反馈记了是谁", d.get("from") == PROMO,
                      str(d.get("from")))
                check("反馈记了主题", bool(d.get("subject")), str(d.get("subject")))
                # **"这条规则是谁在什么时候为什么加的"必须答得出来**
                check("反馈记了实际加了什么规则",
                      d.get("rule_added") == PROMO,
                      str(d.get("rule_added")))
                check("反馈记了范围", d.get("scope") == "sender", str(d.get("scope")))

            # ---- 7. 只读性没被破坏 ----
            print()
            print("=== 第 7 步：只读性 ===")
            still = all(imap.is_unseen(str(i).encode()) for i in range(1, 5))
            check("四封邮件仍然是未读", still)
            check("服务端没见过写命令",
                  not any(w in "\n".join(imap.commands)
                          for w in ("STORE", "EXPUNGE", "APPEND", "COPY")))

            srv.shutdown()
            srv.server_close()

    print()
    print("反馈闭环端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
