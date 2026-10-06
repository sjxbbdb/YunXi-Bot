#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""`mail_server.py` 的测试。

**全程不发真实网络请求。** 用一个假的 IMAP 连接替换 `imaplib.IMAP4_SSL`，
于是"只读"这件事可以被机器检查，而不是靠注释里的承诺。
这跟 Rust 侧"测试绝不出网"是同一条纪律：出网的测试在断网机器上跑不过，
随后会被标 ignore，最后等于没有测试。

跑：python -m pytest sidecar/test_mail_server.py -q
或：python sidecar/test_mail_server.py
"""
from __future__ import annotations

import email
import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import mail_server as ms  # noqa: E402


# --------------------------------------------------------------------------
# 假的 IMAP 连接：记录每一条发出的命令
# --------------------------------------------------------------------------


class FakeIMAP:
    """按 UID 返回预置邮件。**记录所有命令**，供"只读"断言检查。"""

    def __init__(self, messages: dict[bytes, bytes], unseen: list[bytes]):
        self.messages = messages
        self.unseen = unseen
        self.commands: list[tuple] = []
        self.select_readonly: bool | None = None
        self.logged_out = False

    def login(self, user, password):
        self.commands.append(("LOGIN", user))
        if password == "wrong":
            import imaplib

            raise imaplib.IMAP4.error("LOGIN failed")
        return ("OK", [b"logged in"])

    def select(self, mailbox, readonly=False):
        self.commands.append(("SELECT", mailbox, readonly))
        self.select_readonly = readonly
        return ("OK", [str(len(self.messages)).encode()])

    def uid(self, cmd, *args):
        self.commands.append(("UID", cmd, *args))
        cmd_u = cmd.upper()
        if cmd_u == "SEARCH":
            return ("OK", [b" ".join(self.unseen)])
        if cmd_u == "FETCH":
            uid = args[0] if args else b""
            want = args[1] if len(args) > 1 else ""
            raw = self.messages.get(uid)
            if raw is None:
                return ("OK", [None])
            if "HEADER" in want:
                # 只回头部，模拟真实行为
                msg = email.message_from_bytes(raw)
                head = b"".join(
                    f"{k}: {v}\r\n".encode() for k, v in msg.items()
                ) + b"\r\n"
                return ("OK", [(b"1 (BODY[HEADER]", head + b")")])
            return ("OK", [(b"1 (BODY[]", raw + b")")])
        return ("OK", [None])

    def logout(self):
        self.commands.append(("LOGOUT",))
        self.logged_out = True
        return ("BYE", [b"bye"])

    def noop(self):
        self.commands.append(("NOOP",))
        return ("OK", [b"ok"])


def make_message(subject: str, frm: str = "someone@example.com",
                 to: str = "me@example.com", cc: str = "",
                 body: str = "正文内容",
                 charset: str = "utf-8",
                 html: bool = False,
                 attachment: bool = False) -> bytes:
    """造一封测试邮件。

    `set_content` 传 **str** 并指定 charset——传 bytes 时它不接受 charset 参数
    （那是 set_bytes_content 的签名），踩过一次。
    """
    msg = email.message.EmailMessage()
    msg["Subject"] = subject
    msg["From"] = frm
    msg["To"] = to
    if cc:
        msg["Cc"] = cc
    msg["Date"] = "Mon, 05 Oct 2026 10:00:00 +0800"
    msg.set_content(body, subtype="html" if html else "plain", charset=charset)
    if attachment:
        msg.add_attachment(b"data", maintype="application", subtype="pdf",
                           filename="a.pdf")
    return msg.as_bytes()


def with_fake(imap: FakeIMAP):
    """把 imaplib.IMAP4_SSL 换成返回假连接。"""
    return mock.patch.object(ms.imaplib, "IMAP4_SSL", lambda *a, **k: imap)


CFG = {"imap_host": "imap.example.com", "imap_port": 993,
       "username": "me@example.com", "password": "secret", "mailbox": "INBOX"}


# --------------------------------------------------------------------------
# 只读保证：这一组是整个文件里最重要的
# --------------------------------------------------------------------------


class TestReadOnlyGuarantee(unittest.TestCase):
    def test_select_is_read_only_at_the_call_level(self):
        """在**方法调用**这一层，选择必须是只读的。

        「线上真的发了 EXAMINE」这件事由真实协议测试负责
        （见 `TestAgainstRealImapProtocol.test_examine_reaches_the_wire`）——
        假对象看不见线上，只看得见方法调用。
        两边各管一段，不重复也不留空。
        """
        imap = FakeIMAP({}, [])
        with with_fake(imap):
            with ms.Mailbox(CFG) as mb:
                mb.select("INBOX")
        self.assertIs(imap.select_readonly, True, "选择必须是只读的")
        # 审计日志里出现的是**线上真实发出的命令名**，不是包装层的名字
        self.assertIn("EXAMINE", mb.sent_commands, "审计该记 EXAMINE")
        self.assertNotIn("SELECT", mb.sent_commands, "不该记成读模式的 SELECT")

    def test_no_state_changing_command_is_ever_sent(self):
        """列举未读的整条路径上，**一条会改状态的命令都不能出现**。"""
        msgs = {b"1": make_message("s1"), b"2": make_message("s2")}
        imap = FakeIMAP(msgs, [b"1", b"2"])
        with with_fake(imap):
            with ms.Mailbox(CFG) as mb:
                mb.select("INBOX")
                for uid in mb.unread_uids(10):
                    mb.headers(uid)
                    mb.message(uid)

        sent = {c[0].upper() for c in imap.commands}
        forbidden = sent & ms.FORBIDDEN_COMMANDS
        self.assertEqual(forbidden, set(), f"发了会改状态的命令: {forbidden}")

        # UID FETCH 的子命令也要检查
        sub = {c[1].upper() for c in imap.commands if c[0] == "UID"}
        self.assertEqual(sub & ms.FORBIDDEN_COMMANDS, set())

    def test_body_is_fetched_with_peek_not_plain(self):
        """**这是最容易踩的坑。**

        `BODY[]` 会顺带把邮件标成已读，也就等于改动了使用者的邮箱状态。
        一个"只读"的信息源把用户所有未读邮件变成已读，比读不到更糟。
        """
        imap = FakeIMAP({b"1": make_message("s")}, [b"1"])
        with with_fake(imap):
            with ms.Mailbox(CFG) as mb:
                mb.select("INBOX")
                mb.headers(b"1")
                mb.message(b"1")

        fetches = [c for c in imap.commands if c[0] == "UID" and c[1].upper() == "FETCH"]
        self.assertTrue(fetches, "应该至少有两次 FETCH")
        for f in fetches:
            # 假连接记的是 ("UID", "FETCH", uid, spec)，spec 在第 4 位
            spec = str(f[3])
            self.assertIn("PEEK", spec, f"FETCH 没用 PEEK，会标记已读: {spec}")
            self.assertNotIn(
                "BODY[]", spec.replace("BODY.PEEK[]", ""),
                f"出现了裸 BODY[]: {spec}",
            )

    def test_whitelist_rejects_unknown_commands(self):
        """白名单是"只读"的可执行定义。不在名单上的命令一律拒绝。"""
        imap = FakeIMAP({}, [])
        with with_fake(imap):
            with ms.Mailbox(CFG) as mb:
                with self.assertRaises(ms.MailError) as ctx:
                    mb._cmd("STORE", b"1", "+FLAGS", "(\\Seen)")
                self.assertIn("改状态", str(ctx.exception))

    def test_whitelist_and_forbidden_do_not_overlap(self):
        """两个集合不能有交集——有交集说明定义自相矛盾。"""
        self.assertEqual(ms.READONLY_COMMANDS & ms.FORBIDDEN_COMMANDS, set())

    def test_forbidden_commands_are_the_ones_that_change_state(self):
        """把"哪些动作会改状态"写死成断言。

        将来有人往白名单里加命令时，这条会提醒他先想清楚。
        """
        for cmd in ("STORE", "APPEND", "COPY", "MOVE", "EXPUNGE", "DELETE"):
            self.assertIn(cmd, ms.FORBIDDEN_COMMANDS, f"{cmd} 会改状态，应在禁止名单里")


# --------------------------------------------------------------------------
# 头部解码
# --------------------------------------------------------------------------


class TestHeaderDecoding(unittest.TestCase):
    def test_gbk_subject_decodes(self):
        """中文邮件头十有八九不是 UTF-8。整个当 UTF-8 解会全是问号。"""
        raw = "=?GBK?B?xOO6ww==?="  # "你好" 的 GBK base64
        self.assertEqual(ms._decode(raw), "你好")

    def test_utf8_subject_decodes(self):
        raw = "=?UTF-8?B?5L2g5aW9?="
        self.assertEqual(ms._decode(raw), "你好")

    def test_plain_subject_passes_through(self):
        self.assertEqual(ms._decode("Hello"), "Hello")

    def test_none_and_empty_are_safe(self):
        self.assertEqual(ms._decode(None), "")
        self.assertEqual(ms._decode(""), "")

    def test_broken_header_does_not_raise(self):
        """头坏了不该让整封邮件读不出来。"""
        self.assertIsInstance(ms._decode("=?NOPE?B?!!!?="), str)

    def test_address_list_splits_name_and_addr(self):
        out = ms._addr_list("张三 <a@x.com>, b@y.com")
        self.assertEqual(out[0][1], "a@x.com")
        self.assertEqual(out[1][1], "b@y.com")

    def test_date_parses_to_millis(self):
        ms_ = ms._parse_date_ms("Mon, 05 Oct 2026 10:00:00 +0800")
        # 2026-10-05 10:00 +0800 = 02:00 UTC
        self.assertEqual(ms_, 1791165600000)

    def test_bad_date_is_zero_not_now(self):
        """**日期坏掉返回 0 而不是"现在"。**

        用假数据填补比留空更糟：一封日期坏掉的邮件会被当成刚收到，
        从而插到通知队列最前面。
        """
        self.assertEqual(ms._parse_date_ms("完全不是日期"), 0)
        self.assertEqual(ms._parse_date_ms(None), 0)
        self.assertEqual(ms._parse_date_ms(""), 0)


# --------------------------------------------------------------------------
# 正文抽取
# --------------------------------------------------------------------------


class TestBodyExtraction(unittest.TestCase):
    def test_plain_text_body(self):
        msg = email.message_from_bytes(make_message("s", body="你好世界"))
        self.assertIn("你好世界", ms._text_body(msg, 100))

    def test_gbk_body_decodes(self):
        """GB2312 正文要按它声明的编码解，不能当 UTF-8。"""
        msg = email.message_from_bytes(make_message("s", body="中文正文", charset="gb2312"))
        self.assertIn("中文正文", ms._text_body(msg, 100))

    def test_html_is_stripped(self):
        msg = email.message_from_bytes(
            make_message("s", body="<p>正文</p>", html=True)
        )
        out = ms._text_body(msg, 200)
        self.assertNotIn("<p>", out)
        self.assertIn("正文", out)

    def test_truncation_is_by_chars_not_bytes(self):
        """按字节切会落在汉字中间。Python 里是乱码，Rust 里是 panic。"""
        body = "中" * 500
        msg = email.message_from_bytes(make_message("s", body=body))
        out = ms._text_body(msg, 100)
        self.assertEqual(len(out), 100)
        # 每个字符都必须是完整的"中"
        self.assertEqual(set(out), {"中"})

    def test_unknown_charset_falls_back_instead_of_erroring(self):
        """声明了 Python 不认识的 charset。

        退到 UTF-8 + replace，而不是抛错——能读到一半比读不到强，
        而且替换字符会让"这里有东西没解出来"变得可见。
        """
        raw = make_message("s", body="content")
        raw = raw.replace(b"charset=\"utf-8\"", b"charset=\"x-nonsense-9000\"")
        msg = email.message_from_bytes(raw)
        self.assertIsInstance(ms._text_body(msg, 100), str)

    def test_attachment_is_not_treated_as_body(self):
        msg = email.message_from_bytes(make_message("s", body="真正的正文", attachment=True))
        out = ms._text_body(msg, 500)
        self.assertIn("真正的正文", out)
        self.assertNotIn("data", out.replace("真正的正文", ""))

    def test_consecutive_blank_lines_are_collapsed(self):
        msg = email.message_from_bytes(make_message("s", body="a\n\n\n\n\nb"))
        self.assertNotIn("\n\n\n", ms._text_body(msg, 500))


# --------------------------------------------------------------------------
# 汇总出来的字段
# --------------------------------------------------------------------------


class TestSummarize(unittest.TestCase):
    def _sum(self, **kw):
        raw = make_message(**kw)
        return ms.Mailbox._summarize(email.message_from_bytes(raw), b"7")

    def test_direct_when_only_me(self):
        s = self._sum(subject="x", to="me@example.com")
        self.assertTrue(s["direct"])
        self.assertEqual(s["recipient_count"], 1)

    def test_not_direct_when_others_are_copied(self):
        """群发/列表邮件是打扰判定里最该被压下去的一类。"""
        s = self._sum(subject="x", to="me@example.com", cc="a@x.com, b@x.com")
        self.assertFalse(s["direct"])
        self.assertEqual(s["recipient_count"], 3)

    def test_not_direct_when_many_recipients(self):
        s = self._sum(subject="x", to="a@x.com, b@x.com, me@example.com")
        self.assertFalse(s["direct"])

    def test_addressed_directly_differs_from_direct(self):
        """被放在 To 里（要你办）和只在 Cc 里（知会你）是两回事。"""
        in_to = self._sum(subject="x", to="me@example.com, other@x.com")
        self.assertTrue(in_to["addressed_directly"])
        self.assertFalse(in_to["direct"])

    def test_uid_is_carried_through(self):
        s = self._sum(subject="x")
        self.assertEqual(s["uid"], "7")

    def test_attachment_flag(self):
        self.assertTrue(self._sum(subject="x", attachment=True)["has_attachments"])
        self.assertFalse(self._sum(subject="x")["has_attachments"])


# --------------------------------------------------------------------------
# 配置
# --------------------------------------------------------------------------


class TestConfig(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(os.environ.get("TEMP", "/tmp")) / f"yunxi-mail-test-{os.getpid()}"
        (self.tmp / "secrets").mkdir(parents=True, exist_ok=True)

    def tearDown(self):
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)

    def _write(self, payload) -> None:
        (self.tmp / "secrets" / "mail.json").write_text(
            json.dumps(payload), encoding="utf-8"
        )

    def test_missing_file_says_what_to_create(self):
        with self.assertRaises(ms.MailError) as ctx:
            ms.load_config(self.tmp)
        self.assertIn("mail.json", str(ctx.exception))

    def test_missing_fields_are_named(self):
        self._write({"imap_host": "h"})
        with self.assertRaises(ms.MailError) as ctx:
            ms.load_config(self.tmp)
        msg = str(ctx.exception)
        self.assertIn("username", msg)
        self.assertIn("password", msg)

    def test_defaults_are_filled(self):
        self._write({"imap_host": "h", "username": "u", "password": "p"})
        cfg = ms.load_config(self.tmp)
        self.assertEqual(cfg["imap_port"], 993)
        self.assertEqual(cfg["mailbox"], "INBOX")

    def test_broken_json_is_reported_as_such(self):
        (self.tmp / "secrets" / "mail.json").write_text("{not json", encoding="utf-8")
        with self.assertRaises(ms.MailError) as ctx:
            ms.load_config(self.tmp)
        self.assertIn("JSON", str(ctx.exception))

    def test_home_matches_the_rust_side_rule(self):
        """两处必须一致，否则 sidecar 会去空目录找凭证，报出来却是"没配置"。"""
        with mock.patch.dict(os.environ, {"YUNXI_BOT_HOME": r"D:\custom"}, clear=False):
            self.assertEqual(ms.default_home(), Path(r"D:\custom"))

    def test_credentials_live_outside_the_repo(self):
        """凭证路径必须在 secrets/ 下——这是"仓库外"约定的可检查形式。"""
        p = ms.credentials_path(Path("C:/home"))
        self.assertEqual(p.parts[-2], "secrets")


# --------------------------------------------------------------------------
# 登录失败的信息
# --------------------------------------------------------------------------


class TestLoginErrors(unittest.TestCase):
    def test_auth_failure_mentions_the_authorization_code(self):
        """QQ/163 的 IMAP 用的是**授权码**不是登录密码。

        这条错误最容易误导人，失败时把这一点说出来，省掉一轮排查。
        """
        imap = FakeIMAP({}, [])
        bad = dict(CFG, password="wrong")
        with with_fake(imap):
            with self.assertRaises(ms.MailError) as ctx:
                with ms.Mailbox(bad):
                    pass
        msg = str(ctx.exception)
        self.assertIn("授权码", msg)

    def test_connection_failure_is_wrapped(self):
        with mock.patch.object(ms.imaplib, "IMAP4_SSL", side_effect=OSError("boom")):
            with self.assertRaises(ms.MailError) as ctx:
                with ms.Mailbox(CFG):
                    pass
        self.assertIn("连不上", str(ctx.exception))


# --------------------------------------------------------------------------
# 取未读
# --------------------------------------------------------------------------


class TestFetchUnread(unittest.TestCase):
    def test_returns_newest_first(self):
        """UID 递增，助理先看新的。"""
        msgs = {b"1": make_message("旧"), b"2": make_message("新")}
        imap = FakeIMAP(msgs, [b"1", b"2"])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 10, with_preview=False)
        self.assertEqual(out["items"][0]["uid"], "2")

    def test_limit_is_respected(self):
        msgs = {str(i).encode(): make_message(f"s{i}") for i in range(1, 11)}
        imap = FakeIMAP(msgs, [str(i).encode() for i in range(1, 11)])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 3, with_preview=False)
        self.assertEqual(len(out["items"]), 3)

    def test_total_unseen_comes_from_select(self):
        """总数来自 SELECT，不是"我取了几封"——列表被 limit 截断时这点很重要。"""
        msgs = {str(i).encode(): make_message(f"s{i}") for i in range(1, 6)}
        imap = FakeIMAP(msgs, [str(i).encode() for i in range(1, 6)])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 2, with_preview=False)
        self.assertEqual(out["total_unseen"], 5)
        self.assertEqual(len(out["items"]), 2)

    def test_one_broken_message_does_not_fail_the_list(self):
        """单封取不到不该让整次列举失败，但**跳过数要能被看见**。"""
        imap = FakeIMAP({b"1": make_message("ok")}, [b"1", b"99"])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 10, with_preview=False)
        self.assertEqual(out["skipped"], 1)
        self.assertTrue(any("error" in i for i in out["items"]))

    def test_commands_sent_are_reported(self):
        """把发过哪些命令回给调用方——"只读"不该只是我们内部知道。

        **记的是 `UID FETCH` 而不是 `FETCH`**：真实协议里读正文走的是
        `UID FETCH`，只记外层名字会让审计看不出到底做了什么。
        """
        imap = FakeIMAP({b"1": make_message("s")}, [b"1"])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 5, with_preview=False)
        self.assertIn("EXAMINE", out["commands_sent"])
        self.assertIn("UID FETCH", out["commands_sent"])
        self.assertNotIn("UID STORE", out["commands_sent"])
        for c in out["commands_sent"]:
            self.assertNotIn("STORE", c)
            self.assertNotIn("EXPUNGE", c)
            self.assertNotEqual(c, "SELECT", "不该出现读模式的 SELECT")

    def test_no_unread_is_an_empty_list_not_an_error(self):
        """"没有未读"是正常结果。"""
        imap = FakeIMAP({}, [])
        with with_fake(imap):
            out = ms.fetch_unread(CFG, 5, with_preview=False)
        self.assertEqual(out["items"], [])
        self.assertEqual(out["total_unseen"], 0)


class TestFetchOne(unittest.TestCase):
    def test_returns_body(self):
        imap = FakeIMAP({b"5": make_message("主题", body="正文")}, [b"5"])
        with with_fake(imap):
            out = ms.fetch_one(CFG, "5", 100)
        self.assertIn("正文", out["body"])
        self.assertEqual(out["uid"], "5")

    def test_missing_uid_is_a_clear_error(self):
        imap = FakeIMAP({}, [])
        with with_fake(imap):
            with self.assertRaises(ms.MailError) as ctx:
                ms.fetch_one(CFG, "404", 100)
        self.assertIn("404", str(ctx.exception))

    def test_reading_one_message_does_not_mark_it_read(self):
        """读正文这一步尤其危险——它正是 `BODY[]` 会标记已读的地方。"""
        imap = FakeIMAP({b"5": make_message("s")}, [b"5"])
        with with_fake(imap):
            ms.fetch_one(CFG, "5", 100)
        fetches = [c for c in imap.commands if c[0] == "UID" and c[1].upper() == "FETCH"]
        self.assertTrue(fetches, "应该至少有一次 FETCH")
        self.assertTrue(all("PEEK" in str(f[3]) for f in fetches))
        self.assertIs(imap.select_readonly, True)


# --------------------------------------------------------------------------
# 端到端：真的 TCP + 真的 IMAP 协议对话
# --------------------------------------------------------------------------


class TestAgainstRealImapProtocol(unittest.TestCase):
    """这一组跑**真的 socket、真的 IMAP 语法**。

    上面那些用假连接测的是"我们调了哪些方法"；这一组测的是
    **"我们说的那句话 IMAP 认不认"**——`UID SEARCH UNSEEN` 的参数顺序、
    `SELECT ... readonly` 的写法、`BODY.PEEK[HEADER]` 的括号空格。
    那类错误假连接永远测不出来，因为它们不解析语法。

    而最重要的一条：**「只读」由服务端的 `\\Seen` 标志验证**，
    不是靠我们自觉。假 IMAP 真的会按 `BODY[]` vs `BODY.PEEK[]` 的语义置位。
    """

    def test_full_unread_flow_over_real_protocol(self):
        from fake_imap import FakeImapServer

        msgs = {
            b"1": make_message("第一封", body="正文一"),
            b"2": make_message("第二封", body="正文二"),
        }
        with FakeImapServer(msgs, unseen=[b"1", b"2"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            out = ms.fetch_unread(cfg, 10, with_preview=False)

        self.assertEqual(out["total_unseen"], 2)
        self.assertEqual(len(out["items"]), 2)
        # UID 递增，最新的在前
        self.assertEqual(out["items"][0]["uid"], "2")

    def test_reading_does_not_mark_messages_as_seen(self):
        """**这是整个项目里最重要的一条只读断言。**

        用的是服务端的标志位，不是我们自己的说法：
        如果客户端发了裸 `BODY[...]`，假服务器会真的把 `\\Seen` 加上。
        """
        from fake_imap import FakeImapServer

        msgs = {b"1": make_message("未读邮件", body="正文")}
        with FakeImapServer(msgs, unseen=[b"1"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            before = srv.is_unseen(b"1")
            # 完整走一遍：列未读 + 取正文（最危险的那一步）
            ms.fetch_unread(cfg, 10, with_preview=False)
            ms.fetch_one(cfg, "1", 200)
            after = srv.is_unseen(b"1")
            marked = srv.was_marked_seen(b"1")

        self.assertTrue(before, "前提：这封邮件一开始是未读的")
        self.assertTrue(after, "**取完正文之后它必须还是未读的**")
        self.assertFalse(marked, "**服务端认为它被标成已读了——我们破坏了只读性**")

    def test_preview_path_also_does_not_mark_seen(self):
        """带 preview 的那条路会取整封正文，是最容易踩坑的地方。"""
        from fake_imap import FakeImapServer

        msgs = {b"7": make_message("有正文的邮件", body="内容在这里")}
        with FakeImapServer(msgs, unseen=[b"7"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            out = ms.fetch_unread(cfg, 10, with_preview=True)
            marked = srv.was_marked_seen(b"7")

        self.assertEqual(len(out["items"]), 1)
        self.assertIn("内容在这里", out["items"][0].get("preview", ""))
        self.assertFalse(marked, "带 preview 的路径把邮件标成已读了")

    def test_select_is_readonly_over_the_wire(self):
        """`SELECT ... readonly` 要真的发出去，而不是只在我们心里。"""
        from fake_imap import FakeImapServer

        with FakeImapServer({b"1": make_message("s")}, unseen=[b"1"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            ms.fetch_unread(cfg, 5, with_preview=False)
            readonly = srv.selected_readonly

        self.assertIs(readonly, True, "SELECT 没带只读标志")

    def test_examine_reaches_the_wire(self):
        """**线上真的发的是 `EXAMINE`。**

        这是假对象测不出来的那一段：`select(mailbox, True)` 在 imaplib 内部
        才会被翻译成 EXAMINE。而"服务端会拒绝写操作"这个保证，全靠线上
        那个词是不是 EXAMINE。

        这条断言是真实协议测试的价值所在——它替我们看住了线上。
        """
        from fake_imap import FakeImapServer

        with FakeImapServer({b"1": make_message("s")}, unseen=[b"1"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            ms.fetch_unread(cfg, 5, with_preview=False)
            joined = "\n".join(srv.commands)
            readonly = srv.selected_readonly

        self.assertIs(readonly, True, "服务端认为这次选择是只读的")
        self.assertIn("EXAMINE", joined, f"线上没发 EXAMINE:\n{joined}")
        # **不能有裸 SELECT**：那是读模式，之后写操作就是允许的
        self.assertNotRegex(joined, r"(?m)^\S+ SELECT ", "发了一次读模式的 SELECT")

    def test_imap_syntax_is_accepted_by_a_real_parser(self):
        """命令语法要被真服务端接受。

        假连接测不出这个：`UID SEARCH UNSEEN` 写成 `UID SEARCH UNSEEN ALL`
        在假连接里一样"成功"，在真服务端是语法错。
        """
        from fake_imap import FakeImapServer

        with FakeImapServer({b"1": make_message("s")}, unseen=[b"1"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            out = ms.fetch_unread(cfg, 5, with_preview=True)

        self.assertEqual(len(out["items"]), 1)
        # 服务端收到了这些命令，且没有 BAD
        joined = "\n".join(srv.commands)
        self.assertIn("UID SEARCH UNSEEN", joined)
        self.assertIn("UID FETCH", joined)
        self.assertNotIn("STORE", joined)

    def test_no_unread_over_real_protocol(self):
        from fake_imap import FakeImapServer

        with FakeImapServer({b"1": make_message("s")}, unseen=[]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            out = ms.fetch_unread(cfg, 5, with_preview=False)

        self.assertEqual(out["items"], [])
        self.assertEqual(out["total_unseen"], 1, "已读的邮件仍算在 SELECT 的总数里")

    def test_wrong_password_over_real_protocol(self):
        from fake_imap import FakeImapServer

        with FakeImapServer({}, unseen=[]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False, password="wrong")
            with self.assertRaises(ms.MailError) as ctx:
                ms.fetch_unread(cfg, 5, with_preview=False)
        self.assertIn("授权码", str(ctx.exception))

    def test_gbk_subject_survives_the_whole_chain(self):
        """中文主题从 IMAP 字节一路走到结构化字段。"""
        from fake_imap import FakeImapServer

        msgs = {b"1": make_message("这是中文主题", body="中文正文")}
        with FakeImapServer(msgs, unseen=[b"1"]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            out = ms.fetch_unread(cfg, 5, with_preview=True)

        self.assertEqual(out["items"][0]["subject"], "这是中文主题")
        self.assertIn("中文正文", out["items"][0]["preview"])


class TestPlaintextImapIsLoopbackOnly(unittest.TestCase):
    """明文 IMAP 只允许连本机。

    `use_ssl: false` 是为了能连本机邮件服务器、以及让整条链路可测。
    但邮箱密码明文过网是不可接受的——所以这条限制必须由程序强制，
    而不是靠使用者记得别这么配。
    """

    def test_non_loopback_without_ssl_is_refused(self):
        cfg = dict(CFG, imap_host="imap.qq.com", use_ssl=False)
        with self.assertRaises(ms.MailError) as ctx:
            with ms.Mailbox(cfg):
                pass
        msg = str(ctx.exception)
        self.assertIn("明文", msg)
        self.assertIn("imap.qq.com", msg)

    def test_loopback_without_ssl_is_allowed(self):
        from fake_imap import FakeImapServer

        with FakeImapServer({}, unseen=[]) as srv:
            cfg = dict(CFG, imap_host="127.0.0.1", imap_port=srv.port,
                       use_ssl=False)
            with ms.Mailbox(cfg) as mb:
                mb.select("INBOX")

    def test_loopback_detection_covers_the_usual_spellings(self):
        for h in ("127.0.0.1", "localhost", "LOCALHOST", "[::1]", "::1"):
            self.assertTrue(ms._is_loopback(h), f"{h} 应被认作本机")

    def test_loopback_detection_rejects_everything_else(self):
        # 含任何域名——不做 DNS 解析，所以"把域名指到 127.0.0.1"骗不过去
        for h in ("imap.qq.com", "example.com", "127.0.0.1.evil.com",
                  "10.0.0.1", "192.168.1.1", ""):
            self.assertFalse(ms._is_loopback(h), f"{h} 不该被认作本机")


if __name__ == "__main__":
    unittest.main(verbosity=2)