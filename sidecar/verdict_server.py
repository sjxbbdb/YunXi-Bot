#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""YunXi Bot 决策模型 sidecar —— 用 Verdict 实现我们的 `/decide` 契约。

为什么是 Verdict
----------------
选它的四条理由（详见 ADR D12）：

1. **选项顺序不变性**：双编码器独立打分，`choose` 的答案不随选项顺序改变。
   Laya 的翻转率高达 0.23——同样的情况换个问法给不同答案，这种模型不能用来
   决定"要不要打扰你"。
2. **校准诚实**：ECE 0.014–0.030（Laya 未校准时 0.466）。而且它把
   `calibrated` 标志如实报出来，不假装概率可信。
3. **保形弃权**：`calibrate()` 给出分布无关的覆盖率保证，正好满足
   ADR §7.3 第 5 条"不设未校准的阈值"。
4. **适配成本**：`fit()` 在笔记本 CPU 上 0.9 秒，每类 1 个样本起。
   零样本谁都做不了（三家全在 0.32–0.36），能快速重训才是关键。

线协议与 `laya_server.py` **完全一致**，所以 Rust 侧一行都不用改。

用法
----
  python sidecar/verdict_server.py --port 17870
  python sidecar/verdict_server.py --port 17870 --calibration data/decisions.verdict
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

DEFAULT_PORT = 17870
LOOPBACK = "127.0.0.1"
MAX_BODY_BYTES = 1 << 20


def sanitize_proxy_env() -> None:
    """清理上游 HTTP 库解析不了的代理变量。

    本机（Clash）设了两个会炸的变量，且**都不是我们能改的外部环境**：

    - `ALL_PROXY=socks5://127.0.0.1:7890`，但 7890 其实是 **HTTP** 代理，
      httpx 按 socks5 走会报缺 `socksio`；
    - `NO_PROXY` 里含 `[::1]`，httpx 解析它抛
      `InvalidURL: Invalid port: ':1]'`。

    实测在 PowerShell 里改 `$env:NO_PROXY` 对本进程**无效**（Python 读到的仍是
    原值），只有在 Python 内改 `os.environ` 才生效。所以这件事必须由程序自己做，
    而且必须在导入 httpx / huggingface_hub **之前**做。
    """
    for k in ("ALL_PROXY", "all_proxy"):
        os.environ.pop(k, None)
    raw = os.environ.get("NO_PROXY") or os.environ.get("no_proxy") or ""
    keep = [t.strip() for t in raw.split(",") if t.strip() and not t.strip().startswith("[")]
    cleaned = ",".join(keep) if keep else "localhost,127.0.0.1"
    os.environ["NO_PROXY"] = cleaned
    os.environ["no_proxy"] = cleaned


sanitize_proxy_env()

for _s in (sys.stderr, sys.stdout):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass

# —— 模型状态（延迟加载，失败不阻塞服务启动）——
_model = None
_model_error: str | None = None
_model_lock = threading.Lock()
_model_name = "unloaded"
_calibrated = False
_coverage = 0.9


def render_state(state) -> str:
    """把结构化 state 渲染成给编码器读的自然文本。

    不直接 `json.dumps`：标点噪音对编码器没有信息量，而"键：值"逐行更接近
    它预训练时见过的文本形状。嵌套结构递归展开。
    """
    lines: list[str] = []

    def walk(prefix: str, value) -> None:
        if isinstance(value, dict):
            for k, v in value.items():
                walk(f"{prefix}{k}", v)
        elif isinstance(value, list):
            for item in value:
                walk(prefix, item)
        elif value is None:
            return
        elif isinstance(value, bool):
            lines.append(f"{prefix}：{'是' if value else '否'}")
        else:
            lines.append(f"{prefix}：{value}")

    walk("", state)
    return "\n".join(lines) if lines else "(无状态)"


