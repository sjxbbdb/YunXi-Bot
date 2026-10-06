//! 文件类工具：读、列目录、搜内容、写、改。
//!
//! ## 一条从 Claude Code 抄来的硬规则：先读后写
//!
//! `edit_file` 在本次会话里**必须先读过目标文件**才能改它。这不是洁癖：
//! 它挡掉的是"模型凭印象改文件"——模型记得这个文件大概长什么样，
//! 于是给一个看起来对、实际对不上的修改，然后文件被改坏。
//!
//! `ToolContext::note_read` / `has_read` 就是为它准备的。
//!
//! ## 能力划分
//!
//! | 工具 | 能力 | 为什么 |
//! |---|---|---|
//! | `read_file` `list_dir` `search_files` | [`Capability::ReadOnly`] | 无副作用 |
//! | `write_file` `edit_file` | [`Capability::Write`] | 改状态，必须问 |
//!
//! 审批判定不在这里做——那是 [`crate::tool::gate`] 的职责。
//! 混在一起会出现"某个工具自己忘了检查"的洞。
//!
//! ## 输出上限，以及"截断必须说出来"
//!
//! 工具输出会原样进模型上下文，每一段都要花钱、都会挤掉别的信息。
//! 一个 10MB 的日志读进来，这次任务就废了。所以每个吐内容的工具都有上限，
//! 而且**截断必须显式告诉模型**：它不知道被截断，就会把"我没看到"
//! 当成"不存在"，然后给出一个语气很确定的错误结论。
//!
//! ## 失败信息要能让模型自己修正
//!
//! 这个文件里所有的 `Failed` 都尽量带上**可用于下一步的具体事实**：
//! `edit_file` 报"匹配 0 次"而不是"没找到"，报"匹配 3 次，请补上下文或开
//! replace_all"而不是"不唯一"。工具失败的归宿是被回灌给模型换条路走，
//! 信息不够它就只能在同一个位置反复重试。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::tool::{Capability, Tool, ToolContext, ToolError, ToolOutput};

/// 不传 `limit` 时读多少行。
///
/// 400 这个量级是"够看完一个源文件的主要部分，又不至于把上下文吃掉一大块"。
const READ_DEFAULT_LIMIT: usize = 400;

/// 单次 `read_file` 的输出上限（约 200KB）。
///
/// 行数上限挡不住"每行几 KB"的文件（压缩过的 json、单行日志），
/// 所以字节上限必须独立存在。
const READ_MAX_BYTES: usize = 200 * 1024;

/// 允许 `read_file` 打开的文件大小上限。
///
/// 这不是功能上限，是防 OOM 的护栏：`fs::read` 会把整个文件读进内存，
/// 一个几 GB 的镜像文件足以把进程打爆。64MB 足够宽，正常源码和日志碰不到。
const READ_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// 行号列宽。固定 4 而不是按总行数动态算：
/// 模型要靠 `   12| ` 这种固定形状定位，宽度随文件变会让同一份上下文里
/// 出现两种格式，模型就得先判断是哪一种。
const LINE_NO_WIDTH: usize = 4;

/// `list_dir` 一次最多列多少条。
///
/// `node_modules` 这种目录动辄上万条，全列出来除了烧 token 没有别的效果。
const LIST_MAX_ENTRIES: usize = 500;

const SEARCH_DEFAULT_MAX_RESULTS: usize = 50;

/// 不管模型要多少，最多给这么多。一次搜索吐几十万行，这次任务就废了。
const SEARCH_MAX_RESULTS_CAP: usize = 1000;

/// 递归深度上限。
///
/// 除了防止结果里塞满深层噪音，它还是**符号链接环**的兜底：
/// 目录软链接指回祖先时，没有深度上限的遍历不会停。
const SEARCH_MAX_DEPTH: usize = 12;

/// 超过这个大小的文件不搜。
///
/// 搜二进制或压缩包既搜不出东西，又会让遍历卡在读取上。
const SEARCH_MAX_FILE_BYTES: u64 = 1024 * 1024;

/// 单条命中最多回显多少字节。日志文件里一行几十万字符是常态。
const HIT_LINE_MAX_BYTES: usize = 300;

// ---------- 参数处理 ----------

/// 取必填字符串。**允许空串**——`content`/`new_string` 传空串是合法用法
/// （新建空文件、删掉一段文本），不能因为"看起来像没填"就拒绝。
fn require_str(args: &Value, key: &str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(ToolError::BadArgs {
            detail: format!("参数 {key} 必须是字符串，收到: {other}"),
        }),
        None => Err(ToolError::BadArgs {
            detail: format!("缺少必填参数 {key}，实际参数: {}", brief(args)),
        }),
    }
}

/// 取必填字符串，且不接受空白串。
///
/// 空白串单独拦住：`"path": " "` 会被当成合法路径一路走到 fs 调用，
/// 最后报出来的是"找不到文件"，而真正的问题是参数没填——排查方向被带偏。
fn require_nonempty_str(args: &Value, key: &str) -> Result<String, ToolError> {
    let s = require_str(args, key)?;
    if s.trim().is_empty() {
        return Err(ToolError::BadArgs {
            detail: format!("参数 {key} 不能为空"),
        });
    }
    Ok(s)
}

/// 取可选正整数。
///
/// 负数与小数在这里就挡掉，不留给下游去转：把 `-1` 悄悄转成 0 或
/// `usize::MAX`，会变成一个"看起来正常"的数字，比直接报错难查得多。
fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let v = n.as_u64().ok_or_else(|| ToolError::BadArgs {
                detail: format!("参数 {key} 必须是非负整数，收到: {n}"),
            })?;
            let v = usize::try_from(v).map_err(|_| ToolError::BadArgs {
                detail: format!("参数 {key} 超出本机可表示的整数范围: {n}"),
            })?;
            Ok(Some(v))
        }
        Some(other) => Err(ToolError::BadArgs {
            detail: format!("参数 {key} 必须是整数，收到: {other}"),
        }),
    }
}

/// 取可选布尔。默认值由调用方给，这里只负责区分"没给"和"给了什么"。
fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(ToolError::BadArgs {
            detail: format!("参数 {key} 必须是 true/false，收到: {other}"),
        }),
    }
}

/// 错误信息里回显参数要截断。
///
/// `write_file` 的 `content` 可能有几十万字符，原样塞进错误等于把刚才
/// 想避免的大输出又灌回模型上下文一遍。
fn brief(args: &Value) -> String {
    const MAX_CHARS: usize = 300;
    let s = args.to_string();
    if s.chars().count() <= MAX_CHARS {
        s
    } else {
        format!("{}...", s.chars().take(MAX_CHARS).collect::<String>())
    }
}

/// 取必填的 `path` 并解析成绝对路径。返回（模型给的原串, 解析后的路径）。
///
/// 原串要留着：报错时给模型看它自己写的那个路径，它才认得出问题在哪；
/// 只给绝对路径，相对路径写错的情况下两边看起来都对。
fn path_arg(args: &Value, ctx: &ToolContext) -> Result<(String, PathBuf), ToolError> {
    let raw = require_nonempty_str(args, "path")?;
    let abs = ctx.resolve(Path::new(&raw));
    Ok((raw, abs))
}

/// 取可选的 `path`，不给就用工作目录。
///
/// 空串按"没给"处理：模型偶尔会传 `"path": ""` 表示"就当前目录"，
/// 那不是错误，是它没有别的写法。
fn optional_path_arg(args: &Value, ctx: &ToolContext) -> Result<PathBuf, ToolError> {
    match args.get("path") {
        None | Some(Value::Null) => Ok(ctx.cwd.clone()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(ctx.cwd.clone()),
        Some(Value::String(s)) => Ok(ctx.resolve(Path::new(s))),
        Some(other) => Err(ToolError::BadArgs {
            detail: format!("参数 path 必须是字符串，收到: {other}"),
        }),
    }
}

/// 审批粒度 = **工具实际会操作的那个绝对路径**。
///
/// ## 为什么必须是绝对路径（第一版这里是错的）
///
/// 第一版返回模型给的那串字符，理由是"签名里没有 `ToolContext`，用进程 cwd
/// 硬解析会让审批看到的路径 ≠ 工具实际动的文件"。那个顾虑是对的，
/// **但结论反了**：正确的修法是给签名加 `ctx`，而不是返回原串。
///
/// 返回原串会造成两个问题：
///
/// 1. **规则静默失效**：使用者写 `--allow read_file:D:\notes`，
///    模型给相对路径 `notes\a.md` → 前缀对不上 → 每次都问。
///    使用者会以为"我明明写了规则怎么还问"，然后开始怀疑整个审批机制。
/// 2. **更糟的是**：审批提示上显示 `notes\a.md`，而工具真正动的是
///    `<cwd>\notes\a.md`——**人在批准一个自己没看清的东西**。
///
/// 现在用 `ctx.resolve` 解析，与 `call` 里走的是同一个函数，
/// 所以"审批看到的"和"实际动的"必然是同一个路径。
fn path_specifier(args: &Value, ctx: &ToolContext) -> Option<String> {
    let raw = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())?;
    Some(ctx.resolve(Path::new(raw)).to_string_lossy().to_string())
}

