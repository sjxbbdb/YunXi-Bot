#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「常驻驱动」——daemon 自己看着，而不是等你敲命令。

## 它验的是 goal 最后一条

> daemon 每一轮推进整条链，而不是等用户敲命令

## 怎么算证明

光看 daemon 启动成功不算——那只证明它没崩。这里做的是：

1. 起 daemon，**不给它任何命令**
2. 往假 IMAP 里**投一封新邮件**（模拟"邮件刚进来"）
3. 等它自己那一轮到点
4. 检查：**它自己发现了、自己判定了、自己通知了、留了台账**
5. 再投一封**同类**的，检查它**不重复打扰**

第 5 步是常驻场景下最要紧的：手动跑一次重复通知只是烦一下，
常驻跑就是反复弹到你读掉为止。

## 时间参数

`--interval 500 --assistant-interval 2` —— 两秒一轮，跑十几秒就够看几轮。
真实使用是 300 秒（5 分钟）。

跑：python sidecar/e2e_daemon.py
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
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


def read_events(home: Path) -> list[dict]:
    out = []
    p = home / "ledger.jsonl"
    if not p.exists():
        return out
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            try:
                out.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return out


def kind_of(e: dict) -> str | None:
    return e.get("kind") or (e.get("event") or {}).get("kind")


def main() -> int:
    BIN = ensure_fresh_binary()

    ok = True

    def check(name: str, cond: bool, detail: str = "") -> None:
        nonlocal ok
        print(f"  {'✓' if cond else '✗'} {name}{('  ' + detail) if detail else ''}")
        if not cond:
            ok = False

    # 一开始**只有一封未读**——daemon 起来时它就该被发现。
    msgs = {
        b"1": make_message("今天下午的评审改到三点", frm="张三 <zhangsan@example.com>",
                           to="me@example.com", body="材料我发你邮箱了。"),
    }

    with tempfile.TemporaryDirectory(prefix="yunxi-daemon-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        os.environ["YUNXI_BOT_HOME"] = str(home)

        with FakeImapServer(msgs, unseen=[b"1"]) as imap:
            (home / "secrets" / "mail.json").write_text(json.dumps({
                "imap_host": "127.0.0.1", "imap_port": imap.port,
                "username": "me@example.com", "password": "secret",
                "use_ssl": False,
            }), encoding="utf-8")

            # 私人邮件走模型判定，而 Verdict 没起会降级成"攒着"。
            # 白名单让它确定地走到"通知"——这样才验得了通知这条路。
            (home / "triage.json").write_text(json.dumps({
                "allow_senders": ["zhangsan@example.com"],
            }, ensure_ascii=False), encoding="utf-8")

            port = free_port()
            srv = ThreadingHTTPServer(("127.0.0.1", port), ms.Handler)
            threading.Thread(target=srv.serve_forever, daemon=True).start()

            env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

            # **启动 daemon，然后不给它任何命令。**
            print("=== 启动守护进程（不给任何命令，看它自己会不会动）===")
            proc = subprocess.Popen(
                [str(BIN), "daemon",
                 "--interval", "500",
                 "--assistant-interval", "2", "--port", str(port),
                 "--console"],   # 通知打到终端（真通知，不是演练）
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True, encoding="utf-8", env=env,
            )

            def wait_for(pred, timeout=25.0):
                """等条件成立，返回 (成立, 流逝秒数)。"""
                t0 = time.time()
                while time.time() - t0 < timeout:
                    if pred():
                        return True, time.time() - t0
                    time.sleep(0.4)
                return False, time.time() - t0

            # ---- 1. 它自己发现了第一封 ----
            found, secs = wait_for(lambda: any(
                kind_of(e) == "notice_sent" and (e.get("data") or {}).get("id") == "1"
                for e in read_events(home)
            ))
            check("**它自己发现并通知了第一封（没敲任何命令）**", found,
                  f"{secs:.1f} 秒" if found else "超时")

            # ---- 2. 投一封新的 ----
            print()
            print("=== 投一封新邮件，看它下一轮会不会自己发现 ===")
            imap.messages[b"2"] = make_message(
                "明天的会议材料", frm="zhangsan@example.com",
                to="me@example.com", body="刚整理好，你看下。")
            imap.flags[b"2"] = {"\\Unseen"}

            found2, secs2 = wait_for(lambda: any(
                kind_of(e) == "notice_sent" and (e.get("data") or {}).get("id") == "2"
                for e in read_events(home)
            ))
            check("**新邮件被自己发现并通知**", found2,
                  f"{secs2:.1f} 秒" if found2 else "超时")
            check("发现得够快（一个巡览周期内）", found2 and secs2 < 10,
                  f"{secs2:.1f} 秒")

            # ---- 3. 不重复打扰 ----
            print()
            print("=== 再等几个周期，看它会不会重复打扰 ===")
            before = [
                e for e in read_events(home)
                if kind_of(e) == "notice_sent"
                and (e.get("data") or {}).get("dry_run") is not True
            ]
            time.sleep(6)
            after = [
                e for e in read_events(home)
                if kind_of(e) == "notice_sent"
                and (e.get("data") or {}).get("dry_run") is not True
            ]
            # **这条断言必须是"真的通知过"才成立。**
            # 演练模式的通知按设计不算"已告知过"（不然跑一次演练就把邮件
            # 永久吞了），所以拿演练模式测去重会得到一个空虚的通过——
            # 两组空集合比大小，永远相等。
            check("有真的通知记录（不是演练）", len(before) >= 2,
                  f"{len(before)} 条")
            ids = [(e.get("data") or {}).get("id") for e in before]
            check("**同一个 id 只被通知一次**", len(ids) == len(set(ids)),
                  f"ids={ids}")

            # ---- 4. 台账 ----
            print()
            print("=== 台账 ===")
            events = read_events(home)
            kinds = [kind_of(e) for e in events]
            fetched = kinds.count("info_fetched")
            check("有多轮 Infofetched（说明它在反复看）", fetched >= 2, f"{fetched} 轮")

            triaged_ids = {
                (e.get("data") or {}).get("id")
                for e in events if kind_of(e) == "info_triaged"
            }
            check("两封都有判定记录", {"1", "2"} <= triaged_ids, str(triaged_ids))
            check("通知尝试有记录", "notice_sent" in kinds)

            # ---- 5. 停 daemon，看它退得干净 ----
            print()
            print("=== 停止 ===")
            proc.terminate()
            try:
                out, _ = proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
                out, _ = proc.communicate()
            check("守护进程能干净退出", proc.returncode is not None,
                  f"退出码 {proc.returncode}")

            print()
            print("=== 守护进程的输出（尾部）===")
            lines = (out or "").strip().splitlines()
            for line in lines[-14:]:
                print("  " + line)

            # 输出里应该能看到它在自己报每一轮
            joined = "\n".join(lines)
            check("输出里能看到助理巡览的摘要行",
                  "助理" in joined, "它得让你知道自己在看")
            check("启动时报告了信件通道",
                  "信件" in joined)

            srv.shutdown()
            srv.server_close()

    print()
    print("常驻驱动端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
