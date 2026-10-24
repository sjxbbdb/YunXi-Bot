# Qwen3.8-27B Uncensored 本地实测

状态：2026-10-24，已下载、加载并完成首轮/热轮基准；未做 UI 端到端和拒答行为评测。

## 模型

- 仓库：`orcarouter/Qwen3.8-27B-Uncensored-MLX`
- 基座：`Qwen/Qwen3.8-27B`
- 许可证：Apache-2.0
- 量化：4-bit affine，group_size=64
- 体积：约 15GB
- 标签：uncensored / abliterated / vision / function-calling / MLX
- 说明：社区已有 59.5k 下载、1.5k 收藏，是当前较主流的 27B uncensored MLX 版本。
- 仓库是多量化目录；4-bit 的配置在 `4-bit/`，权重在根目录。已合成为 `data/models/Qwen3.8-27B-Uncensored-4bit`。

## 基准

脚本：`scripts/bench_companion_chain.py --model qwen38u`
条件：M2 Max 64GB，当前核心人格 + 画像 + local_chat 协议，`max_tokens=48`，thinking 关闭。

| 指标 | 结果 |
|---|---:|
| 模型加载 | 4.82s |
| 首轮 | **64.3s** |
| 热轮 | **68.28s** |
| 首轮 prompt tokens | 5,531 |
| 热轮 prompt tokens | 5,598 |

结果文件：`data/benchmarks/companion-chain-1792844402.json`。

## 结论

- 它能加载、能生成，但是**当前 sidecar 不可用于日常陪伴**：每轮 60 秒以上，比 Gemma 的 11–13 秒差一个量级。
- 慢的主因是 27B dense 的 prefill（约 5.5k token）和当前 MLX 路径没有有效前缀复用；不是 uncensored 本身导致的。
- MTP/MTPLX 只加速 decode，**不能单独解决 prefill**；需要 prompt cache 或更短本地画像/前缀，才可能把 27B 拉进可用区间。
- 暂不设为默认；保留 `YUNXI_COMPANION_MODEL=qwen38u` 作为可选私密模型。工具保持关闭，仅本地陪伴、不联网、不写文件。

## 安全边界

- 只用于本地私密陪伴；不挂 `run_command` / 文件写入 / 邮件发送 / 电脑操控工具。
- 应用层审批、隐私边界和工具策略不因模型 uncensored 而放宽。
- 未做有害内容、自伤、违法请求的行为测试；不能把“无审查”当成“无风险”。

## 下一步

1. 若坚持 27B：先做 prompt cache / 本地画像压缩，把 5.5k token 前缀降到 2k 左右。
2. 若要无审查 + 速度：试 uncensored Gemma 4 26B 或 9B 级模型，27B 作为质量档。
3. 若要 27B 的 MTP 速度：装 MTPLX，做同机 decode 对照；仍需解决 prefill。