/// 目录类工具（`list_dir` / `search_files`）的审批粒度。
///
/// 没给 `path` 时返回 `"."` 而不是 `None`：作用域本来就是工作目录，
/// 而 `gate` 的 `inside_cwd` 会拿 `ctx.cwd` 去解析 `.`，"只读且在工作区内
/// 免问"这条规则才成立。返回 `None` 会让每一次"在项目里搜一下"都去问人——
/// 它明明什么都没改。
fn dir_specifier(args: &Value, ctx: &ToolContext) -> Option<String> {
    // 没给路径 = 工作目录本身。直接给 cwd 而不是 ".".：后者解析出来是
    // `C:\work\.`，虽然 `inside_cwd` 和规则匹配都能处理，但审批提示上
    // 显示一个带 `\.` 的路径会让人以为哪里不对。
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => ctx.resolve(Path::new(p)),
        _ => ctx.cwd.clone(),
    };
    Some(p.to_string_lossy().to_string())
}

/// 显示路径：能相对工作目录表示就相对表示。
///
/// 绝对路径一长，模型把它抄回 `read_file` 时容易抄错；相对路径的解析基准
/// 和 `read_file` 一致（都是工作目录），抄回去必定指向同一个文件。
/// 工作目录之外没有"相对"可言，照实给绝对路径。
fn display_path(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => path.to_string_lossy().into_owned(),
    }
}

/// 按**字符边界**截断。
///
/// 不能按字节切：切出半个多字节字符后 `&s[..n]` 直接 panic，
/// 而中文内容到处都是多字节字符。
fn truncate_bytes_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if i + c.len_utf8() > max_bytes {
            break;
        }
        end = i + c.len_utf8();
    }
    &s[..end]
}

// ---------- read_file ----------

/// 读文件，返回带行号的内容。
pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读一个文本文件，返回带行号的内容。改文件之前必须先用它读一遍；文件很长时用 offset/limit 分段读。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径；相对路径按当前工作目录解析" },
                "offset": { "type": "integer", "minimum": 1, "description": "从第几行开始读（1 起，默认 1）" },
                "limit": { "type": "integer", "minimum": 1, "description": "最多返回多少行（默认 400）" }
            },
            "required": ["path"]
        })
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        path_specifier(args, ctx)
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let (raw, abs) = path_arg(args, ctx)?;
        let offset = optional_usize(args, "offset")?.unwrap_or(1);
        let limit = optional_usize(args, "limit")?.unwrap_or(READ_DEFAULT_LIMIT);
        if offset == 0 {
            // offset 是 1 起的。容忍 0 会让"第一行"有两个写法，模型引用行号时更容易错位。
            return Err(ToolError::BadArgs {
                detail: "offset 从 1 起，不能是 0".into(),
            });
        }
        if limit == 0 {
            return Err(ToolError::BadArgs {
                detail: "limit 至少为 1；想看更多就别传 limit".into(),
            });
        }

        let md = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Failed {
                    detail: format!("文件不存在: {raw}（解析为 {}）", abs.display()),
                });
            }
            Err(e) => {
                return Err(ToolError::Failed {
                    detail: format!("读不了 {}: {e}", abs.display()),
                });
            }
        };
        if md.is_dir() {
            // 直接说清楚该换哪个工具，别让模型对着"拒绝访问"猜。
            return Err(ToolError::Failed {
                detail: format!("{raw} 是一个目录，不是文件；看目录内容用 list_dir"),
            });
        }
        if md.len() > READ_MAX_FILE_BYTES {
            return Err(ToolError::Failed {
                detail: format!(
                    "文件太大（{} MB），read_file 一次读不进来；用 search_files 定位，或用 run_command 分段处理",
                    md.len() / (1024 * 1024)
                ),
            });
        }

        let bytes = std::fs::read(&abs).map_err(|e| ToolError::Failed {
            detail: format!("读不了 {}: {e}", abs.display()),
        })?;
        let content = String::from_utf8(bytes).map_err(|e| ToolError::Failed {
            detail: format!(
                "不是 UTF-8（可能是二进制文件）: {}（第 {} 字节处编码非法）",
                abs.display(),
                e.utf8_error().valid_up_to()
            ),
        })?;

        let total = content.lines().count();
        if total == 0 {
            ctx.note_read(&abs);
            return Ok(ToolOutput::read(format!(
                "{}（空文件，0 行）",
                display_path(&abs, &ctx.cwd)
            )));
        }
        if offset > total {
            // 这一条**不能** note_read：一页都没看到就算"读过"，
            // "先读后写"会被"传个越界的 offset"绕过，规则等于白设。
            return Err(ToolError::Failed {
                detail: format!("offset {offset} 超出文件总行数 {total}: {}", abs.display()),
            });
        }
        ctx.note_read(&abs);

        // 不先 collect 成 Vec<&str>：千万行的文件会平白多占一两百 MB，
        // 而我们最多只显示几百行。两遍遍历比一次大分配便宜。
        let mut body = String::new();
        let mut shown = 0usize;
        let mut cut_by_bytes = false;
        let mut first_line_cut = false;

        for (i, line) in content.lines().skip(offset - 1).take(limit).enumerate() {
            let no = offset + i;
            let rendered = format!("{no:>width$}| {line}\n", width = LINE_NO_WIDTH);
            if body.len() + rendered.len() > READ_MAX_BYTES {
                if shown == 0 {
                    // 第一行就超过整个输出预算：切短它也要给出开头。
                    // 直接放弃的话，这条工具对"一行几十万字符的压缩文件"完全不可用，
                    // 而那种文件恰恰最需要先看一眼开头。
                    let prefix = format!("{no:>width$}| ", width = LINE_NO_WIDTH);
                    let budget = READ_MAX_BYTES.saturating_sub(prefix.len() + 1);
                    body.push_str(&prefix);
                    body.push_str(truncate_bytes_at_char_boundary(line, budget));
                    shown = 1;
                    first_line_cut = true;
                } else {
                    cut_by_bytes = true;
                }
                break;
            }
            body.push_str(&rendered);
            shown += 1;
        }

        let last = offset + shown - 1;
        let remaining = total - last;

        let mut out = format!("{}（共 {total} 行）\n", display_path(&abs, &ctx.cwd));
        if offset > 1 {
            out.push_str(&format!("（从第 {offset} 行开始）\n"));
        }
        out.push_str(&body);
        if remaining > 0 {
            let because = if cut_by_bytes {
                format!("输出已达 {}KB 上限", READ_MAX_BYTES / 1024)
            } else {
                format!("limit={limit}")
            };
            out.push_str(&format!(
                "\n（已截断，原因：{because}。本次显示第 {offset}-{last} 行，共 {total} 行；继续读用 offset={}）\n",
                last + 1
            ));
        } else if first_line_cut {
            out.push_str(&format!(
                "\n（已截断：第 {offset} 行超过 {}KB 上限，只显示了该行的开头）\n",
                READ_MAX_BYTES / 1024
            ));
        }
        if content.contains("\r\n") {
            // 上面显示的行已经去掉了 \r（只是为了好读），但 edit_file 是在**原始
            // 字节**上匹配的。不提醒的话，模型照抄屏幕上的行当 old_string，
            // 结果是 0 次匹配，然后开始瞎猜——Windows 上这是最高频的一次失败。
            out.push_str(
                "\n（注意：该文件用 CRLF 行尾。edit_file 的 old_string 里必须带上 \\r\\n 才能匹配；上面显示的行去掉 \\r 只是为了好读）\n",
            );
        }
        Ok(ToolOutput::read(out))
    }
}

