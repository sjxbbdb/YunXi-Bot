//! 人格：`<数据目录>/persona.md`。**可改，不再写死在代码里。**
//!
//! ## 为什么是"第一个 `# 标题`当名字"
//!
//! 名字和人格正文是两个东西（`build_persona` 分开写"# 身份"和"# 人格"），
//! 但对**写它的人**来说，最自然的写法就是：
//!
//! ```markdown
//! # 云熙
//!
//! 你是一个……（人格正文）
//! ```
//!
//! **一级标题是名字，其余是正文。** 不用 front-matter、不用 JSON、
//! 不用额外的名字字段——多一个字段就多一处要解释、要校验、
//! 要处理"填了名字没填正文"这类组合。
//!
//! 没有一级标题时整个文件都当正文，名字用默认值。
//!
//! ## 它进稳定前缀，所以改它 = 一次缓存未命中
//!
//! `build_persona` 的文档要求"必须是纯函数，同样输入永远产出同样字节"。
//! 人格是**稳定前缀的第一段**，它一变，后面所有字节的**偏移都变了**，
//! 缓存整段作废。
//!
//! **这个代价要在输出里说清楚**，而不是让人自己去猜为什么改完变慢了。
//! ——不像记忆段（它带版本号，只在内容变时才换），人格没有版本号可用：
//! **它本来就不该频繁改**。
//!
//! ## 和 `profile.md` 的分工
//!
//! | | 写的是谁 | 例子 |
//! |---|---|---|
//! | **`persona.md`** | **助理是谁** | "你叫云熙，说话简洁" |
//! | `profile.md` | **使用者是谁** | "叫我老王，别用感叹号" |
//!
//! 两件事、两个文件。**混在一起的话，"我想换个助理的性格"和
//! "我想让它更懂我"就变成了同一件事**——而它们该分开改。

use std::path::Path;

/// 人格文件的文件名。
pub const PERSONA_FILE: &str = "persona.md";

/// 人格正文的字节上限。
///
/// 比画像的 8KB 大——人格是"这个助理是谁"的完整设定，本来就该写得下。
/// 但不能无限：**它进稳定前缀而且每轮都发**，一份 200KB 的人格
/// 会把前缀和预算一起毁掉。
pub const MAX_PERSONA_BYTES: usize = 16 * 1024;

/// 名字的长度上限。
///
/// 名字是**每轮都发**的，而且它进的是"# 身份"那一行——
/// 一个 500 字的名字说明写的人把正文写进标题了。
pub const MAX_NAME_CHARS: usize = 32;

/// 一份人格。
#[derive(Debug, Clone)]
pub struct Persona {
    /// 助理的名字。
    pub name: String,
    /// 人格正文（不含名字那一行）。
    pub text: String,
    /// 它是从哪来的——默认值还是文件。**这个区分要留着**：
    /// "我在用默认人格"和"我的人格文件没读到"是两件事。
    pub source: PersonaSource,
}

/// 人格从哪来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaSource {
    /// 内置默认（还没建过文件）。
    Builtin,
    /// 读到了使用者自己的文件。
    File,
}

impl Persona {
    /// 解析一份人格文件。
    ///
    /// 规则：**第一个非空行若是 `# 开头`，它就是名字**；
    /// 其余（去掉那一行）全是正文。没有一级标题时名字留空，
    /// 由调用方填默认值。
    pub fn parse(raw: &str) -> (Option<String>, String) {
        let mut name: Option<String> = None;
        let mut body = String::new();
        for (i, line) in raw.lines().enumerate() {
            let t = line.trim();
            if name.is_none() && i < 3 && t.starts_with("# ") {
                let n = t.trim_start_matches("# ").trim();
                // **名字过长就不当名字**：那多半是写的人把正文写进标题了。
                // 硬截断会截出一个奇怪的名字，当成正文更安全。
                if !n.is_empty() && n.chars().count() <= MAX_NAME_CHARS {
                    name = Some(n.to_string());
                    continue;
                }
            }
            body.push_str(line);
            body.push('\n');
        }
        (name, body.trim().to_string())
    }

    /// 超上限时按**字符边界**截断。
    ///
    /// 按字节截会把一个汉字切成两半，渲染出来是乱码。
    fn truncate(raw: String) -> (String, bool) {
        if raw.len() <= MAX_PERSONA_BYTES {
            return (raw, false);
        }
        let mut cut = MAX_PERSONA_BYTES;
        while cut > 0 && !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        (raw[..cut].to_string(), true)
    }
}

/// 内置的名字。
///
/// **它只是"还没建过文件时的起点"**，不再是唯一的可能。
pub const BUILTIN_NAME: &str = "云熙";

