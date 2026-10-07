#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""端到端脚本的公共部分。

## 为什么有一个"先重建二进制"的函数

端到端脚本调的是 `target/debug/yunxi-bot.exe`。而 **`cargo clippy` 和
`cargo test` 都不会刷新那个文件**——它们各自产出自己的中间物。

于是有一个很难发现的陷阱：改完代码跑 `cargo clippy`（干净）+
`cargo test`（全绿），接着跑端到端，**实际验的是上一次 build 的旧二进制**。
结果是要么假通过，要么报一个已经修掉的 bug——而两种都会让人往错的方向查。

这个坑真踩过一次：`NoticeSent` 缺字段的 bug 明明修了，
端到端却还在报旧行为。所以这里**主动重建**。多花十几秒，
比一个假结果便宜得多。
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent


def real_home_dir() -> Path:
    """真实运行目录。**和产品同一套顺序**：`YUNXI_BOT_HOME` → `<repo>/data` → `%LOCALAPPDATA%\\YunXiBot`。

    ## 为什么要从环境变量读，而不是写死 `%LOCALAPPDATA%`

    2026-10-07 运行目录从 `%LOCALAPPDATA%\\YunXiBot` 搬到了 `<repo>\\data`
    （`启动.cmd` / `启动决策模型.cmd` 把 `YUNXI_BOT_HOME` 指过去）。
    而十几个端到端/压力脚本里写死的是旧路径：

        real_home = Path(os.environ.get("LOCALAPPDATA", "")) / "YunXiBot"

    搬家之后那一行指向一个**不存在的目录**（本机上
    `C:\\Users\\<用户>\\AppData\\Local\\YunXiBot` 已经不在），于是：

    - 十三个脚本的 `if not (real_home / "secrets" / "agnes.key").exists()`
      一律成立 → 它们**当场 exit 2，一条检查都没跑**
    - `e2e_allmodules.py` 没有那道门 → 密钥一个也借不到，任务跑不起来
    - `stress_cost.py` 读的台账不存在 → 报"没有台账"

    **共同点是"这些脚本再也不会失败，因为它们根本不再运行"**——
    和一条永远为真的断言是同一个后果。

    ## `YUNXI_BOT_HOME` 优先，而且不做存在性检查

    产品（`crates/yunxi-bot-core/src/lib.rs` 的 `default_home()`）也是这么做的：
    设了就照用。这里跟着它走，**否则脚本会去一个和产品不一样的地方借密钥**，
    而那种不一致本身就是"测试测的不是生产"。

    `<repo>/data` 那一层要求目录真的存在（否则退到 `%LOCALAPPDATA%\\YunXiBot`）：
    它是"启动.cmd 已经把家搬过来了"的证据，而不存在的目录没什么可比的。
    """
    env = (os.environ.get("YUNXI_BOT_HOME") or "").strip()
    if env:
        return Path(env)
    repo_data = REPO / "data"
    if repo_data.is_dir():
        return repo_data
    local = os.environ.get("LOCALAPPDATA")
    if local:
        return Path(local) / "YunXiBot"
    return Path.home() / ".yunxi-bot"


# 真实 home 里**不参与比对**的目录。见 `snapshot_home`。
_SKIP_DIRS = {"models", ".cache"}


def snapshot_home(home: Path) -> dict[str, tuple[int, int]]:
    """记下真实 home 里每个文件的大小与修改时刻（纳秒），键是相对路径。

    **跳过 `models/` 和 `.cache/`**：前者是十几 GB 的权重，列一遍要几十秒，
    而"测试会去下一个模型"不是这条断言要防的事。相对路径做键，
    所以目录整体搬家不影响比对。

    home 不存在时返回空字典——**"不存在"本身也是一个可比的起点**：
    测试要是把它建出来了，`home_untouched` 会报"新增"。
    """
    out: dict[str, tuple[int, int]] = {}
    if not home.is_dir():
        return out
    stack = [home]
    while stack:
        d = stack.pop()
        try:
            entries = list(os.scandir(d))
        except OSError:
            continue
        for e in entries:
            try:
                if e.is_dir(follow_symlinks=False):
                    if e.name in _SKIP_DIRS:
                        continue
                    stack.append(Path(e.path))
                elif e.is_file(follow_symlinks=False):
                    st = e.stat(follow_symlinks=False)
                    out[str(Path(e.path).relative_to(home))] = (st.st_size, st.st_mtime_ns)
            except OSError:
                continue
    return out


def home_untouched(before: dict[str, tuple[int, int]], home: Path) -> tuple[bool, str]:
    """**证明测试没有碰真实 home。** 返回 `(没动过?, 给人看的一句话)`。

    ## 为什么要有这条

    "测试用临时 home、不碰真实数据"是所有端到端脚本的共同前提，
    而在此之前**没有任何一条断言证明它**——`real_home` 只被用来借密钥。
    于是只要哪条链路真的写回了真实 home（`YUNXI_BOT_HOME` 没传下去、
    某处回落到默认目录、sidecar 各算一遍 home），**不会有任何东西变红**。

    这条断言的方式是"跑之前记一份、跑完再记一份、逐文件比大小和修改时刻"，
    所以它**会**因为下面任何一件事失败：

    - 真实 home 里多出一个文件（比如 `sessions/` 里多了一个会话）
    - 任何一个文件被追加/截断（`ledger.jsonl` 变大是最典型的一种）
    - 任何一个文件被删掉

    ## 它证明不了什么

    只比对大小和修改时刻，所以"内容被改成了同样字节数、而且 mtime 被还原"
    这类**刻意伪造**它看不出来。这不是那条要防的东西：要防的是
    "某条链路顺手写回了真实 home"，而那一定会动这两个量里的一个。
    """
    after = snapshot_home(home)
    added = sorted(set(after) - set(before))
    removed = sorted(set(before) - set(after))
    changed = sorted(k for k in set(before) & set(after) if before[k] != after[k])
    if not (added or removed or changed):
        return True, f"真实 home 一个字节没动（比对 {len(after)} 个文件：{home}）"
    parts = []
    if added:
        parts.append(f"新增 {len(added)} 个（{', '.join(added[:3])}）")
    if changed:
        parts.append(f"改动 {len(changed)} 个（{', '.join(changed[:3])}）")
    if removed:
        parts.append(f"删除 {len(removed)} 个（{', '.join(removed[:3])}）")
    return False, f"**真实 home 被动过**：{'；'.join(parts)}——{home}"


def binary_path() -> Path:
    p = REPO / "target" / "debug" / "yunxi-bot.exe"
    return p if p.exists() else REPO / "target" / "debug" / "yunxi-bot"


def ensure_fresh_binary(quiet: bool = False) -> Path:
    """重建并返回二进制路径。**失败直接退出**，绝不退回用旧的。"""
    if not quiet:
        print("重建二进制（避免验到旧版本）…", flush=True)
    r = subprocess.run(
        ["cargo", "build", "--workspace"],
        cwd=str(REPO), capture_output=True, text=True, encoding="utf-8",
    )
    if r.returncode != 0:
        print("cargo build 失败，端到端没法跑：", file=sys.stderr)
        print((r.stderr or "")[-3000:], file=sys.stderr)
        raise SystemExit(2)
    b = binary_path()
    if not b.exists():
        print(f"构建成功但找不到二进制 {b}", file=sys.stderr)
        raise SystemExit(2)
    return b