// ---------- list_dir ----------

/// 列目录，不递归。
pub struct ListDirTool;

impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "列出一个目录下的条目：名称、是文件还是目录、大小。想先看看项目里有什么时用它，比猜路径快。跳过 .git。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "目录路径；相对路径按当前工作目录解析，不传就是当前工作目录" }
            }
        })
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        dir_specifier(args, ctx)
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let abs = optional_path_arg(args, ctx)?;
        let md = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Failed {
                    detail: format!("目录不存在: {}", abs.display()),
                });
            }
            Err(e) => {
                return Err(ToolError::Failed {
                    detail: format!("读不了 {}: {e}", abs.display()),
                });
            }
        };
        if !md.is_dir() {
            return Err(ToolError::Failed {
                detail: format!("{} 不是目录（是文件？那用 read_file）", abs.display()),
            });
        }
        let rd = std::fs::read_dir(&abs).map_err(|e| ToolError::Failed {
            detail: format!("读不了目录 {}: {e}", abs.display()),
        })?;

        let mut entries: Vec<(String, &'static str, Option<u64>)> = Vec::new();
        let mut skipped_git = 0usize;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            // `.git` 是版本库的元数据，不是项目内容。列出来只会占位置。
            if name == ".git" {
                skipped_git += 1;
                continue;
            }
            let (kind, size) = match e.metadata() {
                Ok(m) if m.is_dir() => ("目录", None),
                // 目录的"大小"在 Windows 上是 0、在 Linux 上是 4096，
                // 两种都没有意义，所以宁可不显示。
                Ok(m) if m.is_file() => ("文件", Some(m.len())),
                Ok(m) if m.file_type().is_symlink() => ("链接", None),
                Ok(_) => ("其它", None),
                Err(_) => ("读不到", None),
            };
            entries.push((name, kind, size));
        }
        // 按名字排序：两次调用的输出必须逐字节一致。顺序抖动会让模型以为
        // 目录变了，也会让上游的前缀缓存白作废（同一份历史、不同的字节）。
        // 目录里条目名唯一，所以按整个元组排序等价于按名字排序。
        entries.sort();

        let total = entries.len();
        let truncated = total > LIST_MAX_ENTRIES;
        entries.truncate(LIST_MAX_ENTRIES);

        let mut out = format!("{}（{total} 项）\n", display_path(&abs, &ctx.cwd));
        if truncated {
            out.push_str(&format!(
                "（已截断：共 {total} 项，只列出前 {LIST_MAX_ENTRIES} 项；需要更多就换个更具体的子目录）\n"
            ));
        }
        if entries.is_empty() {
            out.push_str("（空目录）\n");
        }
        for (name, kind, size) in &entries {
            let size = match size {
                Some(n) => format!("{n:>10}"),
                None => format!("{:>10}", "-"),
            };
            out.push_str(&format!("{kind}  {size}  {name}\n"));
        }
        if skipped_git > 0 {
            out.push_str("（已跳过 .git）\n");
        }
        Ok(ToolOutput::read(out))
    }
}

// ---------- search_files ----------

/// 一处命中。
struct Hit {
    path: PathBuf,
    line: usize,
    text: String,
}

/// 一次搜索的累计状态。
///
/// 做成结构体而不是一串 `&mut` 参数：递归里要同时维护命中、两个跳过计数和
/// "已经凑满"的停止标志，参数一多，调用点就容易把位置传错。
struct Finder<'a> {
    pattern: &'a str,
    max: usize,
    hits: Vec<Hit>,
    /// 因二进制或超过 1MB 而跳过的文件数。
    skipped: usize,
    /// 读不了（权限等）的文件数。**和 skipped 分开计**：
    /// "跳过二进制"是预期行为，"读不了"可能是环境问题，混在一起就看不出来了。
    unreadable: usize,
    /// 已凑满 max：递归要立刻停。不停的话，结果一样，但会把整棵树走完——
    /// 在一个大仓库里，这个差别是几秒和几十毫秒。
    full: bool,
}

impl Finder<'_> {
    fn walk(&mut self, dir: &Path, depth: usize) {
        if self.full || depth > SEARCH_MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            self.unreadable += 1;
            return;
        };
        // 排序的理由和 list_dir 一样：输出顺序必须可复现。
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if self.full {
                return;
            }
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // `.git` 是元数据，`target` 是构建产物：又大又没信息量，
            // 搜它们只会让结果里塞满噪音，还把一次搜索拖成好几秒。
            if name == ".git" || name == "target" {
                continue;
            }
            let Ok(md) = std::fs::metadata(&p) else {
                self.unreadable += 1;
                continue;
            };
            if md.is_dir() {
                self.walk(&p, depth + 1);
            } else if md.is_file() {
                self.scan_file(&p);
            }
        }
    }

    fn scan_file(&mut self, path: &Path) {
        let oversize = std::fs::metadata(path)
            .map(|m| m.len() > SEARCH_MAX_FILE_BYTES)
            .unwrap_or(false);
        if oversize {
            self.skipped += 1;
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // 非 UTF-8 基本就是二进制。对它做逐行 contains 没有意义，
            // 而且真匹配上了，回显的也是一串乱码。
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                self.skipped += 1;
                return;
            }
            Err(_) => {
                self.unreadable += 1;
                return;
            }
        };
        for (i, line) in text.lines().enumerate() {
            if !line.contains(self.pattern) {
                continue;
            }
            let text = if line.len() > HIT_LINE_MAX_BYTES {
                format!(
                    "{} ...（该行过长，已截断）",
                    truncate_bytes_at_char_boundary(line, HIT_LINE_MAX_BYTES)
                )
            } else {
                line.to_string()
            };
            self.hits.push(Hit {
                path: path.to_path_buf(),
                line: i + 1,
                text,
            });
            if self.hits.len() >= self.max {
                self.full = true;
                return;
            }
        }
    }
}

/// 递归搜索明文子串。
///
/// **刻意不支持正则**：不引入 regex 依赖，而"找出哪里提到了 X"这一类
/// 查询占了绝大多数。description 里必须写明这一点，否则模型会试正则、
/// 拿到 0 命中，然后困惑于"明明有这个词"。
pub struct SearchFilesTool;

impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "在目录（或单个文件）里递归搜索明文子串，返回 文件:行号: 该行内容。不支持正则表达式，pattern 按字面量匹配。跳过 .git 与 target，不搜超过 1MB 的文件。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "要查找的明文子串（不是正则）" },
                "path": { "type": "string", "description": "搜索起点，可以是目录或单个文件；默认当前工作目录" },
                "max_results": { "type": "integer", "minimum": 1, "description": "最多返回多少处命中（默认 50）" }
            },
            "required": ["pattern"]
        })
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        dir_specifier(args, ctx)
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = require_nonempty_str(args, "pattern")?;
        let root = optional_path_arg(args, ctx)?;
        let requested = optional_usize(args, "max_results")?.unwrap_or(SEARCH_DEFAULT_MAX_RESULTS);
        if requested == 0 {
            return Err(ToolError::BadArgs {
                detail: "max_results 至少为 1".into(),
            });
        }
        let max = requested.min(SEARCH_MAX_RESULTS_CAP);

        let md = match std::fs::metadata(&root) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Failed {
                    detail: format!("路径不存在: {}", root.display()),
                });
            }
            Err(e) => {
                return Err(ToolError::Failed {
                    detail: format!("读不了 {}: {e}", root.display()),
                });
            }
        };

        let mut finder = Finder {
            pattern: &pattern,
            max,
            hits: Vec::new(),
            skipped: 0,
            unreadable: 0,
            full: false,
        };
        if md.is_dir() {
            finder.walk(&root, 1);
        } else {
            // 单文件也能搜：模型手上已经有一个文件路径时，
            // 不该逼它先编出一个目录来。
            finder.scan_file(&root);
        }

        let shown = finder.hits.len();
        let where_ = display_path(&root, &ctx.cwd);
        let mut out = String::new();
        if shown == 0 {
            out.push_str(&format!(
                "在 {where_} 里没有找到包含 \"{pattern}\" 的内容。\n"
            ));
            // 模型最可能踩的坑就是拿正则来试。它现在能拿 0 命中，
            // 只差一句话才知道该换写法。
            out.push_str("pattern 是明文子串，不是正则；如果刚才用了 .* 或 \\d 这类写法，请改成原文片段再试。\n");
        } else {
            out.push_str(&format!("在 {where_} 里找到 {shown} 处匹配：\n"));
            for hit in &finder.hits {
                out.push_str(&format!(
                    "{}:{}: {}\n",
                    display_path(&hit.path, &ctx.cwd),
                    hit.line,
                    hit.text
                ));
            }
            if finder.full {
                out.push_str(&format!(
                    "\n（已达 max_results={max} 上限，可能还有更多匹配；提高 max_results 或缩小搜索范围再试）\n"
                ));
            }
        }
        if finder.skipped > 0 {
            out.push_str(&format!(
                "（另有 {} 个二进制或超过 1MB 的文件被跳过）\n",
                finder.skipped
            ));
        }
        if finder.unreadable > 0 {
            out.push_str(&format!("（{} 个条目读不了，已跳过）\n", finder.unreadable));
        }
        Ok(ToolOutput::read(out))
    }
}

