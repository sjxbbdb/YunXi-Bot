//! 用户画像：`<数据目录>/profile.md`。
//!
//! ## 它和记忆不是一回事
//!
//! 参考实现里这两件事分得很清（Miyu 的 `home/<user>/profile.md`）：
//!
//! | | 谁写的 | 性质 |
//! |---|---|---|
//! | **画像** | **使用者自己** | 自我描述，**权威** |
//! | 记忆 | 助理攒的 | 观察，可能有错 |
//!
//! **来源不同、权威性不同，所以不合并成一张表。**
//!
//! 我一开始判断"画像就是事实/偏好的聚合视图，不该造新概念"——**错了**。
//! 聚合视图能回答"助理认为你是谁"，回答不了"你希望它怎么理解你"。
//! 后者只有使用者自己能写。
//!
//! ## 为什么用 Markdown 而不是 JSON
//!
//! 要写它的是**人**，不是程序。JSON 的引号、逗号、转义全是给机器的负担，
//! 而画像本来就是一段话。Markdown 让人能随手加一条、随手改一句，
//! 改坏了也不会让整个文件解析失败。
//!
//! 而且它**原样进提示词**——段落、列表、语气都保留。写成 JSON 再渲染
//! 一道，等于把人的话翻译成机器的再翻译回来。
//!
//! ## 受众
//!
//! 参考架构里有条规则：**属主档案只在属主类入口注入，通讯平台不生效**
//! （同一份档案，终端里该生效，群里不该生效）。
//!
//! YunXi Bot 现在**只有属主入口**（CLI），所以总是注入。等有了别的入口
//! （Web、聊天平台），这条要补上——`load_and_render` 的调用点就是
//! 该判断的地方。

use std::path::{Path, PathBuf};

/// 画像文件的文件名。
pub const PROFILE_FILE: &str = "profile.md";

/// 画像的字节上限。
///
/// **必须有上限**：它进稳定前缀，而且每轮都发。一份 200KB 的"自我介绍"
/// 会把前缀和预算一起毁掉。
///
/// 比项目规则的 32KB 小得多——规则是项目级的、可能很长；画像是
/// "你希望它怎么理解你"，几句话就够，写不下的该进记忆。
pub const MAX_PROFILE_BYTES: usize = 8 * 1024;

/// 一份画像。
#[derive(Debug, Clone)]
pub struct UserProfile {
    pub text: String,
    pub path: PathBuf,
    /// 超上限被截断了吗。**截断必须让人知道**——静默截断的话，
    /// 使用者以为自己写了，而它只看到一半。
    pub truncated: bool,
}

impl UserProfile {
    /// 渲染成提示词里的一段。
    ///
    /// 标题写"关于你"而不是"用户档案"：**它是给模型看的第二人称**，
    /// 和"关于使用者"（记忆段，第三人称）区分开——
    /// 那两段来源不同，措辞也该不同。
    pub fn render(&self) -> String {
        let mut out = String::from("# 关于你\n");
        out.push_str(self.text.trim());
        if self.truncated {
            out.push_str(&format!(
                "\n\n（这份档案超过 {} KB，**只读了前面一部分**——用 `yunxi-bot profile` 看完整的那份）",
                MAX_PROFILE_BYTES / 1024
            ));
        }
        out.push('\n');
        out
    }
}

/// 从数据目录读画像。没有文件就返回 `None`。
///
/// **读不到不报错**：画像是有则更好、没有也照常工作的东西。
/// 但**文件存在却读不动要说一声**——"你还没写"和"写了没读到"
/// 是两件不同的事。
pub fn load(home: &Path) -> Option<UserProfile> {
    let path = home.join(PROFILE_FILE);
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("提示：画像文件读不动（{}）：{e}", path.display());
            return None;
        }
    };
    Some(from_text(&path, raw))
}

/// 从一段文本造一份画像（截断逻辑在这里，好测）。
pub fn from_text(path: &Path, raw: String) -> UserProfile {
    // 按**字节**截断会让一个多字节字符断成两半，渲染出来是乱码。
    // 所以按字符边界收。
    let (text, truncated) = if raw.len() > MAX_PROFILE_BYTES {
        let mut cut = MAX_PROFILE_BYTES;
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        (raw[..cut].to_string(), true)
    } else {
        (raw, false)
    };
    UserProfile {
        text,
        path: path.to_path_buf(),
        truncated,
    }
}

