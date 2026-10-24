#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export YUNXI_BOT_HOME="${YUNXI_BOT_HOME:-"$ROOT/data"}"
if [[ ! -x "$ROOT/.venv-mlx/bin/python" ]]; then
    echo "请先运行 安装Mac环境.command --local-model。" >&2
    exit 2
fi
MODEL="${YUNXI_COMPANION_MODEL:-Qwen3.5-9B-4bit}"
if [[ "$MODEL" == "qwen" || "$MODEL" == "qwen3.5" ]]; then
    MODEL="Qwen3.5-9B-4bit"
elif [[ "$MODEL" == "qwen38" || "$MODEL" == "qwen3.8" ]]; then
    MODEL="Qwen3.8-27B-4bit"
elif [[ "$MODEL" == "qwen38u" || "$MODEL" == "uncensored" ]]; then
    MODEL="Qwen3.8-27B-Uncensored-4bit"
elif [[ "$MODEL" == "gemma" || "$MODEL" == "gemma4" ]]; then
    MODEL="gemma-4-26B-A4B-it-qat-4bit"
fi
exec "$ROOT/.venv-mlx/bin/python" "$ROOT/sidecar/local_llm_server.py" \
    --model "$MODEL" --device mlx --warmup \
    --idle-unload "${YUNXI_LOCAL_IDLE_UNLOAD:-600}" "$@"
