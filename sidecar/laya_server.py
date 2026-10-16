#!/usr/bin/env python3
"""本地 Laya 多语言决策服务；保留 Rust /decide 契约，失败一律显式降级。

先运行 scripts/fetch_model.py。服务只读取本地权重，不在请求期间联网下载。
概率未经 YunXi 中文数据校准；只提供选项建议，不设置概率放行阈值。
"""
from __future__ import annotations

import argparse
import json
import math
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scripts.fetch_model import default_home, load_manifest

DEFAULT_PORT = 17870
LOOPBACK = "127.0.0.1"
MAX_BODY_BYTES = 1 << 20
_router = None
_router_error: str | None = None
_router_lock = threading.Lock()
_loaded_model = "unloaded"
_device = None


def load_router(model: str, device: str = "auto") -> None:
    """直接加载指定底座，避免 Router 自动分流到另一个语言模型。"""
    global _router, _router_error, _loaded_model, _device
    with _router_lock:
        if _router is not None or _router_error is not None:
            return
        try:
            entry = next((e for e in load_manifest()["models"] if e["name"] == model), None)
            path = default_home() / "models" / entry["dir"] if entry else Path(model)
            if not path.is_dir():
                raise FileNotFoundError("先运行 scripts/fetch_model.py 下载本地权重")
            import torch
            from laya import load

            torch.set_num_threads(4)
            agent = load(str(path.resolve()), device=None if device == "auto" else device,
                         expected_sha256=entry.get("sha256_files") if entry else None)
            if device != "auto" and str(agent.device) != device:
                raise RuntimeError("请求的设备不可用；拒绝静默切换设备")
            # 首次 Metal 编译在就绪前完成，避免第一个真实决策耗尽超时预算。
            agent.system_one("中文服务预热", {"ready": {"type": "noul", "instructions": "这是中文吗？"}})
            if agent.cpu_fallback_count:
                raise RuntimeError("预热发生 CPU 回退，请显式使用 --device cpu")
            _loaded_model, _device = model, str(agent.device)
            _router = agent
            sys.stderr.write(f"[laya-server] 已加载 {_loaded_model}，device={_device}，calibrated=false\n")
        except Exception as exc:
            # 异常文本可能包含路径、模型输入或凭证，仅记录异常类别。
            _router_error = f"{type(exc).__name__}: 本地模型加载失败；检查依赖、权重和设备"
            sys.stderr.write(f"[laya-server] degraded: {_router_error}\n")


def to_laya_questions(questions: list[dict]) -> dict:
    if not isinstance(questions, list) or not questions:
        raise ValueError("questions must be a nonempty list")
    out = {}
    for q in questions:
        qid, kind = q["id"], q["kind"]
        if not isinstance(qid, str) or not qid or qid in out:
            raise ValueError("invalid or duplicate question id")
        t = kind["type"]
        entry = {"type": t, "instructions": q.get("instructions", "")}
        if not isinstance(entry["instructions"], str):
            raise ValueError("invalid instructions")
        if t == "choice":
            criteria = kind["criteria"]
            if not isinstance(criteria, dict) or not criteria or not all(
                isinstance(k, str) and isinstance(v, str) for k, v in criteria.items()
            ):
                raise ValueError("invalid criteria")
            entry["criteria"] = criteria
        elif t == "score":
            levels = kind["levels"]
            if not isinstance(levels, list) or not levels or not all(isinstance(v, str) for v in levels) or len(set(levels)) != len(levels):
                raise ValueError("invalid levels")
            entry["criteria"] = levels
        elif t != "noul":
            raise ValueError("unknown question type")
        out[qid] = entry
    return out


def probability(value):
    if type(value) not in (int, float) or not math.isfinite(value) or not 0 <= value <= 1:
        raise ValueError("invalid probability")
    return float(value)


