//! 复用已授权的Mail只读接口，不再要求为同一邮箱另存IMAP凭证。
use super::{FetchBatch, InfoItem, InfoSource};
use crate::tool::macos::{bundled_app, request};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

type Native<'a> = dyn FnMut(&str, &Value) -> Result<Value, String> + 'a;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Inbox {
    account_id: String,
    mailbox_path: Vec<String>,
}

pub struct MacMailSource {
    inboxes: Vec<Inbox>,
}

fn call(action: &str, args: &Value) -> Result<Value, String> {
    request(&bundled_app(), action, args).map_err(|e| e.to_string())
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value[key]
        .as_array()
        .ok_or_else(|| format!("Mail回执缺少数组{key}"))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("Mail回执缺少文本{key}"))
}

fn item_from_native(
    data: &Value,
    received_at_ms: u64,
    addresses: &[Value],
) -> Result<InfoItem, String> {
    let sender = string(data, "sender")?;
    let (name, addr) = sender
        .rsplit_once('<')
        .filter(|(_, a)| a.ends_with('>'))
        .map(|(n, a)| (n.trim().trim_matches('"'), a.trim_end_matches('>').trim()))
        .unwrap_or(("", sender.trim()));
    let to = data["to"].as_array().cloned().unwrap_or_default();
    let cc = data["cc"].as_array().cloned().unwrap_or_default();
    let directly = to.iter().any(|r| {
        r.as_str().is_some_and(|r| {
            addresses
                .iter()
                .any(|a| a.as_str().is_some_and(|a| a.eq_ignore_ascii_case(r)))
        })
    });
    Ok(InfoItem {
        source: "mac_mail".into(),
        id: json!({"account_id":data["account_id"],"mailbox_path":data["mailbox_path"],"id":data["id"]}).to_string(),
        from_name: name.into(), from_addr: addr.into(), subject: string(data, "subject")?.into(),
        preview: data["body"].as_str().unwrap_or("").chars().take(1200).collect(), received_at_ms,
        direct: directly && to.len() + cc.len() == 1, recipient_count: to.len()+cc.len(),
        addressed_directly: directly, has_attachments: data["attachments"].as_array().is_some_and(|a| !a.is_empty()),
    })
}