/// 从数据目录读人格。没有文件就返回传进来的那份内置默认。
///
/// `builtin_name` / `builtin_text` 由调用方给（现在是
/// `chat_handler` 里那两个常量）——**不在这个模块里硬编码**：
/// 这一层该只负责"读文件和解析"，内置值是什么是 CLI 的事。
pub fn load(home: &Path, builtin_name: &str, builtin_text: &str) -> Persona {
    let path = home.join(PERSONA_FILE);
    if !path.exists() {
        return Persona {
            name: builtin_name.to_string(),
            text: builtin_text.to_string(),
            source: PersonaSource::Builtin,
        };
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            // **文件存在却读不动要说一声**——"你还没写"和"写了没读到"
            // 是两件不同的事。
            eprintln!("提示：人格文件读不动（{}）：{e}", path.display());
            return Persona {
                name: builtin_name.to_string(),
                text: builtin_text.to_string(),
                source: PersonaSource::Builtin,
            };
        }
    };
    let (raw, _truncated) = Persona::truncate(raw);
    let (name, body) = Persona::parse(&raw);
    Persona {
        // 文件里没写名字就还用默认的——**不该因为没写名字就丢人格**
        name: name.unwrap_or_else(|| builtin_name.to_string()),
        // 文件里只有名字、没有正文时也退回内置正文——
        // 一份空人格会让助理失去所有设定，那比用默认的坏得多
        text: if body.is_empty() {
            builtin_text.to_string()
        } else {
            body
        },
        source: PersonaSource::File,
    }
}

/// 初始模板。
pub const TEMPLATE: &str = r#"# 云熙

<!-- 第一行的一级标题就是名字，改成你想要的。下面写人格。
     这份文件整段会进稳定前缀，所以：改它会让缓存失效一次
     （之后重新稳定）。它本来就不该频繁改。 -->

你是一个通用 agent，也是一个一直陪着的助理。

说话直接、简洁，先给结论再给理由。不确定就说不确定，不要编。
遇到需要使用者亲自拍板的事就停下来问，不要替他决定。
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_heading_is_the_name_and_the_rest_is_the_body() {
        let (name, body) = Persona::parse("# 小云\n\n你说话简洁。\n");
        assert_eq!(name.as_deref(), Some("小云"));
        assert_eq!(body, "你说话简洁。");
        assert!(!body.contains("小云"), "名字那一行不该留在正文里");
    }

    #[test]
    fn a_file_without_a_heading_is_all_body() {
        let (name, body) = Persona::parse("你说话简洁。\n");
        assert_eq!(name, None);
        assert_eq!(body, "你说话简洁。");
    }

    #[test]
    fn a_second_level_heading_is_not_the_name() {
        // 只有一级标题算名字。二级标题是正文的一部分——
        // 否则"## 说话方式"会被当成名字。
        let (name, body) = Persona::parse("## 说话方式\n\n简洁。\n");
        assert_eq!(name, None);
        assert!(body.contains("## 说话方式"));
    }

    #[test]
    fn an_overlong_first_heading_is_treated_as_body() {
        // **名字过长就不当名字**：那多半是写的人把正文写进标题了。
        // 硬截断会截出一个奇怪的名字。
        let long = format!("# {}\n\n正文\n", "很".repeat(100));
        let (name, body) = Persona::parse(&long);
        assert_eq!(name, None, "超长标题不该当名字");
        assert!(body.contains("很很很"));
    }

    #[test]
    fn a_heading_deep_in_the_file_is_not_the_name() {
        // 只在前三行里找名字——不然正文中间偶尔出现的 "# xxx"
        // 会把名字抢走，而且**正文不同的人会得到不同的名字**。
        let (name, _) = Persona::parse("正文第一行\n正文第二行\n正文第三行\n# 这不该是名字\n");
        assert_eq!(name, None);
    }

    #[test]
    fn no_file_means_the_builtin_persona() {
        let dir = std::env::temp_dir().join(format!("yunxi-persona-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(PERSONA_FILE));
        let p = load(&dir, BUILTIN_NAME, "内置正文");
        assert_eq!(p.source, PersonaSource::Builtin);
        assert_eq!(p.name, BUILTIN_NAME);
        assert_eq!(p.text, "内置正文");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_only_file_keeps_the_builtin_body() {
        // **一份只有名字的文件不该让助理失去所有设定**——
        // 那比用默认正文坏得多。
        let dir = std::env::temp_dir().join(format!("yunxi-persona-n-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(PERSONA_FILE), "# 小云\n").unwrap();
        let p = load(&dir, BUILTIN_NAME, "内置正文");
        assert_eq!(p.name, "小云");
        assert_eq!(p.text, "内置正文", "只有名字时要退回内置正文");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_written_persona_is_used_as_is() {
        let dir = std::env::temp_dir().join(format!("yunxi-persona-w-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(PERSONA_FILE), "# 小云\n\n只说短句。\n").unwrap();
        let p = load(&dir, BUILTIN_NAME, "内置正文");
        assert_eq!(p.source, PersonaSource::File);
        assert_eq!(p.name, "小云");
        assert_eq!(p.text, "只说短句。");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_template_has_a_name_and_a_body() {
        let (name, body) = Persona::parse(TEMPLATE);
        assert_eq!(name.as_deref(), Some(BUILTIN_NAME));
        assert!(!body.trim().is_empty(), "模板要有正文，不能只有名字");
    }
}
