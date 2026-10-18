#!/usr/bin/env python3
"""Bundle the pinned MIT Driver in YunXi; never run the upstream installer."""
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parent.parent
VERSION = "0.36.0"
ARCHIVE = f"cua-driver-rs-{VERSION}-darwin-universal.tar.gz"
URL = f"https://github.com/trycua/cua/releases/download/cua-driver-rs-v{VERSION}/{ARCHIVE}"
SHA256 = "2a83cf9c3a2fa7ae27f3fe62e426aeca5acf76511bb14dd64d10ccb2de6aecae"
BUNDLE_ID = "local.yunxi.bot.computer"
APP = ROOT / "target/macos/云熙电脑组件.app"


def run(*args):
    return subprocess.run([str(a) for a in args], check=True, capture_output=True, text=True).stdout.strip()


def extract_verified(archive, destination):
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise ValueError("Cua安装包SHA256不匹配；拒绝解包")
    with tarfile.open(archive) as tar:
        # Only copy regular files/directories; no archive-provided links or special files.
        for item in tar.getmembers():
            name = Path(item.name)
            if name.is_absolute() or ".." in name.parts or not (item.isfile() or item.isdir()):
                raise ValueError("Cua安装包包含不安全路径或文件类型")
        tar.extractall(destination, filter="data")


def verified_driver(app):
    run("/usr/bin/codesign", "--verify", "--deep", "--strict", "-R",
        '=anchor apple generic and certificate leaf[subject.OU] = "YCK386LBJ7" and identifier "com.trycua.driver"', app)
    driver = app / "Contents/MacOS/cua-driver"
    if run(driver, "--version") != f"cua-driver {VERSION}":
        raise ValueError("Cua版本不匹配；不会自动升级")
    return driver


def build():
    if platform.system() != "Darwin":
        raise RuntimeError("电脑内置组件需要macOS")
    APP.parent.mkdir(parents=True, exist_ok=True)
    # Never replace an app whose live process would retain the old signing identity.
    for line in run("/bin/ps", "-axo", "comm=").splitlines():
        if line.strip() == str(APP / "Contents/MacOS/YunXiComputer"):
            raise RuntimeError("请先退出云熙电脑组件，再更新；不会中断正在进行的电脑任务")
    with tempfile.TemporaryDirectory(prefix="computer-build-", dir=APP.parent) as temp:
        temp = Path(temp)
        installed = Path("/Applications/CuaDriver.app")
        if installed.is_dir():
            driver = verified_driver(installed)
        else:
            cache = ROOT / "target/downloads" / ARCHIVE
            cache.parent.mkdir(parents=True, exist_ok=True)
            if not cache.is_file():
                partial = cache.with_suffix(".partial")
                run("/usr/bin/curl", "--fail", "--location", "--retry", "2", "--connect-timeout", "15",
                    "--max-time", "1800", URL, "--output", partial)
                if hashlib.sha256(partial.read_bytes()).hexdigest() != SHA256:
                    raise ValueError("Cua下载校验失败；保留现有安装")
                partial.replace(cache)
            extract_verified(cache, temp / "upstream")
            driver = verified_driver(temp / "upstream" / f"cua-driver-rs-{VERSION}-darwin-universal/CuaDriver.app")
        source_hash = hashlib.sha256(driver.read_bytes()).hexdigest()
        staged = temp / APP.name
        binary = staged / "Contents/MacOS"
        resources = staged / "Contents/Resources"
        binary.mkdir(parents=True)
        resources.mkdir()
        shutil.copy2(driver, binary / "cua-driver")
        run("/usr/bin/xcrun", "swiftc", "-O", "-parse-as-library", "-target", f"{platform.machine()}-apple-macosx14.0",
            ROOT / "native/macos/ComputerHost.swift", "-o", binary / "YunXiComputer")
        (staged / "Contents/Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": BUNDLE_ID, "CFBundleExecutable": "YunXiComputer",
            "CFBundleName": "云熙电脑组件", "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": "0.1.0", "CFBundleVersion": "1",
            "LSMinimumSystemVersion": "14.0", "LSUIElement": True,
            "NSScreenCaptureUsageDescription": "云熙通过截图理解你授权操作的窗口。",
        }))
        for source in [ROOT / "third_party/cua/LICENSE.md", ROOT / "third_party/cua/LICENSING.md",
                       ROOT / "third_party/cua/libs/cua-driver/rust/THIRD_PARTY_NOTICES.md"]:
            shutil.copy2(source, resources / source.name)
        (resources / "cua-origin.json").write_text(json.dumps({"version": VERSION, "url": URL,
            "archive_sha256": SHA256, "source_binary_sha256": source_hash, "license": "MIT",
            "source": "https://github.com/trycua/cua/tree/6d6c734c127025fb39ad5d585a10382808b39dc1/libs/cua-driver"}, indent=2))
        # shortcut: local ad-hoc builds need reauthorization after updates; use a stable Developer ID for distribution.
        identity = os.environ.get("YUNXI_SIGN_IDENTITY", "-")
        run("/usr/bin/codesign", "--force", "--sign", identity, binary / "cua-driver")
        run("/usr/bin/codesign", "--force", "--sign", identity, "--identifier", BUNDLE_ID, staged)
        run("/usr/bin/codesign", "--verify", "--deep", "--strict", staged)
        backup = APP.with_suffix(".previous.app")
        if backup.exists():
            shutil.rmtree(backup)
        if APP.exists():
            APP.rename(backup)
        try:
            staged.rename(APP)
        except Exception:
            if backup.exists():
                backup.rename(APP)
            raise
    print(f"内置电脑组件已构建：{APP}")
    print("首次使用请运行：bash scripts/computer-component.sh authorize")


if __name__ == "__main__":
    try:
        build()
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        raise SystemExit(error.stderr if isinstance(error, subprocess.CalledProcessError) else str(error))
