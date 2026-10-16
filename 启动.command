#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PATH="$ROOT/.venv/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export YUNXI_BOT_HOME="${YUNXI_BOT_HOME:-"$ROOT/data"}"
BIN="$ROOT/target/release/yunxi-bot"
[[ -x "$BIN" ]] || BIN="$ROOT/target/debug/yunxi-bot"
if [[ ! -x "$BIN" ]]; then
    echo "未找到主程序，请先运行 安装Mac环境.command 或 cargo build。" >&2
    exit 2
fi
cd "$ROOT"
if [[ $# == 0 ]]; then set -- chat; fi
exec "$BIN" "$@"
