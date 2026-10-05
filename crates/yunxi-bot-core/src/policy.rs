//! 约束层：权限旋钮、预设，与审批结果的封闭词汇表。
//!
//! 设计依据 ADR-0001 §六 D4 / D5：
//!
//! - 权限是**两个正交旋钮**（沙箱级别 × 审批策略），预设只是命名组合；
//! - 审批结果使用**封闭词汇表**，词汇表之外的一切归一为 `Unavailable`（fail closed）；
//! - `Never` 策略的含义是「需要批准的动作自动拒绝」，**不是**「自动放行」。

use serde::{Deserialize, Serialize};

use crate::ledger::{Event, EventKind};

/// 沙箱级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// 只读：不授予任何写入能力。
    ReadOnly,
    /// 仅工作区可写。
    WorkspaceWrite,
    /// 不做文件写入限制。
    DangerFullAccess,
}

/// 审批策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// 需要批准的动作可以询问；**没有可用应答者时失败关闭**。
    Ask,
    /// 需要批准的动作**自动拒绝**。注意：不是自动放行。
    Never,
}

/// 「匹配不上任何预设」时对外展示的名字。
///
/// **它永远不能作为切换目标或事件载荷**——未知状态不许写进日志。
pub const CUSTOM_PRESET: &str = "custom";

/// 一个命名预设：只是两个旋钮值的组合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub sandbox: SandboxMode,
    pub approval: ApprovalPolicy,
}

/// 内置预设。
pub const BUILTIN_PRESETS: &[Preset] = &[
    Preset {
        name: "readonly",
        sandbox: SandboxMode::ReadOnly,
        approval: ApprovalPolicy::Ask,
    },
    Preset {
        name: "standard",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalPolicy::Ask,
    },
    Preset {
        name: "unattended",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalPolicy::Never,
    },
    Preset {
        name: "full",
        sandbox: SandboxMode::DangerFullAccess,
        approval: ApprovalPolicy::Ask,
    },
];

/// 找出与给定旋钮组合匹配的预设名；匹配不上返回 `None`（对外展示为 [`CUSTOM_PRESET`]）。
pub fn matching_preset(sandbox: SandboxMode, approval: ApprovalPolicy) -> Option<&'static str> {
    BUILTIN_PRESETS
        .iter()
        .find(|p| p.sandbox == sandbox && p.approval == approval)
        .map(|p| p.name)
}

/// 按名字查预设。
pub fn preset_by_name(name: &str) -> Option<&'static Preset> {
    // "custom" 不是可切换目标
    if name == CUSTOM_PRESET {
        return None;
    }
    BUILTIN_PRESETS.iter().find(|p| p.name == name)
}

/// 权限状态：由事件日志折叠得出。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PermissionState {
    pub preset: Option<String>,
    pub sandbox: Option<SandboxMode>,
    pub approval: Option<ApprovalPolicy>,
    /// 种子阶段是否已结束。
    pub seeded: bool,
}

impl PermissionState {
    /// 有效沙箱级别：会话覆盖优先，否则用部署默认。
    pub fn effective_sandbox(&self, default: SandboxMode) -> SandboxMode {
        self.sandbox.unwrap_or(default)
    }

    /// 有效审批策略：会话覆盖优先，否则用部署默认。
    pub fn effective_approval(&self, default: ApprovalPolicy) -> ApprovalPolicy {
        self.approval.unwrap_or(default)
    }

    /// 对外展示的预设名：匹配不上就是 `custom`。
    pub fn display_preset(
        &self,
        default_sandbox: SandboxMode,
        default_approval: ApprovalPolicy,
    ) -> &str {
        matching_preset(
            self.effective_sandbox(default_sandbox),
            self.effective_approval(default_approval),
        )
        .unwrap_or(CUSTOM_PRESET)
    }
}

/// 把一个事件折叠进权限状态。
///
/// 返回 `None` 表示**该事件与权限无关、状态未变**——这是 DSH 用「引用相等」
/// 做变更门控的 Rust 惯用等价物：调用方据此跳过无谓的重建与推送。
pub fn apply_permission_event(state: &PermissionState, event: &Event) -> Option<PermissionState> {
    match event.kind {
        EventKind::PermissionPreset => {
            let preset = event.data.get("preset")?.as_str()?.to_string();
            // 未知状态不许写进日志；读到未知值也不接受它作为预设。
            // `preset_by_name` 对 `custom` 与一切未知名都返回 None。
            let p = preset_by_name(&preset)?;
            Some(PermissionState {
                preset: Some(preset),
                sandbox: Some(p.sandbox),
                approval: Some(p.approval),
                seeded: state.seeded,
            })
        }
        EventKind::SandboxModeSet => {
            let mode: SandboxMode = serde_json::from_value(event.data.get("mode")?.clone()).ok()?;
            Some(PermissionState {
                preset: None,
                sandbox: Some(mode),
                approval: state.approval,
                seeded: state.seeded,
            })
        }
        EventKind::ApprovalPolicySet => {
            let policy: ApprovalPolicy =
                serde_json::from_value(event.data.get("policy")?.clone()).ok()?;
            Some(PermissionState {
                preset: None,
                sandbox: state.sandbox,
                approval: Some(policy),
                seeded: state.seeded,
            })
        }
        EventKind::SessionSeeded => Some(PermissionState {
            seeded: true,
            ..state.clone()
        }),
        _ => None,
    }
}

