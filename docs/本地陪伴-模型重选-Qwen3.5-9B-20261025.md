# 本地陪伴模型重选：Qwen3.5-9B

状态：2026-10-25，已接入、设为默认，工具调用端到端通过。

## 约束

- 中文日常聊天，要有人味，不要客服腔
- 参数比 Gemma 4 26B 小
- 本机 MLX 4bit，64GB Mac
- 最好原生工具调用
- 不使用无审查模型

## 社区候选

| 模型 | 体积 | 结论 |
|---|---:|---|
| **Qwen3.5-9B 4bit** | ~6GB | **选中**；中文原生、原生 function calling、MLX 成熟 |
| Qwen3.5-4B 4bit | ~3GB | 更小，但质量档不够，容易变成摘要/客服腔 |
| GLM-4.7-Flash 4bit | ~16.9GB | 中文强，但当前 `mlx_lm` 的 `glm4_moe_lite` 输出乱码，不可用 |
| MiniCPM5-2B | ~1.5GB | 太轻，日常陪伴质量不足 |
| Gemma 4 26B-A4B | ~15GB | 保留为快速档；不是“小参数”选择 |

Qwen3.5-9B 的 MLX 社区下载量约 2.8 万、收藏约 187，属于经过社区验证的版本。

## 实测

### 中文

普通中文输出正常，能接情绪和上下文；比 Gemma 更偏强角色化，当前人格下会更强势，这是 persona 的结果，不是模型不会聊。

### 工具调用

原始工具请求：

```text
输入：北京今天天气怎么样？请调用工具查。
结果：tool_calls: get_weather({"city":"北京"})
耗时：1.6s
```

端到端（chat-server + 只读工具 + 本地记忆）：

```text
输入：请用 memory_search 查一下我是否对花生过敏。
回复：查到了，你确实对花生过敏。
台账：tool_called memory_search / allow / ReadOnly / ok
```

## 接入修复

1. `local_llm_server.py` 删除旧的硬编码：“本地 Qwen 仅提供聊天，不接受工具”。
2. `mlx_backend.py` 增加 Qwen 的 `<function=...>` 工具解析，不再只解析 Gemma。
3. `ToolRunner` 对 Qwen 不再插入第二条 system 工具目录；Qwen 模板要求 system 只能在最前，工具定义已通过 `tools=` 传入。
4. `ModelAdapter` 的 Qwen 9B `tools=true`；默认陪伴模型改为 Qwen3.5-9B。
5. Gemma 仍保留，用 `YUNXI_COMPANION_MODEL=gemma` 显式切回。

## 取舍

- Qwen9 比 Gemma 慢：历史同机基准约首轮 17.6s、热轮 21.7s；Gemma 约 11.5/13.1s。
- 换来的是参数更小、中文原生、工具调用可用。
- 当前人格提示会放大 Qwen 的占有欲/角色化；要更自然，应调 persona，而不是再换模型。

## 结论

**Qwen3.5-9B 是当前“中文日常聊天 + 较小参数 + 真实工具调用”的最优已验证选择。** Gemma 保留为快速/自然语气档。
