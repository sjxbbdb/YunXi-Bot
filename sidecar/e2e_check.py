#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「助理链路」：取邮件 → 判断 → 通知 → 台账。

## 它验证的是什么

`e2e_mail.py` 验证的是"能读到邮件"。这个验证的是**读完之后有没有做出判断**：

    一条真实未读邮件进来
        → 系统自己判断值不值得打扰
        → 自己决定通不通知
        → 通知到达桌面
        → **全程留下台账记录**

## 五封邮件，五种判定

刻意选成能覆盖每条确定性规则的组合，因为"规则命中了没有"
只有跑真数据才看得见：

| 邮件 | 预期 | 为什么 |
|---|---|---|
| 验证码 | 通知 | 时效性，晚五分钟就废了 |
| 私人邮件（明确） | 通知 | 模型判断 |
| 群发周报 | 攒着 | 几乎从来不需要立刻打扰人 |
| noreply 机器通知 | 攒着 | 一般不急 |
| 黑名单发件人 | 不提 | 使用者显式说过的 |

跑：python sidecar/e2e_check.py
（需要先 cargo build --workspace）
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

    msgs = {
        b"1": make_message("验证码 884521", frm="security@bank.example.com",
                           to="me@example.com",
                           body="您的验证码是 884521，5 分钟内有效。"),
        b"2": make_message("明天下午的评审改到三点", frm="张三 <zhangsan@example.com>",
                           to="me@example.com",
                           body="会议室换到 5 楼，材料我发你邮箱了，麻烦你看一下。"),
        b"3": make_message("本周产品周报", frm="noreply@list.example.com",
                           to="team@example.com, me@example.com",
                           cc="a@x.com, b@x.com, c@x.com, d@x.com",
                           body="本周自动周报。"),
        b"4": make_message("您的订单已发货", frm="noreply@shop.example.com",
                           to="me@example.com", body="订单已发出。"),
        b"5": make_message("限时优惠", frm="ads@spam.example.com",
                           to="me@example.com", body="买一送一。"),
    }

    with tempfile.TemporaryDirectory(prefix="yunxi-check-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        os.environ["YUNXI_BOT_HOME"] = str(home)

        with FakeImapServer(msgs, unseen=[b"1", b"2", b"3", b"4", b"5"]) as imap:
            (home / "secrets" / "mail.json").write_text(json.dumps({
                "imap_host": "127.0.0.1", "imap_port": imap.port,
                "username": "me@example.com", "password": "secret",
                "use_ssl": False,
            }), encoding="utf-8")

            # **策略文件**：黑名单和白名单得真的能被配上去，
            # 否则"这类以后别烦我"就没有落点。
            (home / "triage.json").write_text(json.dumps({
                "block_senders": ["@spam.example.com"],
                "allow_senders": [],
            }, ensure_ascii=False), encoding="utf-8")

            port = free_port()
            srv = ThreadingHTTPServer(("127.0.0.1", port), ms.Handler)
            threading.Thread(target=srv.serve_forever, daemon=True).start()

            env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
            # `--dry-run`：不真发通知（免得跑测试时往使用者屏幕上弹五条），
            # 但**判定与台账照走全流程**。这正是 dry-run 该有的意思。
            r = subprocess.run(
                [str(BIN), "check", "--port", str(port), "--dry-run"],
                capture_output=True, text=True, encoding="utf-8", env=env, timeout=90,
            )
            print("=== yunxi-bot check 的输出 ===")
            print(r.stdout or "(空)")
            if r.stderr:
                print("--- stderr ---")
                print(r.stderr)

            out = (r.stdout or "") + (r.stderr or "")

            print()
            print("=== 判定检查 ===")
            check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")
            check("报告了未读总数 5", "未读总数  : 5 封" in out)
            check("报告了生效的策略", "黑名单 1 条" in out, "让人能确认规则读进去了")
            check("验证码被判定为通知", "[通知]" in out and "验证码" in out)
            check("群发周报被攒着", "[攒着]" in out and "周报" in out)
            check("机器发件人被攒着", "机器发件人" in out)
            check("黑名单发件人不提", "[不提]" in out and "限时优惠" in out,
                  "配置的黑名单必须真的生效")
            check("统计行完整", "通知" in out and "攒着" in out and "不提" in out)

            print()
            print("=== 台账检查（全程留痕）===")
            ledger_path = home / "ledger.jsonl"
            check("台账文件已生成", ledger_path.exists())

            events = []
            if ledger_path.exists():
                for line in ledger_path.read_text(encoding="utf-8").splitlines():
                    line = line.strip()
                    if line:
                        try:
                            events.append(json.loads(line))
                        except json.JSONDecodeError:
                            pass

            kinds: list[str] = []
            for ev in events:
                k = ev.get("kind") or (ev.get("event") or {}).get("kind")
                if k:
                    kinds.append(k)

            check("有 InfoFetched 事件", "info_fetched" in kinds)
            triaged = [e for e in events
                       if (e.get("kind") or (e.get("event") or {}).get("kind")) == "info_triaged"]
            check("五条判定都落了台账", len(triaged) == 5, f"实际 {len(triaged)} 条")

            # **"决定不通知"的那些也必须留痕** —— 这是验收标准的核心：
            # 使用者问"为什么这封没告诉我"时，答案必须在台账里。
            held_or_quiet = [
                e for e in triaged
                if (e.get("data") or {}).get("action") in ("Hold", "Quiet")
            ]
            check("不通知的判定也有记录", len(held_or_quiet) >= 2,
                  f"{len(held_or_quiet)} 条")

            reasons = [(e.get("data") or {}).get("reason", "") for e in triaged]
            check("每条判定都写了理由", all(reasons) and len(reasons) == 5)
            check("理由里有规则标识或模型判断",
                  any((e.get("data") or {}).get("rule") for e in triaged))

            notices = [e for e in events
                       if (e.get("kind") or (e.get("event") or {}).get("kind")) == "notice_sent"]
            check("通知尝试也落了台账", len(notices) >= 1, f"{len(notices)} 条")
            if notices:
                d = notices[0].get("data") or {}
                check("记了投递结果", bool(d.get("result")), str(d.get("result")))
                # **"确认送达"必须单独记一位**：事后统计"通知了多少"时
                # 只有这一位为真的才算数
                check("记了'能否声称已通知'这一位", "confirmed" in d)
                check("演练模式下 confirmed 为假",
                      d.get("confirmed") is False, "dry-run 不该声称送达")

            # 只读性：整条链路跑完，邮件必须还是未读的
            print()
            print("=== 只读性 ===")
            still = all(imap.is_unseen(str(i).encode()) for i in range(1, 6))
            check("五封邮件仍然是未读", still,
                  f"flags: {[sorted(imap.flags[str(i).encode()]) for i in range(1, 6)]}")
            check("服务端没见过写命令",
                  not any(w in "\n".join(imap.commands)
                          for w in ("STORE", "EXPUNGE", "APPEND", "COPY")))

            srv.shutdown()
            srv.server_close()

    print()
    print("助理链路端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
