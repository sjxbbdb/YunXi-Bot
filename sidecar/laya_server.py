#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""YunXi Bot 决策模型 sidecar —— 把 Laya 类 System-1 模型包装成固定 JSON 契约。

为什么需要这一层
----------------
上游 `laya` 包的服务端 API 可能随版本变化。把差异关在这里，Rust 侧就只认一个
稳定契约，升级模型不必改 Rust 代码。

有线契约
--------
  POST /decide
  { "state": {...},
    "questions": [ {"id":"q1","kind":{"type":"noul"},"instructions":"..."} ] }

  → 200
  { "answers": { "q1": {"noul": 0.83} },
    "model": "laya-multilingual" }

  GET /health  → 200 {"status":"ok"|"degraded", "model":"...", "detail":"..."}

安全边界
--------
- 只绑回环地址。Rust 侧也会拒绝非回环 endpoint，这里是第二道。
- 不打印 state 内容：里面可能有私人信息。
- 模型不可用时返回明确错误码，让 Rust 侧走降级，而不是伪造一个答案。

用法
----
  python sidecar/laya_server.py --port 17870
  python sidecar/laya_server.py --port 17870 --model laya-multilingual
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_PORT = 17870
LOOPBACK = "127.0.0.1"
MAX_BODY_BYTES = 1 << 20  # 1 MiB

# Windows 上 Python 的 stderr 默认按控制台代码页编码，中文日志会变成乱码。
# 显式改成 UTF-8，保证日志在任何终端里都可读。
for _stream in (sys.stderr, sys.stdout):
    try:
        _stream.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass

# —— 模型加载（延迟且只尝试一次，失败不阻塞服务启动）——
_router = None
_router_error: str | None = None
_router_lock = threading.Lock()
_loaded_model = "unloaded"


def load_router(model: str) -> None:
    """尝试加载模型。失败时记录原因，让 /health 能如实报告。"""
    global _router, _router_error, _loaded_model
    with _router_lock:
        if _router is not None or _router_error is not None:
            return
        try:
            from laya import Router  # type: ignore

            _router = Router(default=model) if model else Router()
            _loaded_model = model or "default"
            sys.stderr.write(f"[laya-server] 模型已加载: {_loaded_model}\n")
        except Exception as exc:  # noqa: BLE001
            _router_error = f"{type(exc).__name__}: {exc}"
            sys.stderr.write(
                f"[laya-server] 模型加载失败，服务将以 degraded 状态运行: {_router_error}\n"
                "              安装：python -m pip install laya\n"
            )


def to_laya_questions(questions: list[dict]) -> dict:
    """把本项目的 question 形状翻译成上游 `Router.predict` 的形状。"""
    out: dict[str, dict] = {}
    for q in questions:
        kind = q.get("kind") or {}
        t = kind.get("type")
        entry: dict = {"instructions": q.get("instructions", "")}
        if t == "choice":
            entry["type"] = "choice"
            entry["criteria"] = kind.get("criteria") or {}
        elif t == "score":
            entry["type"] = "score"
            # 上游 score 用 criteria 表示有序等级
            entry["criteria"] = kind.get("levels") or []
        elif t == "noul":
            entry["type"] = "noul"
        else:
            raise ValueError(f"未知问题类型: {t!r}")
        out[q["id"]] = entry
    return out


def normalize_answers(raw: dict, questions: list[dict]) -> dict:
    """把上游答案归一成本项目的 Answer 形状，并做最小校验。"""
    answers: dict[str, dict] = {}
    for q in questions:
        qid = q["id"]
        a = (raw or {}).get(qid) or {}
        out: dict = {}
        if "choice" in a and a["choice"] is not None:
            out["choice"] = a["choice"]
        if "noul" in a and a["noul"] is not None:
            out["noul"] = float(a["noul"])
        if "distribution" in a and a["distribution"]:
            out["distribution"] = {k: float(v) for k, v in a["distribution"].items()}
        if "expected_score" in a and a["expected_score"] is not None:
            out["expected_score"] = float(a["expected_score"])
        answers[qid] = out
    return answers


class Handler(BaseHTTPRequestHandler):
    server_version = "yunxi-bot-laya/0.1"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        # 默认实现会把请求行写进 stderr，可能带上 state 片段；只记方法+路径
        sys.stderr.write("[laya-server] %s\n" % (fmt % args).split("{")[0].strip())

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/health":
            self._send(404, {"error": "not found"})
            return
        if _router is not None:
            self._send(200, {"status": "ok", "model": _loaded_model})
        else:
            self._send(
                200,
                {
                    "status": "degraded",
                    "model": _loaded_model,
                    "detail": _router_error or "模型尚未加载",
                },
            )

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/decide":
            self._send(404, {"error": "not found"})
            return

        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0 or length > MAX_BODY_BYTES:
            self._send(413, {"error": "body size invalid"})
            return

        try:
            req = json.loads(self.rfile.read(length).decode("utf-8"))
        except Exception as exc:  # noqa: BLE001
            self._send(400, {"error": f"bad json: {exc}"})
            return

        if _router is None:
            # 关键：模型不可用时返回明确错误，让 Rust 侧走降级，绝不伪造答案
            self._send(
                503,
                {"error": "model unavailable", "detail": _router_error or "未加载"},
            )
            return

        try:
            questions = req.get("questions") or []
            state = req.get("state") or {}
            laya_q = to_laya_questions(questions)
            raw = _router.predict(state, laya_q)
            answers = normalize_answers(raw.get("answers") or {}, questions)
            model = ((raw.get("routing") or {}).get("model")) or _loaded_model
            self._send(200, {"answers": answers, "model": model})
        except Exception as exc:  # noqa: BLE001
            # 不把 state 回显进错误信息
            self._send(500, {"error": f"{type(exc).__name__}: {exc}"})


def main() -> int:
    ap = argparse.ArgumentParser(description="YunXi Bot 决策模型 sidecar")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument(
        "--model",
        default="laya-multilingual",
        help="checkpoint 名。中文场景必须用 multilingual（默认上下文 1024 token）",
    )
    ap.add_argument("--host", default=LOOPBACK, help="只允许回环")
    args = ap.parse_args()

    if args.host not in ("127.0.0.1", "localhost", "::1"):
        sys.stderr.write("拒绝：决策模型的 state 可能含私人信息，只允许绑定回环地址\n")
        return 2

    # 后台加载，先让 /health 可用
    threading.Thread(target=load_router, args=(args.model,), daemon=True).start()

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    sys.stderr.write(f"[laya-server] 监听 http://{args.host}:{args.port}  /decide  /health\n")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        sys.stderr.write("\n[laya-server] 收到中断，退出\n")
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