def resolve_model(model: str) -> str:
    """优先用**本地数据目录**里的权重。

    这是"克隆下来就能用、之后完全离线"的关键：`fetch_model.py` 把权重放到
    `<home>/models/<name>`，这里优先读它；没有才回落到 HuggingFace 仓库名。

    实测本地路径加载在 `HF_HUB_OFFLINE=1` 下正常工作（7 秒），
    所以分发形态是自包含的。
    """
    if os.path.isdir(model):  # 调用方直接给了目录
        return model

    home = os.environ.get("YUNXI_BOT_HOME")
    if not home:
        local = os.environ.get("LOCALAPPDATA")
        home = str(Path(local) / "YunXiBot") if local else str(Path.home() / ".yunxi-bot")

    candidate = Path(home) / "models" / model
    if (candidate / "model.safetensors").is_file():
        return str(candidate)

    sys.stderr.write(
        f"[verdict-server] 本地没有 {candidate}，回落到 HuggingFace 仓库名 {model}\n"
        f"[verdict-server] 想离线自包含请先跑: python scripts/fetch_model.py\n"
    )
    return model


def load_model(model: str, device: str, calibration: str | None) -> None:
    global _model, _model_error, _model_name, _calibrated, _coverage
    with _model_lock:
        if _model is not None or _model_error is not None:
            return
        try:
            from verdict import Verdict

            resolved = resolve_model(model)
            _model = Verdict(model=resolved, device=device)
            _model_name = model
            _coverage = float(getattr(_model, "coverage", 0.9))
            sys.stderr.write(f"[verdict-server] 模型已加载: {model}  ← {resolved}\n")

            if calibration:
                # 校准文件是**我们自己的**产物（几十 KB 的 JSON），
                # 与编码器权重分开管理。
                try:
                    import pickle  # noqa: S403

                    with open(calibration, "rb") as fh:
                        _model = pickle.load(fh)  # noqa: S301
                    _calibrated = True
                    sys.stderr.write(f"[verdict-server] 已加载校准: {calibration}\n")
                except Exception as exc:  # noqa: BLE001
                    sys.stderr.write(
                        f"[verdict-server] 校准加载失败，将以未校准状态运行: {exc}\n"
                    )
        except Exception as exc:  # noqa: BLE001
            _model_error = f"{type(exc).__name__}: {exc}"
            sys.stderr.write(f"[verdict-server] 模型加载失败: {_model_error}\n")


def answer_choice(model, question: dict, state_text: str) -> dict:
    criteria = (question.get("kind") or {}).get("criteria") or {}
    if not criteria:
        raise ValueError(f"choice 问题 {question.get('id')} 缺少 criteria")
    # Verdict 的 options 直接接受 {label: description}
    r = model.choose(state_text, criteria, prompt=question.get("instructions") or None)
    dist = {str(k): float(v) for k, v in (r.distribution or {}).items()}
    return {
        "choice": r.label,
        "distribution": dist,
        "confidence": float(r.confidence.probability),
        "abstain": bool(r.confidence.abstain),
        "calibrated": bool(getattr(r.confidence, "calibrated", False)),
    }


def answer_noul(model, question: dict, state_text: str) -> dict:
    # 我们的 noul 语义是"命题为真的概率"，Verdict 的 check(claim=...) 正好一致
    claim = question.get("instructions") or ""
    if not claim:
        raise ValueError(f"noul 问题 {question.get('id')} 缺少 instructions（命题）")
    r = model.check(state_text, claim=claim)
    prob = float(r.probability)
    return {
        "noul": prob,
        "confidence": max(prob, 1.0 - prob),
        "abstain": bool(r.confidence.abstain),
        "calibrated": bool(getattr(r.confidence, "calibrated", False)),
    }


