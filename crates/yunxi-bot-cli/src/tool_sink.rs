//! 把每次工具调用写进台账。
//!
//! ## 这个文件补的是一个真实缺口
//!
//! 在此之前，`ToolRunOutcome.calls` 生产出来就被丢掉——**机器能读你的文件、
//! 跑命令、抓网页，而台账里一条记录都没有**。破了 ADR D11
//! 「台账是唯一事实来源」。
//!
//! 对一个通用助理来说这个缺口的分量比看起来重：它的价值恰恰在于
//! "能替你动手"，而**能动手却没有记录**意味着事后无法回答
//! "它到底动过什么"。那不是审计问题，是信任问题。
//!
//! ## 记什么
//!
//! 每一次尝试都记，**包括被拒绝和没人应答的**：
//!
//! | 情况 | 记不记 | 为什么 |
//! |---|---|---|
//! | 批准并执行 | 记 | 这是"做过什么" |
//! | 被门禁拒绝 | **记** | "谁试图动我的文件"才是审计要问的 |
//! | 需要人工但没人应答 | **记** | 否则"问了但没人理"这件事完全不可见 |
//! | 执行失败 | 记 | 失败也是发生过的事 |
//!
//! 只记成功的那些，等于把问题从"谁试图动我的文件"偷换成"谁成功动了"。
//!
//! ## 写失败怎么办
//!
//! **打到 stderr，不让工具循环中断。** 为了留痕而拒绝干活是本末倒置；
//! 但台账写不进去也是需要人知道的事——所以是"响亮地继续"，
//! 不是"静默地继续"。

use std::path::PathBuf;
use std::sync::Mutex;

use yunxi_bot_core::tool::ToolCallSink;
use yunxi_bot_core::tool::runner::ToolCallRecord;
use yunxi_bot_core::{EventKind, Ledger};

/// 把工具调用写进台账的 sink。
pub struct LedgerToolSink {
    /// 自己的那本台账。
    ///
    /// **不共用调用方手里那本**：工具循环同时持有很多可变借用，
    /// 再挂一个 `&mut Ledger` 会打架。两本指向同一个文件是安全的——
    /// `Ledger::append` 每次追加前会从磁盘对齐序号（见
    /// `Ledger::sync_seq_from_disk` 的文档）。
    ledger: Mutex<Ledger>,
    /// 记住路径，供出错时报告。
    path: PathBuf,
}

impl LedgerToolSink {
    /// 打开（或创建）台账。
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let ledger =
            Ledger::open(&path).map_err(|e| format!("打不开台账 {}: {e}", path.display()))?;
        Ok(Self {
            ledger: Mutex::new(ledger),
            path,
        })
    }

    /// 把一条调用记录拼成事件载荷。
    ///
    /// **参数与输出都要截断，但截断要留痕。** 一次工具调用的参数可能是
    /// 整个文件内容，输出可能几十 KB——原样进台账会让文件迅速膨胀。
    /// 但"截断了"这件事必须写在载荷里，否则读台账的人会以为那就是全部。
    fn payload(call: &ToolCallRecord) -> serde_json::Value {
        const MAX: usize = 2000;

        let (decision, decision_reason) = match &call.decision {
            yunxi_bot_core::tool::GateDecision::Allow { reason } => ("allow", reason.clone()),
            yunxi_bot_core::tool::GateDecision::Ask { reason } => ("ask", reason.clone()),
            yunxi_bot_core::tool::GateDecision::Deny { reason } => ("deny", reason.clone()),
        };

        let (outcome, output, error, executed) = match &call.output {
            None => ("not_run", String::new(), String::new(), false),
            Some(Ok(s)) => ("ok", clip(s, MAX), String::new(), true),
            Some(Err(e)) => ("error", String::new(), e.to_string(), false),
        };

        let (args_text, args_clipped) = {
            let s = serde_json::to_string(&call.arguments).unwrap_or_default();
            let clipped = s.chars().count() > MAX;
            (clip(&s, MAX), clipped)
        };
        let (out_clipped, err_clipped) = match &call.output {
            Some(Ok(s)) => (s.chars().count() > MAX, false),
            Some(Err(e)) => (false, e.to_string().chars().count() > MAX),
            None => (false, false),
        };

        serde_json::json!({
            "tool": call.tool,
            "capability": format!("{:?}", call.capability),
            // **时序也记下来。**
            //
            // 一是让"并行"可证：两个调用的
            // `[started, started+duration]` 区间重叠就是并行最直接的证据
            // ——本地文件读得太快，光看总耗时看不出痕迹。
            //
            // 二是能看到哪个工具慢。而"慢"往往正是模型反复调它的原因。
            "started_at_ms": call.started_at_ms,
            "duration_ms": call.duration_ms,
            "arguments": args_text,
            // **"我截断了"必须能看出来。** 不写这一位，读台账的人
            // 会以为那就是参数的全部。
            "arguments_clipped": args_clipped,
            "decision": decision,
            "decision_reason": decision_reason,
            "outcome": outcome,
            "output": output,
            "output_clipped": out_clipped,
            "error": error,
            "error_clipped": err_clipped,
            // 这次调用**真的动了系统吗**。统计"机器做过什么"时只看这一位。
            "executed": executed,
        })
    }
}

