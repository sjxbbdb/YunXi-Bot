//! 任务拆解：把人类原话变成一组带依赖的步骤。
//!
//! **拆解本身是一次模型调用**，用的是完整系统提示词 + 人格 + 任务。
//! 拆解的质量直接决定后面所有步骤的质量，所以这一步值得用能推理的模型。
//!
//! ## 为什么解析要容错，但不能猜
//!
//! 模型很少老老实实只回一段 JSON：它会在前面加一句"我的拆解如下"，
//! 或者顺手包一层 Markdown 围栏。只按整段解析会把一次已经花掉的调用白白作废，
//! 所以这里按"整段 → 代码块 → 最外层括号子串"三种位置各试一次。
//!
//! 但只认**位置**，不认**语法**：补引号、删尾逗号这类"修复"成功一次，
//! 就会把一次结构错误的模型输出伪装成正常结果，问题被推迟到执行期才爆——
//! 那时代价高得多（见 AGENTS.md §2.3：非法输入不静默降级）。
//!
//! ## 为什么 id、指令、依赖在这里就要查
//!
//! JSON 合法不等于计划可用。重复 id 会让 `depends_on` 指向两义；空白指令
//! 会变成一个模型无从下手的步骤；依赖引用了不存在的 id 或者成环，在状态机里
//! 的表现是"一直有步骤没完成"，而不是一个错误。最后一项交给
//! [`validate_dependencies`]：它是纯函数，在这里查最便宜。

use std::collections::BTreeSet;

use serde_json::Value;

use crate::think::prompt::build_persona;
use crate::think::{Message, TaskKind, ThinkRequest};

use super::model::{Budget, Step, TaskError, validate_dependencies};

/// 报错时随原文带走的字符数上限。
///
/// 带原文是为了能排查，但把整段模型输出（可能几千字）塞进错误对象，
/// 会让它在日志和台账里都没法看。
const RAW_HEAD_CHARS: usize = 300;

/// 拆解请求里的人格名。
///
/// [`build_persona`] 需要一个名字，而 [`planner_request`] 的签名里没有它——
/// 名字是产品级常量，不该由每个调用点各传一份：传法一多，前缀就会在不同调用点
/// 之间漂移，缓存全部落空。写死在这里，跨调用天然稳定。
///
/// TODO：人格像其他配置一样外置（`.md` + CLI 参数）之后，这个名字应该从配置读，
/// 而不是留在代码里；现在硬编码是为了不让 planner_request 的签名再多一个参数。
const PLANNER_NAME: &str = "云熙";

/// 拆解出的一步（还没落成 [`super::model::Step`]）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlannedStep {
    pub id: String,
    pub instruction: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 步骤类型。模型可以不填，由 Rust 侧按文本兜底判。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TaskKind>,
}

/// 解析模型返回的步骤清单。
///
/// 分四步：取 JSON → 拆出步骤数组 → 查可用性（id / 指令 / 步数）→ 查依赖图。
/// **四步都通过才返回**；任何一步失败都是"这次拆解不可用"，
/// 由调用方决定重试还是升级人工，而不是丢一个半成品给执行层。
pub fn parse_plan(raw: &str) -> Result<Vec<PlannedStep>, TaskError> {
    let value = extract_json(raw).map_err(|detail| unparsable(detail, raw))?;
    let steps = steps_from_value(value).map_err(|detail| unparsable(detail, raw))?;
    check_usable(&steps, raw)?;

    // 依赖图交给 model 的纯函数查：它同时管"引用了不存在的步骤"和"成环"，
    // 错误原样传出，不在这一层再包一遍——外面按 TaskError 的变体决定要不要人工介入。
    //
    // 转换用与执行层同一个函数：两处各写一遍，"kind 缺省怎么办"这种细节迟早漂移，
    // 变成"校验时一种解释、执行时另一种"。
    validate_dependencies(&planned_to_steps(&steps))?;
    Ok(steps)
}

