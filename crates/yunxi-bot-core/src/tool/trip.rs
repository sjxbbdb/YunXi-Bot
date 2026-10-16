//! 多日行程复用每天的排程与系统记录；不另建预订数据库或绕过日程版本检查。

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Capability, Tool, ToolContext, ToolError, ToolOutput, agenda};

pub const NAMES: [&str; 2] = ["mac_trip_plan", "mac_trip_apply"];
type Result<T> = std::result::Result<T, ToolError>;
type DayCall<'a> = dyn FnMut(&'static str, &Value) -> Result<Value> + 'a;

fn bad(detail: impl Into<String>) -> ToolError {
    ToolError::BadArgs {
        detail: detail.into(),
    }
}
fn failed(detail: impl Into<String>) -> ToolError {
    ToolError::Failed {
        detail: detail.into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    days: Vec<Value>,
    expected_revision: Option<String>,
}
fn input(args: &Value) -> Result<Input> {
    if args.to_string().len() > 128_000 {
        return Err(bad("多日行程请求过大"));
    }
    let mut i: Input =
        serde_json::from_value(args.clone()).map_err(|e| bad(format!("行程参数不合法：{e}")))?;
    if i.days.is_empty() || i.days.len() > 14 {
        return Err(bad("每次提供1–14个日期；只读取时各日仅提供date"));
    }
    let mut dates = BTreeSet::new();
    let mut tasks = 0;
    for day in &i.days {
        agenda::validate_plan_args(day)?;
        let text = day["date"].as_str().ok_or_else(|| bad("缺少日期"))?;
        let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| bad("日期非法"))?;
        if !dates.insert(date) {
            return Err(bad("同一请求不允许重复日期"));
        }
        tasks += day["tasks"].as_array().map_or(0, Vec::len);
    }
    if tasks > 80
        || dates
            .last()
            .zip(dates.first())
            .is_some_and(|(last, first)| (*last - *first).num_days() > 13)
    {
        return Err(bad("日期跨度最多14天，事项合计最多80项；更长行程分批处理"));
    }
    i.days
        .sort_by(|a, b| a["date"].as_str().cmp(&b["date"].as_str()));
    Ok(i)
}

fn day_report(report: &Value, date: &Value, applied: bool) -> Result<usize> {
    let count = report["entries"]
        .as_array()
        .ok_or_else(|| failed("缺少每日安排回执"))?
        .len();
    if report["date"] != *date
        || report["applied"] != applied
        || report["revision"].as_str().is_none_or(str::is_empty)
        || report["unscheduled_count"]
            .as_u64()
            .is_none_or(|n| n > count as u64)
        || count > 40
        || (applied && report["ready_to_apply"] != true)
        || report["blocking_issues"]
            .as_array()
            .is_none_or(|issues| report["ready_to_apply"].as_bool() != Some(issues.is_empty()))
    {
        return Err(failed("每日安排回执无效，不能声称完成"));
    }
    Ok(count)
}

fn report(days: Vec<Value>, applied: bool) -> Value {
    let count: u64 = days
        .iter()
        .filter_map(|d| d["unscheduled_count"].as_u64())
        .sum();
    let blocking = days
        .iter()
        .flat_map(|d| {
            d["blocking_issues"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|issue| json!({"date":d["date"],"issue":issue}))
        })
        .collect::<Vec<_>>();
    json!({"days":days,"applied":applied,"degraded":false,"unscheduled_count":count,
        "blocking_issues":blocking,"ready_to_apply":blocking.is_empty(),
        "next_step":if applied {"已写入并回读核实；报告实际安排与未安排项，不要重复保存。"} else if blocking.is_empty() {"这只是多日预排。用户本轮已要求保存/同步/更新时，以相同days和整份revision调用mac_trip_apply，由工具审批层收集批准，不要再次口头询问是否同步；不要拿每日revision逐日保存。仅查看或只要方案时停止。"} else {"先解决blocking_issues；当前不能保存，不得去掉约束绕过。"},
        "booking_actions_performed":false,"timezone":"各date均按Mac系统本地日；时间字段保留实际偏移",
        "scope":"日历与提醒事项；不会订票、订房、改签或取消商家订单"})
}

fn plan(i: &Input, call: &mut DayCall<'_>) -> Result<Value> {
    let mut days = Vec::new();
    let mut count = 0;
    for day in &i.days {
        let current = call("mac_agenda_plan", day)?;
        count += day_report(&current, &day["date"], false)?;
        if count > 80 {
            return Err(bad("已保存和新增事项合计超过80项，请缩小日期范围"));
        }
        days.push(current);
    }
    let mut out = report(days, false);
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    // 绑定原参数及所有日期快照；批准期间任一天变化，都不能先写其他天再发现过期。
    json!({"input":i.days,"days":out["days"]})
        .to_string()
        .hash(&mut hash);
    out["revision"] = json!(format!("{:016x}", hash.finish()));
    Ok(out)
}