/// 按字符截断——按字节切会落在汉字中间。
fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…（已截断）")
}

impl ToolCallSink for LedgerToolSink {
    fn record(&self, call: &ToolCallRecord) {
        let payload = Self::payload(call);
        let mut guard = match self.ledger.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = guard.append(EventKind::ToolCalled, None, payload) {
            // **响亮地继续，不静默。**
            //
            // 为了留痕而中断工具循环是本末倒置——但台账写不进去
            // 也是需要人知道的事。所以打到 stderr，让这一轮照常进行。
            eprintln!(
                "警告：工具调用没能写进台账（{}）：{e}。工具「{}」已经{}{}",
                self.path.display(),
                call.tool,
                if call.executed() {
                    "执行"
                } else {
                    "未执行"
                },
                // 说清"已经发生过的事没有记录"——这正是需要人介入的点
                if call.executed() {
                    "，而这次执行没有留下记录"
                } else {
                    ""
                }
            );
        }
    }
}

/// 台账打不开时的退路：把每次调用打到标准错误。
///
/// **它不替代台账**，只是让"动作发生过"这件事仍然可见——总比彻底静默强。
/// 使用者看到这些行就知道该去修台账路径了。
#[derive(Debug, Default, Clone, Copy)]
pub struct StderrToolSink;

