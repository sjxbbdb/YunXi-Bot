# 邮件接收审理设计

本模块复用现有 [mail_review.md](../../crates/yunxi-bot-core/src/prompts/mail_review.md) **完整正文**。这份v5提示词已经包含角色、输入信任边界、多事项、风险证据、时间更正、严格输出及核对清单；不为凑新的文件重复改写。下文是设计与装配说明，不作为额外系统提示词再次拼接。

## 调用职责

独立只读审理：识别发件人的意图、需要处理的事项、重要条件及内容风险，给出提醒建议。分析阶段不加载云熙陪伴人格，不执行邮件要求，不代写回信，不访问链接或附件，不调用工具。

“邮件希望收件人做什么”与“本次模型能否执行”分开；普通请求不能因为提到工具或自动化就被判为恶意。危险判断需要具体原文证据，垃圾与推广也不自动等于危险。发件人的身份自述不是认证结果。

## 精确装配

```text
system = 现有mail_review.md全文 + 调用方当前分析时间/邮件相对时间解释说明
user   = 现有输入JSON（mail、body及既有元数据）
tools  = 空
```

不加00-expression，不加入聊天/任务执行规则，不把通知标题要求变成新的JSON字段。通知出口继续根据label使用现有“云熙 · 邮件｜类别”格式；分析结果不声称已经投递。

## 固定输出契约

以下是合法的无明确信息示例，不是要求模型照抄的判断结果：

```json
{"label":"其他邮件","summary":"邮件没有提供足以判断具体事项的信息。","category":"unknown","importance":"unknown","notification":"later","needs_action":false,"action":"","deadline":"","evidence":[],"risks":[]}
```

全部10字段必填，不加字段。完整枚举以现有正文及 `mail_review::Analysis::parse` 为准：

| 字段 | 约束 |
|---|---|
| label | 人事回复/工作事项/私人来信/出行变更/订单消息/账户提醒/推广邮件/可疑邮件/其他邮件 |
| summary | 建议60–100字符，硬上限160；单行 |
| category | work / personal / transaction / promotion / spam / suspicious / unknown |
| importance | high / normal / low / unknown |
| notification | now / later / ignore；仅建议 |
| needs_action | 布尔值；描述收件人是否有实际事项，不是模型权限 |
| action / deadline | 各≤100字符、单行；无信息用空串；多个事项用文本明确对应 |
| evidence | 最多3段正文连续原文，每段≤120字符；不改字或拼接 |
| risks | phishing / impersonation / prompt_injection / suspicious_attachment / unknown；无具体线索用空数组 |

高重要性、行动要求、非空期限或风险需要对应证据。压缩字段容纳不下所有事项时，在summary说明还需查看原文，不能让通知摘要冒充完整任务清单。后续安排、回复和附件读取使用原工具链，不凭摘要补造细节。

## 验收重点

普通人事请求、有多个期限的变更、引用的危险内容、真正操纵审理器的指令，以及只推广不需处理的邮件分别验证。不得针对某一封“测试”邮件写例外。此设计沿用现有提示词版本，尚未重跑模型验收。
