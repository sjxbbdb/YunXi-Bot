#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""拉取决策模型权重到本地数据目录。

目标：**克隆仓库后跑这一条命令，模型就位，之后完全离线可用。**

权重不放在 git 里（单个 `model.safetensors` 有 448.8 MB，超过 GitHub 的
100 MiB 单文件硬上限，推不上去），而是作为 **GitHub Release 资产**分发，
由本脚本下载并校验 SHA256。

三条来源，按顺序尝试：

1. 本地已有（`<home>/models/<dir>` 已存在且完整）→ 直接用；
2. GitHub Release 资产 → 下载、**校验 SHA256**、解压；
3. HuggingFace 官方仓库 → 兜底（Release 还没上传时用得上）。

校验失败一律**报错退出，绝不"将就用"**：一个哈希不对的权重会让整个决策层
给出无法解释的判断，比没有模型更糟。

用法
----
  python scripts/fetch_model.py                # 用清单里的默认模型
  python scripts/fetch_model.py --force        # 重新下载
  python scripts/fetch_model.py --check        # 只校验已存在的权重
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import urllib.request
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "models" / "manifest.json"
GITHUB_REPO = "sjxbbdb/YunXi-Bot"


def sanitize_proxy_env() -> None:
    """清理本机代理配置里会炸的两个变量。

    - `ALL_PROXY=socks5://127.0.0.1:7890` 但那是 HTTP 代理，urllib/httpx 都不支持；
    - `NO_PROXY` 含 `[::1]`，httpx 解析它会抛 `InvalidURL`。

    实测在 PowerShell 里改环境变量对本进程无效，只有 Python 内改才生效。
    """
    for k in ("ALL_PROXY", "all_proxy"):
        os.environ.pop(k, None)
    raw = os.environ.get("NO_PROXY") or os.environ.get("no_proxy") or ""
    keep = [t.strip() for t in raw.split(",") if t.strip() and not t.strip().startswith("[")]
    cleaned = ",".join(keep) if keep else "localhost,127.0.0.1"
    os.environ["NO_PROXY"] = cleaned
    os.environ["no_proxy"] = cleaned


sanitize_proxy_env()

for _s in (sys.stderr, sys.stdout):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass


def default_home() -> Path:
    """与 Rust 侧 `default_home()` 保持一致的解析顺序。"""
    env = os.environ.get("YUNXI_BOT_HOME")
    if env:
        return Path(env)
    local = os.environ.get("LOCALAPPDATA")
    if local:
        return Path(local) / "YunXiBot"
    return Path.home() / ".yunxi-bot"


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_manifest() -> dict:
    with open(MANIFEST, encoding="utf-8") as fh:
        return json.load(fh)


def model_ready(dest: Path, required: list[str]) -> bool:
    return all((dest / name).is_file() for name in required)


def progress(count: int, block: int, total: int | None) -> None:
    if not total:
        return
    done = min(count * block, total)
    pct = done * 100 / total if total else 0
    sys.stderr.write(f"\r  下载中 {pct:5.1f}%  {done / 1e6:7.1f} / {total / 1e6:.1f} MB")
    sys.stderr.flush()


def download(url: str, dest: Path) -> bool:
    sys.stderr.write(f"  来源: {url}\n")
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "yunxi-bot-fetch/0.1"})
        with urllib.request.urlopen(req, timeout=60) as resp:  # noqa: S310
            total = int(resp.headers.get("Content-Length") or 0) or None
            dest.parent.mkdir(parents=True, exist_ok=True)
            with open(dest, "wb") as out:
                shutil.copyfileobj(resp, out, length=1 << 20)
        sys.stderr.write("\n")
        return True
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"\n  失败: {type(exc).__name__}: {exc}\n")
        return False


