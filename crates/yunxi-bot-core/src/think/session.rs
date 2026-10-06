//! 会话落盘：让一次对话能跨进程活下去。
//!
//! ## 为什么需要它
//!
//! 在此之前，`sessions: BTreeMap<SessionKey, PromptLayout>` 活在
//! `ChatHandler` 里，而 `ChatHandler` 是**每次命令新建的**。进程一退，
//! 上下文全没——所以形态只能是"发一条命令、等一个结果"：
//!
//! ```text
//! yunxi-bot do "改一下 X"     ← 跑完就退
//! yunxi-bot do "再改一下 Y"   ← 完全不知道上一句
//! ```
//!
//! 通用 agent 的核心体验是**对话**，而对话的前提是**记住**。
//!
//! ## 存什么，不存什么
//!
//! | 存 | 不存 |
//! |---|---|
//! | 稳定前缀（含人格、规则、工具定义） | 模型客户端（每次重建） |
//! | 历史消息（含工具调用与结果） | 工具注册表 |
//! | 轮次、时间戳、fingerprint | 密钥、审批状态 |
//!
//! **稳定前缀要一起存**：它是缓存命中的依据。重建时如果重新拼一遍，
//! 只要拼法有任何差别（顺序、空白），缓存就全废——而那种失效是静默的，
//! 只会表现为账单变贵。存下来并核对 fingerprint，才谈得上"接着上次"。
//!
//! ## fingerprint 不匹配怎么办
//!
//! **不静默接受，也不静默丢弃。** 前缀变了意味着人格/规则/工具定义变了
//! （比如刚加载了项目的 `AGENTS.md`），那么这个会话的历史就不再和新的
//! 前缀配套——但历史本身仍然是有效的对话记录。
//!
//! 处理方式：**保留历史，换用新前缀，并如实报告**。
//! 丢弃历史是过度的（使用者会莫名其妙地"失忆"），静默换前缀则会让
//! 使用者以为缓存还在命中。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::prompt::PromptLayout;

/// 会话文件格式版本。
///
/// 独立于台账版本：两者的演进节奏不同，绑在一起会让改一个就得动另一个。
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// 会话目录（相对数据目录）。
pub const SESSION_DIR: &str = "sessions";

/// 一个会话的落盘表示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub yunxi_bot_session: u32,
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// 已经进行了多少轮。**给使用者看的**——"这是个新会话还是聊了很久的"。
    #[serde(default)]
    pub turns: u32,
    /// 稳定前缀的指纹。载入时与当前前缀核对。
    pub fingerprint: u64,
    pub layout: PromptLayout,
}