/// 组装拆解用的提示词。
///
/// 返回**整段**提示词：人格与硬规则（[`build_persona`]）+ 稳定指令块 + 目标。
/// 前两段与 [`planner_request`] 的系统消息逐字节相同，目标落在最后——
/// 位置与 planner_request 的用户消息一一对应（有测试盯着这个等式）。
///
/// 需要把拆解提示词打成一条字符串（排查、单条消息的调用）时用它；
/// **真正发起调用请用 [`planner_request`]**：那里目标在独立的消息里，
/// 前缀不随目标变化，缓存才可能命中。
pub fn plan_prompt(persona: &str, rules: &str, goal: &str, max_steps: u32) -> String {
    let mut out = stable_prefix(persona, rules, max_steps);
    out.push_str("\n# 本次目标\n\n");
    out.push_str(goal);
    out.push('\n');
    out
}

/// 组装拆解请求。
///
/// 前缀（系统消息）只由人格、硬规则和稳定指令块组成，**目标单独作为用户消息**。
/// 顺序不能变：目标一旦混进前缀，换个目标前缀就不同，已建好的缓存全部作废
/// （见 `think::prompt` 的模块文档——那是这一层存在的唯一理由）。
pub fn planner_request(
    persona: &str,
    rules: &str,
    goal: &str,
    max_steps: u32,
    max_tokens: u32,
) -> ThinkRequest {
    // 刻意不调 with_thinking：思考模式是任务的属性，由路由按 TaskKind::Planning 决定。
    // 在这里写死会让"按任务类型开思考"的规则失效，也会让不支持思考的端点收到错字段。
    ThinkRequest::new(vec![
        Message::system(stable_prefix(persona, rules, max_steps)),
        Message::user(goal),
    ])
    .with_max_tokens(max_tokens)
}

/// 拆解请求的稳定前缀：人格 + 硬规则 + 拆解指令。
///
/// 这三段都不含目标，所以只有人格、规则或步数上限变化时前缀才变。
fn stable_prefix(persona: &str, rules: &str, max_steps: u32) -> String {
    format!(
        "{}{}",
        build_persona(PLANNER_NAME, persona, &rule_lines(rules)),
        plan_instructions(max_steps)
    )
}

/// 把多行规则文本切成 [`build_persona`] 要的规则列表。
///
/// 一行一条，丢掉空行：空行会变成一条编号占位却没有内容的"硬规则"，
/// 白白占前缀字节，还会把编号和真实规则错开。
fn rule_lines(rules: &str) -> Vec<String> {
    rules
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// 拆解指令块：只说"怎么输出"，不含人格，也不含目标。
///
/// 上限写进提示词是**提前劝住**，不是约束——提示词拦不住模型，
/// 所以 [`parse_plan`] 还会照着同一个上限再卡一次。
fn plan_instructions(max_steps: u32) -> String {
    let mut out = String::new();
    out.push_str("\n\n# 任务拆解\n\n");
    out.push_str("把目标拆成可独立执行、可验收的步骤，只输出 JSON，不要写解释或寒暄。\n\n");
    out.push_str("## 输出格式\n\n");
    out.push_str("推荐 {\"steps\":[...]}，也接受直接输出数组。每个步骤形如：\n\n");
    out.push_str(
        "{\"id\":\"s1\",\"instruction\":\"做什么\",\"depends_on\":[],\"kind\":\"edit\"}\n\n",
    );
    out.push_str("## 字段规则\n\n");
    out.push_str("- id：非空且唯一，用 s1、s2 这类短标识；\n");
    out.push_str(
        "- instruction：写清做到什么算完成，不要写\"继续\"\"处理一下\"这种没法验收的话；\n",
    );
    out.push_str(
        "- depends_on：只能引用本次清单里出现过的 id；没有依赖就写空数组，不要写 null；\n",
    );
    out.push_str(
        "- kind：可选，取值 conversation / lookup / generation / edit / analysis / planning / execution；\
         拿不准就省略，由本地按指令文本判定。\n\n",
    );
    out.push_str("## 步数\n\n");
    out.push_str(&format!(
        "最多 {max_steps} 步。一步能做完就不要拆两步；目标本身很简单时，只给一步也是正确答案。\n"
    ));
    out
}

/// 不可解析时的统一错误构造。
fn unparsable(detail: impl Into<String>, raw: &str) -> TaskError {
    TaskError::UnparsablePlan {
        detail: detail.into(),
        raw: raw_head(raw),
    }
}

/// 原文开头。只留开头，够定位是哪一类回复，又不至于把日志撑爆。
fn raw_head(raw: &str) -> String {
    raw.chars().take(RAW_HEAD_CHARS).collect()
}

/// 从模型回复里取出 JSON。
///
/// 候选片段按"离原文最近"排序：整段 → 代码块 → 代码块里的最外层容器 →
/// 整段里的最外层容器。全部失败时报出**试图解析了几个片段**和首个错误：
/// 只说"解析失败"的报错，拿到日志的人还得自己再猜一次。
fn extract_json(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Err("模型回复为空".to_string());
    }

    let mut tried = 0usize;
    let mut first_error = String::new();
    for candidate in json_candidates(raw) {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            continue;
        }
        tried += 1;
        match serde_json::from_str::<Value>(candidate) {
            Ok(value) => return Ok(value),
            Err(e) => {
                if first_error.is_empty() {
                    first_error = e.to_string();
                }
            }
        }
    }

    if tried == 0 {
        return Err("回复里找不到任何 JSON 片段".to_string());
    }
    Err(format!(
        "{tried} 个候选片段都没能解析成 JSON；首个错误：{first_error}"
    ))
}