/// 读并渲染。没有画像就返回空串（**不拼空段**——那会白占前缀的 token）。
pub fn load_and_render(home: &Path) -> String {
    match load(home) {
        Some(p) => {
            let r = p.render();
            // 全空白的画像等于没写
            if p.text.trim().is_empty() {
                String::new()
            } else {
                r
            }
        }
        None => String::new(),
    }
}

/// 一条**待确认**的画像提议。
///
/// ## 为什么自动总结的东西必须先 pending
///
/// 参考架构（研究文档 §4.6）的原话是："**自动抽取默认先 pending，
/// 不直接改变核心用户档案**"。
///
/// 理由是：**画像是权威的**（它是"你希望它怎么理解你"），
/// 而自动总结是**猜的**。猜的东西直接写进权威档案，档案就废了——
/// 你没法再信它，因为你不知道哪句是自己写的、哪句是它猜的。
///
/// 而且自动总结**一定会错**：它可能把"帮我看看这个文件"当成
/// "使用者在做文件相关的项目"。那种错如果无声地进了画像，
/// 之后每一次对话都带着它。
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileProposal {
    pub id: String,
    /// 提议写进画像的那句话。
    pub text: String,
    /// **它为什么这么提议。** 没这个的话人没法判断该不该接受——
    /// 只能看那句话本身，而"这句话是从哪来的"恰恰是关键。
    pub reason: String,
    pub created_at: u64,
}