impl MacMailSource {
    pub fn from_home(home: &Path) -> Result<Self, String> {
        let path = home.join("mac-mail.json");
        let text = std::fs::read_to_string(&path).map_err(|_| {
            format!(
                "缺少或无法读取{}，需配置mac_mail_accounts列出的账户与收件箱路径",
                path.display()
            )
        })?;
        Self::parse(&text)
    }
    fn parse(text: &str) -> Result<Self, String> {
        let inboxes: Vec<Inbox> =
            serde_json::from_str(text).map_err(|_| "mac-mail.json格式无效".to_string())?;
        let mut seen = BTreeSet::new();
        if inboxes.is_empty()
            || inboxes.len() > 10
            || inboxes.iter().any(|i| {
                i.account_id.trim().is_empty()
                    || i.account_id == "local"
                    || i.account_id.len() > 200
                    || i.account_id.contains('\0')
                    || i.mailbox_path.is_empty()
                    || i.mailbox_path.len() > 12
                    || i.mailbox_path.iter().any(|p| {
                        p.is_empty()
                            || p.len() > 200
                            || p.contains('\0')
                            || [".", ".."].contains(&p.as_str())
                    })
                    || !seen.insert((i.account_id.clone(), i.mailbox_path.clone()))
            })
        {
            return Err("收件箱配置须为1–10个不重复的真实账户与路径；不接受本地合成邮箱".into());
        }
        Ok(Self { inboxes })
    }
    fn accounts(&self, native: &mut Native<'_>) -> Result<Value, String> {
        let data = native("mac_mail_accounts", &json!({}))?;
        if data["configured"] != true {
            return Err("Mac Mail尚未配置可用账户".into());
        }
        let accounts = array(&data, "accounts")?;
        for inbox in &self.inboxes {
            let account = accounts
                .iter()
                .find(|a| a["id"] == inbox.account_id)
                .ok_or("配置的Mail账户已不可用")?;
            if !array(account, "mailboxes")?
                .iter()
                .any(|b| b["path"] == json!(inbox.mailbox_path))
            {
                return Err("配置的收件箱已移动或不存在，未把它当成空邮箱".into());
            }
            if array(account, "addresses")?.is_empty() {
                return Err("Mail账户缺少本人地址，不能判断直接收件人".into());
            }
        }
        Ok(data)
    }
    fn scan(
        &self,
        unread_only: bool,
        native: &mut Native<'_>,
    ) -> Result<(Value, Vec<(u64, Value)>), String> {
        let accounts = self.accounts(native)?;
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        for inbox in &self.inboxes {
            let mut offset = 0;
            let mut expected_total = None;
            loop {
                let page = native(
                    "mac_mail_search",
                    &json!({"account_id":inbox.account_id,"mailbox_path":inbox.mailbox_path,"unread_only":unread_only,"offset":offset,"limit":1000}),
                )?;
                let count = page["total_matches"].as_u64().ok_or("Mail未报告匹配总数")?;
                if count > 1000 || expected_total.is_some_and(|n| n != count) {
                    return Err("扫描超过1000封或分页期间邮箱变化，请缩小收件箱范围后重试".into());
                }
                expected_total = Some(count);
                let items = array(&page, "items")?;
                for item in items {
                    let id = string(item, "id")?;
                    if id.is_empty()
                        || item["account_id"] != inbox.account_id
                        || item["mailbox_path"] != json!(inbox.mailbox_path)
                        || (unread_only && item["read"] != false)
                        || !seen.insert((
                            inbox.account_id.clone(),
                            inbox.mailbox_path.clone(),
                            id.to_owned(),
                        ))
                    {
                        return Err("Mail分页重复或记录定位不一致，未把部分读取当完整".into());
                    }
                    let time = chrono::DateTime::parse_from_rfc3339(string(item, "received_at")?)
                        .map_err(|_| "Mail收件时间无效")?
                        .timestamp_millis();
                    let time = u64::try_from(time).map_err(|_| "Mail收件时间超出范围")?;
                    candidates.push((time, item.clone()));
                }
                offset += items.len();
                if offset > count as usize {
                    return Err("Mail分页数量矛盾".into());
                }
                if page["next_offset"].is_null() {
                    if page["truncated"] != false || offset != count as usize {
                        return Err("Mail分页不完整".into());
                    }
                    break;
                }
                if items.is_empty()
                    || page["next_offset"].as_u64() != Some(offset as u64)
                    || page["truncated"] != true
                {
                    return Err("Mail分页没有前进，停止读取".into());
                }
            }
        }
        Ok((accounts, candidates))
    }
    fn snapshot_with(&self, native: &mut Native<'_>) -> Result<Vec<InfoItem>, String> {
        let (_, candidates) = self.scan(false, native)?;
        candidates
            .into_iter()
            .map(|(time, meta)| item_from_native(&meta, time, &[]))
            .collect()
    }
    fn fetch_with(&self, limit: usize, native: &mut Native<'_>) -> Result<FetchBatch, String> {
        if !(1..=100).contains(&limit) {
            return Err("Mac邮件每轮读取上限须为1–100".into());
        }
        let (accounts, mut candidates) = self.scan(true, native)?;
        let total = candidates.len();
        candidates.sort_by_key(|a| std::cmp::Reverse(a.0));
        let mut items = Vec::new();
        for (received_at_ms, meta) in candidates.into_iter().take(limit) {
            let location = json!({"account_id":meta["account_id"],"mailbox_path":meta["mailbox_path"],"id":meta["id"]});
            let read = native("mac_mail_read", &location)?;
            if read["read"] != false
                || read["id"] != meta["id"]
                || read["account_id"] != meta["account_id"]
                || read["mailbox_path"] != meta["mailbox_path"]
            {
                return Err("读取期间邮件已读或发生移动，稍后重新巡检".into());
            }
            let account = array(&accounts, "accounts")?
                .iter()
                .find(|a| a["id"] == meta["account_id"])
                .ok_or("Mail账户消失")?;
            items.push(item_from_native(
                &read,
                received_at_ms,
                array(account, "addresses")?,
            )?);
        }
        Ok(FetchBatch {
            items,
            total_unseen: total,
            skipped: 0,
        })
    }
    fn review_with(
        &self,
        item: &InfoItem,
        native: &mut Native<'_>,
    ) -> Result<(InfoItem, String), String> {
        let mut location: Value = serde_json::from_str(&item.id).map_err(|_| "无效Mac邮件定位")?;
        if !self.inboxes.iter().any(|i| {
            location["account_id"] == i.account_id
                && location["mailbox_path"] == json!(i.mailbox_path)
        }) {
            return Err("邮件不属于配置收件箱".into());
        }
        let accounts = self.accounts(native)?;
        let first = native("mac_mail_read", &location)?;
        let count = first["total_characters"]
            .as_u64()
            .ok_or("未报告正文完整性")?;
        if count > 32_000 {
            return Err("正文超过32000字符分析预算，待人工查看；没有用截断内容判断".into());
        }
        let revision = string(&first, "revision")?.to_owned();
        let account = array(&accounts, "accounts")?
            .iter()
            .find(|a| a["id"] == location["account_id"])
            .ok_or("Mail账户消失")?;
        for key in ["to", "cc", "attachments"] {
            array(&first, key)?;
        }
        let full = item_from_native(&first, item.received_at_ms, array(account, "addresses")?)?;
        if full.id != item.id || full.subject != item.subject || full.from_addr != item.from_addr {
            return Err("邮件在发现和读取之间发生变化，待下轮重新获取".into());
        }
        let mut page = first;
        let mut body = String::new();
        for _ in 0..4 {
            body.push_str(string(&page, "body")?);
            if body.chars().count() > 32_000 {
                return Err("正文超过分析预算".into());
            }
            if page["next_offset"].is_null() {
                if body.chars().count() as u64 != count {
                    return Err("正文分页不完整".into());
                }
                return Ok((full, body));
            }
            let next = page["next_offset"].as_u64().ok_or("无效正文分页")?;
            if next != body.chars().count() as u64
                || next <= location["offset"].as_u64().unwrap_or(0)
            {
                return Err("正文分页没有前进".into());
            }
            location["offset"] = json!(next);
            page = native("mac_mail_read", &location)?;
            if string(&page, "revision")? != revision {
                return Err("邮件在读取期间变化".into());
            }
        }
        Err("正文分页超出分析预算".into())
    }
}

