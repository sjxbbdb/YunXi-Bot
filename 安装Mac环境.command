#!/bin/bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
# Finder 启动 Terminal 时也能找到 Homebrew 与 rustup。
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
exec /bin/bash "$ROOT/scripts/setup-macos.sh" "$@"