def normalize_answers(raw: dict, questions: list[dict]) -> dict:
    answers = {}
    for q in questions:
        a = raw[q["id"]]
        if (not isinstance(a, dict) or a.get("abstention") not in (None, "passed")
                or a.get("abstained") or a.get("abstain") or a.get("low_confidence")):
            raise ValueError("missing or abstained answer")
        kind = q["kind"]
        if kind["type"] == "noul":
            out = {"noul": probability(a["noul"])}
        else:
            keys = kind["criteria"] if kind["type"] == "choice" else [str(i) for i in range(len(kind["levels"]))]
            probs = a.get("probabilities", {})
            if not isinstance(probs, dict) or set(probs) != set(keys):
                raise ValueError("missing or unknown probability labels")
            dist = {k: probability(v) for k, v in probs.items()}
            if abs(sum(dist.values()) - 1) > .05:
                raise ValueError("invalid probability sum")
            if kind["type"] == "choice":
                choice = a.get("choice")
                if choice not in keys:
                    raise ValueError("unknown choice")
                out = {"choice": choice, "distribution": dist}
            else:
                levels = kind["levels"]
                score = a["score"]
                if type(score) not in (int, float) or not math.isfinite(score) or not 0 <= score <= len(levels) - 1:
                    raise ValueError("invalid score")
                dist = {levels[int(k)]: v for k, v in dist.items()}
                out = {"choice": max(dist, key=dist.get), "distribution": dist, "expected_score": score}
        answers[q["id"]] = out
    return answers


def predict(state, questions):
    laya_q = to_laya_questions(questions)
    with _router_lock:
        before = _router.cpu_fallback_count
        raw = _router.system_one(state, laya_q)
        usage = raw.get("usage", {})
        if (_router.cpu_fallback_count != before or usage.get("truncated")
                or usage.get("options") or raw.get("abstained")):
            raise ValueError("model fallback, truncated input, collapsed options or abstention")
        return normalize_answers(raw["answers"], questions)


class Handler(BaseHTTPRequestHandler):
    server_version = "yunxi-bot-laya/0.2"

    def setup(self):
        super().setup()
        self.connection.settimeout(5)

    def log_message(self, fmt, *args):
        pass  # 请求路径或异常可能含私密输入，不记录请求正文。

    def _send(self, code, payload):
        body = json.dumps(payload, ensure_ascii=False, allow_nan=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path != "/health":
            self._send(404, {"error": "not found"})
            return
        self._send(200, {"status": "ok" if _router is not None else "degraded",
                         "model": _loaded_model, "device": _device, "calibrated": False,
                         "detail": _router_error or ("" if _router else "模型加载中")})

    def do_POST(self):
        if self.path != "/decide":
            self._send(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length") or 0)
            if not 0 < length <= MAX_BODY_BYTES:
                self._send(413, {"error": "body size invalid"})
                return
            def reject_constant(value):
                raise ValueError("nonfinite JSON")
            req = json.loads(self.rfile.read(length), parse_constant=reject_constant)
            state, questions = req["state"], req["questions"]
            to_laya_questions(questions)
        except (ValueError, KeyError, TypeError, TimeoutError):
            self._send(400, {"error": "invalid request"})
            return
        if _router is None:
            self._send(503, {"error": "model unavailable", "degraded": True,
                             "detail": _router_error or "模型加载中"})
            return
        try:
            answers = predict(state, questions)
            self._send(200, {"answers": answers, "model": _loaded_model,
                             "calibrated": False, "device": _device})
        except Exception as exc:
            self._send(503, {"error": "model decision unavailable", "degraded": True,
                             "detail": type(exc).__name__})


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--model", default="laya-multilingual", help="清单模型名或本地 checkpoint 目录")
    ap.add_argument("--device", choices=("auto", "cpu", "mps", "cuda"), default="auto")
    ap.add_argument("--host", choices=("127.0.0.1", "localhost", "::1"), default=LOOPBACK)
    args = ap.parse_args()
    class Server(HTTPServer):
        address_family = socket.AF_INET6 if args.host == "::1" else socket.AF_INET
    # 单个编码器串行处理，避免 ThreadingHTTPServer 持续负载下耗尽线程。
    srv = Server((args.host, args.port), Handler)
    threading.Thread(target=load_router, args=(args.model, args.device), daemon=True).start()
    sys.stderr.write(f"[laya-server] 监听 http://{args.host}:{args.port}；加载状态见 /health\n")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
