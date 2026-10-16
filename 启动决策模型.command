#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export YUNXI_BOT_HOME="${YUNXI_BOT_HOME:-"$ROOT/data"}"
if [[ ! -x "$ROOT/.venv/bin/python" ]]; then
    echo "请先运行 安装Mac环境.command。" >&2
    exit 2
fi
exec "$ROOT/.venv/bin/python" "$ROOT/sidecar/laya_server.py" --port 17870 --device auto "$@"
