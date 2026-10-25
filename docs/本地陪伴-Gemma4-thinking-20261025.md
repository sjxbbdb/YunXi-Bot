# 本地陪伴默认：Gemma 4 + thinking

状态：2026-10-25。人格保持原始版只读（38,249 字节，SHA-256 `7606512d...beccd`）。

## 结论

默认从 Qwen3.5-9B 切回 **Gemma 4 26B-A4B + thinking**。

原因：

- Qwen9 的 thinking 在 M2 Max + 完整人格下不可用：4096 预算仍只思考、不输出正文，超过 4 分钟
- Gemma 4 是 MoE（128 experts，top-8，约 4B 激活），thinking 能正常收尾
- Gemma 4 已实测工具调用
- 中文表达比 Qwen9 更自然，角色强度由人格决定

## 实测

| 场景 | 结果 |
|---|---|
| Gemma 短 prompt + thinking | 2.9s，正文和 reasoning_content 分离 |
| **默认启动，完整人格，thinking** | **35.3s** |
| **默认启动，完整人格，thinking + memory_search** | **48.8s**，工具台账 allow/ReadOnly/ok |
| Qwen9 对照 | 默认不 thinking；可用 `YUNXI_COMPANION_MODEL=qwen` 切回 |

默认启动不设环境变量，route 已是 Gemma 4，thinking 实际开启；route 事件也同步显示。

## 控制层

模型只当嘴：

- 名字事实表直接应答
- 无长期记忆的“记得/上次”直接拒绝编造
- 无联网能力的实时信息直接说无法核实
- 内部“长期证据/联网能力”标记从回复中剥离
- 工具真实调用写台账
- thinking：Gemma 默认开；Qwen 默认关；`YUNXI_LOCAL_THINKING=0` 可关 Gemma

## 取舍

- Gemma 完整人格 + thinking 每轮约 35–50 秒
- 工具调用可能多一次模型调用，延迟更高
- 普通百科旧知识仍未逐条核验；只硬拦截用户过去和实时信息
