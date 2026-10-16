#!/bin/bash
# 原生 macOS 开发环境；只在项目目录创建 venv 和构建产物。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PYTHON_BIN="${YUNXI_PYTHON:-python3.12}"
SKIP_MODEL=0
LOCAL_MODEL=0
for arg in "$@"; do
    case "$arg" in
        --skip-model) SKIP_MODEL=1 ;;
        --local-model) LOCAL_MODEL=1 ;;
        --help|-h)
            echo "用法：YUNXI_PYTHON=/path/to/python3.12 bash scripts/setup-macos.sh [--skip-model] [--local-model]"
            echo "默认构建 release、安装辅助模型依赖并下载 Laya；--local-model 仅供本地生成模型实验。"
            exit 0 ;;
        *) echo "未知参数：$arg" >&2; exit 2 ;;
    esac
done
if [[ "$(uname -s)" != Darwin ]]; then
    echo "此脚本用于 macOS。" >&2
    exit 2
fi
if [[ "$(uname -m)" != arm64 ]]; then
    echo "此安装脚本面向 Apple Silicon；请使用原生 arm64 终端。" >&2
    exit 2
fi
if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
    echo "未找到 Python 3.12。安装后重试：brew install python@3.12" >&2
    exit 2
fi
"$PYTHON_BIN" -c 'import sys; sys.exit(0 if sys.version_info[:2] == (3, 12) else "请使用 Python 3.12")'
export YUNXI_BOT_HOME="${YUNXI_BOT_HOME:-"$ROOT/data"}"

echo "[1/4] 创建项目 Python 环境"
if [[ ! -x .venv/bin/python ]]; then
    "$PYTHON_BIN" -m venv .venv
fi
.venv/bin/python -c 'import platform, sys; sys.exit(0 if sys.version_info[:2] == (3, 12) and platform.system() == "Darwin" else "现有 .venv 不匹配；请备份后重建")'
echo "[2/4] 安装模型服务依赖"
.venv/bin/python -m pip install --upgrade pip
.venv/bin/python -m pip install -r sidecar/requirements.txt
.venv/bin/python -c 'import torch; print("PyTorch:", torch.__version__, "MPS:", torch.backends.mps.is_available())'
if [[ "$LOCAL_MODEL" == 1 ]]; then
    if [[ ! -x .venv-mlx/bin/python ]]; then
        "$PYTHON_BIN" -m venv .venv-mlx
    fi
    .venv-mlx/bin/python -m pip install -r sidecar/requirements-mlx.txt
    .venv-mlx/bin/python -c 'import mlx.core as mx; print("MLX:", mx.__version__, "Metal:", mx.metal.is_available())'
fi

echo "[3/4] 准备模型权重"
if [[ "$SKIP_MODEL" == 0 ]]; then
    .venv/bin/python scripts/fetch_model.py
fi
if [[ "$LOCAL_MODEL" == 1 ]]; then
    if ! command -v aria2c >/dev/null 2>&1; then
        echo "本地模型下载器需要 aria2c，请先运行 brew install aria2。" >&2
        exit 2
    fi
    .venv-mlx/bin/python scripts/fetch_mlx_candidates.py --model Qwen3.5-9B-4bit --home "$YUNXI_BOT_HOME"
fi

echo "[4/4] 构建 Apple 平台原生程序"
bash scripts/build-macos-tools.sh
cargo build --release --locked
echo "环境就绪。数据目录：$YUNXI_BOT_HOME"
echo "启动对话：./启动.command"
echo "生成使用 DeepSeek；本地仅需决策与按需检索服务。"
echo "远端模型凭证按 README 配置在数据目录 secrets/，不写入启动脚本。"
