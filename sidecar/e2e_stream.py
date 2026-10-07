#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端验证「流式输出」——goal 第 5 条的第二半。

## 怎么证明"真的在流"

捕获下来的输出**看起来和不流式一模一样**——都是那段文字。
所以证据只能来自**时间**：

```text
首字节  <----- 1.2s ----->  结尾
        ↑                    ↑
   内容开始出现        最后一个字到达
```

不流式的话，首字节和结尾几乎同时到达（都等于整段生成完的时刻）。
流式的话，首字节要早得多——**而这个差值就是证据**。

## 还要验的三件事

1. **工具调用在流式下仍然拼得对。** 流式下 `tool_calls` 是按
   `index` 分片过来的，少拼一片就是坏 JSON——而那种坏法不报错，
   只会让工具收到一个解析不了的参数。
2. **`usage` 没丢。** 上下文锚点全靠它；流式如果拿不到
   `prompt_tokens`，压缩就退化成纯估算。
3. **不流式的路径没被影响。** 常驻循环、测试都不该为流式付代价。

跑：python sidecar/e2e_stream.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import (  # noqa: E402
    ensure_fresh_binary,
    home_untouched,
    real_home_dir,
    snapshot_home,
)


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

    with tempfile.TemporaryDirectory(prefix="yunxi-stream-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        shutil.copy(key, home / "secrets" / "agnes.key")
        work = home / "work"
        work.mkdir()
        (work / "notes.txt").write_text("甲\n乙\n丙\n", encoding="utf-8")

        env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        def run_timed(prompt: str, sid: str, timeout: int = 300):
            """跑一轮，记录**每一批字节**到达的时刻。

            **不能用 `readline()`。** Python 的 TextIOWrapper 会缓冲，
            于是所有行会等进程退出时一起交付——时间戳全被抹平，
            看起来像"一次性出来的"（第一版就是这么误报的）。
            按字节从文件描述符直接读，拿到的才是真实的到达时刻。
            """
            p = subprocess.Popen(
                [str(BIN), "chat", "--id", sid, "--thinking", "off"],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                env=env, cwd=str(work),
            )
            p.stdin.write((prompt + "\n/exit\n").encode("utf-8"))
            p.stdin.flush()
            p.stdin.close()

            t0 = time.monotonic()
            batches: list[tuple[float, str]] = []
            fd = p.stdout.fileno()
            while True:
                try:
                    buf = os.read(fd, 4096)
                except OSError:
                    break
                if not buf:
                    break
                batches.append((time.monotonic() - t0, buf.decode("utf-8", "replace")))
            p.wait(timeout=timeout)
            return batches, p.returncode

        # ================= 1. 流式证据 =================
        print("=== 首字节 vs 结尾（时间证据）===")
        # 要一段足够长的正文，生成时间才明显
        batches, rc = run_timed(
            "用大约 500 字详细介绍一下你能做什么，分成几个小节。", "s1"
        )
        check("退出码为 0", rc == 0, f"实际 {rc}")

        # 把字节批拼起来，同时记下每一批的到达时刻。
        # `› ` 提示符是不带换行打出来的，所以它会粘在下一行前面——
        # 第一版没考虑这点，一段正文都没抓到。
        #
        # **不再认模型名。** 这里原来找的是 `[agnes`，而路由标签
        # （`chat.rs` 打的 `  [{模型}·{思考|不思考}]`）里的模型名是会变的：
        # 本地槽位一接上就变成了 `[Qwen3-4B-Instruct-2507·不思考]`，
        # 于是这个循环**一个字符都不收**——而脚本只会报"没抓到正文"，
        # 看起来像流式坏了。所以改成认**结构**：提示符 `›` 之后的第一行
        # 是路由标签（不管里面写着哪个模型），标签之后才是正文。
        body_times = []
        started = False
        pending = ""
        for t, chunk in batches:
            if not started:
                pending += chunk
                if "›" not in pending:
                    continue          # 命令还没回显/提示符还没出来
                after = pending.split("›", 1)[1]
                if "\n" not in after:
                    continue          # 路由标签那一行还没打完
                chunk = after.split("\n", 1)[1]
                pending = ""
                started = True
            if "›" in chunk:
                chunk = chunk.split("›", 1)[0]
                if chunk:
                    body_times.append((t, chunk))
                break
            if chunk:
                body_times.append((t, chunk))

        if not body_times:
            check("**正文到达了**", False, "没抓到正文")
        else:
            # **只看非空批次。** 第一批往往只有提示符和路由标记
            # （正文还没生成），把它算作"首字节"会让时间证据变得含糊：
            # 那个 0.02s 是终端回显的时刻，不是模型吐出第一个字的时刻。
            real = [(t, x) for t, x in body_times if x.strip()]
            first_t, first_text = real[0]
            last_t, _ = real[-1]
            total = sum(len(x) for _, x in body_times)
            print(f"      正文分 {len(real)} 批到达 / 共 {total} 字")
            print(f"      首段 +{first_t:.2f}s：{first_text.strip()[:40]!r}")
            print(f"      末段 +{last_t:.2f}s")
            print(f"      首末差 {last_t - first_t:.2f}s")

            check("**正文是分多批到达的（真流式）**",
                  len(real) >= 3,
                  f"只到了 {len(real)} 批——不流式的话只有 1 批")
            check("**首字节明显早于结尾**",
                  last_t - first_t > 0.3,
                  f"首末差 {last_t - first_t:.2f}s（不流式的话这个差接近 0）")

        # ================= 2. 工具调用在流式下拼得对 =================
        print()
        print("=== 流式下的工具调用 ===")
        batches2, rc2 = run_timed(
            "读一下 notes.txt 的第一行是什么，只回复那一行。", "s2"
        )
        text2 = "".join(x for _, x in batches2)
        check("退出码为 0", rc2 == 0, f"实际 {rc2}")

        # 台账里该有一次成功的 read_file
        ledger = home / "ledger.jsonl"
        calls = []
        if ledger.exists():
            for line in ledger.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("kind") == "tool_called":
                    calls.append(e.get("data") or {})
        reads = [c for c in calls if c.get("tool") == "read_file"]
        check("**工具真的被调用了**", len(reads) >= 1,
              f"台账里 {len(calls)} 次调用，读文件 {len(reads)} 次")
        if reads:
            check("**工具调用成功了（分片拼装没坏）**",
                  reads[0].get("executed") is True,
                  f"outcome={reads[0].get('outcome')}；坏 JSON 会表现为参数解析失败")
        check("回答里说出了文件内容", "甲" in text2,
              "说明工具结果回到了模型")

        # ================= 3. usage 没丢 =================
        print()
        print("=== usage 没丢（上下文锚点的来源）===")
        model_events = []
        if ledger.exists():
            for line in ledger.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("kind") == "model_called":
                    model_events.append(e.get("data") or {})
        # `usage` 是**嵌套**的：`data.usage.prompt_tokens`。
        # 第一版查的是顶层 `prompt_tokens`，于是永远是 0——
        # 差一点就去查"流式是不是丢了 usage"。
        with_tokens = [
            m for m in model_events
            if ((m.get("usage") or {}).get("prompt_tokens") or 0) > 0
        ]
        check("**有调用报告了 prompt_tokens**", len(with_tokens) >= 1,
              f"{len(model_events)} 次调用，其中 {len(with_tokens)} 次带 token 数")
        if with_tokens:
            # `usage` 是**嵌套**的：`data.usage.prompt_tokens`。
            # 查顶层永远是 0——而那会让人误以为"流式把 usage 丢了"
            _u = with_tokens[-1].get("usage") or {}
            print(f"      例如 prompt_tokens={_u.get('prompt_tokens')}"
                  f"、completion_tokens={_u.get('completion_tokens')}")
            print("      （流式带了 stream_options.include_usage）")

        # ================= 4. 不流式的路径没被影响 =================
        print()
        print("=== 不流式的路径（常驻/测试）===")
        # 连跑十一个脚本之后限流是常态——重试一次，
        # **但失败时要把它的原话打出来**：一个分不清"限流"和"真坏了"
        # 的检查比没有检查更糟，因为它会让人以为验过了。
        r = None
        for _ in range(2):
            r = subprocess.run(
                [str(BIN), "think", "说「好」", "--thinking", "off"],
                capture_output=True, text=True, encoding="utf-8",
                env=env, timeout=200, cwd=str(work),
            )
            if r.returncode == 0:
                break
            time.sleep(3)
        out4 = (r.stdout or "") + (r.stderr or "")
        if r.returncode != 0:
            print("      --- think 的原话 ---")
            for line in out4.strip().splitlines()[-6:]:
                print(f"      {line}")
            print("      --------------------")
        check(
            "think 命令仍然可用（不流式的路径没被影响）",
            r.returncode == 0,
            f"退出码 {r.returncode}——看上面的原话判断是限流还是真坏了",
        )

    # **隔离断言：真实 home 一个字节都不该动。**
    # 这条以前不存在——`real_home` 只被用来借密钥，没有任何东西证明
    # 测试没写回真实数据（见 `e2e_common.home_untouched`）。
    check("**没有碰真实 home**", *home_untouched(before_home, real_home))

    print()
    print("流式输出端到端：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
