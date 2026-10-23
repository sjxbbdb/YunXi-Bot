#!/bin/bash
# 双击打开“云熙聊天”原生窗口（首次使用先构建，见下方提示）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
APP="$ROOT/target/macos/云熙聊天.app"
if [[ ! -d "$APP" ]]; then
  echo "聊天窗口还没构建。先在终端运行：" >&2
  echo "  bash \"$ROOT/scripts/build-macos-chat.sh\"" >&2
  echo "（需要先有 target/release/yunxi-bot，构建命令见 README）" >&2
  exit 2
fi
open "$APP"