def answer_score(model, question: dict, state_text: str) -> dict:
    levels = (question.get("kind") or {}).get("levels") or []
    if not levels:
        raise ValueError(f"score 问题 {question.get('id')} 缺少 levels")
    n = len(levels)
    rubric = {i + 1: lv for i, lv in enumerate(levels)}
    r = model.score(
        state_text,
        scale=(1, n),
        rubric=rubric,
        prompt=question.get("instructions") or None,
    )
    dist = {}
    for k, v in (r.distribution or {}).items():
        idx = int(k)
        dist[levels[idx - 1] if 1 <= idx <= n else str(idx)] = float(v)
    chosen = levels[r.value - 1] if 1 <= r.value <= n else str(r.value)
    return {
        "choice": chosen,
        "distribution": dist,
        "expected_score": float(r.expected),
        "confidence": float(r.confidence.probability),
        "abstain": bool(r.confidence.abstain),
        "calibrated": bool(getattr(r.confidence, "calibrated", False)),
    }


class Handler(BaseHTTPRequestHandler):
    server_version = "yunxi-bot-verdict/0.1"

    def log_message(self, fmt: str, *args) -> None:  # noqa: A003
        sys.stderr.write("[verdict-server] %s\n" % (fmt % args).split("{")[0].strip())

    def _send(self, code: int, payload: dict) -> None:
        body = json.dumps(payload, ensure_ascii=False, default=str).encode("utf-8")
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
        if _model is not None:
            self._send(
                200,
                {
                    "status": "ok",
                    "model": _model_name,
                    "calibrated": _calibrated,
                    "coverage": _coverage,
                },
            )
        else:
            self._send(
                200,
                {
                    "status": "degraded",
                    "model": _model_name,
                    "detail": _model_error or "模型尚未加载",
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

        if _model is None:
            # 模型不可用时返回明确错误，让 Rust 侧走降级——绝不伪造答案
            self._send(
                503,
                {"error": "model unavailable", "detail": _model_error or "未加载"},
            )
            return

        try:
            state_text = render_state(req.get("state") or {})
            answers: dict = {}
            calib_flags = []
            abstains = []
            for q in req.get("questions") or []:
                kind = (q.get("kind") or {}).get("type")
                if kind == "choice":
                    a = answer_choice(_model, q, state_text)
                elif kind == "noul":
                    a = answer_noul(_model, q, state_text)
                elif kind == "score":
                    a = answer_score(_model, q, state_text)
                else:
                    raise ValueError(f"未知问题类型: {kind!r}")
                calib_flags.append(a.pop("calibrated", False))
                abstains.append(a.pop("abstain", False))
                answers[q["id"]] = a

            self._send(
                200,
                {
                    "answers": answers,
                    "model": _model_name,
                    # 如实上报：Rust 侧要据此决定能不能拿这些概率卡阈值
                    "calibrated": all(calib_flags) if calib_flags else _calibrated,
                    "abstained": any(abstains),
                },
            )
        except Exception as exc:  # noqa: BLE001
            # 不把 state 回显进错误信息
            self._send(500, {"error": f"{type(exc).__name__}: {exc}"})


def main() -> int:
    ap = argparse.ArgumentParser(description="YunXi Bot 决策模型 sidecar（Verdict 后端）")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--model", default="verdict-small", help="Verdict 模型名")
    ap.add_argument("--device", default="cpu", help="cpu / cuda")
    ap.add_argument("--calibration", default=None, help="校准文件路径（我们自己 fit 出来的）")
    ap.add_argument("--host", default=LOOPBACK, help="只允许回环")
    args = ap.parse_args()

    if args.host not in ("127.0.0.1", "localhost", "::1"):
        sys.stderr.write("拒绝：决策模型的 state 可能含私人信息，只允许绑定回环地址\n")
        return 2

    threading.Thread(
        target=load_model,
        args=(args.model, args.device, args.calibration),
        daemon=True,
    ).start()

    srv = ThreadingHTTPServer((args.host, args.port), Handler)
    sys.stderr.write(
        f"[verdict-server] 监听 http://{args.host}:{args.port}  /decide  /health\n"
    )
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        sys.stderr.write("\n[verdict-server] 收到中断，退出\n")
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