/// JSON 候选片段。
///
/// 整段放最前面：它是正常回复的形态，先试它能让正常路径只解析一次。
fn json_candidates(raw: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(4);
    out.push(raw);
    for block in fenced_blocks(raw) {
        out.push(block);
        out.extend(container_slices(block));
    }
    out.extend(container_slices(raw));
    out
}

/// 取出 markdown 代码围栏（三个反引号）里的内容。
///
/// 未闭合的围栏取到结尾而不是放弃：少一个反引号是模型常见的笔误，
/// 内容本身可能完全正确；是不是合法 JSON 交给解析器判，不在这里替它下结论。
fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        match after.find("```") {
            Some(close) => {
                out.push(&after[..close]);
                rest = &after[close + 3..];
            }
            None => {
                out.push(after);
                break;
            }
        }
    }
    out
}

/// 文本里所有"从某个 `{` 或 `[` 开始、括号闭合"的子串。
///
/// 不是只取第一个：说明文字里可能先出现别的花括号（比如"第 {n} 步"），
/// 只取第一个就会把真正的 JSON 挡在后面，白费一次调用。
fn container_slices(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (start, byte) in text.bytes().enumerate() {
        if matches!(byte, b'{' | b'[')
            && let Some(slice) = balanced_from(text, start)
        {
            out.push(slice);
        }
    }
    out
}

/// 从 `start`（必须是 `{` 或 `[`）开始找配对的闭合括号。
///
/// 逐字节扫描并跟踪字符串状态，而不是"第一个开括号到最后一个闭括号"：
/// 后者会把后面的说明文字一起吞进来，而括号出现在字符串里时又会提前截断。
fn balanced_from(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let open = *bytes.get(start)?;
    let close = match open {
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                // 起点就是开括号，所以这里 depth 至少是 1，不会下溢
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// 从解析出来的 JSON 里取出步骤数组。
///
/// 顶层两种形态都收：`{"steps":[...]}` 和裸数组。前者是提示词里推荐的写法，
/// 后者是模型常见的省略写法——两种都拒会白费一次调用，收下来不影响正确性。
fn steps_from_value(value: Value) -> Result<Vec<PlannedStep>, String> {
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut map) => match map.remove("steps") {
            Some(Value::Array(items)) => items,
            Some(other) => {
                return Err(format!("steps 字段不是数组，而是{}", value_kind(&other)));
            }
            None => return Err("顶层对象里没有 steps 字段".to_string()),
        },
        other => {
            return Err(format!(
                "顶层既不是数组也不是对象，而是{}",
                value_kind(&other)
            ));
        }
    };

    let mut steps = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        // 逐个反序列化：整段一起转的报错只指向"某个地方"，定位不到是哪一步写错的
        let step = serde_json::from_value::<PlannedStep>(item)
            .map_err(|e| format!("第 {} 个步骤的字段不合法：{e}", index + 1))?;
        steps.push(step);
    }
    Ok(steps)
}

/// JSON 值的类型名。报错时说清楚"拿到的是什么"，比只说"类型不对"有用。
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