impl ToolCallSink for StderrToolSink {
    fn record(&self, call: &ToolCallRecord) {
        let args = serde_json::to_string(&call.arguments).unwrap_or_default();
        let outcome = match &call.output {
            None => "未执行".to_string(),
            Some(Ok(_)) => "执行成功".to_string(),
            Some(Err(e)) => format!("执行失败：{e}"),
        };
        eprintln!(
            "[工具调用·未入库] {} {}({}) -> {}",
            match &call.decision {
                yunxi_bot_core::tool::GateDecision::Allow { .. } => "允许",
                yunxi_bot_core::tool::GateDecision::Ask { .. } => "需批准",
                yunxi_bot_core::tool::GateDecision::Deny { .. } => "拒绝",
            },
            call.tool,
            clip(&args, 200),
            outcome
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use yunxi_bot_core::tool::{Capability, GateDecision, ToolError};

    fn tmp_path(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "yunxi-sink-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("ledger.jsonl")
    }

    fn rec(
        tool: &str,
        decision: GateDecision,
        output: Option<Result<String, ToolError>>,
    ) -> ToolCallRecord {
        ToolCallRecord {
            tool: tool.into(),
            arguments: json!({"path": "a.txt"}),
            capability: Capability::Write,
            decision,
            output,
            started_at_ms: 0,
            duration_ms: 0,
        }
    }

    #[test]
    fn a_successful_call_is_recorded() {
        let p = tmp_path("ok");
        let sink = LedgerToolSink::open(&p).unwrap();
        sink.record(&rec(
            "write_file",
            GateDecision::Allow {
                reason: "规则允许".into(),
            },
            Some(Ok("已写入 12 字节".into())),
        ));

        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .expect("该有一条工具调用记录");
        assert_eq!(ev.data["tool"], "write_file");
        assert_eq!(ev.data["decision"], "allow");
        assert_eq!(ev.data["outcome"], "ok");
        assert_eq!(ev.data["executed"], true);
        assert!(ev.data["output"].as_str().unwrap().contains("12 字节"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_denied_call_is_recorded_too() {
        // **"谁试图动我的文件"才是审计要问的。**
        // 只记成功的那些，等于把问题偷换成"谁成功动了"。
        let p = tmp_path("deny");
        let sink = LedgerToolSink::open(&p).unwrap();
        sink.record(&rec(
            "run_command",
            GateDecision::Deny {
                reason: "规则拒绝".into(),
            },
            None,
        ));

        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .unwrap();
        assert_eq!(ev.data["decision"], "deny");
        assert_eq!(ev.data["outcome"], "not_run");
        assert_eq!(ev.data["executed"], false, "没执行就不能记成执行了");
        assert_eq!(ev.data["decision_reason"], "规则拒绝");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_call_that_needed_a_human_and_got_none_is_recorded() {
        // 否则"问了但没人理"这件事完全不可见
        let p = tmp_path("ask");
        let sink = LedgerToolSink::open(&p).unwrap();
        sink.record(&rec(
            "write_file",
            GateDecision::Ask {
                reason: "需要批准".into(),
            },
            None,
        ));
        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .unwrap();
        assert_eq!(ev.data["decision"], "ask");
        assert_eq!(ev.data["executed"], false);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_failed_execution_is_recorded_as_not_executed() {
        let p = tmp_path("fail");
        let sink = LedgerToolSink::open(&p).unwrap();
        sink.record(&rec(
            "run_command",
            GateDecision::Allow {
                reason: "自动".into(),
            },
            Some(Err(ToolError::Failed {
                detail: "命令退出码 1".into(),
            })),
        ));
        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .unwrap();
        assert_eq!(ev.data["outcome"], "error");
        assert_eq!(ev.data["executed"], false, "失败不是执行成功");
        assert!(ev.data["error"].as_str().unwrap().contains("退出码 1"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn long_arguments_are_clipped_and_flagged() {
        // 一次工具调用的参数可能是整个文件内容。原样进台账会让文件爆炸，
        // 但**"截断了"这件事必须写下来**，否则读台账的人以为那就是全部。
        let p = tmp_path("clip");
        let sink = LedgerToolSink::open(&p).unwrap();
        let mut r = rec(
            "write_file",
            GateDecision::Allow { reason: "x".into() },
            Some(Ok("ok".into())),
        );
        r.arguments = json!({"content": "中".repeat(5000)});
        sink.record(&r);

        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .unwrap();
        assert_eq!(ev.data["arguments_clipped"], true);
        let args = ev.data["arguments"].as_str().unwrap();
        assert!(args.chars().count() < 2100, "该被截断");
        assert!(args.contains("已截断"), "截断要留痕");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn long_output_is_clipped_and_flagged() {
        let p = tmp_path("clipout");
        let sink = LedgerToolSink::open(&p).unwrap();
        let mut r = rec(
            "run_command",
            GateDecision::Allow { reason: "x".into() },
            None,
        );
        r.output = Some(Ok("行\n".repeat(3000)));
        sink.record(&r);

        let l = Ledger::open(&p).unwrap();
        let ev = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ToolCalled)
            .unwrap();
        assert_eq!(ev.data["output_clipped"], true);
        assert!(ev.data["output"].as_str().unwrap().contains("已截断"));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn clipping_by_chars_does_not_split_a_character() {
        // 按字节切会落在汉字中间——CLI 里是乱码，别处可能 panic
        let s = "中".repeat(100);
        let out = clip(&s, 10);
        assert!(out.starts_with("中中中中中中中中中中…"));
        assert_eq!(out.chars().filter(|c| *c == '中').count(), 10);
    }

    #[test]
    fn multiple_calls_all_get_recorded_with_rising_seq() {
        // **每一次都要落。** 漏一条就有一条系统动作没有记录。
        let p = tmp_path("many");
        let sink = LedgerToolSink::open(&p).unwrap();
        for i in 0..5 {
            sink.record(&rec(
                &format!("tool{i}"),
                GateDecision::Allow { reason: "x".into() },
                Some(Ok("ok".into())),
            ));
        }
        let l = Ledger::open(&p).unwrap();
        let calls: Vec<_> = l
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::ToolCalled)
            .collect();
        assert_eq!(calls.len(), 5);
        // seq 必须严格递增——两本台账指向同一个文件时最容易出重复 seq
        let seqs: Vec<u64> = l.events().iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone();
        sorted.dedup();
        assert_eq!(seqs, sorted, "seq 不能重复或乱序: {seqs:?}");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_second_sink_on_the_same_file_does_not_reuse_seq() {
        // **这条测的是 `Ledger::sync_seq_from_disk` 的意义。**
        // 两本台账指向同一个文件时，不重新对齐 seq 就会写出重复的序号，
        // 而 seq 是时序与边界的依据——重复了就没有东西能说清谁先谁后。
        let p = tmp_path("two");
        let a = LedgerToolSink::open(&p).unwrap();
        let b = LedgerToolSink::open(&p).unwrap();
        a.record(&rec(
            "first",
            GateDecision::Allow { reason: "x".into() },
            None,
        ));
        b.record(&rec(
            "second",
            GateDecision::Allow { reason: "x".into() },
            None,
        ));

        let l = Ledger::open(&p).unwrap();
        let seqs: Vec<u64> = l.events().iter().map(|e| e.seq).collect();
        assert_eq!(seqs.len(), 2);
        assert_ne!(seqs[0], seqs[1], "两个 sink 写出了重复 seq: {seqs:?}");
        let tools: Vec<&str> = l
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::ToolCalled)
            .map(|e| e.data["tool"].as_str().unwrap())
            .collect();
        assert_eq!(tools, vec!["first", "second"], "顺序不能乱");
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn sink_is_object_safe() {
        // 要能放进 Arc<dyn ToolCallSink>
        let p = tmp_path("obj");
        let s: std::sync::Arc<dyn ToolCallSink> =
            std::sync::Arc::new(LedgerToolSink::open(&p).unwrap());
        s.record(&rec("t", GateDecision::Allow { reason: "x".into() }, None));
        assert!(p.exists());
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