/// 会话读写的错误。
#[derive(Debug)]
pub enum SessionError {
    Io(String),
    Format(String),
    /// 没有这个会话。
    NotFound(String),
    /// id 不合法（会被拼进路径）。
    BadId(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(m) => write!(f, "会话读写失败: {m}"),
            SessionError::Format(m) => write!(f, "会话文件不可解析: {m}"),
            SessionError::NotFound(id) => write!(f, "找不到会话「{id}」"),
            SessionError::BadId(m) => write!(f, "会话 id 不合法: {m}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// 会话仓库。
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// 数据目录下的默认位置。
    pub fn new(home: &Path) -> Self {
        Self {
            dir: home.join(SESSION_DIR),
        }
    }

    /// 自定义目录。测试用。
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 校验 id。
    ///
    /// **id 会被拼进文件路径**，所以这是安全边界而不是风格问题：
    /// `../` 能让"保存会话"写到数据目录外面去。
    /// 只允许字母数字和 `-` `_`，理由和工具层的路径归一化一样——
    /// 宽松的白名单在这里没有任何好处。
    pub fn check_id(id: &str) -> Result<(), SessionError> {
        if id.is_empty() {
            return Err(SessionError::BadId("不能是空字符串".into()));
        }
        if id.len() > 64 {
            return Err(SessionError::BadId(format!("太长了（{} 字符）", id.len())));
        }
        if let Some(bad) = id
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
        {
            return Err(SessionError::BadId(format!(
                "只能含字母、数字、`-`、`_`，出现了 `{bad}`"
            )));
        }
        Ok(())
    }

    fn path_of(&self, id: &str) -> Result<PathBuf, SessionError> {
        Self::check_id(id)?;
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// 存。
    pub fn save(&self, file: &SessionFile) -> Result<(), SessionError> {
        let path = self.path_of(&file.id)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| SessionError::Io(e.to_string()))?;

        let text =
            serde_json::to_string_pretty(file).map_err(|e| SessionError::Format(e.to_string()))?;

        // **先写临时文件再改名。**
        //
        // 直接覆写的话，写到一半被杀（或磁盘满）会留下半个 JSON——
        // 而下一次载入会报"不可解析"，**整个会话就废了**。
        // 改名在同一文件系统上是原子的，所以要么是旧的完整内容，
        // 要么是新的完整内容，不存在中间态。
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &text).map_err(|e| SessionError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| SessionError::Io(e.to_string()))?;
        Ok(())
    }

    /// 取。
    pub fn load(&self, id: &str) -> Result<SessionFile, SessionError> {
        let path = self.path_of(id)?;
        if !path.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| SessionError::Io(e.to_string()))?;
        let file: SessionFile = serde_json::from_str(&text)
            .map_err(|e| SessionError::Format(format!("{}: {e}", path.display())))?;
        if file.yunxi_bot_session != SESSION_FORMAT_VERSION {
            return Err(SessionError::Format(format!(
                "不支持的会话版本 {}，本程序只支持 {}",
                file.yunxi_bot_session, SESSION_FORMAT_VERSION
            )));
        }
        Ok(file)
    }

    /// 列出所有会话，**最近更新的在前**。
    ///
    /// 排在最前的是"接着上次聊"的那个——这是 `--resume` 不给 id 时的默认。
    pub fn list(&self) -> Result<Vec<SessionFile>, SessionError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let rd = std::fs::read_dir(&self.dir).map_err(|e| SessionError::Io(e.to_string()))?;
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            // 坏掉的那个跳过而不是让整次列举失败——
            // 一个会话文件损坏不该让你看不到其他会话。
            let Ok(f) = serde_json::from_str::<SessionFile>(&text) else {
                continue;
            };
            if f.yunxi_bot_session == SESSION_FORMAT_VERSION {
                out.push(f);
            }
        }
        // 最近的在前。用 `Reverse` 而不是手写比较函数——
        // 那样 clippy 能看出这是"按某个键排序"，而不是一个不透明的闭包。
        out.sort_by_key(|f| std::cmp::Reverse(f.updated_at));
        Ok(out)
    }

    /// 最近更新的那个会话。
    pub fn latest(&self) -> Result<Option<SessionFile>, SessionError> {
        Ok(self.list()?.into_iter().next())
    }

    /// 删。
    pub fn remove(&self, id: &str) -> Result<(), SessionError> {
        let path = self.path_of(id)?;
        if !path.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        std::fs::remove_file(&path).map_err(|e| SessionError::Io(e.to_string()))
    }
}

/// 载入时前缀对不上，怎么处理。
#[derive(Debug, Clone, PartialEq)]
pub enum PrefixMismatch {
    /// 对得上，直接接着用。
    Same,
    /// 对不上：**保留历史，换用新前缀**，并如实报告。
    Changed { was: u64, now: u64 },
}