/// 审批结果。**封闭词汇表**——不在此列的一切都归一为 [`ApprovalOutcome::Unavailable`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    /// **唯一的授权**，且只对本次动作有效。
    AllowedOnce,
    Rejected,
    Cancelled,
    /// 无可用应答者 / 应答者异常 / 返回值不在词汇表内。
    Unavailable,
}

impl ApprovalOutcome {
    /// 是否为授权。只有 `AllowedOnce` 是。
    pub fn is_grant(self) -> bool {
        matches!(self, Self::AllowedOnce)
    }

    /// 该结果是否允许继续执行。
    pub fn permits_action(self) -> bool {
        self.is_grant()
    }
}

/// 把应答者返回的原始字符串归一为封闭词汇表内的结果。
///
/// **不信任应答者的返回值**：词汇表之外的一切（包括大小写变体、`"true"`、
/// 空串、`"allowed"` 这类近义词）一律归一为 `Unavailable`，即失败关闭。
pub fn normalize_outcome(raw: &str) -> ApprovalOutcome {
    match raw {
        "allowed-once" => ApprovalOutcome::AllowedOnce,
        "rejected" => ApprovalOutcome::Rejected,
        "cancelled" => ApprovalOutcome::Cancelled,
        "unavailable" => ApprovalOutcome::Unavailable,
        _ => ApprovalOutcome::Unavailable,
    }
}

/// 无可用应答者时的结果：失败关闭。
pub const NO_ANSWERER: ApprovalOutcome = ApprovalOutcome::Unavailable;

/// 从事件流折叠出权限状态。
///
/// 只有真正相关的事件才会改变状态（其余返回 `None` 被跳过），
/// 这与 `apply_permission_event` 的变更门控是同一套语义。
pub fn fold_permission_state(events: &[Event]) -> PermissionState {
    let mut state = PermissionState::default();
    for e in events {
        if let Some(next) = apply_permission_event(&state, e) {
            state = next;
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: EventKind, data: serde_json::Value) -> Event {
        Event {
            seq: 1,
            at: 0,
            kind,
            span: None,
            job: None,
            data,
        }
    }

    #[test]
    fn unknown_outcome_fails_closed() {
        // 不信任应答者：任何词汇表外的值都必须归一为 unavailable
        for raw in ["true", "allowed", "ALLOWED-ONCE", "", "yes", "ok"] {
            assert_eq!(
                normalize_outcome(raw),
                ApprovalOutcome::Unavailable,
                "raw={raw:?} 必须失败关闭"
            );
        }
        assert_eq!(
            normalize_outcome("allowed-once"),
            ApprovalOutcome::AllowedOnce
        );
    }

    #[test]
    fn only_allowed_once_is_a_grant() {
        assert!(ApprovalOutcome::AllowedOnce.is_grant());
        assert!(!ApprovalOutcome::Rejected.is_grant());
        assert!(!ApprovalOutcome::Cancelled.is_grant());
        assert!(!ApprovalOutcome::Unavailable.is_grant());
    }

    #[test]
    fn unrelated_event_returns_none() {
        let st = PermissionState::default();
        assert!(apply_permission_event(&st, &ev(EventKind::JobStarted, json!({}))).is_none());
    }

    #[test]
    fn custom_is_not_a_switch_target() {
        assert!(preset_by_name(CUSTOM_PRESET).is_none());
        // 写入日志的未知预设被拒绝，状态不变
        let st = PermissionState::default();
        let out = apply_permission_event(
            &st,
            &ev(
                EventKind::PermissionPreset,
                json!({"preset": CUSTOM_PRESET}),
            ),
        );
        assert!(out.is_none(), "custom 不能作为事件载荷生效");
    }

    #[test]
    fn preset_fold_sets_both_knobs() {
        let st = PermissionState::default();
        let out = apply_permission_event(
            &st,
            &ev(EventKind::PermissionPreset, json!({"preset": "unattended"})),
        )
        .expect("应生效");
        assert_eq!(out.sandbox, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(out.approval, Some(ApprovalPolicy::Never));
    }

    #[test]
    fn display_preset_falls_back_to_custom() {
        let st = PermissionState {
            sandbox: Some(SandboxMode::ReadOnly),
            approval: Some(ApprovalPolicy::Never),
            ..Default::default()
        };
        assert_eq!(
            st.display_preset(SandboxMode::ReadOnly, ApprovalPolicy::Ask),
            CUSTOM_PRESET
        );
    }
}