// ---------- write_file ----------

/// 整体写文件（新建或覆盖）。
pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "把内容写入文件，新建或整体覆盖，父目录会自动创建。覆盖已存在的文件前必须先 read_file 读过它。只改一小段用 edit_file 更安全。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径；相对路径按当前工作目录解析" },
                "content": { "type": "string", "description": "要写入的完整内容（可以是空串，表示建一个空文件）" }
            },
            "required": ["path", "content"]
        })
    }

    fn capability(&self) -> Capability {
        Capability::Write
    }

    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        path_specifier(args, ctx)
    }

    /// 覆盖或新建之前，把"文件会变成什么样"给出来。
    ///
    /// **覆盖是破坏性的**——上面 `call` 里那段注释说得很清楚：
    /// 模型凭印象重写一个文件，就是把用户没让它动的东西一起抹掉。
    /// 而在此之前，人在审批框里看到的是参数 JSON，**看不到被抹掉的是什么**。
    ///
    /// 只读：读一下现有内容算个 diff，不动任何东西。
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        let (raw, abs) = path_arg(args, ctx).ok()?;
        let content = args.get("content")?.as_str()?;
        let label = if abs.exists() {
            format!("要覆盖 {raw}")
        } else {
            format!("要新建 {raw}")
        };
        // 读不了就当没有旧内容——比如它是个二进制文件。
        // 那正是最该看 diff 的场合，所以照样给预览（全部算新增）。
        let before = std::fs::read_to_string(&abs).unwrap_or_default();
        Some(crate::diff::render(&before, content, &label))
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let (raw, abs) = path_arg(args, ctx)?;
        let content = require_str(args, "content")?;

        if abs.is_dir() {
            return Err(ToolError::Failed {
                detail: format!("{raw} 是一个目录，写不进去；要写入的是目录下的哪个文件？"),
            });
        }
        // **覆盖已存在的文件要先读过它。**
        //
        // 新建不设门槛：那不会毁掉任何已有内容。但覆盖是破坏性的——
        // 模型凭印象重写一个文件，就是把用户没让它动的东西一起抹掉，
        // 而且没有 diff 可看。这条从 Claude Code 抄来，专门挡这种事故。
        let existed = abs.exists();
        if existed && !ctx.has_read(&abs) {
            return Err(ToolError::Denied {
                reason: format!(
                    "覆盖已存在的文件前必须先 read_file 读过它（当前会话里还没读过 {raw}）；只是想加内容就用 edit_file"
                ),
            });
        }
        // 父目录自动创建：让模型先建目录只是多一步，而少一步它就会
        // 反复撞"路径不存在"。
        let parent = abs.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(parent) = parent {
            std::fs::create_dir_all(parent).map_err(|e| ToolError::Failed {
                detail: format!("建不了目录 {}: {e}", parent.display()),
            })?;
        }
        std::fs::write(&abs, content.as_bytes()).map_err(|e| ToolError::Failed {
            detail: format!("写不了 {}: {e}", abs.display()),
        })?;
        // 刚写下去的内容就是文件的全部内容，模型手里已经有它了。
        // 不记这一笔的话，紧接着的 edit_file 会被"没读过"挡下来，
        // 逼模型白读一遍自己刚写的文件。
        ctx.note_read(&abs);

        let action = if existed { "覆盖" } else { "新建" };
        Ok(ToolOutput::changed(format!(
            "已{action} {}（{} 字节）",
            display_path(&abs, &ctx.cwd),
            content.len()
        )))
    }
}

// ---------- edit_file ----------

