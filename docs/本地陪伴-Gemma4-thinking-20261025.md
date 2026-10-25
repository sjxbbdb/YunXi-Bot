# 本地陪伴默认切换到 Gemma 4 + thinking

状态：2026-10-25。人格保持原始版只读（38,249 字节，SHA-256 `7606512d...beccd`）。

## 为什么换回 Gemma 4

- Qwen3.5-9B 的 thinking 在 M2 Max + 完整人格下无法日常使用：4096 输出预算仍只思考、不输出正文，超过 4 分钟
- Gemma 4 26B-A4B 是 MoE，约 4B 激活；thinking 实测能正常结束
- Gemma 4 已通过工具调用验证
- 中文表达比 Qwen9 更自然，角色表达强度由人格决定

## 实测

| 场景 | 结果 |
|---|---|
| Gemma plain（短 prompt） | 0.6s |
| Gemma thinking（短 prompt） | 2.9s，正文和 reasoning_content 分离 |
| 完整人格 + thinking 普通聊天 | **37.0s** |
| 完整人格 + thinking + `memory_search` | **49.2s**，工具台账 allow/ReadOnly/ok |

默认启动（不设环境变量）已验证：route 为 Gemma 4，thinking 生效，工具目录存在。

## 策略

- 默认：Gemma 4 + thinking
- Qwen9：`YUNXI_COMPANION_MODEL=qwen` 可选；thinking 默认关（环境变量 `YUNXI_LOCAL_THINKING=1` 才开）
- Gemma thinking 可用 `YUNXI_LOCAL_THINKING=0` 关闭
- 内部证据标记（长期证据/联网能力）会在返回前剥离，避免模型复述给用户

## 外部事实控制

模型只当嘴，Rust 层继续保留：

- 明确名字事实表，身份直接应答
- 无长期记忆的“还记得/上次”直接答没有记录
- 无联网能力的实时问题直接答无法核实
- 工具真实调用并写台账

## 已知限制

- 完整人格下每轮 35–50 秒，仍是 prefill + thinking 成本
- 工具调用可能多轮，延迟更高
- 普通百科旧知识还未逐条核验