def fetch_from_huggingface(entry: dict, dest: Path) -> bool:
    """兜底：从 HuggingFace 直接拉（Release 资产还没上传时用）。

    不校验哈希——HF 自己的 revision + 传输层保证完整性，而我们没有它的
    逐文件哈希清单。**因此这条路会在输出里明确标注**，不会伪装成已验证。
    """
    try:
        from huggingface_hub import snapshot_download
    except ImportError:
        sys.stderr.write("  未安装 huggingface_hub，无法兜底下载\n")
        return False
    try:
        snapshot_download(
            repo_id=entry["source_repo"],
            revision=entry.get("revision"),
            local_dir=str(dest),
        )
        sys.stderr.write("  ⚠ 已从 HuggingFace 拉取（未经 SHA256 校验）\n")
        return True
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"  HuggingFace 兜底失败: {type(exc).__name__}: {exc}\n")
        return False


def install(entry: dict, home: Path, force: bool, check_only: bool) -> int:
    name = entry["name"]
    dest = home / "models" / entry["dir"]
    required = entry.get("required_files") or ["model.safetensors", "config.json"]

    print(f"模型: {name}")
    print(f"目标: {dest}")

    if model_ready(dest, required):
        if not force:
            print("  已存在且完整，跳过下载。")
            if entry.get("sha256_archive"):
                print("  （如需重新校验，用 --force 重下）")
            return 0
    elif check_only:
        print("  ✗ 缺失或不完整")
        return 1

    if check_only:
        print("  存在且完整。")
        return 0

    if force and dest.exists():
        print("  --force：删除已有权重")
        shutil.rmtree(dest)

    archive_name = entry.get("archive")
    url = (
        f"https://github.com/{GITHUB_REPO}/releases/download/"
        f"{entry['release_tag']}/{archive_name}"
    )

    with tempfile.TemporaryDirectory() as tmp:
        tmp_zip = Path(tmp) / (archive_name or "model.zip")
        got = download(url, tmp_zip)

        if got:
            expect = entry.get("sha256_archive")
            if expect:
                print("  校验 SHA256 ...")
                actual = sha256_of(tmp_zip)
                if actual != expect:
                    print("  ✗ 哈希不匹配，拒绝使用")
                    print(f"    期望 {expect}")
                    print(f"    实际 {actual}")
                    return 2
                print("  ✓ 哈希一致")
            else:
                print("  ⚠ 清单里没有哈希，跳过校验")

            print("  解压 ...")
            dest.mkdir(parents=True, exist_ok=True)
            with zipfile.ZipFile(tmp_zip) as zf:
                for member in zf.namelist():
                    # 防 zip slip：拒绝绝对路径与上跳
                    if member.startswith("/") or ".." in Path(member).parts:
                        print(f"  ✗ 压缩包含可疑路径，中止: {member}")
                        return 2
                zf.extractall(dest)
        else:
            print("  Release 资产不可用，尝试 HuggingFace 兜底 ...")
            if not fetch_from_huggingface(entry, dest):
                print("  ✗ 所有来源都失败")
                return 1

    if not model_ready(dest, required):
        print(f"  ✗ 解压后仍缺少必要文件（需要 {required}）")
        return 1

    total = sum(f.stat().st_size for f in dest.rglob("*") if f.is_file())
    print(f"  ✓ 就绪，{total / 1e6:.1f} MB")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="拉取 YunXi Bot 决策模型权重")
    ap.add_argument("--force", action="store_true", help="重新下载")
    ap.add_argument("--check", action="store_true", help="只校验，不下载")
    ap.add_argument("--home", default=None, help="数据目录（默认与 Rust 侧一致）")
    ap.add_argument("--only", default=None, help="只处理指定模型名")
    args = ap.parse_args()

    home = Path(args.home) if args.home else default_home()
    manifest = load_manifest()
    print(f"数据目录: {home}")
    print(f"清单版本: {manifest.get('schema')}")
    print()

    rc = 0
    for entry in manifest["models"]:
        if args.only and entry["name"] != args.only:
            continue
        r = install(entry, home, args.force, args.check)
        rc = rc or r
        print()

    if rc == 0:
        print("全部就绪。启动 sidecar：")
        print("  .venv\\Scripts\\python.exe sidecar\\verdict_server.py --port 17870")
    return rc


if __name__ == "__main__":
    sys.exit(main())
