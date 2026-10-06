#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证：Rust CLI → 邮件 sidecar → 真实 IMAP 协议 → 回来。

## 为什么需要一个这样的脚本

单元测试各自证明了零件是对的，但没有证明**零件接起来**是对的：
Rust 拼的 JSON 字段名对不对、sidecar 的端口和配置路径是不是同一套、
中文主题跨过三次编码转换之后还在不在。

更重要的是一条**只有端到端才能测**的性质：
**整个链路跑完之后，那封邮件仍然是未读的。**

## 它跑的是什么

1. 起一个假 IMAP 服务器（真 TCP、真 IMAP 语法）
2. 在临时目录里写一份 `mail.json`（仓库外的约定照旧）
3. 在临时端口上起真的 sidecar
4. **真的去调 `yunxi-bot mail`**（编译好的二进制）
5. 检查它输出的内容
6. **回头查假服务器上的 `\\Seen` 标志**——邮件必须还是未读的

跑：python sidecar/e2e_mail.py
（需要先 `cargo build --workspace`）
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

    # 三封邮件：一封直接给我、一封群发、一封中文主题
    msgs = {
        b"1": make_message("会议改到下午三点", frm="张三 <zhangsan@example.com>",
                           to="me@example.com", body="原定上午的会改到下午三点。"),
        b"2": make_message("本周产品周报", frm="noreply@list.example.com",
                           to="team@example.com, me@example.com",
                           cc="a@x.com, b@x.com, c@x.com, d@x.com",
                           body="这是本周的自动周报。"),
        b"3": make_message("验证码 123456", frm="security@bank.example.com",
                           to="me@example.com", body="您的验证码是 123456，5 分钟内有效。"),
    }

    with tempfile.TemporaryDirectory(prefix="yunxi-e2e-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)

        # **sidecar 是本进程内的，所以它读的是本进程的环境变量。**
        # 第一次跑这个脚本时漏了这一步，于是 sidecar 去找默认 home、
        # 而 CLI 去临时 home——两边不一致。端到端测试当场抓到了这件事，
        # 而它恰好证明了"两边算法必须对齐"这条约束是真的会咬人的。
        os.environ["YUNXI_BOT_HOME"] = str(home)

        with FakeImapServer(msgs, unseen=[b"1", b"2", b"3"]) as imap:
            (home / "secrets" / "mail.json").write_text(json.dumps({
                "imap_host": "127.0.0.1",
                "imap_port": imap.port,
                "username": "me@example.com",
                "password": "secret",
                "use_ssl": False,          # 回环才允许；这条限制本身也有测试
            }), encoding="utf-8")

            port = free_port()
            srv = ThreadingHTTPServer(("127.0.0.1", port), ms.Handler)
            threading.Thread(target=srv.serve_forever, daemon=True).start()

            print(f"假 IMAP : 127.0.0.1:{imap.port}")
            print(f"sidecar : 127.0.0.1:{port}")
            print(f"临时 home: {home}")
            print()

            env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")
            r = subprocess.run(
                [str(BIN), "mail", "--port", str(port)],
                capture_output=True, text=True, encoding="utf-8", env=env, timeout=60,
            )

            print("=== yunxi-bot mail 的输出 ===")
            print(r.stdout or "(stdout 为空)")
            if r.stderr:
                print("--- stderr ---")
                print(r.stderr)

            out = (r.stdout or "") + (r.stderr or "")

            def check(name: str, cond: bool, detail: str = "") -> None:
                nonlocal ok
                mark = "✓" if cond else "✗"
                print(f"  {mark} {name}{('  ' + detail) if detail else ''}")
                if not cond:
                    ok = False

            print()
            print("=== 检查 ===")
            check("退出码为 0", r.returncode == 0, f"实际 {r.returncode}")
            check("报告未读总数 3", "未读总数  : 3 封" in out)
            check("取到 3 封", "本次取到  : 3 封" in out)
            check("说出了账号", "me@example.com" in out, "让人能确认读的是哪个邮箱")
            check("中文主题完好", "会议改到下午三点" in out)
            check("识别出直接给我", "[直接]" in out)
            check("识别出群发", "[群发]" in out)
            check("中文正文预览完好", "改到下午三点" in out)
            check("声明了只读", "只读" in out)

            # **最关键的一条：整条链路跑完之后，邮件必须还是未读的。**
            still_unseen = all(srv_flag for srv_flag in
                               (imap.is_unseen(b"1"), imap.is_unseen(b"2"), imap.is_unseen(b"3")))
            check("三封邮件仍然是未读（只读性）", still_unseen,
                  f"flags: 1={sorted(imap.flags[b'1'])} 2={sorted(imap.flags[b'2'])} "
                  f"3={sorted(imap.flags[b'3'])}")
            check("服务端没见过任何写命令",
                  not any(w in "\n".join(imap.commands)
                          for w in ("STORE", "EXPUNGE", "APPEND", "COPY")))

            # 再跑一次读取正文，同样不能标记已读
            r2 = subprocess.run(
                [str(BIN), "mail", "--port", str(port), "--read", "3"],
                capture_output=True, text=True, encoding="utf-8", env=env, timeout=60,
            )
            check("读正文成功", r2.returncode == 0 and "123456" in (r2.stdout or ""))
            check("读正文之后仍是未读", imap.is_unseen(b"3"),
                  f"flags: {sorted(imap.flags[b'3'])}")

            srv.shutdown()
            srv.server_close()

    print()
    print("端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