/// 语法之外的可用性检查：**JSON 合法不等于计划能用**。
///
/// 放在这里而不是留给执行层，是因为同一类错误的代价差一个数量级：
/// 现在报出来只需重新拆解一次，进了执行层就会变成"任务卡住"或"悄悄空转"。
fn check_usable(steps: &[PlannedStep], raw: &str) -> Result<(), TaskError> {
    if steps.is_empty() {
        // 0 步的任务永远跑不起来（model::Task::all_settled 对空集合返回 false），
        // 静默接受等于造一个不会报错的空转
        return Err(unparsable(
            "步骤清单是空的：拆解至少要给出一步，否则任务永远跑不起来",
            raw,
        ));
    }

    // 上限取默认预算。签名里没有 limit，这里只能兜底：引擎拿到清单后还会用
    // 真实预算再卡一次（见 model::BudgetLedger::steps_allowed），
    // 这一步的作用是拆解失控时尽早止损，而不是替代预算。
    let limit = Budget::default().max_steps;
    if steps.len() as u32 > limit {
        return Err(TaskError::TooManySteps {
            got: steps.len() as u32,
            limit,
        });
    }

    let mut seen = BTreeSet::new();
    for (index, step) in steps.iter().enumerate() {
        if step.id.trim().is_empty() {
            return Err(unparsable(
                format!("第 {} 个步骤的 id 是空白", index + 1),
                raw,
            ));
        }
        // 重复 id 会让 depends_on 指向两义：写这个 id 时不知道指哪一步。
        // 不自动改名——改名会悄悄改掉依赖的含义。
        if !seen.insert(step.id.as_str()) {
            return Err(unparsable(format!("步骤 id 重复：{}", step.id), raw));
        }
        if step.instruction.trim().is_empty() {
            return Err(unparsable(
                format!("步骤 {} 的 instruction 只有空白字符", step.id),
                raw,
            ));
        }
    }
    Ok(())
}

/// 把拆解结果落成执行层用的 [`Step`]。
///
/// 校验和执行都要用同一份转换，所以它是 `pub(crate)`：两处各写一遍，
/// 迟早会在"kind 缺省怎么办"这种细节上漂移。
pub(crate) fn planned_to_steps(steps: &[PlannedStep]) -> Vec<Step> {
    steps.iter().map(planned_to_step).collect()
}