impl InfoSource for MacMailSource {
    fn name(&self) -> &'static str {
        "Mac Mail收件箱（本机已同步内容）"
    }
    fn health(&self) -> Result<(), String> {
        self.accounts(&mut call).map(|_| ())
    }
    fn fetch(&self, limit: usize) -> Result<FetchBatch, String> {
        self.fetch_with(limit, &mut call)
    }
    fn snapshot(&self) -> Result<Vec<InfoItem>, String> {
        self.snapshot_with(&mut call)
    }
    fn read_for_review(&self, item: &InfoItem) -> Result<(InfoItem, String), String> {
        self.review_with(item, &mut call)
    }
    fn read(&self, id: &str) -> Result<String, String> {
        let mut location: Value = serde_json::from_str(id).map_err(|_| "无效的Mac邮件定位")?;
        if !self.inboxes.iter().any(|i| {
            location["account_id"] == i.account_id
                && location["mailbox_path"] == json!(i.mailbox_path)
        }) {
            return Err("邮件不属于已配置收件箱".into());
        }
        let mut body = String::new();
        let mut revision = None;
        for _ in 0..251 {
            let page = call("mac_mail_read", &location)?;
            let current = string(&page, "revision")?;
            if revision.as_ref().is_some_and(|r| r != current) {
                return Err("邮件在读取期间变化".into());
            }
            revision = Some(current.to_owned());
            body.push_str(string(&page, "body")?);
            if body.len() > 8_000_000 {
                return Err("邮件正文过大".into());
            }
            if page["next_offset"].is_null() {
                return Ok(body);
            }
            let next = page["next_offset"].as_u64().ok_or("无效正文分页")?;
            if next <= location["offset"].as_u64().unwrap_or(0) {
                return Err("正文分页没有前进".into());
            }
            location["offset"] = json!(next);
        }
        Err("正文分页超出读取上限".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> MacMailSource {
        MacMailSource::parse(r#"[{"account_id":"a","mailbox_path":["INBOX"]}]"#).unwrap()
    }
    fn reply(action: &str, _: &Value) -> Result<Value, String> {
        Ok(match action {
            "mac_mail_accounts" => {
                json!({"configured":true,"accounts":[{"id":"a","addresses":["me@example.com"],"mailboxes":[{"path":["INBOX"]}]}]})
            }
            "mac_mail_search" => {
                json!({"total_matches":1,"truncated":false,"next_offset":null,"items":[{"account_id":"a","mailbox_path":["INBOX"],"id":"1","received_at":"2026-10-16T10:00:00+08:00","read":false,"sender":"张经理 <boss@example.com>","subject":"确认"}]})
            }
            "mac_mail_read" => {
                json!({"account_id":"a","mailbox_path":["INBOX"],"id":"1","read":false,"sender":"张经理 <boss@example.com>","subject":"确认","body":"请今天回复。","to":["ME@example.com"],"cc":[],"attachments":[]})
            }
            _ => panic!("不能调用写入接口"),
        })
    }
    #[test]
    fn snapshot_includes_read_mail_and_does_not_read_bodies() {
        let items = source()
            .snapshot_with(&mut |action, args| {
                assert_ne!(action, "mac_mail_read");
                let mut result = reply(action, args)?;
                if action == "mac_mail_search" {
                    assert_eq!(args["unread_only"], false);
                    result["items"][0]["read"] = json!(true);
                    result["items"][0]["sender"] = json!("noreply@example.com");
                    result["items"][0]["subject"] = json!("航班取消");
                }
                Ok(result)
            })
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "航班取消");
    }

    #[test]
    fn bounded_inbox_snapshot_uses_one_native_metadata_request() {
        let mut searches = 0;
        let items = source()
            .snapshot_with(&mut |action, args| {
                assert_ne!(action, "mac_mail_read");
                let mut result = reply(action, args)?;
                if action == "mac_mail_search" {
                    searches += 1;
                    let start = args["offset"].as_u64().unwrap();
                    let end = (start + args["limit"].as_u64().unwrap()).min(550);
                    let template = result["items"][0].clone();
                    result["items"] = json!(
                        (start..end)
                            .map(|i| {
                                let mut item = template.clone();
                                item["id"] = json!((i + 1).to_string());
                                item
                            })
                            .collect::<Vec<_>>()
                    );
                    result["total_matches"] = json!(550);
                    result["truncated"] = json!(end < 550);
                    result["next_offset"] = if end < 550 { json!(end) } else { Value::Null };
                }
                Ok(result)
            })
            .unwrap();
        assert_eq!(items.len(), 550);
        assert_eq!(searches, 1, "避免每页重新启动原生程序并读取整箱属性");
    }

    #[test]
    fn analysis_reads_tail_of_read_mail_and_rejects_partial_or_changed_pages() {
        let head = source().snapshot_with(&mut reply).unwrap().remove(0);
        for failure in ["none", "changed", "incomplete", "oversize"] {
            let result = source().review_with(&head, &mut |action, args| {
                let mut value = reply(action, args)?;
                if action == "mac_mail_read" {
                    value["read"] = json!(true);
                    value["revision"] = json!(if failure == "changed" && args["offset"] == 4 {
                        "v2"
                    } else {
                        "v1"
                    });
                    value["total_characters"] =
                        json!(if failure == "oversize" { 32001 } else { 8 });
                    value["body"] = json!(if args["offset"] == 4 {
                        "明天面试"
                    } else {
                        "背景资料"
                    });
                    value["next_offset"] = if args["offset"] == 4 || failure == "incomplete" {
                        Value::Null
                    } else {
                        json!(4)
                    };
                }
                Ok(value)
            });
            if failure == "none" {
                assert_eq!(result.unwrap().1, "背景资料明天面试");
            } else {
                assert!(result.is_err(), "{failure}");
            }
        }
    }

    #[test]
    fn native_mail_source_reads_content_without_marking_read_and_keeps_stable_identity() {
        let a = source().fetch_with(20, &mut reply).unwrap();
        let b = source().fetch_with(20, &mut reply).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.items[0].from_addr, "boss@example.com");
        assert_eq!(a.items[0].preview, "请今天回复。");
        assert!(a.items[0].addressed_directly && a.items[0].direct);
    }
    #[test]
    fn missing_account_partial_pagination_and_changed_message_do_not_become_silence() {
        for (action, key, value) in [
            ("mac_mail_accounts", "configured", json!(false)),
            ("mac_mail_search", "truncated", json!(true)),
            ("mac_mail_search", "total_matches", json!(2)),
            ("mac_mail_read", "read", json!(true)),
        ] {
            assert!(
                source()
                    .fetch_with(20, &mut |a, v| {
                        let mut r = reply(a, v)?;
                        if a == action {
                            r[key] = value.clone();
                        }
                        Ok(r)
                    })
                    .is_err()
            );
        }
        assert!(
            source()
                .fetch_with(20, &mut |_, _| Err("权限关闭".into()))
                .is_err()
        );
    }
    #[test]
    fn pagination_respects_native_catalog_and_selects_newest_before_limit() {
        let catalog: Value = serde_json::from_str(include_str!(
            "../../../../native/macos/assistant-tools.json"
        ))
        .unwrap();
        let max = catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "mac_mail_search")
            .unwrap()["parameters"]["properties"]["limit"]["maximum"]
            .as_u64()
            .unwrap();
        let mut searched = 0;
        let mut read_ids = Vec::new();
        let batch = source()
            .fetch_with(1, &mut |action, args| {
                let mut value = reply(action, args)?;
                if action == "mac_mail_search" {
                    assert!(args["limit"].as_u64().unwrap() <= max);
                    let page = args["offset"].as_u64().unwrap();
                    assert_eq!(page, searched);
                    searched += 1;
                    value["total_matches"] = json!(2);
                    value["truncated"] = json!(page == 0);
                    value["next_offset"] = if page == 0 { json!(1) } else { Value::Null };
                    value["items"][0]["id"] = json!((page + 1).to_string());
                    value["items"][0]["received_at"] =
                        json!(format!("2026-10-16T1{page}:00:00+08:00"));
                } else if action == "mac_mail_read" {
                    read_ids.push(args["id"].clone());
                    value["id"] = args["id"].clone();
                }
                Ok(value)
            })
            .unwrap();
        assert_eq!(searched, 2);
        assert_eq!(batch.total_unseen, 2);
        assert_eq!(batch.items.len(), 1);
        assert_eq!(read_ids, vec![json!("2")]);
    }
    #[test]
    fn invalid_or_duplicate_inbox_configuration_is_rejected() {
        for text in [
            "[]",
            r#"[{"account_id":"local","mailbox_path":["INBOX"]}]"#,
            r#"[{"account_id":"a","mailbox_path":[]}]"#,
            r#"[{"account_id":"a","mailbox_path":["INBOX"]},{"account_id":"a","mailbox_path":["INBOX"]}]"#,
        ] {
            assert!(MacMailSource::parse(text).is_err());
        }
        assert!(source().fetch_with(0, &mut reply).is_err());
    }
}
