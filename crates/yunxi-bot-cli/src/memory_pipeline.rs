//! 本地记忆整理流水线。
//!
//! chat-server 的闲置线程和 daemon 共用同一套逻辑与 `memory.lock`：
//! Laya 门控 → 本地模型抽取 → 自动确认 → 本地向量化。
//! 这里不碰云端；本机模型未监听时直接返回 Unavailable，不写“已完成”标记。

use std::path::PathBuf;
use std::sync::Arc;

use yunxi_bot_core::decide::Decider;
use yunxi_bot_core::think::Thinker;
use yunxi_bot_core::{EventKind, Ledger};

use crate::companion_runtime::{
    GateAction, KnowledgeAction, clean_knowledge_text, gate_knowledge, gate_memory,
    warm_knowledge_vectors, warm_memory_vectors,
};

#[derive(Debug)]
pub enum MemoryOutcome {
    /// 没有待整理的对话。
    Idle,
    /// 另一个整理线程持有锁。
    Busy,
    /// 本机模型未监听；不消耗、不标记。
    Unavailable,
    /// 门控判定不值得存。
    Skipped,
    /// 已写入并尝试向量化。
    Stored { count: usize, vectorized: bool },
    /// 已写入知识库并尝试向量化。
    KnowledgeStored { id: String, vectorized: bool },
    /// 抽取失败；来源保留给下次重试。
    Failed(String),
}

pub struct MemoryPipeline {
    home: PathBuf,
    thinker: Arc<dyn Thinker>,
    decider: Arc<dyn Decider>,
}

impl MemoryPipeline {
    pub fn new(home: PathBuf, thinker: Arc<dyn Thinker>, decider: Arc<dyn Decider>) -> Self {
        Self {
            home,
            thinker,
            decider,
        }
    }

