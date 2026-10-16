#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export YUNXI_BOT_HOME="${YUNXI_BOT_HOME:-"$ROOT/data"}"
if [[ ! -x "$ROOT/.venv-mlx/bin/python" ]]; then
    echo "请先运行 安装Mac环境.command --local-model。" >&2
    exit 2
fi
exec "$ROOT/.venv-mlx/bin/python" "$ROOT/sidecar/local_llm_server.py" --model Qwen3.5-9B-4bit --device mlx --warmup "$@"
