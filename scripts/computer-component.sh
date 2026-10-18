#!/bin/bash
# Trusted local setup only; never exposed as model tool arguments.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/target/macos/云熙电脑组件.app"
STATE="$HOME/Library/Application Support/YunXiBot/computer"
DRIVER="$APP/Contents/MacOS/cua-driver"
export CUA_DRIVER_EMBEDDED=1
export CUA_DRIVER_RS_TELEMETRY_ENABLED=false
export CUA_TELEMETRY_ENABLED=false
export CUA_DRIVER_RS_UPDATE_CHECK=false
case "${1:-status}" in
    start) /usr/bin/open -g "$APP" ;;
    authorize) /usr/bin/open -n "$APP" --args --authorize ;;
    status) "$DRIVER" status --socket "$STATE/driver.sock" ;;
    stop)
        # Stop the owning host so its bounded child cleanup also covers a stalled daemon.
        python3 - "$APP/Contents/MacOS/YunXiComputer" <<'PY'
import os, signal, subprocess, sys, time
processes = subprocess.check_output(['/bin/ps', '-axo', 'pid=,comm='], text=True).splitlines()
for line in processes:
    fields = line.strip().split(maxsplit=1)
    if len(fields) == 2 and fields[1] == sys.argv[1]:
        try:
            os.kill(int(fields[0]), signal.SIGTERM)
        except ProcessLookupError:
            pass
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    processes = subprocess.check_output(['/bin/ps', '-axo', 'comm='], text=True).splitlines()
    if sys.argv[1] not in [p.strip() for p in processes]:
        break
    time.sleep(0.1)
else:
    raise SystemExit('电脑组件尚未退出；不要重放动作或覆盖安装')
PY
        ;;
    allow-existing-profile)
        # This explicit setup command records the user's browser-profile authorization.
        python3 - "$STATE" <<'PY'
import os, pathlib, stat, sys
p = pathlib.Path(sys.argv[1])
p.mkdir(parents=True, exist_ok=True, mode=0o700)
s = p.lstat()
if not stat.S_ISDIR(s.st_mode) or s.st_uid != os.getuid() or s.st_mode & 0o077:
    raise SystemExit("电脑组件状态目录权限不安全")
fd = os.open(p / 'existing-profile.allowed', os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
with os.fdopen(fd, 'w') as f:
    os.fchmod(f.fileno(), 0o600)
    f.write('allowed\n')
PY
        echo "已保存日常浏览器配置接入授权；组件下次启动生效，动作仍需审批。" ;;
    *) echo "用法：$0 start|stop|status|authorize|allow-existing-profile" >&2; exit 2 ;;
esac
