#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""一个够用的假 IMAP 服务器，给端到端测试用。

## 为什么值得写这个

用假对象（`unittest.mock`）替换 `imaplib` 能测出"我们调了哪些方法"，
但测不出**我们说的那句话 IMAP 协议认不认**——比如 `UID SEARCH UNSEEN`
的参数顺序、`BODY.PEEK[HEADER]` 的括号与空格、`SELECT ... readonly` 的写法。

这个假服务器跑**真的 TCP + 真的 IMAP 协议对话**，于是：
- `imaplib` 真的在解析响应
- 我们的命令真的被服务端按 IMAP 语法接受或拒绝
- 「只读」这件事由**服务端的 `\Seen` 标志**验证，而不是靠我们自觉

**它同时是"只读"最强的证据**：如果客户端用了 `BODY[]` 而不是 `BODY.PEEK[]`，
服务端会把 `\Seen` 加上——而这个假服务器**真的会加**，测试再去查它。
不是"我们相信我们没标记已读"，是"服务端说没有"。

只为测试而存在，不是给人用的 IMAP 服务器。
"""
from __future__ import annotations

import socket
import threading
from email.parser import BytesParser


class FakeImapServer:
    """最小 IMAP4rev1 子集：CAPABILITY / LOGIN / SELECT / UID SEARCH / UID FETCH / LOGOUT。

    刻意**实现 `\Seen` 语义**：用 `BODY[]` 取正文会置位，用 `BODY.PEEK[]` 不会。
    这是这个假服务器存在的核心价值——没有它，"只读"就只是我们自己的说法。
    """

    def __init__(self, messages: dict[bytes, bytes], unseen: list[bytes],
                 user: str = "me@example.com", password: str = "secret"):
        self.messages = messages          # uid -> 原始邮件字节
        self.flags = {u: set() for u in messages}
        for u in unseen:
            self.flags.setdefault(u, set()).add("\\Unseen")
        self.user = user
        self.password = password
        self.commands: list[str] = []
        self.selected_readonly: bool | None = None
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind(("127.0.0.1", 0))
        self._sock.listen(5)
        self.port = self._sock.getsockname()[1]
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)

    # ---- 生命周期 ----

    def start(self) -> "FakeImapServer":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        try:
            self._sock.close()
        except OSError:
            pass
        self._thread.join(timeout=3)

    def __enter__(self) -> "FakeImapServer":
        return self.start()

    def __exit__(self, *_exc) -> None:
        self.stop()

    # ---- 记号 ----

    def is_unseen(self, uid: bytes) -> bool:
        return "\\Unseen" in self.flags.get(uid, set())

    def was_marked_seen(self, uid: bytes) -> bool:
        """**测试只读性就用这个。**

        客户端读过之后，如果这个返回 True，说明它用了 `BODY[]`
        ——也就是把使用者的未读邮件变成了已读。
        """
        return "\\Seen" in self.flags.get(uid, set())

    # ---- 协议 ----

    def _serve(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._sock.accept()
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn: socket.socket) -> None:
        f = conn.makefile("rwb")
        try:
            f.write(b"* OK [CAPABILITY IMAP4rev1] fake ready\r\n")
            f.flush()
            state = {"authed": False}
            while True:
                line = f.readline()
                if not line:
                    return
                text = line.decode("utf-8", "replace").strip()
                if not text:
                    continue
                self.commands.append(text)
                parts = text.split()
                tag = parts[0] if parts else "?"
                cmd = parts[1].upper() if len(parts) > 1 else ""

                if cmd == "CAPABILITY":
                    f.write(b"* CAPABILITY IMAP4rev1\r\n")
                    f.write(f"{tag} OK done\r\n".encode())
                elif cmd == "LOGIN":
                    if self._login_ok(parts):
                        state["authed"] = True
                        f.write(f"{tag} OK logged in\r\n".encode())
                    else:
                        f.write(f"{tag} NO [AUTHENTICATIONFAILED] bad credentials\r\n".encode())
                elif cmd == "EXAMINE":
                    # **IMAP 里 SELECT 的只读变体。** 服务端在这里承诺：
                    # 之后任何写操作都会被拒绝。这个假服务器真的会拒绝
                    # （见 _uid 里对 STORE 的处理）——否则"只读"就只是
                    # 客户端自己的一句话。
                    self.selected_readonly = True
                    n = len(self.messages)
                    f.write(f"* {n} EXISTS\r\n".encode())
                    f.write(b"* OK [PERMANENTFLAGS ()] read-only\r\n")
                    f.write(f"{tag} OK [READ-ONLY] done\r\n".encode())
                elif cmd == "SELECT":
                    # 读模式选择。**我们不该发这个**——测试会检查。
                    self.selected_readonly = False
                    n = len(self.messages)
                    f.write(f"* {n} EXISTS\r\n".encode())
                    f.write(f"{tag} OK [READ-WRITE] done\r\n".encode())
                elif cmd == "UID":
                    self._uid(f, tag, parts, state)
                elif cmd == "LOGOUT":
                    f.write(b"* BYE bye\r\n")
                    f.write(f"{tag} OK done\r\n".encode())
                    f.flush()
                    return
                elif cmd == "NOOP":
                    f.write(f"{tag} OK done\r\n".encode())
                else:
                    f.write(f"{tag} BAD unknown command {cmd}\r\n".encode())
                f.flush()
        except (OSError, ValueError):
            return
        finally:
            try:
                f.close()
                conn.close()
            except OSError:
                pass

    def _login_ok(self, parts: list[str]) -> bool:
        # LOGIN user pass —— 参数可能带引号
        args = [p.strip('"') for p in parts[2:]]
        if len(args) < 2:
            return False
        return args[0] == self.user and args[1] == self.password

    def _uid(self, f, tag: str, parts: list[str], state: dict) -> None:
        if not state["authed"]:
            f.write(f"{tag} NO not authenticated\r\n".encode())
            return
        sub = parts[2].upper() if len(parts) > 2 else ""
        if sub == "SEARCH":
            # UID SEARCH UNSEEN
            crit = " ".join(parts[3:]).upper()
            if "UNSEEN" in crit:
                uids = [u for u in sorted(self.messages, key=int) if self.is_unseen(u)]
            else:
                uids = sorted(self.messages, key=int)
            f.write(b"* SEARCH " + b" ".join(uids) + b"\r\n")
            f.write(f"{tag} OK done\r\n".encode())
        elif sub == "FETCH":
            self._uid_fetch(f, tag, parts)
        elif sub == "STORE":
            # **假服务器也拒绝 STORE**：如果客户端发了，测试要能看见
            f.write(f"{tag} NO store not allowed on this fixture\r\n".encode())
        else:
            f.write(f"{tag} BAD unsupported UID {sub}\r\n".encode())

    def _uid_fetch(self, f, tag: str, parts: list[str]) -> None:
        uid = parts[3].encode() if len(parts) > 3 else b""
        spec = " ".join(parts[4:])
        raw = self.messages.get(uid)
        if raw is None:
            f.write(f"{tag} OK done\r\n".encode())
            return

        # **\Seen 语义在这里。** 这是整个假服务器的核心：
        # 裸 BODY[...] 会置位，BODY.PEEK[...] 不会。
        peeks = spec.upper().count("PEEK")
        bare_body = _has_bare_body(spec.upper())
        if bare_body and peeks == 0:
            self.flags.setdefault(uid, set()).add("\\Seen")
            self.flags[uid].discard("\\Unseen")

        if "HEADER" in spec.upper():
            head = _headers_only(raw)
            payload = head
            label = b"BODY[HEADER]"
        else:
            payload = raw
            label = b"BODY[]"

        f.write(b"* 1 FETCH (UID " + uid + b" " + label + b" {" +
                str(len(payload)).encode() + b"}\r\n")
        f.write(payload)
        f.write(b")\r\n")
        f.write(f"{tag} OK done\r\n".encode())


def _has_bare_body(spec_upper: str) -> bool:
    """spec 里有没有**裸的** `BODY[`（不是 `BODY.PEEK[`）。

    `BODY.PEEK[]` 里也含 `BODY`，所以要专门排掉 PEEK 形式。
    """
    i = 0
    while True:
        i = spec_upper.find("BODY", i)
        if i < 0:
            return False
        after = spec_upper[i + 4:]
        if after.startswith(".PEEK"):
            i += 4
            continue
        if after.startswith("["):
            return True
        i += 4


def _headers_only(raw: bytes) -> bytes:
    """剥出邮件头（含结尾空行）。"""
    for sep in (b"\r\n\r\n", b"\n\n"):
        i = raw.find(sep)
        if i >= 0:
            return raw[: i + len(sep)]
    return raw


def parse_headers(raw: bytes):
    return BytesParser().parsebytes(raw)
