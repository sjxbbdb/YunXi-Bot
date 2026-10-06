# -*- coding: utf-8 -*-
"""压力测试（二）：故障注入——坏 key / 侧车不可达 / 侧车中途挂掉。

跑它：python sidecar/stress_faults.py

## 验的是什么

不是"坏了能不能恢复"，而是**坏了的时候它会不会悄悄做错事**。
判据按危险程度排：

| 危险 | 表现 | 为什么最坏 |
|---|---|---|
| 最坏 | **静默错**：报成功而实际没做 | 整条链路会在错误前提上继续跑 |
| 次坏 | **获得新权限**：降级时放宽了门禁 | 无人值守时等于门没了 |
| 次坏 | **乱通知**：拿不准还去打扰人 | 陪伴型助理的信任基础 |
| 可接受 | 报错清楚、什么都不做 | 安全失败 |

所以每一条断言都在问："它有没有往危险的方向偏？"

## 怎么注入

- **坏 key**：`YUNXI_BOT_AGNES_KEY` 环境变量覆盖（代码里写了这条覆盖路径）
- **侧车不可达**：把 `--port` 指到一个没人监听的端口
- **中途挂掉**：先跑一轮，杀了侧车，再跑一轮

跑完都看台账，确认"发生过什么"留了痕。
"""
from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from e2e_common import ensure_fresh_binary  # noqa: E402


def free_port() -> int:
    """找一个**没人监听**的端口——用它模拟「侧车没起来」。"""
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


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

    with tempfile.TemporaryDirectory(prefix="yunxi-fault-") as tmp:
        home = Path(tmp)
        (home / "secrets").mkdir(parents=True, exist_ok=True)
        for n in ("agnes.key", "deepseek.key", "bocha.key", "mail.json"):
            src = real_home / "secrets" / n
            if src.exists():
                shutil.copy(src, home / "secrets" / n)
        work = home / "work"
        work.mkdir()
        base_env = dict(os.environ, YUNXI_BOT_HOME=str(home), PYTHONIOENCODING="utf-8")

        # ================= 一、坏 API key =================
        print("=== 故障注入 1：坏 API key ===")
        bad_env = dict(base_env, YUNXI_BOT_AGNES_KEY="sk-this-key-is-definitely-wrong")
        r = subprocess.run(
            [str(BIN), "think", "说「好」", "--thinking", "off"],
            capture_output=True, text=True, encoding="utf-8",
            env=bad_env, timeout=300, cwd=str(work),
        )
        out = (r.stdout or "") + (r.stderr or "")
        print("\n".join(out.strip().splitlines()[-4:]))

        check("**坏 key 时非零退出**", r.returncode != 0,
              f"退出码 {r.returncode}——报成功了才危险")
        # **最关键的一条**：不许编一个看起来像回复的东西
        check(
            "**没有编出一个回复**",
            "--- 回复 ---" not in out,
            "打出了回复段，说明它在没有有效 key 的情况下给出了内容",
        )
        says_why = any(
            x in out for x in ["密钥", "key", "认证", "401", "Auth", "拒绝", "无效", "失败"]
        )
        check("说清了是密钥/认证的问题", says_why, "报错没指出原因，人不知道去改什么")

        # ================= 二、侧车不可达 =================
        print()
        print("=== 故障注入 2：邮件侧车不可达 ===")
        dead = free_port()
        r = subprocess.run(
            [str(BIN), "check", "--dry-run", "--port", str(dead)],
            capture_output=True, text=True, encoding="utf-8",
            env=base_env, timeout=600, cwd=str(work),
        )
        out = (r.stdout or "") + (r.stderr or "")
        print("\n".join(out.strip().splitlines()[-6:]))

        # **侧车连不上 ≠ 通知成功。**
        check(
            "**没有报告「通知了」**",
            "通知 " not in out or "通知 0" in out,
            "侧车都连不上却报有通知——那是静默错",
        )
        check(
            "**如实说了连不上**",
            any(x in out for x in ["连不上", "不可用", "拒绝", "sidecar", "侧车"]),
            "没说是侧车的问题，人只会看到「什么都没发生」",
        )
        check(
            "说了怎么起来",
            "mail_server" in out or "17871" in out or "启动" in out,
            "不告诉人下一步做什么，等于把问题丢回去",
        )

        # ================= 三、侧车中途挂掉 =================
        print()
        print("=== 故障注入 3：侧车在两次运行之间挂掉 ===")
        # 先起来
        side = subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                          "mail_server.py"),
             "--port", "17899"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            env=dict(base_env, YUNXI_BOT_HOME=str(home)),
        )
        try:
            import time
            time.sleep(3)
            r1 = subprocess.run(
                [str(BIN), "check", "--dry-run", "--port", "17899"],
                capture_output=True, text=True, encoding="utf-8",
                env=base_env, timeout=600, cwd=str(work),
            )
            up = (r1.stdout or "") + (r1.stderr or "")
            got_mail = "未读总数" in up or "本次判定" in up
            check("侧车活着时能取到邮件", got_mail,
                  "起不来就没法验后半段——这是前置条件")

            # 杀掉侧车
            side.kill()
            side.wait(timeout=30)
            time.sleep(1)

            r2 = subprocess.run(
                [str(BIN), "check", "--dry-run", "--port", "17899"],
                capture_output=True, text=True, encoding="utf-8",
                env=base_env, timeout=600, cwd=str(work),
            )
            down = (r2.stdout or "") + (r2.stderr or "")
            print("\n".join(down.strip().splitlines()[-4:]))
            check(
                "**侧车挂了之后如实报，不假装没事**",
                any(x in down for x in ["连不上", "不可用", "拒绝", "sidecar", "侧车"]),
                "侧车已经死了却什么都没有说",
            )
            check(
                "**没有假装取到了邮件**",
                "未读总数" not in down or "本次判定" in down,
                "侧车死了还报出未读数——那是编的",
            )
        finally:
            if side.poll() is None:
                side.kill()

        # ================= 台账留痕 =================
        print()
        print("=== 台账留痕 ===")
        ledger = home / "ledger.jsonl"
        events = {}
        if ledger.exists():
            for line in ledger.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                k = e.get("kind")
                events[k] = events.get(k, 0) + 1
        print(f"      台账事件统计：{events}")
        check("台账存在且有记录", bool(events), "什么都没留痕")

    print()
    print("故障注入压力测试：" + ("全部通过" if ok else "**有失败项**"))
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
