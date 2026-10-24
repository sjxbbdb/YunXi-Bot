# Qwen3.8-27B 复测记录

状态：2026-10-24，**未完成当前链路的重新实测**；下载传输受阻，记录清楚避免误报。

## 先回答“为什么之前没测”

不是没测过。`docs/Mac本地模型实测.md` 的 2026-10-15 五模型评测包含 Qwen3.8-27B：
- 每模型 133 次请求，五模型共 665 次；
- Qwen3.8-27B：14.2 tok/s，工具匹配 24/24，MLX 峰值约 16.69 GB；
- 工具首答约 15.93s，响应代价明显高于 Gemma。

当时用户确认只保留 Gemma 4 26B-A4B 与 Qwen3.5-9B，Qwen3.8-27B 目录被删除。T9 基准因此只覆盖仍在磁盘上的两个生成模型。

## 这次重新下载

- 仓库：`mlx-community/Qwen3.8-27B-4bit`（约 16GB）。
- 第一次 Xet 传输 7 分钟只到 96MB，卡住；切 `HF_HUB_DISABLE_XET=1` 后恢复到约 3.7MB/s。
- 到约 608MB / 16GB 时暂停，预计还需约 70 分钟；没有完成加载、推理或当前链路测试。

续传命令：

```bash
cd "$HOME/Desktop/YunXi bot"
HF_HUB_DISABLE_XET=1 .venv-mlx/bin/hf download mlx-community/Qwen3.8-27B-4bit \
  --local-dir data/models/Qwen3.8-27B-4bit --max-workers 4
.venv-mlx/bin/python scripts/bench_companion_chain.py --model qwen38
```

适配器已预留 `YUNXI_COMPANION_MODEL=qwen38` / `qwen3.8` 映射；**工具暂时关闭**，等本地工具协议实测通过再开。`scripts/bench_companion_chain.py` 已加入 `qwen38` 选项。

## 为什么仍值得测

27B 的社区生态明显更完整：MLX 4/6/8bit、GGUF K/IQ、MTP 头、工具模板、微调/abliterated 衍生都多。若用 MTPLX 的原生 MTP，公开基准给到约 87.6 tok/s（M5 Max，非本机实测）；这可能同时解决 27B 的质量与延迟。但在 M2 Max 上必须自己测，不能拿公开数字当结论。

## 当前判断

- 在现有 Python MLX sidecar 下，27B 的历史实测是 14.2 tok/s，明显慢于 Gemma；不作为默认。
- 若下载完成且 MTPLX/MTP 可用，27B 应作为“本机质量档”候选，与 Gemma 对比首轮/热轮、工具调用、内存峰值。
- 未完成当前链路实测前，不把 27B 写进默认启动配置。