    fn model_listening(&self) -> bool {
        std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], 17872)),
            std::time::Duration::from_millis(250),
        )
        .is_ok()
    }

    /// 处理下一条未整理的 `ConversationObserved`。每次调用只处理一条。
    pub fn process_next(&self) -> MemoryOutcome {
        if !self.model_listening() {
            return MemoryOutcome::Unavailable;
        }
        let lock =
            match yunxi_bot_core::instance::InstanceLock::acquire(self.home.join("memory.lock")) {
                Ok(lock) => lock,
                Err(e) => return MemoryOutcome::busy_or_lock_error(e.to_string()),
            };
        // 锁在函数退出时释放；下面的写入都在锁内。
        let outcome = self.process_locked();
        drop(lock);
        outcome
    }

    fn process_locked(&self) -> MemoryOutcome {
        let now = match yunxi_bot_core::now_millis() {
            Ok(now) => now,
            Err(e) => return MemoryOutcome::Failed(e.to_string()),
        };
        let mut ledger = match Ledger::open(self.home.join("ledger.jsonl")) {
            Ok(l) => l,
            Err(e) => return MemoryOutcome::Failed(format!("台账不可用: {e}")),
        };
        let events = ledger.events().to_vec();
        let Some(source) = yunxi_bot_core::continuity::next_source(&events, now) else {
            return MemoryOutcome::Idle;
        };
        let (action, reason, degraded) = gate_memory(&*self.decider, &source);
        if action == GateAction::Skip {
            let _ = ledger.append(
                EventKind::InfoTriaged,
                None,
                serde_json::json!({
                    "source":"companion_memory",
                    "source_seq":source.seq,
                    "phase":"gate_skip",
                    "degraded":degraded,
                    "reason":reason,
                }),
            );
            return MemoryOutcome::Skipped;
        }
        let _ = ledger.append(
            EventKind::InfoTriaged,
            None,
            serde_json::json!({
                "source":"companion_memory_gate",
                "source_seq":source.seq,
                "phase":"gate_store",
                "degraded":false,
                "reason":reason,
            }),
        );
        let (knowledge_action, knowledge_reason, knowledge_degraded) =
            gate_knowledge(&*self.decider, &source);
        let _ = ledger.append(
            EventKind::InfoTriaged,
            None,
            serde_json::json!({
                "source":"companion_knowledge_gate",
                "source_seq":source.seq,
                "phase":"gate_store",
                "degraded":knowledge_degraded,
                "reason":knowledge_reason,
            }),
        );
        if knowledge_action == KnowledgeAction::Knowledge {
            let text = clean_knowledge_text(source.data["text"].as_str().unwrap_or(""));
            let label = format!("conversation#{}", source.seq);
            let id = match yunxi_bot_core::retrieval::import_text(&self.home, &label, &text) {
                Ok(id) => id,
                Err(e) => return MemoryOutcome::Failed(format!("知识入库失败: {e}")),
            };
            let _ = ledger.append(
                EventKind::InfoTriaged,
                None,
                serde_json::json!({
                    "source":"companion_knowledge",
                    "source_seq":source.seq,
                    "phase":"imported",
                    "id":id,
                    "degraded":false,
                    "reason":knowledge_reason,
                }),
            );
            // 标记该对话已处理，避免下一轮又被记忆流水线捡起。
            let _ = ledger.append(
                EventKind::InfoTriaged,
                None,
                serde_json::json!({
                    "source":"companion_memory",
                    "source_seq":source.seq,
                    "phase":"gate_knowledge",
                    "degraded":false,
                    "reason":knowledge_reason,
                }),
            );
            let _ = yunxi_bot_core::retrieval::clear_cache(&self.home, "knowledge");
            let vectorized = warm_knowledge_vectors(&self.home).is_ok();
            return MemoryOutcome::KnowledgeStored { id, vectorized };
        }

        let batch = yunxi_bot_core::continuity::consolidate(&*self.thinker, &source, &events, now);
        let source_seq = batch.source_seq;
        let n = match yunxi_bot_core::continuity::save_consolidation(&mut ledger, batch, now) {
            Ok(n) => n,
            Err(e) => return MemoryOutcome::Failed(e),
        };
        if n == 0 {
            return MemoryOutcome::Failed("本地抽取没有产生有效候选".into());
        }
        let ids: Vec<String> = match yunxi_bot_core::continuity::pending(ledger.events(), now) {
            Ok(pending) => pending
                .into_iter()
                .filter(|candidate| candidate.source_seq == source_seq)
                .map(|candidate| candidate.id)
                .collect(),
            Err(e) => return MemoryOutcome::Failed(e),
        };
        for id in &ids {
            if let Err(e) = yunxi_bot_core::continuity::resolve(&mut ledger, id, true, now) {
                return MemoryOutcome::Failed(format!("自动确认失败: {e}"));
            }
        }
        let _ = yunxi_bot_core::retrieval::clear_cache(&self.home, "memory");
        let vectorized = warm_memory_vectors(&self.home).is_ok();
        MemoryOutcome::Stored {
            count: ids.len(),
            vectorized,
        }
    }
}

// 锁占用和系统错误分开只是为了调用方可读；两者都不写“已完成”标记。
impl MemoryOutcome {
    fn busy_or_lock_error(reason: String) -> Self {
        if reason.contains("已有实例") || reason.contains("in use") {
            MemoryOutcome::Busy
        } else {
            MemoryOutcome::Failed(format!("memory.lock: {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_unavailable_does_not_consume_source() {
        let dir =
            std::env::temp_dir().join(format!("yunxi-memory-pipeline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let decider: Arc<dyn Decider> = Arc::new(yunxi_bot_core::decide::StubDecider::succeeding());
        let thinker: Arc<dyn Thinker> =
            Arc::new(yunxi_bot_core::think::client::StubThinker::replying("{}"));
        let pipeline = MemoryPipeline::new(dir.clone(), thinker, decider);
        assert!(matches!(
            pipeline.process_next(),
            MemoryOutcome::Unavailable
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
