#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""拉取决策模型权重到本地数据目录。

目标：**克隆仓库后跑这一条命令，模型就位，之后完全离线可用。**

默认 Laya 多语言权重从固定 HuggingFace revision 下载，按清单校验 SHA256。
历史 Verdict 可用 --only verdict-small 下载原 Release 归档。

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
from urllib.parse import quote
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
    # macOS 的系统代理会被后来写入的 NO_PROXY 环境变量遮住，先保留 HTTP(S)。
    proxies = urllib.request.getproxies()
    for scheme in ("http", "https"):
        if proxies.get(scheme):
            os.environ.setdefault(f"{scheme.upper()}_PROXY", proxies[scheme])
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


def model_ready(dest: Path, required: list[str], hashes: dict | None = None) -> bool:
    return all((dest / name).is_file() for name in required) and all(
        (dest / name).is_file() and sha256_of(dest / name) == digest
        for name, digest in (hashes or {}).items()
    )


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
    """按清单版本下载；安装前统一校验已记录的文件哈希。"""
    if entry.get("sha256_files"):
        # 固定文件清单可直接用标准库下载，避免 Hub/Xet 的额外网络探测。
        base = f"https://huggingface.co/{entry['source_repo']}/resolve/{entry['revision']}/"
        return all(download(base + quote(name) + "?download=true", dest / name)
                   for name in entry["required_files"])
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
    hashes = entry.get("sha256_files")

    print(f"模型: {name}")
    print(f"目标: {dest}")

    if model_ready(dest, required, hashes) and not force:
        print("  已存在且完整，跳过下载。")
        return 0
    elif check_only:
        if model_ready(dest, required, hashes):
            print("  存在且完整。")
            return 0
        print("  ✗ 缺失或不完整")
        return 1

    archive_name = entry.get("archive")
    url = (
        f"https://github.com/{GITHUB_REPO}/releases/download/"
        f"{entry['release_tag']}/{archive_name}"
    ) if archive_name else None

    # —— 先下到暂存区，校验通过再替换 ——
    #
    # 早先版本是"先删旧的、再下新的"。实测踩到：拿`--force`做哈希篡改测试时，
    # 旧的 487MB 权重被删掉、新的因校验失败没装上，最后**一份都没有**。
    # 对要花 20 秒以上下载的东西，"先毁后建"是错的顺序。
    staging = home / "models" / f".staging-{entry['dir']}"
    if staging.exists():
        shutil.rmtree(staging)

    try:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_zip = Path(tmp) / (archive_name or "model.zip")
            got = download(url, tmp_zip) if url else False

            if got:
                expect = entry.get("sha256_archive")
                if expect:
                    print("  校验 SHA256 ...")
                    actual = sha256_of(tmp_zip)
                    if actual != expect:
                        print("  ✗ 哈希不匹配，拒绝使用")
                        print(f"    期望 {expect}")
                        print(f"    实际 {actual}")
                        if model_ready(dest, required):
                            print("  已有的旧权重**未被破坏**，继续可用。")
                        return 2
                    print("  ✓ 哈希一致")
                else:
                    print("  ⚠ 清单里没有哈希，跳过校验")

                print("  解压到暂存区 ...")
                staging.mkdir(parents=True, exist_ok=True)
                with zipfile.ZipFile(tmp_zip) as zf:
                    for member in zf.namelist():
                        # 防 zip slip：拒绝绝对路径与上跳
                        if member.startswith("/") or ".." in Path(member).parts:
                            print(f"  ✗ 压缩包含可疑路径，中止: {member}")
                            return 2
                    zf.extractall(staging)
            else:
                print("  从清单固定的 HuggingFace revision 下载 ...")
                if not fetch_from_huggingface(entry, staging):
                    print("  ✗ 所有来源都失败（旧权重未受影响）")
                    return 1

        if not model_ready(staging, required, hashes):
            print("  ✗ 暂存区文件缺失或 SHA256 不匹配，放弃替换")
            shutil.rmtree(staging, ignore_errors=True)
            return 1

        # 校验都过了，这时才动旧的
        if dest.exists():
            shutil.rmtree(dest)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(staging), str(dest))
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)

    total = sum(f.stat().st_size for f in dest.rglob("*") if f.is_file())
    print(f"  ✓ 就绪，{total / 1e6:.1f} MB")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="拉取 YunXi Bot 决策模型权重")
    ap.add_argument("--force", action="store_true", help="重新下载")
    ap.add_argument("--check", action="store_true", help="只校验，不下载")
    ap.add_argument("--home", default=None, help="数据目录（默认与 Rust 侧一致）")
    ap.add_argument("--only", default="laya-multilingual", help="模型名（默认 laya-multilingual）")
    args = ap.parse_args()

    home = Path(args.home) if args.home else default_home()
    manifest = load_manifest()
    if args.only not in {entry["name"] for entry in manifest["models"]}:
        ap.error(f"未知模型：{args.only}")
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
        if args.only in ("multilingual-e5-small", "embeddinggemma-2"):
            print("向量权重就绪。两个候选均下载完成后运行 scripts/benchmark_embeddings.py。")
        else:
            print("全部就绪。启动 sidecar：")
            script = "laya_server.py" if args.only == "laya-multilingual" else "verdict_server.py"
            print(f"  {sys.executable} sidecar/{script} --port 17870")
    return rc


if __name__ == "__main__":
    sys.exit(main())