fn apply(i: &Input, call: &mut DayCall<'_>) -> Result<Value> {
    if i.expected_revision.as_deref().is_none_or(str::is_empty) {
        return Err(bad(
            "先用相同days调用mac_trip_plan，再把其revision作为expected_revision传入",
        ));
    }
    let proposed = plan(i, call)?;
    if i.expected_revision.as_deref() != proposed["revision"].as_str() {
        return Err(failed(
            "行程参数或任一天安排已变化，请重新mac_trip_plan后再批准；未写入",
        ));
    }
    agenda::ensure_ready(&proposed)?;
    let mut completed = Vec::new();
    for (index, day) in i.days.iter().enumerate() {
        let mut args = day.clone();
        args["expected_revision"] = proposed["days"][index]["revision"].clone();
        let result = call("mac_agenda_apply", &args).and_then(|v| {
            day_report(&v, &day["date"], true)?;
            Ok(v)
        });
        match result {
            Ok(current) => completed.push(current),
            Err(error) => {
                let detail = json!({"degraded":true,"applied":false,
                    "failed_date":day["date"],"completed_days":completed,
                    "not_attempted_dates":i.days[index+1..].iter().map(|d|d["date"].clone()).collect::<Vec<_>>(),
                    "reason":error.to_string(),"next":"可能部分完成；按原日期和事项id重新预排核对，禁止换id重建或声称商家订单已变更"});
                // 台账截短正文时也必须看得到降级标记和失败日期。
                return Err(failed(format!(
                    "degraded=true：{}保存或回读失败，可能部分完成。\n{detail}",
                    day["date"]
                )));
            }
        }
    }
    Ok(report(completed, true))
}

pub struct TripTool {
    name: &'static str,
}
impl TripTool {
    pub fn new(name: &'static str) -> Self {
        Self { name }
    }
}
fn call_day(name: &'static str, args: &Value, ctx: &mut ToolContext) -> Result<Value> {
    let output = agenda::AgendaTool::new(name).call(args, ctx)?;
    serde_json::from_str(&output.text).map_err(|_| failed("每日安排返回无效JSON"))
}
impl Tool for TripTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        if self.name == "mac_trip_plan" {
            "查询或预排多日出行/住宿行程，不写入。days为1–14个本机日期、最多80项，复用每日排程；仅给date可跨会话读回。先统筹原有日历和待办，新增/修改只传变化的完整task，沿用各日原id和约束，其他事项保留。交通、活动、入住和退房分别作时间块；住宿整段不占满白天，酒店/地址/预订来源等存task.details。固定车次、酒店、必要路途用must_schedule=true，先后衔接用同日after_task；blocking_issues非空则整份不可保存。时间和时长缺失先追问，不能把网页价格/房态当实时或已预订。固定车次可配同日travel_for准备通勤；跨午夜按本机日期分段，各段需明确时间。跨日改期须在旧日取消原事项、在新日安排，不能只新增导致旧日遗留。返回整份revision、ready_to_apply、每日冲突/未安排项及原时间previous_start/end，说明哪些普通事项被调整。"
        } else {
            "应用刚才mac_trip_plan完全相同的days，expected_revision用整份行程的revision。经公共审批写入iCloud云熙日历/提醒事项；先核对全部日期与硬约束，ready_to_apply=false拒绝全部写入，不能去掉must_schedule/after_task绕过。可行后逐日执行并回读，运行中故障可能部分完成，degraded和completed_days必须报告，按原id重新预排恢复。只改变日程，不订票/订房/退款/改签；取消日程不等于取消订单。applied=true只证明系统记录回读通过，仍需报告排不下的普通事项。"
        }
    }
    fn parameters(&self) -> Value {
        let day = agenda::AgendaTool::new("mac_agenda_plan").parameters();
        let mut schema = json!({"type":"object","additionalProperties":false,
            "properties":{"days":{"type":"array","minItems":1,"maxItems":14,"items":day}},"required":["days"]});
        if self.name == "mac_trip_apply" {
            schema["properties"]["expected_revision"] = json!({"type":"string","description":"来自包含所有本次改动的mac_trip_plan整份结果，不是某一天的revision"});
            schema["required"] = json!(["days", "expected_revision"]);
        }
        schema
    }
    fn capability(&self) -> Capability {
        if self.name == "mac_trip_plan" {
            Capability::ReadOnly
        } else {
            Capability::Outbound
        }
    }
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        if self.name != "mac_trip_apply" {
            return None;
        }
        let mut local = ToolContext::new(ctx.cwd.clone(), ctx.sandbox);
        let result = input(args).and_then(|i| plan(&i, &mut |n, a| call_day(n, a, &mut local)));
        Some(
            match result {
                Ok(p) => format!("将同步多日行程到iCloud云熙日历与提醒事项；不操作商家订单：\n{p}"),
                Err(e) => format!("行程校验失败，不能执行：{e}"),
            }
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect(),
        )
    }
    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput> {
        let i = input(args)?;
        let mut call = |n, a: &Value| call_day(n, a, ctx);
        match self.name {
            "mac_trip_plan" if i.expected_revision.is_none() => {
                plan(&i, &mut call).map(|v| ToolOutput::read(v.to_string()))
            }
            "mac_trip_apply" => apply(&i, &mut call).map(|v| ToolOutput::changed(v.to_string())),
            _ => Err(bad("未知行程工具或预排携带了expected_revision")),
        }
    }
}

#[cfg(test)]
#[path = "trip_tests.rs"]
mod tests;