/// 载入一个会话并把它接到当前前缀上。
///
/// 返回（布局，前缀是否变过）。**前缀变了不丢历史**——见模块文档：
/// 丢弃是过度的（使用者会莫名其妙"失忆"），而静默换前缀会让使用者
/// 以为缓存还在命中。
pub fn resume_onto(file: &SessionFile, current_stable: &str) -> (PromptLayout, PrefixMismatch) {
    let mut layout = file.layout.clone();
    let now = layout.fingerprint_for(current_stable);
    if file.fingerprint == now {
        return (layout, PrefixMismatch::Same);
    }
    let was = file.fingerprint;
    layout.replace_stable(current_stable);
    (layout, PrefixMismatch::Changed { was, now })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::think::Message;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "yunxi-session-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = tmp(tag);
        (SessionStore::at(dir.join("sessions")), dir)
    }

    fn layout_with(turns: usize) -> PromptLayout {
        let mut l = PromptLayout::new("稳定前缀");
        for i in 0..turns {
            l.ask(format!("问题 {i}"));
            l.record_reply(format!("回答 {i}"));
        }
        l
    }

    fn file(id: &str, layout: PromptLayout) -> SessionFile {
        SessionFile {
            yunxi_bot_session: SESSION_FORMAT_VERSION,
            id: id.to_string(),
            created_at: 1,
            updated_at: 2,
            turns: (layout.history_len() / 2) as u32,
            fingerprint: layout.fingerprint(),
            layout,
        }
    }

    // ---- id 是安全边界 ----

    #[test]
    fn a_path_traversal_id_is_refused() {
        // **id 会被拼进文件路径**，所以这是安全边界而不是风格问题：
        // `../` 能让"保存会话"写到数据目录外面去。
        for bad in ["../evil", "..", "a/b", "a\\b", "a.b", "a b", "会话"] {
            assert!(SessionStore::check_id(bad).is_err(), "`{bad}` 该被拒绝");
        }
    }

    #[test]
    fn ordinary_ids_are_accepted() {
        for good in ["abc", "ABC-123", "a_b", "2026-10-06T12-00"] {
            assert!(SessionStore::check_id(good).is_ok(), "`{good}` 该被接受");
        }
    }

    #[test]
    fn an_empty_or_overlong_id_is_refused() {
        assert!(SessionStore::check_id("").is_err());
        assert!(SessionStore::check_id(&"a".repeat(65)).is_err());
        assert!(SessionStore::check_id(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn a_bad_id_never_touches_the_filesystem() {
        // 拒绝要发生在拼路径之前，不是之后
        let (s, dir) = store("badid");
        let f = file("../escaped", layout_with(1));
        assert!(matches!(s.save(&f), Err(SessionError::BadId(_))));
        assert!(!dir.join("..").join("escaped.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 往返 ----

    #[test]
    fn a_session_round_trips() {
        let (s, dir) = store("rt");
        let l = layout_with(3);
        s.save(&file("t1", l.clone())).unwrap();

        let got = s.load("t1").unwrap();
        assert_eq!(got.turns, 3);
        assert_eq!(got.layout.stable(), l.stable());
        assert_eq!(got.layout.history_len(), l.history_len());
        assert_eq!(got.layout.fingerprint(), l.fingerprint());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_calls_and_results_survive_the_round_trip() {
        // **工具调用必须完整存下来。** 少一个 tool_call_id，服务端就报 400
        // ——而那是"接着上次"最常见的场景：上一次调了工具，这一次要接着用。
        let (s, dir) = store("tools");
        let mut l = PromptLayout::new("p");
        l.ask("查一下天气");
        l.push_raw(Message::assistant_tool_calls(vec![serde_json::json!({
            "id": "call_1", "type": "function",
            "function": {"name": "web_search", "arguments": "{}"}
        })]));
        l.push_raw(Message::tool_result("call_1", "晴"));
        l.record_reply("今天晴");

        s.save(&file("t1", l)).unwrap();
        let got = s.load("t1").unwrap();
        let msgs = got.layout.build();
        assert!(
            msgs.iter()
                .any(|m| m.tool_call_id.as_deref() == Some("call_1")),
            "工具结果该带着 tool_call_id 活下来: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| !m.tool_calls.is_empty()),
            "助手请求的 tool_calls 该活下来"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_missing_session_says_which_one() {
        let (s, dir) = store("missing");
        let e = s.load("nope").unwrap_err();
        assert!(e.to_string().contains("nope"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_session_file_is_reported_not_silently_empty() {
        let (s, dir) = store("broken");
        std::fs::create_dir_all(s.dir()).unwrap();
        std::fs::write(s.dir().join("x.json"), "{不是 json").unwrap();
        let e = s.load("x").unwrap_err();
        assert!(matches!(e, SessionError::Format(_)), "{e:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsupported_version_is_refused() {
        // 格式变了就当新格式读，会读出**看起来对但其实是错的**历史
        let (s, dir) = store("ver");
        std::fs::create_dir_all(s.dir()).unwrap();
        std::fs::write(
            s.dir().join("x.json"),
            r#"{"yunxi_bot_session":999,"id":"x","created_at":0,"updated_at":0,
                "fingerprint":0,"layout":{"stable":"s","history":[],"volatile":""}}"#,
        )
        .unwrap();
        let e = s.load("x").unwrap_err();
        assert!(e.to_string().contains("版本"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 原子写 ----

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let (s, dir) = store("tmp");
        s.save(&file("a", layout_with(1))).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(s.dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "改名之后不该留下临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_twice_overwrites_atomically() {
        let (s, dir) = store("over");
        s.save(&file("a", layout_with(1))).unwrap();
        s.save(&file("a", layout_with(5))).unwrap();
        assert_eq!(s.load("a").unwrap().turns, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 列举 ----

    #[test]
    fn list_is_newest_first() {
        // 排在最前的是"接着上次聊"的那个
        let (s, dir) = store("list");
        let mut a = file("old", layout_with(1));
        a.updated_at = 100;
        let mut b = file("new", layout_with(1));
        b.updated_at = 200;
        s.save(&a).unwrap();
        s.save(&b).unwrap();
        let ids: Vec<String> = s.list().unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["new", "old"]);
        assert_eq!(s.latest().unwrap().unwrap().id, "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_lists_nothing_rather_than_failing() {
        let (s, dir) = store("empty");
        assert!(s.list().unwrap().is_empty());
        assert!(s.latest().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_broken_file_does_not_hide_the_others() {
        // 一个会话文件损坏不该让你看不到其他会话
        let (s, dir) = store("mixed");
        s.save(&file("good", layout_with(1))).unwrap();
        std::fs::write(s.dir().join("bad.json"), "不是 json").unwrap();
        let ids: Vec<String> = s.list().unwrap().into_iter().map(|f| f.id).collect();
        assert_eq!(ids, vec!["good"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_says_when_there_was_nothing_to_remove() {
        let (s, dir) = store("rm");
        assert!(matches!(s.remove("x"), Err(SessionError::NotFound(_))));
        s.save(&file("x", layout_with(1))).unwrap();
        assert!(s.remove("x").is_ok());
        assert!(matches!(s.load("x"), Err(SessionError::NotFound(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 前缀对不上 ----

    #[test]
    fn an_unchanged_prefix_resumes_as_is() {
        let l = layout_with(2);
        let f = file("s", l.clone());
        let (got, m) = resume_onto(&f, l.stable());
        assert_eq!(m, PrefixMismatch::Same);
        assert_eq!(got.history_len(), l.history_len());
    }

    #[test]
    fn a_changed_prefix_keeps_the_history_and_reports_it() {
        // **前缀变了不丢历史。** 丢弃是过度的（使用者会莫名其妙"失忆"），
        // 而静默换前缀会让使用者以为缓存还在命中。
        let f = file("s", layout_with(3));
        let (got, m) = resume_onto(&f, "换过的前缀");
        assert!(matches!(m, PrefixMismatch::Changed { .. }), "{m:?}");
        assert_eq!(got.history_len(), 6, "历史必须保留");
        assert_eq!(got.stable(), "换过的前缀", "前缀要换成新的");
    }

    #[test]
    fn after_replacing_the_prefix_the_fingerprint_matches_the_new_one() {
        // 否则下一次载入又会报"变了"，变成每次都报
        let f = file("s", layout_with(1));
        let (got, _) = resume_onto(&f, "新前缀");
        let f2 = SessionFile {
            fingerprint: got.fingerprint(),
            layout: got.clone(),
            ..f
        };
        let (_, m) = resume_onto(&f2, "新前缀");
        assert_eq!(m, PrefixMismatch::Same);
    }

    #[test]
    fn the_mismatch_reports_both_fingerprints() {
        // 报告要能对比，否则"变了"这句话没有下一步
        let f = file("s", layout_with(1));
        let (_, m) = resume_onto(&f, "别的");
        match m {
            PrefixMismatch::Changed { was, now } => assert_ne!(was, now),
            other => panic!("该报变化: {other:?}"),
        }
    }
}
