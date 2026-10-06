//! 项目规则加载：让它在某个项目里工作时**自动知道那个项目的约定**。
//!
//! ## 为什么这条是"通用 agent"的分界
//!
//! 通用 agent 的价值恰恰在"换个项目直接能用"。而在此之前，它不知道
//! 这个项目该用哪个测试命令、目录结构什么意思、有哪些硬规矩——
//! 每次都要使用者在提示词里重说一遍。
//!
//! ## 两个必须先定清楚的问题
//!
//! ### 一、向上找到哪里停
//!
//! 从 cwd 往上走，每层都可能有一份 `AGENTS.md`。**必须有停止条件**，
//! 否则从 `C:\Users\你\Desktop\foo` 出发会把 `Users\`、`C:\` 下面
//! 不相干的文件也读进来。
//!
//! 规则是：
//!
//! 1. **遇到含 `.git` 的目录，读完它就不往上找了**——那是项目边界，
//!    项目外面的东西不属于这个项目。
//! 2. **到文件系统根为止**（没有 git 仓库时）。
//! 3. **深度硬上限 12 层**，防病态路径。
//!
//! ### 二、多份规则的优先级
//!
//! 顺序是**从远到近**：
//!
//! ```text
//! 用户级（<数据目录>/AGENTS.md）
//!   → 项目根（最远的祖先）
//!     → 逐级向下
//!       → cwd
//! ```
//!
//! 越靠后越具体，而**模型读到最后说的那句印象最深**——这和人类看
//! "通用规范 + 本项目补充"的顺序一致。
//!
//! **每份规则都带来源路径**。不标来源的话，模型说"按项目约定应该……"
//! 而你无从知道它指的是哪一条；出了错也追不回去。
//!
//! ## 与缓存纪律的关系
//!
//! 规则**进稳定前缀**，所以它必须在会话开始时读一次就定下来。
//! 中途 cwd 变了会导致前缀变——那条路由 `PrefixMismatch` 处理：
//! 保留历史、换新前缀、如实报告。

use std::path::{Path, PathBuf};

/// 每份规则文件的大小上限。
///
/// 超过就**截断并说明**，不是静默丢弃——一份 200KB 的 AGENTS.md
/// 塞进前缀会把缓存和预算一起毁掉，但"它被截断了"这件事
/// 使用者必须知道。
pub const MAX_RULE_BYTES: usize = 32 * 1024;

/// 最多读几份。防病态路径把几十个目录的文件都读进来。
pub const MAX_RULE_FILES: usize = 8;

/// 向上找的深度上限。
pub const MAX_DEPTH: usize = 12;

/// 认得的规则文件名。**顺序即优先级**（同名时先匹配到的赢）。
///
/// `CLAUDE.md` 放在后面是兼容考虑：两个都存在时 `AGENTS.md` 更"通用"，
/// 而 `CLAUDE.md` 通常是给别的工具准备的。
pub const RULE_FILENAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// 一份规则来自哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    /// 用户级：`<数据目录>/AGENTS.md`。跨项目通用。
    User,
    /// 项目里找到的。附带的深度用于排序与显示。
    Project,
}

impl RuleScope {
    pub fn label(self) -> &'static str {
        match self {
            RuleScope::User => "用户级",
            RuleScope::Project => "项目",
        }
    }
}

/// 一份被加载的规则文件。
#[derive(Debug, Clone, PartialEq)]
pub struct RuleFile {
    pub path: PathBuf,
    pub scope: RuleScope,
    /// 目录深度。用户级恒为 0。**越小越"远"**，读得越早。
    pub depth: usize,
    pub text: String,
    /// 是不是被截断了。
    pub truncated: bool,
}

impl RuleFile {
    /// 原始字节数（截断前）。
    pub fn original_bytes(&self) -> usize {
        self.text.len()
    }
}

/// 加载结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleSet {
    pub files: Vec<RuleFile>,
}