/// 定点替换文件内容。
pub struct EditFileTool;

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "把文件里的 old_string 替换成 new_string，用于定点修改。必须先 read_file 读过该文件。old_string 必须在文件中恰好出现一次（除非 replace_all=true）。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "文件路径；相对路径按当前工作目录解析" },
                "old_string": { "type": "string", "description": "要被替换的原文，必须与文件内容逐字符一致（含缩进与行尾）" },
                "new_string": { "type": "string", "description": "替换成什么（可以是空串，表示删除这段）" },
                "replace_all": { "type": "boolean", "description": "true 表示替换全部匹配（默认 false，要求 old_string 唯一）" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn capability(&self) -> Capability {
        Capability::Write
    }

    fn specifier(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        path_specifier(args, ctx)
    }

    /// 改之前把"文件会变成什么样"给出来。
    ///
    /// 这是最需要预览的一个工具：审批框里原本显示的是
    /// `{"old_string":"...","new_string":"..."}`，那是**参数**，不是**后果**。
    /// 而定点修改最容易出的错恰恰是"改到了不该改的地方"——
    /// 那种错只有看到上下文才看得出来。
    ///
    /// 只读：读现有内容、算一遍替换结果、渲染 diff，不动任何东西。
    fn preview(&self, args: &Value, ctx: &ToolContext) -> Option<String> {
        let (raw, abs) = path_arg(args, ctx).ok()?;
        let old = args.get("old_string")?.as_str()?;
        let new = args.get("new_string")?.as_str()?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old.is_empty() {
            // 空 old_string 会被 `call` 拒掉，这里也没法算 diff
            return None;
        }
        let before = std::fs::read_to_string(&abs).ok()?;

        // **先说清"这次编辑根本不会成功"。**
        //
        // 这是端到端测试抓到的：模型发来的 `old_string` 里是字面量 `\r\n`
        // 而不是真的回车换行，于是它在文件里一个都找不到，`call` 必然失败。
        // 而预览当时说的是"内容没有变化"——人看到那句会以为
        // "没什么可批的"，**而实际是"批了也会失败"**。
        //
        // 预览最大的价值恰恰在这儿：**在问人之前就发现这次编辑是白问的。**
        let hits = before.matches(old).count();
        if hits == 0 {
            let head: String = old
                .lines()
                .take(3)
                .map(|l| format!("      {l}\n"))
                .collect();
            return Some(format!(
                "要改 {raw}：**文件里找不到这段内容，这次编辑会失败。**\n  \
                 想找的是（前 3 行）：\n{}  \
                 （多半是空白或换行对不上——批准它也不会有任何改动。）",
                head.trim_end()
            ));
        }
        if hits > 1 && !replace_all {
            return Some(format!(
                "要改 {raw}：**这段内容出现了 {hits} 次，而 replace_all 没开，\
                 这次编辑会失败。**\n  \
                 （要全改就开 replace_all；只想改一处就把 old_string 写得更具体。）"
            ));
        }

        let after = if replace_all {
            before.replace(old, new)
        } else {
            before.replacen(old, new, 1)
        };
        Some(crate::diff::render(&before, &after, &format!("要改 {raw}")))
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let (raw, abs) = path_arg(args, ctx)?;
        let old = require_str(args, "old_string")?;
        let new = require_str(args, "new_string")?;
        let replace_all = optional_bool(args, "replace_all")?.unwrap_or(false);

        if old.is_empty() {
            // 空串在任意位置都"匹配"，replace_all 会把整个文件切碎。
            // 这不是洁癖：它是最容易造成不可逆破坏的一种参数。
            return Err(ToolError::BadArgs {
                detail: "old_string 不能为空；要整段重写用 write_file".into(),
            });
        }
        if !abs.exists() {
            // 先判存在再判"读过"：文件根本没有时，说"必须先读一遍"会把模型
            // 引向 read_file，然后在那边再撞一次"不存在"。
            return Err(ToolError::Failed {
                detail: format!("文件不存在: {raw}；要新建文件用 write_file"),
            });
        }
        if !ctx.has_read(&abs) {
            return Err(ToolError::Denied {
                reason: format!(
                    "改文件前必须先 read_file 读过它（当前会话里还没读过 {raw}），否则就是凭印象改"
                ),
            });
        }

        let content = std::fs::read_to_string(&abs).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                ToolError::Failed {
                    detail: format!("不是 UTF-8（可能是二进制文件）: {}", abs.display()),
                }
            } else {
                ToolError::Failed {
                    detail: format!("读不了 {}: {e}", abs.display()),
                }
            }
        })?;

        let count = content.matches(&old).count();
        if count == 0 {
            return Err(ToolError::Failed {
                detail: format!(
                    "在 {} 里没找到 old_string（匹配 0 次）。它必须与文件内容逐字符一致，包括缩进和行尾；建议先 read_file 看一遍原文再改",
                    display_path(&abs, &ctx.cwd)
                ),
            });
        }
        if count > 1 && !replace_all {
            // **把实际次数报出来。** 只说"不唯一"的话，模型不知道是 2 次还是 200 次，
            // 也就判断不出该补上下文还是该开 replace_all。
            return Err(ToolError::Failed {
                detail: format!(
                    "old_string 在 {} 里匹配到 {count} 次，无法确定要改哪一处；请把上下文补足到唯一，或设 replace_all=true 全部替换",
                    display_path(&abs, &ctx.cwd)
                ),
            });
        }

        // 在整段原文上做替换，**不逐行重组**：`lines()` + join 会把文件里
        // 其余的 CRLF 统一成 LF，只想改一行，diff 却是整个文件。
        let updated = if replace_all {
            content.replace(&old, &new)
        } else {
            content.replacen(&old, &new, 1)
        };
        std::fs::write(&abs, updated.as_bytes()).map_err(|e| ToolError::Failed {
            detail: format!("写不了 {}: {e}", abs.display()),
        })?;

        Ok(ToolOutput::changed(format!(
            "已修改 {}：替换 {count} 处",
            display_path(&abs, &ctx.cwd)
        )))
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::policy::SandboxMode;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "yunxi-preview-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir, SandboxMode::WorkspaceWrite)
    }

    // ---- edit_file ----

    #[test]
    fn edit_preview_shows_the_result_not_the_arguments() {
        // **这是整件事的要点。** 审批框里原来只有参数 JSON，
        // 而那是参数，不是后果。
        let d = tmp("edit");
        std::fs::write(d.join("a.md"), "第一行\n旧的\n第三行\n").unwrap();
        let args = json!({"path": "a.md", "old_string": "旧的", "new_string": "新的"});
        let p = EditFileTool.preview(&args, &ctx(&d)).expect("该有预览");

        assert!(p.contains("要改 a.md"), "{p}");
        assert!(p.contains("- 旧的"), "要看到改掉什么: {p}");
        assert!(p.contains("+ 新的"), "要看到换成什么: {p}");
        assert!(p.contains("第一行"), "要看得到上下文: {p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_works_for_a_replacement_that_spans_lines() {
        let d = tmp("edit-multi");
        std::fs::write(d.join("a.md"), "一\n二\n三\n四\n五\n").unwrap();
        let args = json!({
            "path": "a.md",
            "old_string": "二\n三",
            "new_string": "二和三合成了一行"
        });
        let p = EditFileTool.preview(&args, &ctx(&d)).expect("该有预览");
        assert!(p.contains("删 2 行"), "{p}");
        assert!(p.contains("加 1 行"), "{p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_honours_replace_all() {
        // `replace_all` 会改多处，而只算一处的话**预览会骗人**——
        // 人看到"改 1 行"就批了，实际改了 5 处
        let d = tmp("edit-all");
        std::fs::write(d.join("a.md"), "x\nx\nx\n").unwrap();
        let one = EditFileTool
            .preview(
                &json!({"path": "a.md", "old_string": "x", "new_string": "y"}),
                &ctx(&d),
            )
            .unwrap();
        let all = EditFileTool
            .preview(
                &json!({"path": "a.md", "old_string": "x", "new_string": "y", "replace_all": true}),
                &ctx(&d),
            )
            .unwrap();
        assert!(all.contains("加 3 行"), "全替换要如实说改了几处: {all}");
        assert!(!one.contains("加 3 行"), "默认只改一处: {one}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_says_when_the_old_string_is_not_there_at_all() {
        // **这是端到端抓到的。**
        //
        // 模型发来的 `old_string` 里是字面量 `\r\n` 而不是真的回车换行，
        // 于是它在文件里一个都找不到，`call` 必然失败。
        // 而预览当时说的是"内容没有变化"——人看到那句会以为
        // "没什么可批的"，**而实际是"批了也会失败"**。
        //
        // 预览最大的价值正在这儿：**在问人之前就发现这次编辑是白问的。**
        let d = tmp("edit-nomatch");
        std::fs::write(d.join("a.md"), "第一行\n旧的这一行\n第三行\n").unwrap();
        let args = json!({
            "path": "a.md",
            "old_string": "旧的这一行\\r\\n",   // 字面量反斜杠，不是真换行
            "new_string": "新的这一行\\r\\n"
        });
        let p = EditFileTool.preview(&args, &ctx(&d)).expect("该有预览");
        assert!(p.contains("找不到"), "要说清找不到: {p}");
        assert!(p.contains("会失败"), "要说清后果: {p}");
        assert!(
            !p.contains("没有变化"),
            "**不能说「没有变化」**——那句会让人以为没什么可批的: {p}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_says_when_there_are_too_many_matches() {
        // 出现多次而没开 replace_all —— `call` 也会失败
        let d = tmp("edit-dup");
        std::fs::write(d.join("a.md"), "x\nx\n").unwrap();
        let args = json!({"path": "a.md", "old_string": "x", "new_string": "y"});
        let p = EditFileTool.preview(&args, &ctx(&d)).unwrap();
        assert!(p.contains("2 次"), "要说清出现了几次: {p}");
        assert!(p.contains("replace_all"), "要给出下一步怎么办: {p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_still_diffs_when_the_edit_would_work() {
        // 能成的编辑不该被上面那两条拦掉
        let d = tmp("edit-ok");
        std::fs::write(d.join("a.md"), "甲\n乙\n丙\n").unwrap();
        let args = json!({"path": "a.md", "old_string": "乙", "new_string": "贰"});
        let p = EditFileTool.preview(&args, &ctx(&d)).unwrap();
        assert!(p.contains("- 乙"), "{p}");
        assert!(p.contains("+ 贰"), "{p}");
        assert!(!p.contains("会失败"), "{p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_is_none_when_there_is_nothing_to_read() {
        // 文件不存在时给不出后果——**那时宁可说"没有预览"，
        // 也不要编一个看起来像后果的东西**
        let d = tmp("edit-missing");
        let args = json!({"path": "nope.md", "old_string": "a", "new_string": "b"});
        assert!(EditFileTool.preview(&args, &ctx(&d)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_is_none_for_an_empty_old_string() {
        // 空 old_string 会被 `call` 拒掉，所以也没什么后果可言
        let d = tmp("edit-empty");
        std::fs::write(d.join("a.md"), "内容\n").unwrap();
        let args = json!({"path": "a.md", "old_string": "", "new_string": "x"});
        assert!(EditFileTool.preview(&args, &ctx(&d)).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_does_not_touch_the_file() {
        // **预览必须在真正执行之前算，所以它自己不能有副作用。**
        // 否则"看一眼"就等于"改一下"。
        let d = tmp("edit-readonly");
        let original = "旧的\n";
        std::fs::write(d.join("a.md"), original).unwrap();
        let args = json!({"path": "a.md", "old_string": "旧的", "new_string": "新的"});
        let _ = EditFileTool.preview(&args, &ctx(&d));
        assert_eq!(
            std::fs::read_to_string(d.join("a.md")).unwrap(),
            original,
            "预览把文件改了"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn edit_preview_does_not_require_read_before_write() {
        // 预览是给人看的，不是执行——不该受"先读后写"约束。
        // 受了的话，审批框里永远不会有预览（因为还没读过）
        let d = tmp("edit-noread");
        std::fs::write(d.join("a.md"), "旧的\n").unwrap();
        let c = ctx(&d); // 注意：ctx 里 `read_files` 是空的
        assert!(!c.has_read(&d.join("a.md")));
        let args = json!({"path": "a.md", "old_string": "旧的", "new_string": "新的"});
        assert!(EditFileTool.preview(&args, &c).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- write_file ----

    #[test]
    fn write_preview_says_new_when_the_file_is_new() {
        let d = tmp("write-new");
        let args = json!({"path": "new.md", "content": "第一行\n第二行\n"});
        let p = WriteFileTool.preview(&args, &ctx(&d)).expect("该有预览");
        assert!(p.contains("要新建 new.md"), "{p}");
        assert!(p.contains("+ 第一行"), "{p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_preview_shows_what_an_overwrite_would_destroy() {
        // **覆盖是破坏性的。** 而在此之前，人在审批框里看到的
        // 只是参数 JSON——看不到被抹掉的是什么。
        let d = tmp("write-over");
        std::fs::write(d.join("old.md"), "很重要的旧内容\n另一行\n").unwrap();
        let args = json!({"path": "old.md", "content": "全新的内容\n"});
        let p = WriteFileTool.preview(&args, &ctx(&d)).expect("该有预览");
        assert!(p.contains("要覆盖 old.md"), "{p}");
        assert!(
            p.contains("- 很重要的旧内容"),
            "被抹掉的东西必须看得见: {p}"
        );
        assert!(p.contains("+ 全新的内容"), "{p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_preview_says_plainly_when_nothing_changes() {
        let d = tmp("write-same");
        std::fs::write(d.join("a.md"), "一样的内容\n").unwrap();
        let args = json!({"path": "a.md", "content": "一样的内容\n"});
        let p = WriteFileTool.preview(&args, &ctx(&d)).unwrap();
        assert!(p.contains("没有变化"), "{p}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_preview_does_not_touch_the_file() {
        let d = tmp("write-readonly");
        std::fs::write(d.join("a.md"), "原来的\n").unwrap();
        let args = json!({"path": "a.md", "content": "换掉的\n"});
        let _ = WriteFileTool.preview(&args, &ctx(&d));
        assert_eq!(std::fs::read_to_string(d.join("a.md")).unwrap(), "原来的\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- 读取类工具不该有预览 ----

    #[test]
    fn read_only_tools_have_no_preview() {
        // **读取没有"后果"可言。** 硬塞一段预览只会让审批框变长、
        // 让人更不想读——而"不想读"正是这套机制要防的事。
        let d = tmp("reads");
        std::fs::write(d.join("a.md"), "内容\n").unwrap();
        let c = ctx(&d);
        let args = json!({"path": "a.md"});
        assert!(ReadFileTool.preview(&args, &c).is_none());
        assert!(ListDirTool.preview(&json!({"path": "."}), &c).is_none());
        assert!(
            SearchFilesTool
                .preview(&json!({"pattern": "x"}), &c)
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxMode;

    /// 审批粒度测试用的固定工作目录。
    ///
    /// 用固定值而不是 `env::temp_dir()`：粒度的断言要逐字符比对路径，
    /// 而 temp_dir 在不同机器上不一样，断言就变成看运气了。
    fn spec_ctx() -> ToolContext {
        ToolContext::new(PathBuf::from(r"C:\work"), SandboxMode::WorkspaceWrite)
    }

    /// 每个测试一个独立子目录，`Drop` 时删掉。
    ///
    /// 目录名带 pid：`cargo test` 是多线程跑的，同一个 crate 的测试二进制
    /// 还可能同时存在多个（比如一边 `cargo test` 一边 `cargo clippy --all-targets`）。
    /// 只按测试名命名的话，两个进程会互删对方正在用的文件，
    /// 表现成"偶发找不到文件"——最难查的那类失败。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("yunxi-files-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("建临时目录");
            Self(p)
        }

        fn ctx(&self) -> ToolContext {
            ToolContext::new(&self.0, SandboxMode::WorkspaceWrite)
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn seed(&self, rel: &str, body: &str) -> PathBuf {
            let p = self.path(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("建父目录");
            }
            std::fs::write(&p, body).expect("写测试文件");
            p
        }

        fn body(&self, rel: &str) -> String {
            std::fs::read_to_string(self.path(rel)).expect("读测试文件")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // 清理失败不 panic：残留一个临时目录不该让测试变红。
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- read_file ----

    #[test]
    fn read_file_returns_numbered_lines() {
        let t = TempDir::new("read-numbered");
        t.seed("a.txt", "第一行\n第二行\n第三行\n");
        let mut ctx = t.ctx();
        let out = ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("   1| 第一行"), "{}", out.text);
        assert!(out.text.contains("   2| 第二行"), "{}", out.text);
        assert!(out.text.contains("   3| 第三行"), "{}", out.text);
        assert!(!out.changed_state, "读文件不该标成改了状态");
    }

    #[test]
    fn read_file_records_it_for_later_edits() {
        let t = TempDir::new("read-records");
        t.seed("a.txt", "x\n");
        let mut ctx = t.ctx();
        assert!(!ctx.has_read(&t.path("a.txt")));
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        assert!(
            ctx.has_read(&t.path("a.txt")),
            "read_file 必须记下读过的文件"
        );
    }

    #[test]
    fn read_file_offset_out_of_range_does_not_grant_edit_rights() {
        // 一页都没看到就不算"读过"，否则"先读后写"会被一个越界的 offset 绕过
        let t = TempDir::new("read-offset-oob");
        t.seed("a.txt", "只有一行\n");
        let mut ctx = t.ctx();
        let err = ReadFileTool
            .call(&json!({ "path": "a.txt", "offset": 99 }), &mut ctx)
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
        assert!(!ctx.has_read(&t.path("a.txt")));
    }

    #[test]
    fn read_file_missing_is_failed_with_the_path() {
        let t = TempDir::new("read-missing");
        let mut ctx = t.ctx();
        let err = ReadFileTool
            .call(&json!({ "path": "nope.txt" }), &mut ctx)
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => {
                assert!(
                    detail.contains("不存在") && detail.contains("nope.txt"),
                    "{detail}"
                );
            }
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    #[test]
    fn read_file_non_utf8_says_binary() {
        let t = TempDir::new("read-binary");
        std::fs::write(t.path("blob.bin"), [0xff_u8, 0xfe, 0x00, 0x01]).unwrap();
        let mut ctx = t.ctx();
        let err = ReadFileTool
            .call(&json!({ "path": "blob.bin" }), &mut ctx)
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => assert!(detail.contains("不是 UTF-8"), "{detail}"),
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    #[test]
    fn read_file_directory_points_at_list_dir() {
        let t = TempDir::new("read-dir");
        t.seed("sub/x.txt", "x");
        let mut ctx = t.ctx();
        let err = ReadFileTool
            .call(&json!({ "path": "sub" }), &mut ctx)
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => assert!(detail.contains("list_dir"), "{detail}"),
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    #[test]
    fn read_file_offset_skips_earlier_lines() {
        let t = TempDir::new("read-offset");
        t.seed("a.txt", "1\n2\n3\n4\n5\n");
        let mut ctx = t.ctx();
        let out = ReadFileTool
            .call(
                &json!({ "path": "a.txt", "offset": 3, "limit": 2 }),
                &mut ctx,
            )
            .unwrap();
        assert!(out.text.contains("   3| 3"), "{}", out.text);
        assert!(out.text.contains("   4| 4"), "{}", out.text);
        assert!(
            !out.text.contains("   2| 2"),
            "不该回显 offset 之前的行: {}",
            out.text
        );
    }

    #[test]
    fn read_file_limit_truncation_is_announced() {
        let t = TempDir::new("read-trunc-limit");
        let body: String = (1..=50).map(|i| format!("L{i}\n")).collect();
        t.seed("big.txt", &body);
        let mut ctx = t.ctx();
        let out = ReadFileTool
            .call(&json!({ "path": "big.txt", "limit": 10 }), &mut ctx)
            .unwrap();
        // 行号右对齐到 4 列：两位数就是「  10| 」
        assert!(out.text.contains("  10| L10"), "{}", out.text);
        assert!(
            !out.text.contains("L11"),
            "limit 之外的行不该出现: {}",
            out.text
        );
        assert!(out.text.contains("已截断"), "截断必须说出来: {}", out.text);
        assert!(out.text.contains("共 50 行"), "{}", out.text);
        assert!(
            out.text.contains("offset=11"),
            "要告诉模型怎么接着读: {}",
            out.text
        );
    }

    #[test]
    fn read_file_byte_cap_truncates_without_exploding_context() {
        // 400 行的默认限制挡不住"每行几 KB"，字节上限必须独立生效
        let t = TempDir::new("read-trunc-bytes");
        let line = format!("{}\n", "x".repeat(999));
        t.seed("wide.txt", &line.repeat(600));
        let mut ctx = t.ctx();
        let out = ReadFileTool
            .call(&json!({ "path": "wide.txt" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("已截断"), "{}", out.text);
        assert!(
            out.text.len() < READ_MAX_BYTES + 4096,
            "输出必须在字节上限附近收住，实际 {}",
            out.text.len()
        );
    }

    #[test]
    fn read_file_warns_about_crlf_before_edits() {
        // 显示时去掉了 \r，但 edit_file 是在原始字节上匹配的：
        // 不提醒，模型照抄屏幕上的行去当 old_string 会 0 次匹配
        let t = TempDir::new("read-crlf");
        t.seed("a.txt", "one\r\ntwo\r\n");
        let mut ctx = t.ctx();
        let out = ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("CRLF"), "{}", out.text);
    }

    // ---- write_file ----

    #[test]
    fn write_file_creates_file_and_parents() {
        let t = TempDir::new("write-new");
        let mut ctx = t.ctx();
        let out = WriteFileTool
            .call(
                &json!({ "path": "a/b/c.txt", "content": "hello" }),
                &mut ctx,
            )
            .unwrap();
        assert!(out.changed_state);
        assert!(out.text.contains("新建"), "{}", out.text);
        assert!(out.text.contains("5 字节"), "{}", out.text);
        assert_eq!(t.body("a/b/c.txt"), "hello");
    }

    #[test]
    fn write_file_accepts_empty_content() {
        // "把这个文件清空"是合法请求，不能被参数校验挡掉
        let t = TempDir::new("write-empty");
        let mut ctx = t.ctx();
        WriteFileTool
            .call(&json!({ "path": "empty.txt", "content": "" }), &mut ctx)
            .unwrap();
        assert_eq!(t.body("empty.txt"), "");
    }

    #[test]
    fn write_file_refuses_to_overwrite_an_unread_file() {
        let t = TempDir::new("write-unread");
        t.seed("a.txt", "原内容\n");
        let mut ctx = t.ctx();
        let err = WriteFileTool
            .call(&json!({ "path": "a.txt", "content": "覆盖" }), &mut ctx)
            .unwrap_err();
        match err {
            ToolError::Denied { reason } => assert!(reason.contains("read_file"), "{reason}"),
            other => panic!("应是 Denied: {other:?}"),
        }
        assert_eq!(t.body("a.txt"), "原内容\n", "被拒之后文件不能被动过");
    }

    #[test]
    fn write_file_overwrites_after_reading() {
        let t = TempDir::new("write-read");
        t.seed("a.txt", "旧\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let out = WriteFileTool
            .call(&json!({ "path": "a.txt", "content": "新" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("覆盖"), "{}", out.text);
        assert_eq!(t.body("a.txt"), "新");
    }

    // ---- edit_file ----

    #[test]
    fn edit_file_replaces_a_unique_match() {
        let t = TempDir::new("edit-unique");
        t.seed("a.txt", "fn a() {}\nfn b() {}\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let out = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "fn b() {}", "new_string": "fn c() {}" }),
                &mut ctx,
            )
            .unwrap();
        assert!(out.changed_state);
        assert!(out.text.contains("1 处"), "{}", out.text);
        assert_eq!(t.body("a.txt"), "fn a() {}\nfn c() {}\n");
    }

    #[test]
    fn edit_file_without_reading_is_denied() {
        let t = TempDir::new("edit-unread");
        t.seed("a.txt", "abc\n");
        let mut ctx = t.ctx();
        let err = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "a", "new_string": "z" }),
                &mut ctx,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
        assert_eq!(t.body("a.txt"), "abc\n");
    }

    #[test]
    fn edit_file_zero_matches_reports_the_count() {
        let t = TempDir::new("edit-zero");
        t.seed("a.txt", "abc\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let err = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "zzz", "new_string": "y" }),
                &mut ctx,
            )
            .unwrap_err();
        match err {
            // "匹配 0 次"这句是给模型修正用的：没有它，模型只会重试同一个字符串
            ToolError::Failed { detail } => assert!(detail.contains("0 次"), "{detail}"),
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    #[test]
    fn edit_file_multiple_matches_reports_count_and_the_way_out() {
        let t = TempDir::new("edit-many");
        t.seed("a.txt", "x\nx\nx\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let err = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "x", "new_string": "y" }),
                &mut ctx,
            )
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => {
                assert!(detail.contains("3 次"), "{detail}");
                assert!(detail.contains("replace_all"), "要给出路: {detail}");
            }
            other => panic!("应是 Failed: {other:?}"),
        }
        assert_eq!(t.body("a.txt"), "x\nx\nx\n", "报错时不能改文件");
    }

    #[test]
    fn edit_file_replace_all_replaces_every_occurrence() {
        let t = TempDir::new("edit-all");
        t.seed("a.txt", "x\nx\nx\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let out = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true }),
                &mut ctx,
            )
            .unwrap();
        assert!(out.text.contains("3 处"), "{}", out.text);
        assert_eq!(t.body("a.txt"), "y\ny\ny\n");
    }

    #[test]
    fn edit_file_preserves_crlf_line_endings() {
        // 逐行重组会把整个文件的 CRLF 变成 LF：只想改一行，diff 却是全文件
        let t = TempDir::new("edit-crlf");
        t.seed("a.txt", "one\r\ntwo\r\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "two", "new_string": "three" }),
                &mut ctx,
            )
            .unwrap();
        assert_eq!(
            std::fs::read(t.path("a.txt")).unwrap(),
            b"one\r\nthree\r\n".to_vec()
        );
    }

    #[test]
    fn edit_file_rejects_an_empty_old_string() {
        // 空串在任意位置都匹配，replace_all 会把文件切成碎片
        let t = TempDir::new("edit-empty");
        t.seed("a.txt", "abc\n");
        let mut ctx = t.ctx();
        ReadFileTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap();
        let err = EditFileTool
            .call(
                &json!({ "path": "a.txt", "old_string": "", "new_string": "z" }),
                &mut ctx,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "{err:?}");
        assert_eq!(t.body("a.txt"), "abc\n");
    }

    #[test]
    fn edit_file_on_a_missing_file_points_at_write_file() {
        let t = TempDir::new("edit-missing");
        let mut ctx = t.ctx();
        let err = EditFileTool
            .call(
                &json!({ "path": "nope.txt", "old_string": "a", "new_string": "b" }),
                &mut ctx,
            )
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => assert!(detail.contains("write_file"), "{detail}"),
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    // ---- search_files ----

    #[test]
    fn search_finds_plain_substrings_with_line_numbers() {
        let t = TempDir::new("search-basic");
        t.seed("src/a.rs", "fn main() {}\nlet needle = 1;\n");
        t.seed("src/b.rs", "fn other() {}\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("needle"), "{}", out.text);
        assert!(out.text.contains("a.rs:2"), "行号要带上: {}", out.text);
        assert!(
            !out.text.contains("b.rs"),
            "没命中的文件不该出现: {}",
            out.text
        );
        assert!(!out.changed_state, "搜索不改状态");
    }

    #[test]
    fn search_skips_git_and_target() {
        // 元数据与构建产物又大又没信息量，搜它们只会让结果塞满噪音
        let t = TempDir::new("search-skips");
        t.seed(".git/config", "needle\n");
        t.seed("target/debug/x.txt", "needle\n");
        t.seed("src/a.txt", "needle\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("a.txt:1"), "{}", out.text);
        assert!(!out.text.contains(".git"), "不该搜进 .git: {}", out.text);
        assert!(
            !out.text.contains("target"),
            "不该搜进 target: {}",
            out.text
        );
    }

    #[test]
    fn search_without_hits_is_a_normal_result() {
        let t = TempDir::new("search-none");
        t.seed("src/a.txt", "hello\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "不存在的字样" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("没有找到"), "{}", out.text);
        // 模型试正则拿 0 命中时，靠这句话知道该换成明文
        assert!(out.text.contains("正则"), "{}", out.text);
    }

    #[test]
    fn search_respects_max_results_and_says_it_stopped() {
        let t = TempDir::new("search-max");
        t.seed("a.txt", "needle\nneedle\nneedle\nneedle\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle", "max_results": 2 }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("找到 2 处匹配"), "{}", out.text);
        assert!(out.text.contains("上限"), "停下必须说出来: {}", out.text);
    }

    #[test]
    fn search_skips_oversized_files_and_says_so() {
        let t = TempDir::new("search-bigfile");
        let big = format!(
            "{}\nneedle\n",
            "x".repeat(SEARCH_MAX_FILE_BYTES as usize + 10)
        );
        t.seed("big.txt", &big);
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle" }), &mut ctx)
            .unwrap();
        assert!(
            !out.text.contains("big.txt"),
            "超过 1MB 的文件不该被读: {}",
            out.text
        );
        assert!(out.text.contains("跳过"), "{}", out.text);
    }

    #[test]
    fn search_skips_binary_files_but_finds_text_ones() {
        let t = TempDir::new("search-binary");
        std::fs::write(t.path("blob.bin"), [0xff_u8, 0xfe, 0xfd, 0x00]).unwrap();
        t.seed("ok.txt", "needle\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("ok.txt:1"), "{}", out.text);
        assert!(
            !out.text.contains("blob"),
            "二进制文件不该出现在命中里: {}",
            out.text
        );
    }

    #[test]
    fn search_accepts_a_single_file_as_root() {
        let t = TempDir::new("search-file-root");
        t.seed("a.txt", "needle\n");
        t.seed("b.txt", "needle\n");
        let mut ctx = t.ctx();
        let out = SearchFilesTool
            .call(&json!({ "pattern": "needle", "path": "a.txt" }), &mut ctx)
            .unwrap();
        assert!(out.text.contains("a.txt:1"), "{}", out.text);
        assert!(
            !out.text.contains("b.txt"),
            "指定单文件时不该搜别的: {}",
            out.text
        );
    }

    // ---- list_dir ----

    #[test]
    fn list_dir_reports_kind_and_size() {
        let t = TempDir::new("list-basic");
        t.seed("src/a.txt", "abc");
        t.seed("top.txt", "hello");
        let mut ctx = t.ctx();
        let out = ListDirTool.call(&json!({}), &mut ctx).unwrap();
        assert!(out.text.contains("目录"), "{}", out.text);
        assert!(out.text.contains("文件"), "{}", out.text);
        assert!(out.text.contains("src"), "{}", out.text);
        assert!(out.text.contains("5"), "文件大小要显示: {}", out.text);
        assert!(!out.changed_state);
    }

    #[test]
    fn list_dir_skips_git() {
        let t = TempDir::new("list-git");
        t.seed(".git/config", "x");
        t.seed("a.txt", "x");
        let mut ctx = t.ctx();
        let out = ListDirTool.call(&json!({ "path": "." }), &mut ctx).unwrap();
        assert!(out.text.contains("a.txt"), "{}", out.text);
        // 结尾那行「（已跳过 .git）」里本来就写着 .git，所以这里查的是
        // **.git 里的内容没有出现**，而不是输出里没有 ".git" 这四个字符。
        assert!(
            !out.text.contains("config"),
            ".git 里的条目不该被列出: {}",
            out.text
        );
        assert!(
            out.text.contains("（1 项）"),
            "只该数到 1 个条目: {}",
            out.text
        );
    }

    #[test]
    fn list_dir_missing_path_is_failed() {
        let t = TempDir::new("list-missing");
        let mut ctx = t.ctx();
        let err = ListDirTool
            .call(&json!({ "path": "nope" }), &mut ctx)
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
    }

    #[test]
    fn list_dir_of_a_file_points_at_read_file() {
        let t = TempDir::new("list-file");
        t.seed("a.txt", "x");
        let mut ctx = t.ctx();
        let err = ListDirTool
            .call(&json!({ "path": "a.txt" }), &mut ctx)
            .unwrap_err();
        match err {
            ToolError::Failed { detail } => assert!(detail.contains("read_file"), "{detail}"),
            other => panic!("应是 Failed: {other:?}"),
        }
    }

    // ---- 元信息 ----

    #[test]
    fn capabilities_match_the_approval_model() {
        // 能力标错就是审批标错：把 write_file 标成只读，它会自动放行。
        assert_eq!(ReadFileTool.capability(), Capability::ReadOnly);
        assert_eq!(ListDirTool.capability(), Capability::ReadOnly);
        assert_eq!(SearchFilesTool.capability(), Capability::ReadOnly);
        assert_eq!(WriteFileTool.capability(), Capability::Write);
        assert_eq!(EditFileTool.capability(), Capability::Write);
    }

    #[test]
    fn names_are_stable() {
        // 名字进 schema、进审批规则、进台账：改名等于让已有规则静默失效
        assert_eq!(ReadFileTool.name(), "read_file");
        assert_eq!(ListDirTool.name(), "list_dir");
        assert_eq!(SearchFilesTool.name(), "search_files");
        assert_eq!(WriteFileTool.name(), "write_file");
        assert_eq!(EditFileTool.name(), "edit_file");
    }

    #[test]
    fn specifier_is_the_resolved_absolute_path() {
        // **粒度必须是工具实际会动的那个路径，不是模型给的那串字符。**
        //
        // 第一版返回原串，理由是"specifier 拿不到 cwd"。那个顾虑对，但结论反了：
        // 正确的修法是给签名加 ctx。返回原串会让规则静默失效——
        // 使用者写 --allow read_file:C:\work，模型给相对路径 a.txt，
        // 前缀对不上，于是每次都问，使用者以为规则坏了。
        assert_eq!(
            ReadFileTool.specifier(&json!({ "path": "a.txt" }), &spec_ctx()),
            Some(r"C:\work\a.txt".to_string()),
            "相对路径要解析成绝对路径，规则才对得上"
        );
        assert_eq!(
            ReadFileTool.specifier(&json!({ "path": "D:\\x\\a.txt" }), &spec_ctx()),
            Some("D:\\x\\a.txt".to_string()),
            "本来就是绝对路径的不要动它"
        );
        assert_eq!(
            ReadFileTool.specifier(&json!({}), &spec_ctx()),
            None,
            "没给路径就没有粒度，门禁只能每次都问"
        );
    }

    #[test]
    fn specifier_and_call_agree_on_the_same_path() {
        // 这条是上面那段注释的机器化表达：**审批看到的 = 实际动的**。
        // 两者不一致时，人是在批准一个自己没看清的东西。
        // `c` 故意不是 mut：specifier 不该改动 ctx，
        // 如果哪天需要 mut 了，说明它有了副作用，那本身就是问题。
        let c = spec_ctx();
        let args = json!({ "path": "子目录/文件.txt" });
        let spec = ReadFileTool.specifier(&args, &c).expect("应有粒度");
        // 让 call 正常返回需要文件存在；不建文件，只取它解析出的路径做对比
        let resolved = c.resolve(Path::new("子目录/文件.txt"));
        assert_eq!(spec, resolved.to_string_lossy());
        // 顺带确认 ctx 没被 specifier 改动过
        assert!(!c.has_read(&resolved), "specifier 不该有副作用");
    }

    #[test]
    fn dir_tools_without_a_path_scope_to_the_working_directory() {
        // 返回 cwd 而不是 None：否则每一次"在项目里搜一下"都要问人，
        // 而它明明什么都没改
        assert_eq!(
            SearchFilesTool.specifier(&json!({ "pattern": "x" }), &spec_ctx()),
            Some(r"C:\work".to_string())
        );
        assert_eq!(
            ListDirTool.specifier(&json!({ "path": "src" }), &spec_ctx()),
            Some(r"C:\work\src".to_string())
        );
    }

    #[test]
    fn schemas_require_the_args_that_matter() {
        assert_eq!(ReadFileTool.parameters()["required"], json!(["path"]));
        assert_eq!(
            WriteFileTool.parameters()["required"],
            json!(["path", "content"])
        );
        assert_eq!(
            EditFileTool.parameters()["required"],
            json!(["path", "old_string", "new_string"])
        );
        assert_eq!(SearchFilesTool.parameters()["required"], json!(["pattern"]));
        // list_dir 的 path 可省：不传就是工作目录
        assert!(ListDirTool.parameters().get("required").is_none());
    }
}
