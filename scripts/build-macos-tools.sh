#!/bin/bash
# 固定 bundle 身份让系统授权归属于云熙；不安装第三方运行时。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[[ "$(uname -s)" == Darwin ]] || { echo "需要 macOS 14+" >&2; exit 2; }
APP="${YUNXI_NATIVE_BUILD_APP:-$ROOT/target/macos/云熙本机助理.app}"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/native/macos/Info.plist" "$APP/Contents/Info.plist"
xcrun swiftc -O -parse-as-library -target "$(uname -m)-apple-macosx14.0" \
  "$ROOT/native/macos/YunXiNative.swift" "$ROOT/native/macos/Assistant.swift" "$ROOT/native/macos/Notifications.swift" "$ROOT/native/macos/Location.swift" -o "$APP/Contents/MacOS/YunXiNative"
cp "$ROOT/native/macos/assistant-tools.json" "$ROOT/native/macos/Mail.js" "$APP/Contents/Resources/"
/usr/bin/codesign --force --sign - --identifier local.yunxi.bot.native "$APP"
"$APP/Contents/MacOS/YunXiNative" --self-test
echo "原生工具已构建：$APP"
