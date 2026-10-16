"""下载固定 revision 的候选模型；可断点续传，SHA256 通过后才标记可测。"""
import concurrent.futures
import argparse
import json
import os
import subprocess
from pathlib import Path

from fetch_model import REPO_ROOT, sha256_of

def verify(model, dest):
    for file in model["files"]:
        path = dest / file["path"]
        if not path.is_file() or path.stat().st_size != file["size"]:
            raise ValueError(f"文件缺失或大小不符：{path.name}")
        if file.get("sha256") and sha256_of(path) != file["sha256"]:
            raise ValueError(f"SHA256 不符：{path.name}")


def download(model, home=None):
    from fetch_model import download as fetch_file

    name = model["repo_id"].split("/")[-1]
    home = Path(home) if home is not None else REPO_ROOT / "data"
    dest = home / "models" / name
    dest.mkdir(parents=True, exist_ok=True)
    marker = dest / "benchmark-verified.json"
    marker.unlink(missing_ok=True)
    print(f"START {name}: {model['bytes'] / 1e9:.2f} GB", flush=True)
    transfers = []
    for file in model["files"]:
        path = dest / file["path"]
        if path.is_file() and path.stat().st_size == file["size"]:
            if not file.get("sha256") or sha256_of(path) == file["sha256"]:
                continue
        suffix = f"/{model['repo_id']}/resolve/{model['revision']}/{file['path']}?download=true"
        if file.get("sha256"):
            transfers.append(file.get("mirror_url", "https://hf-mirror.com" + suffix) + "\n"
                             + f" dir={dest}\n out={file['path']}\n"
                             + f" checksum=sha-256={file['sha256']}\n")
        elif not fetch_file("https://huggingface.co" + suffix, path):
            raise RuntimeError(f"官方配置文件下载失败：{file['path']}")
    if transfers:
        jobs = home / "benchmarks/downloads"
        jobs.mkdir(parents=True, exist_ok=True)
        inputs = jobs / f"{name}.txt"
        inputs.write_text("\n".join(transfers))
        # 镜像只用于有官方 SHA256 的文件；aria2 校验通过前不作为就绪模型。
        env = {k: v for k, v in os.environ.items() if k.lower() not in
               ("http_proxy", "https_proxy", "all_proxy")}
        with (jobs / f"{name}.log").open("a") as log:
            subprocess.run(["aria2c", "--input-file=" + str(inputs), "--continue=true",
                            "--check-integrity=true", "--auto-file-renaming=false",
                            "--split=8", "--max-connection-per-server=8",
                            "--max-concurrent-downloads=4", "--min-split-size=4M",
                            "--file-allocation=none", "--summary-interval=30",
                            "--console-log-level=warn", "--max-tries=5", "--retry-wait=3",
                            "--connect-timeout=20", "--timeout=60"],
                           env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    verify(model, dest)
    marker.write_text(json.dumps({"revision": model["revision"], "bytes": model["bytes"]}) + "\n")
    print(f"VERIFIED {name}", flush=True)
    return name


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--model", help="只准备指定模型短名；省略则下载原五个参测候选")
    ap.add_argument("--home", type=Path, default=REPO_ROOT / "data")
    args = ap.parse_args()
    manifest = json.loads((REPO_ROOT / "models/local-benchmark-manifest.json").read_text())
    # 下载可并行；推理脚本仍然只加载一个模型。
    models = sorted(manifest["models"], key=lambda model: model["bytes"])
    if args.model:
        models = [m for m in models if m["repo_id"].split("/")[-1] == args.model]
        if not models:
            ap.error("模型名不在固定版本清单中")
    failed = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = {pool.submit(download, model, args.home): model for model in models}
        for future in concurrent.futures.as_completed(futures):
            model = futures[future]
            try:
                future.result()
            except Exception as exc:
                failed.append(model["repo_id"])
                print(f"FAILED {model['repo_id']}: {type(exc).__name__}: {exc}", flush=True)
    return int(bool(failed))


if __name__ == "__main__":
    raise SystemExit(main())