fn planned_to_step(planned: &PlannedStep) -> Step {
    // 模型没给 kind 时按指令文本判一次。判不出来（文本长且没有信号词）就按"生成"，
    // 而不是"分析"：拆解出来的步骤已经是具体动作，误判成生成最多少开一次思考，
    // 误判成分析会让每一步都开思考，白烧配额。
    let kind = planned.kind.unwrap_or_else(|| {
        TaskKind::classify(&planned.instruction).unwrap_or(TaskKind::Generation)
    });
    Step::new(planned.id.clone(), planned.instruction.clone(), kind)
        .with_depends_on(planned.depends_on.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::model::StepState;
    use crate::think::Role;

    const PERSONA: &str = "你是常驻在用户电脑上的助理，说话克制。";
    const RULES: &str = "不可逆动作必须人工批准\n不确定就不打扰";
    /// 和 parse_plan 内部卡的是同一个上限（默认预算）。
    const MAX_STEPS: u32 = 12;

    fn plan_request(goal: &str) -> ThinkRequest {
        planner_request(PERSONA, RULES, goal, MAX_STEPS, 800)
    }

    /// 除最后一条（本轮要问的问题）之外的全部内容——缓存能命中的就是这一段。
    fn prefix_of(req: &ThinkRequest) -> String {
        let cut = req.messages.len() - 1;
        req.messages[..cut]
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 造一份 n 步、每步都合法的清单 JSON。
    fn steps_json(n: usize) -> String {
        let items: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"id":"s{i}","instruction":"第 {i} 步"}}"#))
            .collect();
        format!("[{}]", items.join(","))
    }

    // ---------- 取 JSON 的三种位置 ----------

    #[test]
    fn parses_a_whole_json_reply() {
        let raw = r#"{"steps":[{"id":"s1","instruction":"备份数据库","depends_on":[]},{"id":"s2","instruction":"迁移数据","depends_on":["s1"]}]}"#;
        let steps = parse_plan(raw).expect("整段 JSON 应能解析");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "s1");
        assert_eq!(steps[1].depends_on, vec!["s1".to_string()]);
    }

    #[test]
    fn accepts_a_bare_array_at_the_top_level() {
        // 裸数组是模型常见的省略写法，拒掉它就是白费一次调用
        let raw = r#"[{"id":"s1","instruction":"先备份"},{"id":"s2","instruction":"再迁移","depends_on":["s1"]}]"#;
        let steps = parse_plan(raw).expect("裸数组应能解析");
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn parses_a_fenced_code_block_with_or_without_a_language_tag() {
        let tagged = "拆好了：\n```json\n{\"steps\":[{\"id\":\"s1\",\"instruction\":\"先备份\"}]}\n```\n要我继续吗？";
        assert_eq!(parse_plan(tagged).expect("带 json 标签的代码块").len(), 1);

        let bare = "```\n[{\"id\":\"s1\",\"instruction\":\"先备份\"}]\n```";
        assert_eq!(parse_plan(bare).expect("不带标签的代码块").len(), 1);
    }

    #[test]
    fn finds_json_embedded_in_prose() {
        let raw =
            "我的拆解如下：{\"steps\":[{\"id\":\"s1\",\"instruction\":\"先备份\"}]}，就这些。";
        assert_eq!(parse_plan(raw).expect("嵌在说明文字里的 JSON").len(), 1);
    }

    #[test]
    fn a_stray_brace_in_prose_does_not_hide_the_plan() {
        // 只取第一个花括号的实现会在这里失败：它先撞上 "{1}"
        let raw = "第 {1} 步之前先说明：{\"steps\":[{\"id\":\"s1\",\"instruction\":\"先备份\"}]}";
        assert_eq!(
            parse_plan(raw)
                .expect("真正的 JSON 不该被前面的花括号挡住")
                .len(),
            1
        );
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_slice_early() {
        // 字符串里的 } 不该被当成结构性的闭合括号
        let raw = "说明：{\"steps\":[{\"id\":\"s1\",\"instruction\":\"把 {a} 换成 }b{\"}]} 结束";
        assert_eq!(parse_plan(raw).expect("字符串里的括号不该截断").len(), 1);
    }

    // ---------- 失败路径 ----------

    #[test]
    fn plain_text_reply_is_rejected_with_the_raw_text() {
        let raw = "我建议先备份，再迁移。";
        match parse_plan(raw) {
            Err(TaskError::UnparsablePlan { detail, raw: kept }) => {
                assert!(!detail.is_empty(), "必须说清失败在哪一步");
                assert!(kept.starts_with("我建议先备份"), "报错要带原文开头: {kept}");
            }
            other => panic!("应报不可解析，实际 {other:?}"),
        }
    }

    #[test]
    fn unparsable_error_keeps_at_most_the_head_of_a_long_reply() {
        let raw = "这不是 JSON。".repeat(100);
        match parse_plan(&raw) {
            Err(TaskError::UnparsablePlan { raw: kept, .. }) => {
                assert_eq!(kept.chars().count(), RAW_HEAD_CHARS);
                assert!(raw.starts_with(&kept));
            }
            other => panic!("应报不可解析，实际 {other:?}"),
        }
    }

    #[test]
    fn an_empty_reply_is_rejected() {
        assert!(matches!(
            parse_plan("   \n"),
            Err(TaskError::UnparsablePlan { .. })
        ));
    }

    #[test]
    fn an_empty_plan_is_rejected_instead_of_silently_accepted() {
        // 0 步的任务永远跑不起来，接受它等于造一个不会报错的空转
        assert!(matches!(
            parse_plan(r#"{"steps":[]}"#),
            Err(TaskError::UnparsablePlan { .. })
        ));
    }

    #[test]
    fn steps_field_must_be_an_array() {
        match parse_plan(r#"{"steps":3}"#) {
            Err(TaskError::UnparsablePlan { detail, .. }) => {
                assert!(detail.contains("数组"), "要说清拿到的是什么: {detail}")
            }
            other => panic!("应报不可解析，实际 {other:?}"),
        }
    }

    #[test]
    fn top_level_must_be_an_object_or_an_array() {
        assert!(matches!(
            parse_plan("\"先备份再迁移\""),
            Err(TaskError::UnparsablePlan { .. })
        ));
        assert!(matches!(
            parse_plan(r#"{"goal":"先备份"}"#),
            Err(TaskError::UnparsablePlan { .. })
        ));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        // 重复 id 会让 depends_on 指向两义
        let raw = r#"[{"id":"s1","instruction":"第一步"},{"id":"s1","instruction":"又一步"}]"#;
        match parse_plan(raw) {
            Err(TaskError::UnparsablePlan { detail, .. }) => {
                assert!(detail.contains("s1"), "要点名是哪个 id: {detail}")
            }
            other => panic!("应报不可解析，实际 {other:?}"),
        }
    }

    #[test]
    fn a_blank_id_is_rejected() {
        assert!(matches!(
            parse_plan(r#"[{"id":"   ","instruction":"第一步"}]"#),
            Err(TaskError::UnparsablePlan { .. })
        ));
    }

    #[test]
    fn a_blank_instruction_is_rejected() {
        assert!(matches!(
            parse_plan(r#"[{"id":"s1","instruction":"  \t "}]"#),
            Err(TaskError::UnparsablePlan { .. })
        ));
    }

    #[test]
    fn too_many_steps_is_rejected() {
        let over = MAX_STEPS as usize + 1;
        match parse_plan(&steps_json(over)) {
            Err(TaskError::TooManySteps { got, limit }) => {
                assert_eq!(got as usize, over);
                assert_eq!(limit, MAX_STEPS);
            }
            other => panic!("应报步数超限，实际 {other:?}"),
        }
        // 正好等于上限要放行，否则上限就成了"最多 N-1 步"
        assert_eq!(
            parse_plan(&steps_json(MAX_STEPS as usize)).unwrap().len(),
            MAX_STEPS as usize
        );
    }

    #[test]
    fn an_unknown_dependency_is_reported_as_such() {
        // model::validate_dependencies 的错误要原样传出，不在这里再包一层
        let raw = r#"{"steps":[{"id":"s1","instruction":"第一步","depends_on":["s9"]}]}"#;
        match parse_plan(raw) {
            Err(TaskError::UnknownDependency { step, dep }) => {
                assert_eq!(step, "s1");
                assert_eq!(dep, "s9");
            }
            other => panic!("应报未知依赖，实际 {other:?}"),
        }
    }

    #[test]
    fn a_circular_dependency_is_reported_as_such() {
        // 成环必须在执行前报出来，否则表现为"一直在等"
        let raw = r#"[{"id":"a","instruction":"甲","depends_on":["b"]},{"id":"b","instruction":"乙","depends_on":["a"]}]"#;
        assert!(matches!(
            parse_plan(raw),
            Err(TaskError::CircularDependency { .. })
        ));
    }

    #[test]
    fn a_self_dependency_is_a_cycle_too() {
        let raw = r#"[{"id":"s1","instruction":"第一步","depends_on":["s1"]}]"#;
        assert!(matches!(
            parse_plan(raw),
            Err(TaskError::CircularDependency { .. })
        ));
    }

    #[test]
    fn a_dag_with_multiple_roots_is_accepted() {
        let raw = r#"[{"id":"a","instruction":"甲"},{"id":"b","instruction":"乙"},{"id":"c","instruction":"丙","depends_on":["a","b"]}]"#;
        assert!(parse_plan(raw).is_ok());
    }

    // ---------- 落成 Step ----------

    #[test]
    fn planned_steps_convert_to_pending_steps_with_their_dependencies() {
        let raw =
            r#"[{"id":"a","instruction":"甲"},{"id":"b","instruction":"乙","depends_on":["a"]}]"#;
        let converted = planned_to_steps(&parse_plan(raw).unwrap());
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].state, StepState::Pending);
        assert_eq!(converted[0].attempts, 0);
        assert!(converted[0].depends_on.is_empty());
        assert_eq!(converted[1].depends_on, vec!["a".to_string()]);
    }

    #[test]
    fn a_missing_kind_is_filled_from_the_instruction_text() {
        let steps = parse_plan(r#"[{"id":"s1","instruction":"分析这段日志为什么报错"}]"#).unwrap();
        assert_eq!(planned_to_steps(&steps)[0].kind, TaskKind::Analysis);
    }

    #[test]
    fn an_explicit_kind_from_the_model_wins() {
        let steps =
            parse_plan(r#"[{"id":"s1","instruction":"跑一下测试","kind":"execution"}]"#).unwrap();
        assert_eq!(planned_to_steps(&steps)[0].kind, TaskKind::Execution);
        // 落成 Step 之后 kind 也该是模型给的那个，不能被兜底判覆盖
        assert_eq!(steps[0].kind, Some(TaskKind::Execution));
    }

    #[test]
    fn an_unclassifiable_instruction_falls_back_to_generation() {
        // 长且无信号词 → 分类器弃权；步骤已经拆细，按"不需要推理"处理最省
        let long = "嗯".repeat(250);
        let raw = format!(r#"[{{"id":"s1","instruction":"{long}"}}]"#);
        let steps = parse_plan(&raw).unwrap();
        assert_eq!(steps[0].kind, None);
        assert_eq!(planned_to_steps(&steps)[0].kind, TaskKind::Generation);
    }

    // ---------- 提示词组装 ----------

    #[test]
    fn plan_prompt_carries_persona_rules_ceiling_and_goal() {
        let prompt = plan_prompt(PERSONA, RULES, "把日志归档", 7);
        assert!(prompt.contains("# 身份"), "人格必须在前缀里");
        assert!(prompt.contains("1. 不可逆动作必须人工批准"), "硬规则要编号");
        assert!(prompt.contains("2. 不确定就不打扰"));
        assert!(
            prompt.contains("最多 7 步"),
            "步数上限要写进提示词: {prompt}"
        );
        assert!(prompt.contains("depends_on"), "输出格式要说清: {prompt}");
        assert!(
            prompt.ends_with("把日志归档\n"),
            "目标落在最后，位置与用户消息对应"
        );
    }

    #[test]
    fn plan_prompt_stable_head_matches_the_request_prefix() {
        // 两处必须用同一段稳定前缀，否则"整段提示词"和实际请求会悄悄漂移
        let prompt = plan_prompt(PERSONA, RULES, "把日志归档", MAX_STEPS);
        let req = plan_request("把日志归档");
        assert!(prompt.starts_with(&req.messages[0].content));
    }

    #[test]
    fn rules_are_split_one_per_line() {
        let req = planner_request(PERSONA, "第一条\n\n  第二条  \n", "目标", MAX_STEPS, 800);
        let system = &req.messages[0].content;
        assert!(system.contains("1. 第一条"));
        assert!(system.contains("2. 第二条"), "空行不该占一条编号: {system}");
    }

    // ---------- 请求：前缀稳定性 ----------

    #[test]
    fn planner_prefix_is_byte_stable_when_only_the_goal_changes() {
        // 这是缓存能否命中的前提：换目标只该换最后一条消息
        let a = plan_request("把日志归档");
        let b = plan_request("把数据库备份到移动硬盘");
        assert_eq!(
            prefix_of(&a),
            prefix_of(&b),
            "换了目标前缀就变了，已经建好的缓存会全部作废"
        );
        assert_eq!(a.messages.last().unwrap().content, "把日志归档");
        assert_eq!(b.messages.last().unwrap().content, "把数据库备份到移动硬盘");
    }

    #[test]
    fn planner_request_is_deterministic() {
        // 掺进时间、随机数、HashMap 迭代顺序都会让前缀每次都不同
        assert_eq!(plan_request("目标").messages, plan_request("目标").messages);
    }

    #[test]
    fn planner_request_puts_persona_in_the_system_message_and_the_goal_last() {
        let req = plan_request("把日志归档");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].role, Role::User);
        assert!(req.messages[0].content.contains("云熙"));
        assert!(req.messages[0].content.contains("# 任务拆解"));
        assert!(
            !req.messages[0].content.contains("把日志归档"),
            "目标不能出现在前缀里"
        );
    }

    #[test]
    fn planner_request_does_not_pin_the_thinking_mode() {
        // 思考模式由路由按任务类型决定，写死在这里会让那条规则失效
        let req = plan_request("目标");
        assert_eq!(req.thinking, None);
        assert_eq!(req.max_tokens, Some(800));
        assert!(req.tools.is_empty());
    }
}
