#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Laya sidecar 的**测试替身** —— 按脚本作答，绝不用于生产。

存在的理由
----------
真实的判断模型要装 `laya` 包并下载约 640MB 权重。没有它的时候：

- 决策层会降级成 fail-closed（不打扰），Agent 永远不开口；
- 于是"判断 → 约束收紧 → 表达 → 落台账"这条链路的**后半段无法被验证**，
  也无法在 CI 里跑。

本替身实现与 `laya_server.py` **完全相同的有线契约**，只是答案来自脚本，
这样就可以在没有模型的情况下端到端验证集成。

用法
----
  python sidecar/mock_laya.py --port 17870 --choice speak
  python sidecar/mock_laya.py --port 17870 --choice quiet --noul 0.1

⚠️ 它**不做任何判断**。别拿它的输出当决策依据。
"""
from __future__ import annotations

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_PORT = 17870
LOOPBACK = "127.0.0.1"

for _s in (sys.stderr, sys.stdout):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass

# 脚本化答案，由 main() 填入
SCRIPT: dict = {"choice": "quiet", "noul": 0.5}


class Handler(BaseHTTPRequestHandler):
    server_version = "yunxi-bot-mock-laya/0.1"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        sys.stderr.write("[mock-laya] %s\n" % (fmt % args).split("{")[0].strip())

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._send(200, {"status": "ok", "model": "mock（测试替身，不做判断）"})
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/decide":
            self._send(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length") or 0)
            req = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception as exc:  # noqa: BLE001
            self._send(400, {"error": f"bad json: {exc}"})
            return

        answers: dict = {}
        for q in req.get("questions") or []:
            kind = (q.get("kind") or {}).get("type")
            if kind == "choice":
                criteria = (q.get("kind") or {}).get("criteria") or {}
                # 脚本给的选项若不在声明范围内，退回第一个——替身也要守契约
                pick = SCRIPT["choice"]
                if criteria and pick not in criteria:
                    pick = next(iter(criteria))
                answers[q["id"]] = {"choice": pick}
            elif kind == "score":
                levels = (q.get("kind") or {}).get("levels") or []
                answers[q["id"]] = {"choice": levels[0] if levels else None}
            elif kind == "noul":
                answers[q["id"]] = {"noul": SCRIPT["noul"]}

        self._send(200, {"answers": answers, "model": "mock-laya"})


def main() -> int:
    ap = argparse.ArgumentParser(description="Laya sidecar 测试替身（不做真实判断）")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument(
        "--choice",
        default="quiet",
        help="intervention 问题的答案：speak / hold / quiet",
    )
    ap.add_argument("--noul", type=float, default=0.5, help="noul 问题的概率")
    args = ap.parse_args()

    SCRIPT["choice"] = args.choice
    SCRIPT["noul"] = args.noul

    srv = ThreadingHTTPServer((LOOPBACK, args.port), Handler)
    sys.stderr.write(
        f"[mock-laya] 测试替身监听 http://{LOOPBACK}:{args.port}"
        f"  choice={args.choice} noul={args.noul}\n"
        "[mock-laya] ⚠️ 它不做任何判断，只按脚本作答，别用于生产\n"
    )
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
