//! 固定投递批次先持久化，再调用系统；逐封审计复用同一批回执。
use super::*;
use crate::{
    assistant::PassInput,
    companion::{Intervention, notification_ceiling},
    decide::Decider,
    feedback,
    notify::{Delivery, Notification, Notifier},
    triage::{TriagePolicy, deterministic_triage, rules},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingNotice {
    digest: bool,
    notification: Notification,
    members: Vec<InfoItem>,
    inputs: Vec<String>,
    next_ms: u64,
    receipt: Option<Delivery>,
}

pub(super) fn validate(state: &State) -> Result<(), String> {
    let mut lanes = std::collections::BTreeSet::new();
    for p in &state.notices {
        let n = &p.notification;
        if !lanes.insert(p.digest)
            || p.members.is_empty()
            || p.members.len() > 100
            || p.inputs.len() != p.members.len()
            || n.title.is_empty()
            || n.title.len() > 500
            || n.body.is_empty()
            || n.body.len() > 4000
            || n.tag.is_empty()
            || n.tag.len() > 4000
            || [&n.title, &n.body, &n.tag].iter().any(|s| s.contains('\0'))
            || p.receipt
                .as_ref()
                .is_some_and(|r| !r.left_the_process() || r.detail().is_empty())
        {
            return Err("邮件通知批次损坏，未丢弃或重建".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for (m, input) in p.members.iter().zip(&p.inputs) {
            if !ids.insert(key(m)) || !state.entries.contains_key(&key(m)) {
                return Err("邮件通知批次身份不一致".into());
            }
            let v: Value = serde_json::from_str(input).map_err(|_| "通知原文快照损坏")?;
            if v["mail"] != json!(m)
                || v["body"]
                    .as_str()
                    .is_none_or(|body| body.chars().count() > 32_000)
            {
                return Err("通知原文快照身份或范围无效".into());
            }
        }
    }
    Ok(())
}

impl MailReview {
    fn build_notice(&self, digest: bool, now_ms: u64) -> Result<Option<PendingNotice>, String> {
        if digest && now_ms < self.state.digest_after_ms {
            return Ok(None);
        }
        let mut entries: Vec<_> = self
            .state
            .entries
            .values()
            .filter(|e| e.status == if digest { "digest" } else { "ready" })
            .filter(|e| e.next_ms <= now_ms)
            .collect();
        entries.sort_by_key(|e| e.head.received_at_ms);
        let mut members = Vec::new();
        let mut inputs = Vec::new();
        let mut parts = Vec::new();
        let mut bytes = 0;
        for e in entries {
            let a = e.analysis.as_ref().ok_or("通知候选缺少有效分析")?;
            let (_, body) = a.notification(&e.item);
            let part = format!(
                "【{}】{}",
                if a.risks.is_empty() {
                    &a.label
                } else {
                    "可疑邮件"
                },
                body
            );
            if bytes + part.len() + 2 > 3900 {
                if members.is_empty() {
                    return Err("单封邮件通知超长，待查看；未截断关键信息".into());
                }
                break;
            }
            bytes += part.len() + 2;
            parts.push(part);
            members.push(e.item.clone());
            inputs.push(e.input.clone().ok_or("通知候选缺少原文快照")?);
        }
        let Some(first) = members.first() else {
            return Ok(None);
        };
        let (title, body) = if !digest && members.len() == 1 {
            self.state.entries[&key(first)]
                .analysis
                .as_ref()
                .ok_or("通知缺少分析")?
                .notification(first)
        } else {
            (
                format!(
                    "云熙 · 邮件｜{}（{}封）",
                    if digest {
                        "摘要"
                    } else {
                        "重要事项汇总"
                    },
                    members.len()
                ),
                parts.join("\n\n"),
            )
        };
        Ok(Some(PendingNotice {
            digest,
            notification: Notification::new(title, body)
                .with_tag(format!("mail-batch:{digest}:{now_ms}:{}", key(first))),
            members,
            inputs,
            next_ms: now_ms,
            receipt: None,
        }))
    }

    fn recheck_notice(
        &self,
        p: &PendingNotice,
        source: &dyn InfoSource,
        policy: &TriagePolicy,
    ) -> Result<(), String> {
        for (m, original_input) in p.members.iter().zip(&p.inputs) {
            if deterministic_triage(m, policy)
                .is_some_and(|(_, rule, _)| rule == rules::BLOCKED_SENDER)
            {
                return Err("投递前发件人已被屏蔽，批次取消并重新分流".into());
            }
            let e = self.state.entries.get(&key(m)).ok_or("通知邮件记录丢失")?;
            if e.input.as_ref() != Some(original_input) {
                return Err("固定通知批次的原文版本已变化，需重新分流".into());
            }
            let input: Value =
                serde_json::from_str(e.input.as_deref().ok_or("通知缺少已审理原文")?)
                    .map_err(|_| "通知原文损坏")?;
            let (full, body) = source
                .read_for_review(m)
                .map_err(|_| "投递前原邮件无法回读，批次转待查看")?;
            if input["mail"] != json!(full) || input["body"] != body || e.item != *m {
                return Err("投递前邮件内容或身份变化，需重新审理".into());
            }
        }
        Ok(())
    }

    fn record_notice<N: Notifier + ?Sized>(
        &mut self,
        p: &PendingNotice,
        delivery: &Delivery,
        ledger: &mut Ledger,
        notifier: &N,
        now_ms: u64,
        dry_run: bool,
    ) -> Result<(), String> {
        let seen = feedback::seen(ledger.events());
        for m in &p.members {
            if !feedback::already_seen(&seen, m) {
                ledger.append(EventKind::NoticeSent, None, json!({
                    "channel":notifier.name(), "source":m.source, "id":m.id,
                    "from":m.from_addr, "from_name":m.from_name, "subject":m.subject,
                    "title":p.notification.title, "body":p.notification.body,
                    "result":delivery.label(), "detail":delivery.detail(),
                    "confirmed":delivery.can_claim_user_notified(), "dry_run":dry_run, "at_ms":now_ms,
                })).map_err(|_| "邮件通知回执审计失败；保留固定批次待恢复")?;
            }
            self.finish(
                m,
                if delivery.left_the_process() {
                    "notified"
                } else {
                    "held"
                },
                &format!(
                    "{}；{}",
                    if p.digest {
                        "普通邮件合并摘要"
                    } else {
                        "及时邮件通知"
                    },
                    delivery.label()
                ),
                now_ms,
            )?;
        }
        if p.digest && delivery.left_the_process() {
            self.state.digest_after_ms = now_ms.saturating_add(RETRY_MS);
        }
        self.save()
    }

    pub(crate) fn flush_notifications<D: Decider, N: Notifier + ?Sized>(
        &mut self,
        ledger: &mut Ledger,
        input: &PassInput<'_, D, N>,
    ) -> Result<usize, String> {
        let quiet = notification_ceiling(&input.ctx, &input.companion, input.local_hour);
        let mut sent = 0;
        // 每轮最多各投递一个有限长度批次，余项留存；重要邮件不等普通摘要。
        for digest in [false, true] {
            let existing = self
                .state
                .notices
                .iter()
                .find(|p| p.digest == digest)
                .cloned();
            if input.dry_run && existing.is_some() {
                continue;
            }
            // 已取得回执只补审计，静默时段也不再次调用通知出口。
            if let Some(p) = &existing
                && let Some(receipt) = &p.receipt
            {
                self.record_notice(p, receipt, ledger, input.notifier, input.now_ms, false)?;
                self.state.notices.retain(|p| p.digest != digest);
                self.save()?;
                continue;
            }
            if quiet.ceiling != Intervention::Speak {
                for e in self
                    .state
                    .entries
                    .values_mut()
                    .filter(|e| ["ready", "digest"].contains(&e.status.as_str()))
                {
                    e.reason = quiet.reason.clone();
                    e.next_ms = e.next_ms.max(input.now_ms.saturating_add(RETRY_MS));
                }
                self.save()?;
                continue;
            }
            let p = match existing {
                Some(p) if p.next_ms > input.now_ms => continue,
                Some(p) => p,
                None => match self.build_notice(digest, input.now_ms)? {
                    Some(p) => p,
                    None => continue,
                },
            };
            if let Err(reason) = self.recheck_notice(&p, input.source, &input.policy) {
                for m in &p.members {
                    ledger
                        .append(
                            EventKind::InfoTriaged,
                            None,
                            json!({"source":m.source,"id":m.id,
                        "action":"Hold","reason":reason,"degraded":true,"at_ms":input.now_ms}),
                        )
                        .map_err(|_| "通知前复核失败的审计不可写")?;
                    let blocked = deterministic_triage(m, &input.policy)
                        .is_some_and(|(_, rule, _)| rule == rules::BLOCKED_SENDER);
                    self.finish(
                        m,
                        if blocked { "filtered" } else { "held" },
                        &reason,
                        input.now_ms,
                    )?;
                }
                self.state.notices.retain(|n| n.digest != digest);
                self.save()?;
                continue;
            }
            if !input.dry_run {
                self.state.notices.retain(|n| n.digest != digest);
                let mut staged = p.clone();
                staged.next_ms = input.now_ms.saturating_add(RETRY_MS);
                self.state.notices.push(staged);
                validate(&self.state)?;
                self.save()?;
            }
            for m in &p.members {
                ledger.append(EventKind::InfoTriaged, None, json!({
                    "source":m.source,"id":m.id,"action":"Speak","used_model":false,
                    "reason": if digest { "普通邮件合并窗口到期，静默与原文复核已通过" } else { "及时通知批次已准备，静默与原文复核已通过" },
                    "degraded":false,"at_ms":input.now_ms,
                })).map_err(|_| "邮件批次投递决策审计失败")?;
            }
            let delivery = if input.dry_run {
                Delivery::Blocked {
                    reason: "演练模式：没有真的发通知".into(),
                }
            } else {
                input.notifier.notify(&p.notification)
            };
            sent += 1;
            if delivery.left_the_process() {
                self.state
                    .notices
                    .iter_mut()
                    .filter(|n| n.digest == digest)
                    .for_each(|n| n.receipt = Some(delivery.clone()));
                self.save()?;
            }
            self.record_notice(
                &p,
                &delivery,
                ledger,
                input.notifier,
                input.now_ms,
                input.dry_run,
            )?;
            if delivery.left_the_process() || input.dry_run {
                self.state.notices.retain(|p| p.digest != digest);
                self.save()?;
            }
        }
        Ok(sent)
    }
}
