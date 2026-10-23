#!/bin/bash
# 构建 macOS 原生聊天窗口“云熙”。运行时由窗口拉起 target/release/yunxi-bot chat-server。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo "需要 macOS 14+" >&2; exit 2; }
APP="$ROOT/target/macos/云熙聊天.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/native/macos/chat/Info.plist" "$APP/Contents/Info.plist"
xcrun swiftc -O -target "$(uname -m)-apple-macosx14.0" \
  "$ROOT/native/macos/chat/main.swift" \
  "$ROOT/native/macos/chat/MacChatWindow.swift" \
  "$ROOT/native/macos/chat/ChatService.swift" \
  "$ROOT/native/macos/chat/MarkdownRenderer.swift" \
  "$ROOT/native/macos/chat/CapabilityModels.swift" \
  "$ROOT/native/macos/chat/CapabilityWindow.swift" \
  -o "$APP/Contents/MacOS/YunXiChat"
/usr/bin/codesign --force --sign - --identifier local.yunxi.bot.chat "$APP"
echo "聊天窗口已构建：$APP"
echo "打开：open \"$APP\"（或双击 target/macos/云熙聊天.app）"
