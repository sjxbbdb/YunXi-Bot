#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""把决策模型权重打包成可发布的 zip，并更新 `models/manifest.json`（维护者用）。

为什么不用 git 存权重
--------------------
单个 `model.safetensors` 有 **448.8 MB**，超过 GitHub 的 **100 MiB 单文件硬上限**，
推送会被直接拒绝。而且二进制权重进 git 历史后无法回收，克隆体积永久膨胀。

所以权重走 **GitHub Release 资产**（单文件上限 2 GB，不进 git 历史），
仓库里只放本脚本、`manifest.json` 和模型卡。

用法
----
  python scripts/build_model_release.py                    # 打包 + 更新清单
  python scripts/build_model_release.py --upload           # 再上传到 Release
  python scripts/build_model_release.py --src <本地目录>    # 指定权重来源
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MANIFEST = REPO_ROOT / "models" / "manifest.json"
DIST = REPO_ROOT / "dist"
GITHUB_REPO = "sjxbbdb/YunXi-Bot"

for _s in (sys.stderr, sys.stdout):
    try:
        _s.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass


def hf_snapshot_dir(repo_id: str) -> Path | None:
    """在 HuggingFace 缓存里找某个仓库的最新快照目录。"""
    cache = Path(os.environ.get("HF_HOME") or (Path.home() / ".cache" / "huggingface"))
    slug = "models--" + repo_id.replace("/", "--")
    snaps = cache / "hub" / slug / "snapshots"
    if not snaps.is_dir():
        return None
    dirs = sorted((d for d in snaps.iterdir() if d.is_dir()), key=lambda d: d.name)
    return dirs[-1] if dirs else None


def sha256_of(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def build_zip(src: Path, out: Path) -> tuple[int, int]:
    """把 src 目录打成 zip（不带顶层目录，解压即可用）。"""
    files = sorted(f for f in src.rglob("*") if f.is_file())
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
        for f in files:
            zf.write(f, f.relative_to(src))
    return len(files), out.stat().st_size


def main() -> int:
    ap = argparse.ArgumentParser(description="打包决策模型权重")
    ap.add_argument("--src", default=None, help="权重来源目录（默认取 HF 快照）")
    ap.add_argument("--upload", action="store_true", help="打包后上传到 GitHub Release")
    ap.add_argument("--tag", default=None, help="Release 标签（默认用清单里的）")
    args = ap.parse_args()

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    rc = 0

    for entry in manifest["models"]:
        name = entry["name"]
        print(f"=== {name} ===")

        src = Path(args.src) if args.src else hf_snapshot_dir(entry["source_repo"])
        if src is None or not src.is_dir():
            print(f"  ✗ 找不到权重来源（{entry['source_repo']}）")
            print("    先运行一次 sidecar 让它下载，或用 --src 指定目录")
            rc = 1
            continue
        print(f"  来源: {src}")

        archive = entry["archive"]
        out = DIST / archive
        count, size = build_zip(src, out)
        digest = sha256_of(out)
        print(f"  ✓ 打包 {count} 个文件  {size / 1e6:.1f} MB")
        print(f"    {archive}")
        print(f"    sha256 {digest}")

        # 把模型卡一并存入仓库（第三方许可与归属声明）
        for cand in ("README.md", "LICENSE", "LICENSE.txt"):
            f = src / cand
            if f.is_file():
                dest = REPO_ROOT / "models" / f"{name}-{cand}"
                shutil.copy2(f, dest)
                print(f"  已归档 {dest.name}")

        entry["sha256_archive"] = digest
        entry["size_bytes"] = size
        entry["file_count"] = count
        entry["revision"] = entry.get("revision") or src.name

    MANIFEST.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print()
    print(f"清单已更新: {MANIFEST}")

    if args.upload:
        tag = args.tag or manifest["models"][0].get("release_tag") or "models-v1"
        assets = [str(DIST / e["archive"]) for e in manifest["models"]]
        print()
        print(f"=== 上传到 Release {tag} ===")
        # 先看 Release 是否存在；不存在就创建
        probe = subprocess.run(
            ["gh", "release", "view", tag, "--repo", GITHUB_REPO],
            capture_output=True,
            text=True,
        )
        if probe.returncode != 0:
            print(f"  Release {tag} 不存在，创建 ...")
            created = subprocess.run(
                [
                    "gh", "release", "create", tag,
                    "--repo", GITHUB_REPO,
                    "--title", f"决策模型权重 {tag}",
                    "--notes",
                    "决策模型的编码器权重。单个 model.safetensors 有 448.8MB，"
                    "超过 GitHub 的 100MiB 单文件上限，因此作为 Release 资产分发，不进 git 历史。\n\n"
                    "用 `python scripts/fetch_model.py` 下载并校验 SHA256。",
                ],
                capture_output=True,
                text=True,
            )
            if created.returncode != 0:
                print("  ✗ 创建失败:", created.stderr.strip())
                return 1
        up = subprocess.run(
            ["gh", "release", "upload", tag, *assets, "--repo", GITHUB_REPO, "--clobber"],
            capture_output=True,
            text=True,
        )
        if up.returncode != 0:
            print("  ✗ 上传失败:", up.stderr.strip())
            return 1
        print("  ✓ 上传完成")
        print(f"    https://github.com/{GITHUB_REPO}/releases/tag/{tag}")

    return rc


if __name__ == "__main__":
    sys.exit(main())