/// 从台账事件投影出**还没处理**的提议。
///
/// 事件驱动而不是存一个 JSON 文件：接受和否决**都要留痕**——
/// "它提议过什么、我否掉了什么"本身是有用的信息
/// （它总在提议某一类你不想要的东西时，该调的是提取的判据）。
pub fn pending_from_events(events: &[crate::ledger::Event]) -> Vec<ProfileProposal> {
    use crate::ledger::EventKind;
    use std::collections::BTreeMap;

    let mut open: BTreeMap<String, ProfileProposal> = BTreeMap::new();
    for e in events {
        match e.kind {
            EventKind::ProfileProposed => {
                let (Some(id), Some(text)) = (
                    e.data.get("id").and_then(|v| v.as_str()),
                    e.data.get("text").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                open.insert(
                    id.to_string(),
                    ProfileProposal {
                        id: id.to_string(),
                        text: text.to_string(),
                        reason: e
                            .data
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        created_at: e.at,
                    },
                );
            }
            // 接受和否决都把提议移出待办。**否决也要记**——
            // 不然同一条会被反复提议，而使用者会以为它没听见。
            EventKind::ProfileAccepted | EventKind::ProfileRejected => {
                if let Some(id) = e.data.get("id").and_then(|v| v.as_str()) {
                    open.remove(id);
                }
            }
            _ => {}
        }
    }
    open.into_values().collect()
}

/// 被接受过的提议正文，按接受顺序。用来拼进画像文件。
pub fn accepted_texts(events: &[crate::ledger::Event]) -> Vec<String> {
    use crate::ledger::EventKind;
    events
        .iter()
        .filter(|e| e.kind == EventKind::ProfileAccepted)
        .filter_map(|e| e.data.get("text").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .collect()
}

/// 渲染待确认列表（给人看的）。
pub fn render_pending(items: &[ProfileProposal]) -> String {
    if items.is_empty() {
        return "（没有待确认的提议）".to_string();
    }
    let mut out = String::new();
    for p in items {
        out.push_str(&format!("  [{}] {}\n", p.id, p.text));
        if !p.reason.trim().is_empty() {
            out.push_str(&format!("        它这么想的依据：{}\n", p.reason.trim()));
        }
    }
    out
}

/// 初始模板。`yunxi-bot profile --init` 写它。
///
/// ## 为什么给模板而不是让人对着空文件发呆
///
/// "写一段自我介绍"这件事，不给抓手的话大多数人写不出东西——
/// 不是不会写，是**不知道该写到什么颗粒度**。模板里的注释就是在
/// 示范颗粒度：什么该写（怎么称呼、在意什么、别做什么），
/// 什么不该写（这个该进记忆的日常事实）。
///
/// 注释用 `<!-- -->` 包着：**它是给写的人看的，不是给模型看的**，
/// 而 HTML 注释渲染出去也不影响阅读。模型看到也无害——
/// 它一眼能认出那是说明文字。
pub const TEMPLATE: &str = r#"# 关于我

<!-- 这是一份"你希望助理怎么理解你"的说明，会进它的系统提示词。
     写几句话就够，不用长。改完下次对话生效。 -->

## 怎么称呼我

<!-- 例：叫我老王就行，不用加"先生"。 -->

## 我在意什么

<!-- 例：回答先给结论再给理由；别用感叹号；不确定的事直接说不确定。 -->

## 别做什么

<!-- 例：不要在我没问的时候主动给建议。 -->

## 背景

<!-- 例：我在做后端，主要在写 Rust。术语可以直接用，不用解释。 -->
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> UserProfile {
        from_text(Path::new("test-profile.md"), text.to_string())
    }

    #[test]
    fn the_rendered_block_says_it_is_about_the_reader() {
        // 标题是第二人称，和记忆段的"关于使用者"（第三人称）区分开：
        // **那两段来源不同，措辞也该不同**
        let p = at("叫我老王。");
        let r = p.render();
        assert!(r.starts_with("# 关于你"), "{r}");
        assert!(r.contains("叫我老王。"));
    }

    #[test]
    fn the_text_goes_in_verbatim() {
        // **原样进提示词**——段落、列表、语气都保留。
        // 写成 JSON 再渲染一道，等于把人的话翻译成机器的再翻译回来。
        let src = "## 怎么称呼我\n\n叫我老王。\n\n## 别做什么\n\n- 别用感叹号\n- 别主动给建议\n";
        let r = at(src).render();
        assert!(r.contains("## 怎么称呼我"));
        assert!(r.contains("- 别用感叹号"));
        assert!(r.contains("- 别主动给建议"));
    }

    #[test]
    fn an_oversized_profile_is_truncated_on_a_char_boundary() {
        // **按字节截断会把一个多字节字符断成两半，渲染出来是乱码。**
        // 用中文试，因为一个汉字三字节，最容易踩到。
        let mut src = String::new();
        while src.len() < MAX_PROFILE_BYTES + 1000 {
            src.push('好');
        }
        let p = at(&src);
        assert!(p.truncated, "该标记成被截断了");
        assert!(p.text.len() <= MAX_PROFILE_BYTES);
        // 截出来的必须还是合法 UTF-8（不合法的话 `String` 本身就构造不出来，
        // 所以这里验的是"末尾没切出半个字"——能正常渲染就说明没切坏）
        assert!(p.text.chars().all(|c| c == '好'), "切出了别的东西");
    }

    #[test]
    fn truncation_is_announced_not_silent() {
        // **静默截断的话，使用者以为自己写了，而它只看到一半。**
        let mut src = String::new();
        while src.len() < MAX_PROFILE_BYTES + 100 {
            src.push('x');
        }
        let r = at(&src).render();
        assert!(r.contains("只读了前面一部分"), "截断必须说出来：{r}");
    }

    #[test]
    fn a_profile_under_the_limit_is_not_marked() {
        let p = at("叫我老王。");
        assert!(!p.truncated);
        assert!(!p.render().contains("只读了"));
    }

    #[test]
    fn no_file_means_no_block() {
        // **一条没有就不拼空段**：那会白占前缀的 token，还每轮都一样地占
        let dir = std::env::temp_dir().join(format!("yunxi-profile-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(PROFILE_FILE));
        assert_eq!(load_and_render(&dir), "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_whitespace_only_profile_is_treated_as_unwritten() {
        let dir = std::env::temp_dir().join(format!("yunxi-profile-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(PROFILE_FILE), "   \n\n  \t\n").unwrap();
        assert_eq!(load_and_render(&dir), "", "全是空白等于没写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_profile_is_read_and_rendered() {
        let dir = std::env::temp_dir().join(format!("yunxi-profile-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(PROFILE_FILE), "叫我老王，回答先给结论。").unwrap();
        let r = load_and_render(&dir);
        assert!(r.contains("叫我老王"), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_template_has_sections_but_no_filled_in_content() {
        // 模板是**抓手**不是**答案**：它有结构，但没有替你写内容。
        // 替人写好内容的话，大多数人会原样留着，于是所有人的画像都一样。
        assert!(TEMPLATE.contains("## 怎么称呼我"));
        assert!(TEMPLATE.contains("<!--"), "说明要用注释包起来");

        // **先把注释整块剥掉，再看剩下什么。**
        //
        // 一开始是按行跳过 `<!--` 开头的行，结果**跨行注释**中间那几行
        // 漏了出来，测试误报。注释是跨行的，按行判不够——
        // 而且它误报时我第一反应是"模板写错了"，差点去改模板。
        let mut stripped = String::new();
        let mut in_comment = false;
        for line in TEMPLATE.lines() {
            let t = line.trim();
            if in_comment {
                if t.ends_with("-->") {
                    in_comment = false;
                }
                continue;
            }
            if t.starts_with("<!--") {
                if !t.ends_with("-->") {
                    in_comment = true;
                }
                continue;
            }
            stripped.push_str(line);
            stripped.push('\n');
        }
        // 剥完剩下的应该只有标题
        for line in stripped.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            panic!("模板里不该有填好的内容行：{t}");
        }
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;
    use crate::ledger::{Event, EventKind};

    fn ev(kind: EventKind, at: u64, data: serde_json::Value) -> Event {
        Event {
            seq: at,
            at,
            kind,
            span: None,
            job: None,
            data,
        }
    }

    #[test]
    fn a_proposal_shows_up_as_pending() {
        let evs = vec![ev(
            EventKind::ProfileProposed,
            10,
            serde_json::json!({"id": "p1", "text": "使用者住在杭州", "reason": "他说「我在杭州」"}),
        )];
        let open = pending_from_events(&evs);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].text, "使用者住在杭州");
        assert_eq!(open[0].reason, "他说「我在杭州」");
    }

    #[test]
    fn accepting_removes_it_from_pending_and_remembers_the_text() {
        let evs = vec![
            ev(
                EventKind::ProfileProposed,
                10,
                serde_json::json!({"id": "p1", "text": "使用者住在杭州"}),
            ),
            ev(
                EventKind::ProfileAccepted,
                20,
                serde_json::json!({"id": "p1", "text": "使用者住在杭州"}),
            ),
        ];
        assert!(
            pending_from_events(&evs).is_empty(),
            "接受之后不该还在待办里"
        );
        assert_eq!(accepted_texts(&evs), vec!["使用者住在杭州"]);
    }

    #[test]
    fn rejecting_also_removes_it() {
        // **否决也要移出待办。** 不然同一条会被反复提议，
        // 而使用者会以为它没听见——那比不提议更烦人。
        let evs = vec![
            ev(
                EventKind::ProfileProposed,
                10,
                serde_json::json!({"id": "p1", "text": "使用者住在杭州"}),
            ),
            ev(
                EventKind::ProfileRejected,
                20,
                serde_json::json!({"id": "p1"}),
            ),
        ];
        assert!(pending_from_events(&evs).is_empty());
        assert!(accepted_texts(&evs).is_empty(), "否决的不该进画像");
    }

    #[test]
    fn a_malformed_proposal_event_is_skipped_not_fatal() {
        // 台账是**追加式**的，写坏的一行不能让它整本读不出来
        let evs = vec![
            ev(
                EventKind::ProfileProposed,
                10,
                serde_json::json!({"id": "p1"}),
            ),
            ev(
                EventKind::ProfileProposed,
                11,
                serde_json::json!({"id": "p2", "text": "好的那条"}),
            ),
        ];
        let open = pending_from_events(&evs);
        assert_eq!(open.len(), 1, "缺 text 的那条该被跳过");
        assert_eq!(open[0].text, "好的那条");
    }

    #[test]
    fn several_pending_items_all_show() {
        let evs = vec![
            ev(
                EventKind::ProfileProposed,
                10,
                serde_json::json!({"id": "p1", "text": "住在杭州"}),
            ),
            ev(
                EventKind::ProfileProposed,
                11,
                serde_json::json!({"id": "p2", "text": "用 Rust"}),
            ),
        ];
        assert_eq!(pending_from_events(&evs).len(), 2);
    }

    #[test]
    fn an_empty_list_says_so_instead_of_printing_nothing() {
        // **空白输出会让人以为命令坏了。** 说一句"没有"。
        assert!(render_pending(&[]).contains("没有待确认"));
    }
}
