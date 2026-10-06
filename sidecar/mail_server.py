#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""YunXi Bot 邮件 sidecar —— **只读**的 IMAP 信息源。

为什么走 sidecar 而不是在 Rust 里写 IMAP
----------------------------------------
三条：

1. **MIME 解码是个无底洞。** 中文邮件可能是 GB2312 / GBK / GB18030 / UTF-8 /
   ISO-8859-1 编码，头里的 `=?GBK?B?...?=` 要按 RFC 2047 解，附件要按
   RFC 2231 拆，multipart 还可能嵌套。Python 的 `email` 标准库这些全做了，
   而且是几十年打磨过的。Rust 侧自己写一遍是几个月的活，还大概率写得更差。
2. **零新依赖。** `imaplib` + `ssl` + `email` 都在标准库里。项目对 Rust 依赖
   很克制（ADR D9/D20 都先量代价），这条路径一个新依赖都不用加。
3. **与既有架构一致。** 决策模型也是 Python sidecar（ADR D12：把上游库差异
   关在 Python 侧）。同一个模式，同一套回环 HTTP 契约。

只读是怎么保证的
----------------
**不是靠"我们只调只读命令"这句话，是靠两件具体的事：**

- **只发 `SELECT` / `SEARCH` / `FETCH`。** 绝不发 `STORE` / `APPEND` / `COPY` /
  `MOVE` / `EXPUNGE`。有一个测试用假 IMAP 连接断言这一点。
- **读正文一律用 `BODY.PEEK[]`，不用 `BODY[]`。** 这是最容易踩的坑：
  `BODY[]` 会**顺带把邮件标成已读**，也就等于改了使用者的邮箱状态。
  一个"只读"的信息源把用户所有未读邮件变成已读，是比读不到更糟的事故。

凭证
----
`<home>/secrets/mail.json`（仓库之外，与 agnes/deepseek/bocha 同一套约定）：

    {
      "imap_host": "imap.qq.com",
      "imap_port": 993,
      "username": "you@qq.com",
      "password": "授权码，不是登录密码"
    }

**凭证只在 Python 侧读**，Rust 侧永远看不到密码——它只知道有个 sidecar 在
17871 端口上。少一处传播就少一处泄露。

用法
----
  python sidecar/mail_server.py --port 17871
  python sidecar/mail_server.py --check      # 只检查配置与连通性，不起服务
