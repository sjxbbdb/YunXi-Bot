//! 请求发出前持久预留；崩溃留下未知用量，不能恢复成免费额度。
use super::{ThinkError, ThinkResponse, Usage};
use crate::{
    instance::exclusive_file_lock,
    ledger::{EventKind, Ledger},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct RequestBudget {
    pub home: PathBuf,
    pub task: String,
    pub limit: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reserved {
    task: String,
    ordinal: u32,
    limit: u32,
    model: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settled {
    task: String,
    ordinal: u32,
    success: bool,
    usage: Option<Usage>,
}

pub(crate) fn validate(kind: EventKind, data: &serde_json::Value) -> Result<(), String> {
    match kind {
        EventKind::ModelRequestReserved => {
            let r: Reserved =
                serde_json::from_value(data.clone()).map_err(|_| "请求预留结构无效")?;
            if r.task.trim().is_empty()
                || r.model.trim().is_empty()
                || r.ordinal == 0
                || r.ordinal > r.limit
            {
                return Err("请求预留计数无效".into());
            }
        }
        EventKind::ModelRequestSettled => {
            let r: Settled =
                serde_json::from_value(data.clone()).map_err(|_| "请求结算结构无效")?;
            if r.task.trim().is_empty() || r.ordinal == 0 {
                return Err("请求结算计数无效".into());
            }
        }
        _ => {}
    }
    Ok(())
}

fn failed() -> ThinkError {
    ThinkError::BudgetUnavailable("请求预算台账无法核实或写入，未放行后续请求".into())
}

impl RequestBudget {
    pub fn reserve(&self, model: &str) -> Result<Reservation, ThinkError> {
        let _lock = exclusive_file_lock(&self.home.join("model-request-budget.lock"))
            .map_err(|_| failed())?;
        let mut ledger = Ledger::open(self.home.join("ledger.jsonl")).map_err(|_| failed())?;
        if ledger.events().iter().any(|e| {
            e.kind == EventKind::TaskCreated
                && e.data["task"] == self.task
                && e.data["request_budget_version"] != 4
        }) {
            return Err(ThinkError::BudgetUnavailable("旧任务没有可证明的历史请求计数，不能续跑时重置额度；请核对已有结果后明确建立剩余工作的新任务".into()));
        }
        let mut used = 0u32;
        let mut limit = self.limit;
        for e in ledger
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::ModelRequestReserved && e.data["task"] == self.task)
        {
            let r: Reserved = serde_json::from_value(e.data.clone()).map_err(|_| failed())?;
            used = used.checked_add(1).ok_or_else(failed)?;
            if r.ordinal != used {
                return Err(failed());
            }
            limit = limit.min(r.limit);
        }
        let mut settled = std::collections::BTreeSet::new();
        for e in ledger
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::ModelRequestSettled && e.data["task"] == self.task)
        {
            let r: Settled = serde_json::from_value(e.data.clone()).map_err(|_| failed())?;
            if r.ordinal > used || !settled.insert(r.ordinal) {
                return Err(failed());
            }
        }
        if used >= limit {
            return Err(ThinkError::RequestBudgetExhausted { limit });
        }
        let ordinal = used.checked_add(1).ok_or_else(failed)?;
        let data = serde_json::to_value(Reserved {
            task: self.task.clone(),
            ordinal,
            limit,
            model: model.into(),
        })
        .map_err(|_| failed())?;
        ledger
            .append(EventKind::ModelRequestReserved, None, data)
            .map_err(|_| failed())?;
        // append flushes userspace buffers; quota must also survive an abrupt process/power loss.
        std::fs::OpenOptions::new()
            .write(true)
            .open(self.home.join("ledger.jsonl"))
            .and_then(|f| f.sync_all())
            .map_err(|_| failed())?;
        Ok(Reservation {
            budget: self.clone(),
            ordinal,
        })
    }
}

pub struct Reservation {
    budget: RequestBudget,
    ordinal: u32,
}
impl Reservation {
    pub fn settle(self, result: &Result<ThinkResponse, ThinkError>) -> Result<(), ThinkError> {
        let usage = match result {
            Ok(r) if r.usage.prompt_tokens != 0 || r.usage.completion_tokens != 0 => Some(r.usage),
            Err(ThinkError::OutputLimit { usage }) => Some(*usage),
            _ => None,
        };
        let _lock = exclusive_file_lock(&self.budget.home.join("model-request-budget.lock"))
            .map_err(|_| failed())?;
        let mut ledger =
            Ledger::open(self.budget.home.join("ledger.jsonl")).map_err(|_| failed())?;
        let data = serde_json::to_value(Settled {
            task: self.budget.task,
            ordinal: self.ordinal,
            success: result.is_ok(),
            usage,
        })
        .map_err(|_| failed())?;
        ledger
            .append(EventKind::ModelRequestSettled, None, data)
            .map_err(|_| failed())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn budget(name: &str, limit: u32) -> RequestBudget {
        let home = std::env::temp_dir().join(format!(
            "yunxi-budget-{}-{name}-{}",
            std::process::id(),
            crate::now_millis().unwrap()
        ));
        std::fs::create_dir_all(&home).unwrap();
        RequestBudget {
            home,
            task: "task".into(),
            limit,
        }
    }
    #[test]
    fn restart_and_unknown_failure_never_refill_or_raise_the_budget() {
        let b = budget("restart", 2);
        let r = b.reserve("model").unwrap();
        r.settle(&Err(ThinkError::Network("unknown".into())))
            .unwrap();
        let _crashed = b.reserve("model").unwrap();
        let restored = RequestBudget {
            limit: 200,
            ..b.clone()
        };
        assert!(matches!(
            restored.reserve("model"),
            Err(ThinkError::RequestBudgetExhausted { limit: 2 })
        ));
        let l = Ledger::open(b.home.join("ledger.jsonl")).unwrap();
        let settled = l
            .events()
            .iter()
            .find(|e| e.kind == EventKind::ModelRequestSettled)
            .unwrap();
        assert!(settled.data["usage"].is_null());
        std::fs::remove_dir_all(b.home).unwrap();
    }
    #[test]
    fn unavailable_lock_bad_event_or_old_ledger_never_grants_a_request() {
        let b = budget("blocked", 1);
        let lock = exclusive_file_lock(&b.home.join("model-request-budget.lock")).unwrap();
        assert!(b.reserve("m").is_err());
        drop(lock);
        std::fs::write(b.home.join("ledger.jsonl"), "{\"yunxi_bot_ledger\":3}\n").unwrap();
        assert!(b.reserve("m").is_err());
        std::fs::remove_dir_all(b.home).unwrap();
    }

    #[test]
    fn legacy_tasks_and_duplicate_settlements_do_not_create_free_quota() {
        let b = budget("legacy", 3);
        let mut l = Ledger::open(b.home.join("ledger.jsonl")).unwrap();
        l.append(
            EventKind::TaskCreated,
            None,
            serde_json::json!({"task":"task","goal":"old"}),
        )
        .unwrap();
        assert!(b.reserve("model").is_err());
        drop(l);
        std::fs::remove_dir_all(b.home).unwrap();
        let b = budget("duplicate", 3);
        b.reserve("m")
            .unwrap()
            .settle(&Err(ThinkError::Network("unknown".into())))
            .unwrap();
        let mut l = Ledger::open(b.home.join("ledger.jsonl")).unwrap();
        let data = l.events().last().unwrap().data.clone();
        l.append(EventKind::ModelRequestSettled, None, data)
            .unwrap();
        assert!(b.reserve("m").is_err());
        drop(l);
        std::fs::remove_dir_all(b.home).unwrap();
    }

    #[test]
    fn concurrent_reservations_never_overdraw_a_task() {
        let b = budget("concurrent", 3);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let b = b.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    b.reserve("m").is_ok()
                })
            })
            .collect();
        let granted = threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|ok| *ok)
            .count();
        assert!((1..=3).contains(&granted));
        let l = Ledger::open(b.home.join("ledger.jsonl")).unwrap();
        assert_eq!(
            l.events()
                .iter()
                .filter(|e| e.kind == EventKind::ModelRequestReserved)
                .count(),
            granted
        );
        std::fs::remove_dir_all(b.home).unwrap();
    }
}
