"""下载本地小模型。**按 `YUNXI_BOT_HOME` 落盘**——和决策模型同一个规矩，
这样便携版整个 `data/` 目录搬走就能用。

## 为什么要单独一个脚本而不是让人手敲一条命令

1. **国内到 HuggingFace 常常不通**，要能自动回落镜像。手敲的话每次都得
   记得加 `HF_ENDPOINT`，而忘了的表现是"卡住不动"，看不出是网络问题。
2. **模型名要能改**：这一轮的重点是把槽位做成配置项，选型不该写死在代码里。
3. 下载完**要能验证**：权重不完整的话，加载时才报错，而那时候
   已经浪费了一次调试。

用法：
    python scripts/fetch_local_model.py                    # 默认 Qwen/Qwen3-1.7B
    python scripts/fetch_local_model.py Qwen/Qwen3-4B      # 换一个
    python scripts/fetch_local_model.py --home <dir>       # 指定 home
"""

import argparse
import os
import sys
from pathlib import Path

# 国内访问 HuggingFace 的常用镜像。**顺序即优先级**：
# 官方先试，不通再走镜像——镜像的完整性不保证，能直连就别绕。
ENDPOINTS = ["https://huggingface.co", "https://hf-mirror.com"]


def default_home() -> Path:
    """和 `verdict_server.py:119` 同一套解析，**一处都不能多**。"""
    env = os.environ.get("YUNXI_BOT_HOME")
    if env:
        return Path(env)
    local = os.environ.get("LOCALAPPDATA")
    if local:
        return Path(local) / "YunXiBot"
    return Path.home() / ".yunxi-bot"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("model", nargs="?", default="Qwen/Qwen3-1.7B")
    ap.add_argument("--home", default=None)
    args = ap.parse_args()

    try:
        from huggingface_hub import snapshot_download
    except ImportError:
        print("需要 huggingface_hub。先跑： scripts/setup.ps1", file=sys.stderr)
        return 2

    home = Path(args.home) if args.home else default_home()
    # 目录名取仓库名的后半段：`Qwen/Qwen3-1.7B` → `Qwen3-1.7B`
    name = args.model.split("/")[-1]
    target = home / "models" / name

    # **不许悄悄下到系统盘。**
    #
    # 这次踩过：一路都用默认路径（`%LOCALAPPDATA%`），结果 15 GB
    # 落在 C 盘、把系统盘塞满，**而过程里一个提示都没有**。
    # 所以这里不是"禁止"，是"说出来"——下 5-10 GB 之前，
    # 人至少该知道它要去哪。
    if str(target).upper().startswith("C:"):
        print(f"[下载] ⚠️  目标是系统盘：{target}")
        print(f"[下载]    模型动辄 5-10 GB。想换位置就设")
        print(f"[下载]    YUNXI_BOT_HOME 到一个非 C 盘的目录。")
        print()

    target.mkdir(parents=True, exist_ok=True)

    # **不要下那些和推理无关的大文件。** safetensors 之外的东西
    # 动辄几百 MB，而它们在加载时一点用都没有。
    ignore = ["*.h5", "*.msgpack", "*.onnx", "*.tflite", "*.ot",
              "original/*", "*.gguf"]

    last = None
    for ep in ENDPOINTS:
        os.environ["HF_ENDPOINT"] = ep
        print(f"[下载] 试 {ep} → {target}")
        try:
            path = snapshot_download(
                repo_id=args.model,
                local_dir=str(target),
                ignore_patterns=ignore,
                # **断点续传**：下到一半断了重跑不该从头再来。
                resume_download=True,
                max_workers=4,
            )
            n = sum(1 for _ in target.rglob("*") if _.is_file())
            size = sum(f.stat().st_size for f in target.rglob("*") if f.is_file())
            print(f"[下载] 完成：{n} 个文件，{size / 1024 / 1024:.1f} MB")
            print(f"[下载] 位置：{path}")
            print()
            print("下一步：")
            print(f"    python sidecar/local_llm_server.py --model {name}")
            return 0
        except Exception as e:  # noqa: BLE001 —— 换端点重试是这里的正常流程
            last = e
            print(f"[下载] {ep} 失败：{type(e).__name__}: {e}", file=sys.stderr)

    print(f"[下载] 两个端点都失败了，最后一个错：{last}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