"""
from __future__ import annotations

import argparse
import email
import email.utils
import imaplib
import json
import os
import re
import socket
import ssl
import sys
import threading
from datetime import datetime, timezone
from email.header import decode_header, make_header
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

DEFAULT_PORT = 17871
LOOPBACK = "127.0.0.1"
MAX_BODY_BYTES = 1 << 20
# 单封邮件正文最多回灌多少字符。够判定"要不要打扰"，又不会一条邮件就把
# 模型上下文吃掉大半。
MAX_PREVIEW_CHARS = 1200
# 连接与命令超时。IMAP 服务器偶尔会卡住，而没有超时的常驻进程会被它拖死。
TIMEOUT_SECONDS = 20


def default_home() -> Path:
    """与 Rust 侧 `default_home()` 同一套规则。

    两处必须一致，否则 sidecar 会去一个空的目录里找凭证，而报出来的错
    是"没配置"——让人以为是没配，其实是找错了地方。
    """
    env = os.environ.get("YUNXI_BOT_HOME")
    if env:
        return Path(env)
    local = os.environ.get("LOCALAPPDATA")
    if local:
        return Path(local) / "YunXiBot"
    return Path.home() / ".yunxi-bot"


def credentials_path(home: Path | None = None) -> Path:
    return (home or default_home()) / "secrets" / "mail.json"


class MailError(Exception):
    """配置或连接层面的失败。**要能被翻译成使用者看得懂的话。**"""


def _is_loopback(host: str) -> bool:
    """这台主机是不是本机。

    只认字面量，**不做 DNS 解析**：`localhost` 理论上可以被 hosts 文件指到
    别处，但把它当回环是所有人的预期；而真去解析一个域名再判断，
    反而给了攻击者一个"把域名解析到 127.0.0.1 骗过检查"的面。
    这里的两条判断合起来是安全的：字面量回环 → 允许明文；
    其它一切（含任何域名）→ 强制 TLS。
    """
    h = (host or "").strip().strip("[]").lower()
    return h in ("127.0.0.1", "localhost", "::1")


# --------------------------------------------------------------------------
# 只读的 IMAP 访问
# --------------------------------------------------------------------------

#: 允许发出的 IMAP 命令白名单。
#:
#: **这是"只读"的可执行定义。** 有一个测试断言这里不含任何会改状态的动作。
#: 靠注释说"我们只读"是不够的——注释会过期，白名单会被测试盯着。
#:
#: ## 为什么是 `EXAMINE` 而不是 `SELECT`
#:
#: 是真实协议测试逼出来的：`imaplib` 的 `select(mailbox, readonly=True)`
#: 在线上发的是 **`EXAMINE`**——IMAP 里 `SELECT` 的只读变体。
#:
#: 而这比"发 SELECT 但自称只读"硬得多：`EXAMINE` 之后**服务端会直接拒绝
#: 任何写操作**，不是靠我们自觉不发。所以白名单里**只放 `EXAMINE`，
#: 不放 `SELECT`**——没有只读保证的读模式选择不该有入口。
#:
#: 同理不放 `CLOSE`：它在读写下会 expunge（永久删掉标了 \Deleted 的邮件）。
#: 我们收尾用 `LOGOUT`。
READONLY_COMMANDS = frozenset({"EXAMINE", "UID", "FETCH", "NOOP", "LOGOUT"})

#: 明确禁止的动作。放在这里是为了让"为什么不能加"有一个显式的位置。
FORBIDDEN_COMMANDS = frozenset(
    {"STORE", "APPEND", "COPY", "MOVE", "EXPUNGE", "CREATE", "DELETE", "RENAME", "SUBSCRIBE"}
)


def _decode(value: str | None) -> str:
    """把邮件头解成可读文本。

    `decode_header` 把 `=?GBK?B?...?=` 这类编码拆成 (bytes, charset) 列表，
    再逐个按各自声明的编码解。**不能整个当 UTF-8 解**——中文邮件的头
    十有八九不是 UTF-8，那样解出来全是问号。
    """
    if not value:
        return ""
    try:
        return str(make_header(decode_header(value))).strip()
    except Exception:
        # 头坏了不该让整封邮件读不出来；退化成"尽力而为"的替换解码
        return value.encode("utf-8", "replace").decode("utf-8", "replace").strip()


def _addr_list(raw: str | None) -> list[tuple[str, str]]:
    """解出 [(名字, 地址)]。"""
    if not raw:
        return []
    out = []
    for name, addr in email.utils.getaddresses([raw]):
        if addr:
            out.append((_decode(name), addr.strip()))
    return out


def _first_addr(raw: str | None) -> tuple[str, str]:
    a = _addr_list(raw)
    return a[0] if a else ("", "")


def _parse_date_ms(raw: str | None) -> int:
    """邮件日期 → Unix 毫秒。解不出来返回 0。

    **返回 0 而不是"现在"**：一封日期坏掉的邮件被当成刚收到，会让它插到
    通知队列最前面——用假数据填补比留空更糟。
    """
    if not raw:
        return 0
    try:
        dt = email.utils.parsedate_to_datetime(raw)
        if dt is None:
            return 0
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return int(dt.timestamp() * 1000)
    except Exception:
        return 0


def _text_body(msg: email.message.Message, limit: int) -> str:
    """抽出正文纯文本，截到 limit 个**字符**（不是字节）。

    按字节切会落在汉字中间，Python 里表现为切出半个字符（UnicodeDecodeError
    或乱码），Rust 里更直接——按字节切会 panic。
    """
    parts: list[str] = []
    if msg.is_multipart():
        for part in msg.walk():
            ctype = part.get_content_type()
            disp = str(part.get("Content-Disposition") or "")
            if "attachment" in disp.lower():
                continue
            if ctype == "text/plain":
                parts.append(_part_text(part))
            elif ctype == "text/html" and not parts:
                parts.append(_strip_html(_part_text(part)))
    else:
        raw = _part_text(msg)
        parts.append(_strip_html(raw) if msg.get_content_type() == "text/html" else raw)

    body = "\n".join(p for p in parts if p.strip())
    # 压掉连续空行：模型看重的是内容，不是排版
    body = re.sub(r"\n{3,}", "\n\n", body).strip()
    return body[:limit]


def _part_text(part: email.message.Message) -> str:
    try:
        payload = part.get_payload(decode=True)
        if payload is None:
            return ""
        charset = part.get_content_charset() or "utf-8"
        try:
            return payload.decode(charset, "replace")
        except LookupError:
            # 声明了一个 Python 不认识的 charset。**退到 UTF-8 并替换**，
            # 而不是抛错——能读到一半总比读不到强，而且替换字符会让
            # "这里有东西没解出来"变得可见。
            return payload.decode("utf-8", "replace")
    except Exception:
        return ""


def _strip_html(html: str) -> str:
    html = re.sub(r"(?is)<(script|style)[^>]*>.*?</\1>", " ", html)
    html = re.sub(r"(?s)<[^>]+>", " ", html)
    html = (
        html.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", '"')
    )
    return re.sub(r"[ \t]{2,}", " ", html)


class Mailbox:
    """一次连接。**用完就关**——常驻进程不该长期占着一个 IMAP 会话。"""

    def __init__(self, cfg: dict):
        self.cfg = cfg
        self.conn: imaplib.IMAP4_SSL | imaplib.IMAP4 | None = None
        self.sent_commands: list[str] = []

    def __enter__(self) -> "Mailbox":
        host = self.cfg["imap_host"]
        port = int(self.cfg.get("imap_port", 993))
        use_ssl = bool(self.cfg.get("use_ssl", True))

        # **明文 IMAP 只允许连回环。**
        #
        # 这条限制的理由：`use_ssl: false` 是为了能连本机邮件服务器、
        # 以及让整条链路可以被测试（mock IMAP 起在 127.0.0.1）。但邮箱密码
        # 明文过网是不可接受的——所以非回环主机一律强制 TLS，
        # 而不是靠使用者记得把它打开。
        if not use_ssl and not _is_loopback(host):
            raise MailError(
                f"拒绝以明文连接非本机 IMAP 服务器 {host}："
                f"邮箱密码会明文过网。去掉 use_ssl:false，或改用 TLS。"
            )

        factory = imaplib.IMAP4_SSL if use_ssl else imaplib.IMAP4
        kwargs: dict = {"timeout": TIMEOUT_SECONDS}
        if use_ssl:
            kwargs["ssl_context"] = ssl.create_default_context()
        try:
            self.conn = factory(host, port, **kwargs)
        except (socket.timeout, TimeoutError):
            raise MailError(f"连接 {host}:{port} 超时（{TIMEOUT_SECONDS} 秒）")
        except ssl.SSLError as e:
            raise MailError(f"连接 {host}:{port} 的 TLS 握手失败: {e}")
        except OSError as e:
            raise MailError(f"连不上 {host}:{port}: {e}")
        except imaplib.IMAP4.error as e:
            raise MailError(f"{host}:{port} 的服务端问候不合法: {e}")
        try:
            self.conn.login(self.cfg["username"], self.cfg["password"])
        except imaplib.IMAP4.error as e:
            # 这条错误信息最容易误导人：QQ/163 的 IMAP 用的是**授权码**，
            # 不是登录密码。失败时把这一点说出来，省掉一轮排查。
            raise MailError(
                f"IMAP 登录被拒（{e}）。注意：QQ/163 等邮箱的 IMAP 用的是"
                f"「授权码」而不是登录密码，要去邮箱设置里单独开启并生成。"
            )
        return self

    def __exit__(self, *_exc) -> None:
        if self.conn is not None:
            try:
                self.conn.logout()
            except Exception:
                # 退出失败不影响已经取到的数据，也不该把异常盖到真正的错误上
                pass
            self.conn = None

    def _cmd(self, name: str, *args):
        """发一条命令，并记录它。**白名单在这里强制执行。**"""
        if name.upper() in FORBIDDEN_COMMANDS:
            raise MailError(f"内部错误：试图发送会改状态的命令 {name}")
        if name.upper() not in READONLY_COMMANDS:
            raise MailError(f"内部错误：命令 {name} 不在只读白名单里")
        # 记录**有意义的那个名字**：`UID FETCH` 走的是 `_cmd("UID", "FETCH", ...)`，
        # 只记 "UID" 会让审计日志看不出到底做了什么。
        # 而"发过哪些命令"正是"只读"这件事唯一的对外证据。
        if name.upper() == "UID" and args and isinstance(args[0], str):
            self.sent_commands.append(f"UID {args[0].upper()}")
        else:
            self.sent_commands.append(name.upper())
        assert self.conn is not None

        # `imaplib` 没有 `examine()` 方法——它在线上是通过
        # `select(mailbox, readonly=True)` 发 `EXAMINE` 的。
        # 这里做一次映射，好让**审计日志里出现的就是线上真实发出的命令名**：
        # 记成 "SELECT" 会让"我们发的是只读的还是读写的"变得看不出来。
        if name.upper() == "EXAMINE":
            return self.conn.select(*args, True)
        return getattr(self.conn, name.lower())(*args)

    def select(self, mailbox: str = "INBOX") -> int:
        """选中邮箱——**只读**。

        用 `EXAMINE` 而不是 `SELECT`：前者是 IMAP 里的只读变体，
        **服务端会直接拒绝后续任何写操作**。这比"我们自觉不发 STORE"
        硬得多——一个是承诺，一个是协议保证。
        """
        typ, data = self._cmd("EXAMINE", mailbox)
        if typ != "OK":
            raise MailError(f"打不开邮箱 {mailbox}: {data}")
        try:
            return int(data[0])
        except (TypeError, ValueError, IndexError):
            return 0

    def unread_uids(self, limit: int) -> list[bytes]:
        """最新的 limit 封未读。**最新的在前**——助理先看新的。"""
        typ, data = self._cmd("UID", "SEARCH", None, "UNSEEN")
        if typ != "OK":
            raise MailError(f"搜索未读失败: {data}")
        uids = (data[0] or b"").split()
        # UID 递增，所以取尾部就是最新的
        return list(reversed(uids[-limit:])) if uids else []

    def headers(self, uid: bytes) -> dict:
        """取一封邮件的头。

        **`BODY.PEEK[HEADER]` 而不是 `BODY[HEADER]`。** 后者会把邮件标成已读，
        也就等于改动了使用者的邮箱——一个只读的信息源不该有那种副作用。
        """
        typ, data = self._cmd("UID", "FETCH", uid, "(BODY.PEEK[HEADER])")
        if typ != "OK":
            return {}
        raw = b""
        for part in data:
            if isinstance(part, tuple) and len(part) >= 2:
                raw = part[1]
                break
        if not raw:
            return {}
        return self._summarize(email.message_from_bytes(raw), uid)

    def message(self, uid: bytes) -> email.message.Message | None:
        """取整封邮件（正文用）。同样用 PEEK，不标记已读。"""
        typ, data = self._cmd("UID", "FETCH", uid, "(BODY.PEEK[])")
        if typ != "OK":
            return None
        for part in data:
            if isinstance(part, tuple) and len(part) >= 2:
                return email.message_from_bytes(part[1])
        return None

    @staticmethod
    def _summarize(msg: email.message.Message, uid: bytes) -> dict:
        name, addr = _first_addr(msg.get("From"))
        to_all = _addr_list(msg.get("To"))
        cc_all = _addr_list(msg.get("Cc"))
        # 「直接给我」的判据：收件人里只有我一个，且没有抄送别人。
        # 群发/列表邮件是打扰判定里最该被压下去的一类，所以要能区分出来。
        direct = len(to_all) <= 1 and not cc_all
        return {
            "uid": uid.decode("ascii", "replace"),
            "from_name": name,
            "from_addr": addr,
            "subject": _decode(msg.get("Subject")),
            "received_at_ms": _parse_date_ms(msg.get("Date")),
            "direct": direct,
            "recipient_count": len(to_all) + len(cc_all),
            "has_attachments": any(
                "attachment" in str(p.get("Content-Disposition") or "").lower()
                for p in (msg.walk() if msg.is_multipart() else [msg])
            ),
            # 有没有在邮件头里被点名（To 而不是 Cc/Bcc）。列表邮件常常
            # 只把你放在 Cc，那是"知会"而不是"要你办"。
            "addressed_directly": bool(to_all),
        }


# --------------------------------------------------------------------------
# 配置
# --------------------------------------------------------------------------


def load_config(home: Path | None = None) -> dict:
    path = credentials_path(home)
    if not path.exists():
        raise MailError(
            f"找不到邮件配置 {path}。需要这样一份 JSON：\n"
            '  {"imap_host":"imap.qq.com","imap_port":993,'
            '"username":"you@qq.com","password":"授权码"}'
        )
    try:
        cfg = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as e:
        raise MailError(f"{path} 不是合法 JSON: {e}")
    except OSError as e:
        raise MailError(f"读不到 {path}: {e}")

    missing = [k for k in ("imap_host", "username", "password") if not cfg.get(k)]
    if missing:
        raise MailError(f"{path} 缺少必填字段: {', '.join(missing)}")
    cfg.setdefault("imap_port", 993)
    cfg.setdefault("mailbox", "INBOX")
    return cfg


def fetch_unread(cfg: dict, limit: int, with_preview: bool) -> dict:
    """取未读邮件。

    ## 为什么列表只取邮件头，正文要另外调 `/v1/read`

    取 20 封邮件的头是很快的；取 20 封**整封**（含附件）可能几十 MB、
    几十秒。而判定"要不要打扰你"绝大多数时候只需要发件人和主题。
    所以列表给头，正文按需取——**先便宜地筛，再花代价地看**，
    和模型路由"先用确定性信号，拿不准才问模型"是同一条纪律。
    """
    with Mailbox(cfg) as mb:
        total = mb.select(cfg["mailbox"])
        uids = mb.unread_uids(limit)
        items = []
        for uid in uids:
            head = mb.headers(uid)
            if not head:
                # 单封取不到不该让整次列举失败——跳过它，但**记下跳过数**，
                # 否则"少了 3 封"这件事会完全不可见。
                items.append({"uid": uid.decode("ascii", "replace"), "error": "取邮件头失败"})
                continue
            if with_preview:
                msg = mb.message(uid)
                head["preview"] = _text_body(msg, MAX_PREVIEW_CHARS) if msg else ""
            items.append(head)
        return {
            "items": items,
            "total_unseen": total,
            "skipped": sum(1 for i in items if "error" in i),
            "commands_sent": sorted(set(mb.sent_commands)),
        }


def fetch_one(cfg: dict, uid: str, limit: int = MAX_PREVIEW_CHARS) -> dict:
    with Mailbox(cfg) as mb:
        mb.select(cfg["mailbox"])
        raw_uid = uid.encode("ascii")
        msg = mb.message(raw_uid)
        if msg is None:
            raise MailError(f"取不到 UID {uid} 的邮件（可能已被删除或移动）")
        head = Mailbox._summarize(msg, raw_uid)
        head["body"] = _text_body(msg, limit)
        return head


# --------------------------------------------------------------------------
# HTTP 契约
# --------------------------------------------------------------------------


class Handler(BaseHTTPRequestHandler):
    server_version = "yunxi-mail/1"
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args) -> None:
        # 默认实现会把每条请求打到 stderr，常驻起来刷屏
        pass

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/health":
            self._send(404, {"error": "unknown path"})
            return
        # **一定要报出自己用的是哪个 home 和哪个配置文件。**
        #
        # sidecar 与 CLI 各自算一遍 `default_home()`，两边靠 `YUNXI_BOT_HOME`
        # 这个约定对齐。约定一旦没对齐（比如只给 CLI 设了环境变量），
        # 表现是 sidecar 说"没配置"、而使用者的配置文件明明就在那——
        # 这是最难查的一类错。把路径报出来，调用方就能直接把两个路径一摆，
        # 一眼看出是找错了地方，而不是"没配"。
        home = default_home()
        try:
            cfg = load_config()
            self._send(200, {
                "ok": True,
                "configured": True,
                "imap_host": cfg["imap_host"],
                "username": cfg["username"],
                "mailbox": cfg.get("mailbox", "INBOX"),
                "home": str(home),
                "config_path": str(credentials_path(home)),
            })
        except MailError as e:
            self._send(200, {
                "ok": True,
                "configured": False,
                "reason": str(e),
                "home": str(home),
                "config_path": str(credentials_path(home)),
            })

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        if length > MAX_BODY_BYTES:
            self._send(413, {"error": "body too large"})
            return
        raw = self.rfile.read(length) if length else b"{}"
        try:
            req = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as e:
            self._send(400, {"error": f"请求不是合法 JSON: {e}"})
            return

        path = self.path.rstrip("/")
        try:
            if path == "/v1/unread":
                cfg = load_config()
                limit = max(1, min(int(req.get("limit", 20)), 100))
                with_preview = bool(req.get("preview", True))
                self._send(200, fetch_unread(cfg, limit, with_preview))
            elif path == "/v1/read":
                uid = req.get("uid")
                if not uid:
                    self._send(400, {"error": "缺少 uid"})
                    return
                cfg = load_config()
                self._send(200, fetch_one(cfg, str(uid), int(req.get("limit", MAX_PREVIEW_CHARS))))
            else:
                self._send(404, {"error": "unknown path"})
        except MailError as e:
            # **500 而不是 200 + 空列表。** "取不到邮件"和"没有未读邮件"
            # 是完全不同的两件事，压成后者会让故障伪装成安静。
            self._send(502, {"error": str(e)})
        except Exception as e:  # noqa: BLE001
            self._send(500, {"error": f"未预期的失败: {type(e).__name__}: {e}"})


def main() -> int:
    ap = argparse.ArgumentParser(description="YunXi Bot 只读邮件 sidecar")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--check", action="store_true", help="只检查配置与连通性，不起服务")
    ap.add_argument("--limit", type=int, default=5, help="--check 时试取几封")
    args = ap.parse_args()

    if args.check:
        try:
            cfg = load_config()
            print(f"配置   : {credentials_path()}")
            print(f"服务器 : {cfg['imap_host']}:{cfg['imap_port']}")
            print(f"账号   : {cfg['username']}")
            out = fetch_unread(cfg, args.limit, with_preview=False)
            print(f"未读   : {out['total_unseen']} 封（试取了 {len(out['items'])} 封的头）")
            print(f"只读命令: {', '.join(out['commands_sent'])}")
            for it in out["items"]:
                if "error" in it:
                    print(f"  ✗ uid={it['uid']} {it['error']}")
                else:
                    print(f"  · [{it['from_addr']}] {it['subject'][:40]} 直接={it['direct']}")
            return 0
        except MailError as e:
            print(f"失败: {e}", file=sys.stderr)
            return 2

    try:
        load_config()
    except MailError as e:
        # 起服务前先报一次：让"没配"在启动时就可见，而不是等第一次请求
        print(f"警告: {e}", file=sys.stderr)

    srv = ThreadingHTTPServer((LOOPBACK, args.port), Handler)
    print(f"邮件 sidecar 监听 http://{LOOPBACK}:{args.port}", flush=True)
    print(f"配置: {credentials_path()}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