impl RuleSet {
    /// 什么都没找到。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 从一个目录出发找。`home` 是数据目录（用户级规则放在那儿）。
    ///
    /// **读一次就够**：规则进稳定前缀，中途变会让缓存全废。
    pub fn discover(cwd: &Path, home: &Path) -> Self {
        let mut files = Vec::new();

        // 用户级：跨项目的通用偏好
        if let Some(f) = read_rule(&home.join(RULE_FILENAMES[0]), RuleScope::User, 0) {
            files.push(f);
        }

        // 项目级：从**最远的祖先**开始，逐级向下到 cwd。
        //
        // 这样排序才是"通用 → 具体"。从 cwd 往上读的话，
        // 具体的规则会先被读到，然后被泛泛的规则盖过——方向反了。
        let mut chain = Vec::new();
        let mut cur = Some(cwd.to_path_buf());
        let mut depth = 0usize;
        while let Some(dir) = cur {
            chain.push(dir.clone());
            depth += 1;

            // **停止条件一：项目边界。**
            // 含 `.git` 的目录读完就不往上找了——项目外面的
            // 东西不属于这个项目。
            if dir.join(".git").exists() {
                break;
            }
            // **停止条件二：深度上限。**
            if depth >= MAX_DEPTH {
                break;
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
        // 反转成"最远 → 最近"
        chain.reverse();

        for (i, dir) in chain.iter().enumerate() {
            for name in RULE_FILENAMES {
                if let Some(f) = read_rule(&dir.join(name), RuleScope::Project, chain.len() - i) {
                    files.push(f);
                    break; // 同一目录里找到一份就够了
                }
            }
            if files.len() > MAX_RULE_FILES {
                break;
            }
        }

        // 排序：**用户级永远在最前，然后项目级从远到近。**
        //
        // 用显式的两段键而不是直接比 `depth`：用户级的 depth 是 0，
        // 而项目级最小的也是 1，直接降序排会把它挤到最后——
        // 那个错犯过一次（测试当场抓到）。
        files.sort_by(|a, b| {
            let key = |f: &RuleFile| match f.scope {
                // 第一段 0 = 用户级，永远最前
                RuleScope::User => (0usize, 0usize),
                // 第一段 1 = 项目级；第二段取反，让 depth 大的（远的）排前面
                RuleScope::Project => (1, usize::MAX - f.depth),
            };
            key(a).cmp(&key(b)).then_with(|| a.path.cmp(&b.path))
        });
        Self { files }
    }

    /// 拼成要进稳定前缀的那一段。
    ///
    /// 没有规则时返回空串——**不要拼一段"（没有项目规则）"的空话**，
    /// 那会平白占掉前缀的 token，还每轮都一样地占。
    pub fn render(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# 项目约定\n\n\
             以下规则来自这个工作目录里的文件。**它们的优先级高于我的一般做法**，\
             但低于使用者在对话里明确说的要求。\n",
        );
        for f in &self.files {
            out.push_str(&format!(
                "\n## 来自 {}（{}）\n\n",
                f.path.display(),
                f.scope.label()
            ));
            out.push_str(&f.text);
            if f.truncated {
                // **截断必须说出来**，否则模型以为规则就这么多
                out.push_str(&format!(
                    "\n\n（这一份超过 {} KB，上面只是开头部分。）",
                    MAX_RULE_BYTES / 1024
                ));
            }
            out.push('\n');
        }
        out
    }

    /// 一行说明，给使用者看：加载了哪几份。
    pub fn summary(&self) -> String {
        if self.files.is_empty() {
            return "没有找到项目规则文件".to_string();
        }
        let mut parts: Vec<String> = self
            .files
            .iter()
            .map(|f| {
                format!(
                    "{}{}",
                    f.path.display(),
                    if f.truncated { "（已截断）" } else { "" }
                )
            })
            .collect();
        parts.dedup();
        format!("{} 份规则：{}", parts.len(), parts.join("、"))
    }
}

/// 读一份规则文件。不存在、读不了、或内容为空都返回 `None`。
///
/// **读不了不报错**：规则文件是锦上添花，一个权限不对的文件
/// 不该让整个会话起不来。但它会被 `discover` 跳过——
/// 而使用者可以用 `yunxi-bot rules` 看看到底加载了什么。
fn read_rule(path: &Path, scope: RuleScope, depth: usize) -> Option<RuleFile> {
    if !path.is_file() {
        return None;
    }
    let raw = std::fs::read(path).ok()?;
    // BOM 要去掉：留在前缀里会让首行变成 "\u{feff}# 标题"，
    // 而模型看到的是个乱码开头的标题
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&raw);
    let text = String::from_utf8_lossy(raw).into_owned();
    if text.trim().is_empty() {
        return None;
    }
    let truncated = text.len() > MAX_RULE_BYTES;
    let text = if truncated {
        // **按字符边界截**：按字节切会把一个汉字劈成两半
        let mut end = MAX_RULE_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_string()
    } else {
        text
    };
    Some(RuleFile {
        path: path.to_path_buf(),
        scope,
        depth,
        text,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "yunxi-rules-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, name: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), text).unwrap();
    }

    // ---- 基础 ----

    #[test]
    fn nothing_found_is_not_an_error() {
        // 多数目录里没有 AGENTS.md，那是常态
        let root = tmp("none");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert!(rs.is_empty());
        assert_eq!(rs.render(), "", "没有规则时不该拼一段空话占 token");
        assert!(rs.summary().contains("没有找到"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_file_is_found() {
        let root = tmp("found");
        write(&root, "AGENTS.md", "测试命令用 cargo test");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.files.len(), 1);
        assert_eq!(rs.files[0].scope, RuleScope::Project);
        assert!(rs.render().contains("cargo test"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_md_is_also_read() {
        let root = tmp("claude");
        write(&root, "CLAUDE.md", "旧工具的约定");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.files.len(), 1);
        assert!(rs.render().contains("旧工具的约定"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agents_md_wins_when_both_exist() {
        // 同一目录里两份都在时，`AGENTS.md` 更"通用"；
        // `CLAUDE.md` 通常是给别的工具准备的
        let root = tmp("both");
        write(&root, "AGENTS.md", "通用的那份");
        write(&root, "CLAUDE.md", "别的工具那份");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.files.len(), 1, "同一目录只读一份");
        assert!(rs.render().contains("通用的那份"));
        assert!(!rs.render().contains("别的工具那份"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_file_is_skipped() {
        // 空文件进前缀只会白占 token
        let root = tmp("empty");
        write(&root, "AGENTS.md", "   \n\n  ");
        assert!(RuleSet::discover(&root, &root.join("home")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bom_is_stripped() {
        // BOM 留在前缀里会让首行变成 "\u{feff}# 标题"，
        // 模型看到的是个乱码开头的标题
        let root = tmp("bom");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("标题".as_bytes());
        fs::write(root.join("AGENTS.md"), &bytes).unwrap();
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.files[0].text, "标题");
        assert!(!rs.files[0].text.starts_with('\u{feff}'));
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 停止条件 ----

    #[test]
    fn the_search_stops_at_the_project_boundary() {
        // **项目外面的东西不属于这个项目。**
        let root = tmp("boundary");
        let outer = root.join("outer");
        let project = outer.join("project");
        write(&root, "AGENTS.md", "根上的规则（不该被读到）");
        write(&outer, "AGENTS.md", "项目外的规则（不该被读到）");
        write(&project, "AGENTS.md", "项目里的规则");
        fs::create_dir_all(project.join(".git")).unwrap();

        let rs = RuleSet::discover(&project, &root.join("home"));
        let text = rs.render();
        assert!(text.contains("项目里的规则"));
        assert!(!text.contains("项目外的规则"), "项目边界没起作用: {text}");
        assert!(!text.contains("根上的规则"), "项目边界没起作用: {text}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_directories_are_all_read() {
        // 项目根有一份通用约定，子目录有一份更具体的
        let root = tmp("nested");
        let sub = root.join("crates").join("core");
        write(&root, "AGENTS.md", "根：用 cargo test");
        write(&sub, "AGENTS.md", "子目录：这个 crate 有自己的测试");
        fs::create_dir_all(root.join(".git")).unwrap();

        let rs = RuleSet::discover(&sub, &root.join("home"));
        assert_eq!(rs.files.len(), 2);
        let text = rs.render();
        assert!(text.contains("根：用 cargo test"));
        assert!(text.contains("子目录"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_nearest_rule_comes_last() {
        // **模型读到最后说的那句印象最深。**
        // 顺序反了的话，泛泛的规则会盖过具体的规则。
        let root = tmp("order");
        let sub = root.join("deep");
        write(&root, "AGENTS.md", "远的");
        write(&sub, "AGENTS.md", "近的");
        fs::create_dir_all(root.join(".git")).unwrap();

        let rs = RuleSet::discover(&sub, &root.join("home"));
        let text = rs.render();
        let far = text.find("远的").expect("远的该在");
        let near = text.find("近的").expect("近的该在");
        assert!(far < near, "远的该先读、近的后读（后读的印象更深）");
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 用户级 ----

    #[test]
    fn the_user_level_file_is_read_first() {
        let root = tmp("user");
        let home = root.join("home");
        let project = root.join("proj");
        write(&home, "AGENTS.md", "用户级：回答用中文");
        write(&project, "AGENTS.md", "项目级：用 cargo test");
        fs::create_dir_all(project.join(".git")).unwrap();

        let rs = RuleSet::discover(&project, &home);
        assert_eq!(rs.files.len(), 2);
        assert_eq!(rs.files[0].scope, RuleScope::User, "用户级该在最前");
        let text = rs.render();
        let u = text.find("用户级").expect("用户级该在");
        let p = text.find("项目级").expect("项目级该在");
        assert!(u < p, "用户级该先读");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_scope_is_labelled_in_the_rendered_text() {
        // 模型要能区分"这是通用的"和"这是这个项目的"
        let root = tmp("label");
        let home = root.join("home");
        write(&home, "AGENTS.md", "通用");
        write(&root, "AGENTS.md", "专用");
        fs::create_dir_all(root.join(".git")).unwrap();

        let rs = RuleSet::discover(&root, &home);
        let text = rs.render();
        assert!(text.contains("用户级"), "{text}");
        assert!(text.contains("项目"), "{text}");
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 来源可查 ----

    #[test]
    fn every_rule_carries_its_source_path() {
        // **不标来源的话，模型说"按项目约定应该……"而你无从知道
        // 它指的是哪一条**；出了错也追不回去。
        let root = tmp("source");
        write(&root, "AGENTS.md", "规矩");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert!(rs.render().contains("AGENTS.md"), "渲染里要带来源文件名");
        assert!(rs.summary().contains("AGENTS.md"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_summary_lists_the_paths() {
        let root = tmp("sum");
        write(&root, "AGENTS.md", "x");
        let rs = RuleSet::discover(&root, &root.join("home"));
        let s = rs.summary();
        assert!(s.contains("1 份"), "{s}");
        assert!(s.contains("AGENTS.md"), "{s}");
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 大小与截断 ----

    #[test]
    fn an_oversized_file_is_truncated() {
        // 一份 200KB 的 AGENTS.md 塞进前缀会把缓存和预算一起毁掉
        let root = tmp("big");
        write(&root, "AGENTS.md", &"字".repeat(MAX_RULE_BYTES));
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert!(rs.files[0].truncated);
        assert!(rs.files[0].text.len() <= MAX_RULE_BYTES);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncation_is_said_out_loud() {
        // **截断必须说出来**，否则模型以为规则就这么多
        let root = tmp("bigsay");
        write(&root, "AGENTS.md", &"字".repeat(MAX_RULE_BYTES + 100));
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert!(
            rs.render().contains("只是开头部分"),
            "{}",
            &rs.render()[..200]
        );
        assert!(rs.summary().contains("已截断"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // 按字节切会把一个汉字劈成两半，而半个汉字在 UTF-8 里是非法序列
        let root = tmp("cut");
        write(&root, "AGENTS.md", &"中".repeat(MAX_RULE_BYTES));
        let rs = RuleSet::discover(&root, &root.join("home"));
        // 能正常当成 &str 用就说明没切坏（切坏了这里会 panic）
        assert!(rs.files[0].text.chars().count() > 1000);
        assert!(std::str::from_utf8(rs.files[0].text.as_bytes()).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_normal_file_is_not_marked_truncated() {
        let root = tmp("normal");
        write(&root, "AGENTS.md", "短规则");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert!(!rs.files[0].truncated);
        assert!(!rs.render().contains("只是开头部分"));
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 渲染的措辞 ----

    #[test]
    fn the_rendered_rules_state_their_priority() {
        // **优先级要写进提示词。** 不写的话模型不知道
        // "项目约定"和"我的一般做法"冲突时听谁的。
        let root = tmp("prio");
        write(&root, "AGENTS.md", "x");
        let rs = RuleSet::discover(&root, &root.join("home"));
        let text = rs.render();
        assert!(
            text.contains("优先"),
            "要说清和一般做法冲突时听谁的: {text}"
        );
        assert!(
            text.contains("使用者") && text.contains("对话"),
            "也要说清它的上限在哪（使用者的明确要求更高）: {text}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_rendered_rules_are_byte_stable() {
        // **规则进稳定前缀，所以渲染结果必须逐字节可重复**——
        // 每次渲染出来不一样的话，缓存每轮都未命中。
        let root = tmp("stable");
        write(&root, "AGENTS.md", "规矩");
        let rs = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.render(), rs.render());
        // 重新扫一遍也该得到一样的（同一份文件、同一个目录）
        let again = RuleSet::discover(&root, &root.join("home"));
        assert_eq!(rs.render(), again.render());
        let _ = fs::remove_dir_all(&root);
    }

    // ---- 防御 ----

    #[test]
    fn a_directory_named_agents_md_is_not_a_file() {
        // `is_file()` 挡住了它 —— 不然读目录会失败
        let root = tmp("dir");
        fs::create_dir_all(root.join("AGENTS.md")).unwrap();
        assert!(RuleSet::discover(&root, &root.join("home")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovery_does_not_panic_on_a_rootless_path() {
        // 相对路径没有 parent 链，走到头就该停
        let rs = RuleSet::discover(Path::new("."), Path::new("./nonexistent-home"));
        // 结果是什么不重要，不 panic 才重要
        let _ = rs.render();
    }
}
