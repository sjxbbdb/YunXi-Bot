//! 系统类工具：时间、执行命令、问人。
//!
//! ## `now` 为什么是第一个该做的工具
//!
//! 常驻 Agent 不知道"今天几号"，就没法处理任何含时间的任务——
//! "明天提醒我"、"这周的邮件"、"三天后"全都落不了地。
//! 而且它**纯本地、零成本、无副作用**，是性价比最高的一个。
//!
//! ## `run_command` 已经有实现，只差接成工具
//!
//! [`crate::exec::run_command`] 已经做好了该做的所有事：
//! **不经 shell 解析**、隔离级别、凭证剥离（通配规则而非穷举清单）、
//! 输出上限、**进程树清理**（只杀直接子进程不够，孙子会继续占着句柄和端口）。
//!
//! 接成工具时要做的只有：参数校验、能力标成 [`Capability::Execute`]、
//! 把 `ExecOutcome` 转成 [`ToolOutput`]。**不要重新实现执行逻辑。**
//!
//! ## `ask_user` 是最重要的一条退路
//!
//! D18 定了步骤产出契约：模型必须以 `OK:` 或 `BLOCKED:` 开头。
//! 但在那之前，模型缺信息时**唯一正确的动作是问，而不是编**——
//! 而在此之前它没有这个选项，只能二选一：编一个像样的答案，或者放弃。
//! `ask_user` 补上的正是这条路。
//!
//! 能力标 [`Capability::ReadOnly`]：问人不改变任何系统状态。
//!
//! ## 三个工具都不做审批判断
//!
//! "这个动作要不要先问人"是 [`super::gate`] 的事。工具里再判一遍，等于同一个
//! 判定有两份实现，早晚会不一致——而不一致的那一次就是一个洞。

use std::path::Path;
use std::str::FromStr;

use chrono::{Datelike, FixedOffset, Local, Weekday};
use serde_json::{Value, json};

use super::{Capability, Tool, ToolContext, ToolError, ToolOutput};
use crate::exec::{ExecOptions, ExecOutcome, IsolationRequirement};

/// 没有外部粒度、纯本地只读的工具用的常量 specifier。
///
/// **为什么不用 `None`**：`gate` 对 `specifier == None` 的工具会一路走到
/// "问人"那一层，而 `now` 和 `ask_user` 恰恰是**没人可问时最需要能跑**的两个
/// 工具——一个是常驻 Agent 的时间基准，一个是缺信息时唯一的退路。
/// 让 `now` 每次都要人批准一次"读一下时钟"，或者让 `ask_user` 先批准
/// "我能不能问你一句"，都是明确更坏的结果。
///
/// 常量 `local` 命中既有的"只读 + 落在工作区内 → 免问"规则（相对路径按
/// 工作目录解析，因而恒在工作区内），同时仍然给审批规则留了一个可写的粒度：
/// `Rule::scoped("now", "local")`、deny 也照样优先。取舍是这里**借用了路径
/// 判定**，语义上它不是路径；要根治得让 `gate` 认识"无粒度的纯本地只读"
/// 这个类别，但那不在本文件的范围内。这个常量只给真正无副作用的本地工具用，
/// 会改状态或出网的工具拿它放行就是绕过门禁。
const LOCAL_SCOPE: &str = "local";

/// 参数回显的最大字符数。
///
/// 模型可能把整个文件塞进参数里；`BadArgs` 必须带上它实际发来的参数（否则
/// 没法排查它给错了什么），但不能因为回显把报错本身撑爆。
const ECHO_MAX_CHARS: usize = 240;

/// argv 回显的最大字符数。
///
/// 这里比 [`ECHO_MAX_CHARS`] 宽松：模型需要看清自己到底跑了什么，但同样不能
/// 没有上限——一个参数里塞进整段脚本是常态。
const ARGV_MAX_CHARS: usize = 600;

/// 默认超时：与 [`ExecOptions::default`] 一致（60 秒）。
///
/// 显式写出来而不是隐式继承默认值：这个数字会同时出现在给模型看的工具说明
/// 和报错里，两处必须同源，否则改了一处就会互相矛盾。
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// 单次工具调用的超时上限（10 分钟）。
///
/// 常驻进程里一次同步工具调用不该霸占主循环十几分钟，更久的活应该交给 job
/// 调度去后台跑。超过上限**直接拒绝**而不是悄悄截断——静默截断会让模型以为
/// 它拿到了那么长的窗口，超时后又误判成"命令有问题"。
const MAX_TIMEOUT_MS: u64 = 600_000;

// ---------------------------------------------------------------- now

/// 现在几点。
///
/// 无参数、零副作用，返回本地日期时间、星期几、时区偏移与 Unix 时间戳。
pub struct NowTool;

impl Tool for NowTool {
    fn name(&self) -> &str {
        "now"
    }

    fn description(&self) -> &str {
        "取当前的日期与时间。任何涉及“今天 / 明天 / 这周 / 三天后”的任务都先调它——\
         常驻进程自己不知道今天是几号，凭印象算日期一定会错。\
         返回本地日期时间、星期几、时区偏移和 Unix 时间戳，纯本地、无副作用、不花钱。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "可选。目前只认固定偏移写法，如 \"+08:00\"、\"-05:00\"；\
                                    IANA 时区名（Asia/Shanghai）暂不支持，给了不会报错，\
                                    结果里会说明已忽略、用的是本机本地时区。"
                }
            },
            "required": []
        })
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn specifier(&self, _args: &Value) -> Option<String> {
        Some(LOCAL_SCOPE.to_string())
    }

    fn call(&self, args: &Value, _ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let local = Local::now().fixed_offset();

        // 时区参数只影响展示用的换算，不影响 Unix 时间戳。
        let requested = optional_string(args, "timezone")?;
        let (dt, zone_note) = match requested.as_deref() {
            None => (local, "本机本地时区".to_string()),
            Some(raw) => match FixedOffset::from_str(raw.trim()) {
                Ok(offset) => (
                    local.with_timezone(&offset),
                    format!("按 timezone 参数 {} 换算", head(raw, 32)),
                ),
                // 不报错，也不假装换算过：直接说明"已忽略"。
                // 静默降级比失败更坏——模型会以为自己拿到的是那个时区的时间。
                Err(_) => (
                    local,
                    format!(
                        "本机本地时区；timezone 参数 {:?} 暂不支持，已忽略\
                         （只认 +08:00 这类固定偏移，IANA 时区名需要额外的时区库）",
                        head(raw, 32)
                    ),
                ),
            },
        };

        let text = [
            format!(
                "日期时间：{}（{}）",
                dt.format("%Y-%m-%d %H:%M:%S"),
                weekday_cn(dt.weekday())
            ),
            format!("时区：UTC{}（{}）", dt.offset(), zone_note),
            format!("Unix 时间戳：{}", dt.timestamp()),
        ]
        .join("\n");

        // 只读、不改任何状态，所以不是 changed：这条结论可以放心长期记住。
        Ok(ToolOutput::read(text))
    }
}

/// 星期几的中文写法。
///
/// 不用 `%A`：它给的是 "Sunday"，会和其他工具返回的中文不一致；而且它依赖
/// locale，没设 locale 时各平台行为并不统一。
fn weekday_cn(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "星期一",
        Weekday::Tue => "星期二",
        Weekday::Wed => "星期三",
        Weekday::Thu => "星期四",
        Weekday::Fri => "星期五",
        Weekday::Sat => "星期六",
        Weekday::Sun => "星期日",
    }
}

// -------------------------------------------------------- run_command

/// 跑一条外部命令。
///
/// 执行本身交给 [`crate::exec::run_command`]，这里只负责参数校验与结果渲染。
pub struct RunCommandTool;

impl Tool for RunCommandTool {
    fn name(&self) -> &str {
        "run_command"
    }

    fn description(&self) -> &str {
        "运行一条外部命令，取回退出码、stdout 和 stderr。\
         command 必须是 argv 数组（[\"cargo\",\"test\"]），不是一整条命令行字符串——\
         本工具不经 shell 解析，管道、重定向、通配符、变量展开、&& 都不会生效；\
         确实需要它们时请显式起 shell（Windows：[\"cmd\",\"/C\",\"…\"]，\
         Unix：[\"sh\",\"-c\",\"…\"]），并明白那等于把注入面重新打开。\
         执行命令可能改变系统状态，因此会先过审批门禁。\
         可能跑很久的命令请设 timeout_ms，超时会连整个进程树一起终止。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "argv 数组：第 0 项是可执行文件（名字或路径），其余是参数。\
                                    例：[\"git\",\"status\",\"--short\"]。不接受单个字符串。"
                },
                "cwd": {
                    "type": "string",
                    "description": "可选的工作目录。相对路径按当前工作目录解析；默认就是当前工作目录。"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "可选超时，单位毫秒，默认 60000，上限 600000。超时会杀掉整个进程树。"
                }
            },
            "required": ["command"]
        })
    }

    fn capability(&self) -> Capability {
        Capability::Execute
    }

    /// 粒度是**程序名**（`command[0]`），不是整条命令。
    ///
    /// 取舍：按整条命令记，等于每次参数一变就要重新问一遍，"总是允许"这条规则
    /// 就废了；按程序名记才能写出"总是允许 `git`"这种使用者真想说的话。
    /// 代价是它连带放行了该程序的**任何**子命令——允许 `git` 就等于允许
    /// `git push` 和 `git reset --hard`。再往细就得解析子命令，而那既不可靠
    /// （别名、`-C`、`--exec-path`、`!` 前缀都能绕过）又会让规则形同虚设。
    fn specifier(&self, args: &Value) -> Option<String> {
        let first = args.get("command")?.as_array()?.first()?.as_str()?.trim();
        // 参数不合法时**不给粒度**：报一个假的粒度比没有粒度更坏，
        // 那会让规则看起来命中了一个其实没发生过的调用。
        if first.is_empty() {
            return None;
        }
        Some(first.to_string())
    }

    fn call(&self, args: &Value, ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let command = command_arg(args)?;

        let cwd = match optional_string(args, "cwd")? {
            Some(raw) if !raw.trim().is_empty() => {
                let resolved = ctx.resolve(Path::new(&raw));
                // 提前判一次，比让子进程派生失败后抛一句系统错误码清楚得多。
                if !resolved.is_dir() {
                    return Err(ToolError::Failed {
                        detail: format!("工作目录不存在或不是目录：{}", resolved.display()),
                    });
                }
                resolved
            }
            // 空字符串等于没给，不必当成错误。
            _ => ctx.cwd.clone(),
        };

        let timeout_ms = timeout_arg(args)?;

        let opts = ExecOptions {
            cwd,
            timeout_ms,
            // **不要求** OS 级写入隔离：一是不属于工具层该做的判断（要不要跑、
            // 跑在什么隔离里是策略层的事），二是该要求的语义是"拿不到就拒绝
            // 执行"，而非 Windows 平台根本提供不了这个级别，那会让本工具在
            // 那些平台上整个不可用。实际达到的隔离级别由 exec 如实返回，并写进
            // 下面回灌给模型的文本里——绝不把进程内策略说成沙箱。
            isolation: IsolationRequirement::ProcessOnly,
            // 资源上限留给策略配置。工具层擅自设限，是另一种"静默降级"：
            // 使用者没要求过，命令却因为一个它不知道的限制被杀掉。
            memory_limit_bytes: None,
            max_processes: None,
        };

        match crate::exec::run_command(&command, &opts) {
            Ok(out) => Ok(ToolOutput::changed(render_outcome(
                &command, &opts.cwd, timeout_ms, &out,
            ))),
            // 派生失败、隔离级别拿不到——都是"这个工具这次做不到"，
            // 属于正常结果：回灌给模型，让它换条路，而不是让整个任务失败。
            Err(e) => Err(ToolError::Failed {
                detail: format!("{}（命令：{}）", e, display_argv(&command)),
            }),
        }
    }
}

/// 校验并取出 `command` 参数。
fn command_arg(args: &Value) -> Result<Vec<String>, ToolError> {
    let Some(raw) = args.get("command") else {
        return Err(ToolError::BadArgs {
            detail: format!(
                "缺少必填参数 command（argv 数组，如 [\"git\",\"status\"]）。本次参数：{}",
                echo(args)
            ),
        });
    };

    match raw {
        // 单个字符串**必须拒掉**：它的旁边就是"那就调个 shell 解析一下吧"，
        // 而 shell 解析正是本工具要消灭的注入面。模型改成数组只是多打几个字。
        Value::String(s) => Err(ToolError::BadArgs {
            detail: format!(
                "command 必须是 argv 数组，不是单个字符串。收到的是 {:?}。\
                 请自己按 argv 切分（引号里的空格算同一个参数），例如 [\"git\",\"status\"]；\
                 传一整条命令行字符串只能被当成一个可执行文件名，那样必然派生失败。\
                 本次参数：{}",
                head(s, ECHO_MAX_CHARS),
                echo(args)
            ),
        }),
        Value::Array(items) => {
            let mut command = Vec::with_capacity(items.len());
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(ToolError::BadArgs {
                        detail: format!(
                            "command 数组的每一项都必须是字符串，收到了一项 {}。本次参数：{}",
                            kind_of(item),
                            echo(args)
                        ),
                    });
                };
                command.push(s.to_string());
            }
            if command.is_empty() {
                return Err(ToolError::BadArgs {
                    detail: format!(
                        "command 不能是空数组：没有程序可执行。本次参数：{}",
                        echo(args)
                    ),
                });
            }
            if command[0].trim().is_empty() {
                return Err(ToolError::BadArgs {
                    detail: format!("command[0] 是程序名，不能为空。本次参数：{}", echo(args)),
                });
            }
            Ok(command)
        }
        other => Err(ToolError::BadArgs {
            detail: format!(
                "command 必须是字符串数组，收到的是 {}。本次参数：{}",
                kind_of(other),
                echo(args)
            ),
        }),
    }
}

/// 校验并取出 `timeout_ms` 参数。
fn timeout_arg(args: &Value) -> Result<u64, ToolError> {
    let Some(raw) = args.get("timeout_ms") else {
        return Ok(DEFAULT_TIMEOUT_MS);
    };
    if raw.is_null() {
        return Ok(DEFAULT_TIMEOUT_MS);
    }
    let Some(ms) = raw.as_u64() else {
        return Err(ToolError::BadArgs {
            detail: format!(
                "timeout_ms 必须是正整数毫秒数，收到的是 {}。本次参数：{}",
                kind_of(raw),
                echo(args)
            ),
        });
    };
    if ms == 0 {
        // 0 很容易被理解成"不设超时"，而 Duration 的语义是"立刻超时"。
        // 两种理解都不能接受，所以拒掉而不是猜。
        return Err(ToolError::BadArgs {
            detail: format!(
                "timeout_ms 必须大于 0（0 既可能被理解成不设超时、又会被理解成立刻超时，所以不接受）。\
                 本次参数：{}",
                echo(args)
            ),
        });
    }
    if ms > MAX_TIMEOUT_MS {
        return Err(ToolError::BadArgs {
            detail: format!(
                "timeout_ms={ms} 超过单次调用上限 {MAX_TIMEOUT_MS}ms。\
                 常驻进程不能被一次工具调用挂住太久；更久的活请拆成几步，或交给后台任务调度。\
                 本次参数：{}",
                echo(args)
            ),
        });
    }
    Ok(ms)
}

/// 把执行结果渲染成给模型看的文本。
///
/// 三种结局（成功 / 非零退出 / 超时）必须一眼可辨：模型下一步做什么完全取决于
/// 它——成功就用输出，失败要换条路，超时要缩小范围或加长超时。
fn render_outcome(command: &[String], cwd: &Path, timeout_ms: u64, out: &ExecOutcome) -> String {
    let mut lines = Vec::new();
    lines.push(if out.timed_out {
        format!("命令超时（timeout_ms={timeout_ms}），整个进程树已被强制终止，退出码不可用。")
    } else if out.exit_code == 0 {
        "命令成功，退出码 0。".to_string()
    } else {
        format!("命令失败，退出码 {}。", out.exit_code)
    });
    // 回显实际执行的 argv 和目录：模型对"我到底跑了什么"的误解，
    // 十有八九能在这两行里当场解开。
    lines.push(format!("argv：{}", display_argv(command)));
    lines.push(format!("工作目录：{}", cwd.display()));
    lines.push(format!(
        "用时 {}ms；实际隔离级别：{}",
        out.duration_ms,
        out.isolation.describe()
    ));
    lines.push("stdout：".to_string());
    lines.push(section(&out.stdout));
    lines.push("stderr：".to_string());
    lines.push(section(&out.stderr));
    lines.join("\n")
}

/// 一段输出。空的显式写"（空）"。
///
/// 为什么要显式写：留白和"被截断 / 没写"分不清，模型可能据此编出一个
/// 根本不存在的错误信息。只去掉尾部空白，内容一字不改。
fn section(s: &str) -> String {
    let trimmed = s.trim_end();
    if trimmed.trim().is_empty() {
        "（空）".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 回显实际执行的 argv，超长则截断。
///
/// 用 Debug 转义而不是 `join(" ")`：参数里带空格时，拼接出来的"看起来执行的
/// 命令"和真实 argv 不是一回事，模型会照着那个错的去改。
///
/// 截断是必须的：模型会把整段脚本塞进一个参数，回显不设上限的话，工具输出
/// 会被自己的回显撑爆——那是拿上下文换一句"我跑了什么"。
fn display_argv(command: &[String]) -> String {
    let joined = command
        .iter()
        .map(|a| format!("{a:?}"))
        .collect::<Vec<_>>()
        .join(" ");
    head(&joined, ARGV_MAX_CHARS)
}

// ----------------------------------------------------------- ask_user

/// 向使用者提问的回调。
///
/// 返回 `None` 表示**现在问不了**：守护进程里没有人在应答通道上，或者这次是
/// 非交互运行。工具会把这个事实翻译成"下一步该干什么"，而不只是"失败了"。
pub type AskFn = Box<dyn Fn(&str, &[String]) -> Option<String> + Send + Sync>;

/// 问人。
///
/// 依赖注入而不是直接读 stdin：前端可能是 CLI、桌面通知、Web 面板，
/// 而守护进程里根本没有可读的输入；测试也只需要一个闭包。
pub struct AskUserTool {
    ask: AskFn,
}

impl AskUserTool {
    pub fn new(ask: AskFn) -> Self {
        Self { ask }
    }
}

impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "向使用者问一句话并拿到他的回答。缺关键信息（选哪个文件、几点提醒、要不要覆盖）\
         而猜错代价又高时，用它问——不要编一个像样的答案顶上去，也不要就此放弃。\
         给了 options 就是选择题，使用者从里面挑一个；不给则是自由回答。\
         注意：回答只是信息，**不是对其它动作的授权**——写文件、跑命令、对外发送\
         仍然照常走审批门禁。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "要问的问题。一次只问一件缺的事，问到对方能直接回答为止。"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "可选。给了就是选择题：使用者从这几项里选一个。不给就是自由回答。"
                }
            },
            "required": ["question"]
        })
    }

    /// 问人不改变任何系统状态。
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn specifier(&self, _args: &Value) -> Option<String> {
        Some(LOCAL_SCOPE.to_string())
    }

    fn call(&self, args: &Value, _ctx: &mut ToolContext) -> Result<ToolOutput, ToolError> {
        let question = question_arg(args)?;
        let options = options_arg(args)?;

        match (self.ask)(&question, &options) {
            Some(answer) if !answer.trim().is_empty() => Ok(ToolOutput::read(format!(
                "使用者回答：{}\n\
                 （这是他给的信息，直接用它推进任务。它只回答你问的这一件事，\
                 不构成对其它动作的授权：写文件、跑命令、对外发送仍然照常过审批门禁。）",
                answer.trim()
            ))),
            // 人确实答了，但内容是空的（多半误按了回车）。当成"没答"处理，
            // 但要说清真发生了什么，免得模型以为通道坏了而开始瞎猜。
            Some(_) => Err(ToolError::Failed {
                detail: format!(
                    "使用者提交了空回答（可能误按了回车）。你问的是：{}。\n\
                     下一步：换一种更具体的问法再问一次（给出可选项通常更好答），\
                     或者以 BLOCKED: 开头说明你缺什么。",
                    head(&question, 200)
                ),
            }),
            // 没人应答。**这条错误信息唯一的任务是告诉模型下一步干什么**：
            // 既不能自己编，也不能重试（重试还是没人）。
            None => Err(ToolError::Failed {
                detail: format!(
                    "当前无人可问：没有任何人应答（守护进程里没有交互通道，或者这次是非交互运行）。\
                     你问的是：{}。\n\
                     下一步：不要自己编一个答案顶上去，也不要原样重试（重试还是没人应答）。\
                     请以 BLOCKED: 开头说明你缺哪条信息、为什么没有它就没法继续——\
                     使用者下次打开会话时能直接补上。",
                    head(&question, 200)
                ),
            }),
        }
    }
}

/// 校验并取出必填的 `question`。
fn question_arg(args: &Value) -> Result<String, ToolError> {
    let Some(raw) = args.get("question") else {
        return Err(ToolError::BadArgs {
            detail: format!("缺少必填参数 question。本次参数：{}", echo(args)),
        });
    };
    match raw {
        Value::String(s) if !s.trim().is_empty() => Ok(s.clone()),
        Value::String(_) => Err(ToolError::BadArgs {
            detail: format!(
                "question 不能是空字符串：空问题只会浪费使用者一次注意力。本次参数：{}",
                echo(args)
            ),
        }),
        other => Err(ToolError::BadArgs {
            detail: format!(
                "question 必须是字符串，收到的是 {}。本次参数：{}",
                kind_of(other),
                echo(args)
            ),
        }),
    }
}

/// 校验并取出可选的 `options`。
fn options_arg(args: &Value) -> Result<Vec<String>, ToolError> {
    let Some(raw) = args.get("options") else {
        return Ok(Vec::new());
    };
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let Value::Array(items) = raw else {
        return Err(ToolError::BadArgs {
            detail: format!(
                "options 必须是字符串数组，收到的是 {}。本次参数：{}",
                kind_of(raw),
                echo(args)
            ),
        });
    };
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let Some(s) = item.as_str() else {
            return Err(ToolError::BadArgs {
                detail: format!(
                    "options 的每一项都必须是字符串，收到了一项 {}。本次参数：{}",
                    kind_of(item),
                    echo(args)
                ),
            });
        };
        options.push(s.to_string());
    }
    // 空数组等于"没有选项"，按自由回答处理：这不是错误，
    // 模型把可选字段填成 [] 很常见，拒掉只会白费一轮往返。
    Ok(options)
}

// ------------------------------------------------------------- 共用

/// 取一个可选字符串参数。
///
/// `null` 视为"没给"：模型经常把可选字段显式填成 null，把它当类型错误拒掉
/// 只是白费一轮往返。给了别的类型才是真错误。
fn optional_string(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(ToolError::BadArgs {
            detail: format!(
                "{key} 必须是字符串，收到的是 {}。本次参数：{}",
                kind_of(other),
                echo(args)
            ),
        }),
    }
}

/// 参数回显。所有 `BadArgs` 都要带上它——否则没法排查模型给错了什么。
fn echo(args: &Value) -> String {
    head(&args.to_string(), ECHO_MAX_CHARS)
}

/// 按字符（不是字节）截断，避免把一个多字节字符劈成两半。
fn head(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// JSON 值类型的中文名，用在"你给错了类型"的报错里。
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxMode;
    use crate::tool::{Rule, ToolPolicy, gate};

    /// 测试用的执行上下文：工作目录取系统临时目录（绝对路径，跨平台都有）。
    fn tool_ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), SandboxMode::WorkspaceWrite)
    }

    /// 本机一定有、且**无害**的 echo 命令。
    ///
    /// Windows 上 echo 是 cmd 的内建命令，必须经 `cmd /C` 才能跑；
    /// 两个平台都不碰文件、不联网、瞬间返回。
    #[cfg(windows)]
    fn echo_argv(text: &str) -> Vec<String> {
        vec!["cmd".into(), "/C".into(), format!("echo {text}")]
    }
    #[cfg(not(windows))]
    fn echo_argv(text: &str) -> Vec<String> {
        vec!["echo".into(), text.into()]
    }

    /// 以指定退出码结束的无害命令。
    #[cfg(windows)]
    fn exit_argv(code: i32) -> Vec<String> {
        vec!["cmd".into(), "/C".into(), format!("exit {code}")]
    }
    #[cfg(not(windows))]
    fn exit_argv(code: i32) -> Vec<String> {
        vec!["sh".into(), "-c".into(), format!("exit {code}")]
    }

    // ---- now ----

    #[test]
    fn now_returns_a_date_with_the_current_year() {
        let out = NowTool
            .call(&json!({}), &mut tool_ctx())
            .expect("now 不该失败");
        let year = Local::now().format("%Y").to_string();
        assert!(out.text.contains(&year), "结果里要有当前年份：{}", out.text);
        assert!(out.text.contains("星期"), "结果里要有星期几：{}", out.text);
        assert!(out.text.contains("时区"), "结果里要有时间：{}", out.text);
        // 只读结论可以永久记住，所以不能标成 changed
        assert!(!out.changed_state);
    }

    #[test]
    fn now_reports_a_unix_timestamp_that_is_actually_now() {
        // 时间戳要和真实时间对得上：不能是 0（AGENTS §2.7 那个坑），
        // 也不能是某个看似合理的常数。
        let before = chrono::Utc::now().timestamp();
        let out = NowTool
            .call(&json!({}), &mut tool_ctx())
            .expect("now 不该失败");
        let after = chrono::Utc::now().timestamp();

        let ts: i64 = out
            .text
            .lines()
            .find_map(|l| l.strip_prefix("Unix 时间戳："))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("结果里应有可解析的 Unix 时间戳：{}", out.text));
        assert!(
            (before..=after).contains(&ts),
            "时间戳 {ts} 不在 [{before}, {after}] 内"
        );
    }

    #[test]
    fn now_converts_a_fixed_offset() {
        let out = NowTool
            .call(&json!({ "timezone": "+09:00" }), &mut tool_ctx())
            .expect("now 不该失败");
        assert!(out.text.contains("UTC+09:00"), "{}", out.text);
        assert!(out.text.contains("+09:00"), "{}", out.text);
    }

    #[test]
    fn now_says_when_it_ignored_an_unsupported_timezone() {
        // 暂不支持 IANA 名（core 没引时区库）。**不报错，也不假装换算过**：
        // 静默降级会让模型以为拿到的是那个时区的时间。
        let out = NowTool
            .call(&json!({ "timezone": "Asia/Shanghai" }), &mut tool_ctx())
            .expect("不支持的时区名不该让工具失败");
        assert!(out.text.contains("Asia/Shanghai"), "{}", out.text);
        assert!(out.text.contains("已忽略"), "{}", out.text);
    }

    #[test]
    fn now_rejects_a_non_string_timezone() {
        let err = NowTool
            .call(&json!({ "timezone": 8 }), &mut tool_ctx())
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "{err:?}");
    }

    // ---- run_command ----

    #[test]
    fn run_command_runs_an_argv_array() {
        let out = RunCommandTool
            .call(
                &json!({ "command": echo_argv("hi-from-yunxi") }),
                &mut tool_ctx(),
            )
            .expect("echo 应能跑起来");
        assert!(out.text.contains("hi-from-yunxi"), "{}", out.text);
        assert!(out.text.contains("退出码 0"), "{}", out.text);
        // 执行命令可能改状态，**保守假设它改了**：这样"上次允许过"不会被
        // 记成永久的只读结论。
        assert!(out.changed_state);
    }

    #[test]
    fn run_command_rejects_a_single_string_command() {
        // 字符串形式必须被拒：它旁边就是"要不要调 shell 解析"，而 shell
        // 解析正是本工具要消灭的注入面。
        let err = RunCommandTool
            .call(&json!({ "command": "git status" }), &mut tool_ctx())
            .unwrap_err();
        match err {
            ToolError::BadArgs { detail } => {
                assert!(detail.contains("数组"), "要说清该给什么形式：{detail}");
                assert!(
                    detail.contains("git status"),
                    "要回显实际收到的参数：{detail}"
                );
            }
            other => panic!("字符串命令应被拒为 BadArgs，实际是 {other:?}"),
        }
    }

    #[test]
    fn run_command_rejects_an_empty_command() {
        let mut ctx = tool_ctx();
        let empty = RunCommandTool
            .call(&json!({ "command": [] }), &mut ctx)
            .unwrap_err();
        assert!(matches!(empty, ToolError::BadArgs { .. }), "{empty:?}");

        let missing = RunCommandTool.call(&json!({}), &mut ctx).unwrap_err();
        assert!(matches!(missing, ToolError::BadArgs { .. }), "{missing:?}");

        let non_string = RunCommandTool
            .call(&json!({ "command": ["echo", 3] }), &mut ctx)
            .unwrap_err();
        assert!(
            matches!(non_string, ToolError::BadArgs { .. }),
            "{non_string:?}"
        );
    }

    #[test]
    fn run_command_reports_a_nonzero_exit_code() {
        let out = RunCommandTool
            .call(&json!({ "command": exit_argv(7) }), &mut tool_ctx())
            .expect("退出码非零不是工具失败，是正常结果");
        assert!(
            out.text.contains("退出码 7"),
            "退出码要如实报告：{}",
            out.text
        );
        // 没有输出时也要明说，别让模型把留白当成"信息被吞了"
        assert!(out.text.contains("（空）"), "{}", out.text);
    }

    #[test]
    fn run_command_rejects_an_impossible_timeout() {
        let mut ctx = tool_ctx();
        for bad in [0_u64, MAX_TIMEOUT_MS + 1] {
            let err = RunCommandTool
                .call(
                    &json!({ "command": echo_argv("hi"), "timeout_ms": bad }),
                    &mut ctx,
                )
                .unwrap_err();
            assert!(matches!(err, ToolError::BadArgs { .. }), "{bad}: {err:?}");
        }
    }

    #[test]
    fn run_command_reports_a_missing_cwd_before_spawning() {
        let missing =
            std::env::temp_dir().join(format!("yunxi-no-such-dir-{}", std::process::id()));
        let err = RunCommandTool
            .call(
                &json!({ "command": echo_argv("hi"), "cwd": missing.to_string_lossy() }),
                &mut tool_ctx(),
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");
        assert!(err.to_string().contains("工作目录"), "{err}");
    }

    #[test]
    fn run_command_specifier_is_the_program_name() {
        // 有了程序名这一级粒度，使用者才写得出"总是允许 git"。
        assert_eq!(
            RunCommandTool
                .specifier(&json!({ "command": ["git", "push", "--force"] }))
                .as_deref(),
            Some("git")
        );
        // 参数不合法时不给粒度：宁可每次都问，也不要报一个假的粒度。
        assert_eq!(
            RunCommandTool.specifier(&json!({ "command": "git push" })),
            None
        );
        assert_eq!(RunCommandTool.specifier(&json!({ "command": [] })), None);
        assert_eq!(RunCommandTool.specifier(&json!({})), None);
    }

    #[test]
    fn run_command_never_parses_a_shell_string() {
        // 一条"看起来像 shell"的参数必须原样进 argv：它会被当成一个普通的
        // 参数，而不是被展开。这条性质就是注入面的边界本身。
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo a && echo b".to_string(),
        ];
        let spec = RunCommandTool.specifier(&json!({ "command": argv }));
        assert_eq!(spec.as_deref(), Some("cmd"));
        // argv 回显用的是转义形式，不是 join(" ")——后者会把带空格的参数
        // 显示成一个根本不存在的命令行。
        assert_eq!(
            display_argv(&["echo".into(), "two words".into()]),
            r#""echo" "two words""#
        );
    }

    // ---- ask_user ----

    #[test]
    fn ask_user_returns_the_answer() {
        let tool = AskUserTool::new(Box::new(|question: &str, options: &[String]| {
            assert_eq!(question, "几点提醒你？");
            assert_eq!(options.len(), 2);
            assert_eq!(options[0], "9 点");
            Some("9 点".to_string())
        }));
        let out = tool
            .call(
                &json!({ "question": "几点提醒你？", "options": ["9 点", "18 点"] }),
                &mut tool_ctx(),
            )
            .expect("拿到答案就该成功");
        assert!(out.text.contains("9 点"), "{}", out.text);
        // 问人不改状态
        assert!(!out.changed_state);
    }

    #[test]
    fn ask_user_works_without_options() {
        // 自由回答：没有 options 时给回调一个空切片，而不是让工具自己造选项。
        let tool = AskUserTool::new(Box::new(|_q: &str, options: &[String]| {
            assert!(options.is_empty());
            Some("我的仓库叫 yunxi".to_string())
        }));
        let out = tool
            .call(&json!({ "question": "你的仓库叫什么？" }), &mut tool_ctx())
            .expect("自由回答也该成功");
        assert!(out.text.contains("yunxi"), "{}", out.text);
    }

    #[test]
    fn ask_user_without_a_human_says_what_to_do_next() {
        // 这条是最重要的失败路径：守护进程里没人应答。
        // 只说"失败了"等于把模型推回"编一个答案"或"放弃"这两条老路。
        let tool = AskUserTool::new(Box::new(|_q: &str, _o: &[String]| None));
        let err = tool
            .call(&json!({ "question": "你的仓库叫什么？" }), &mut tool_ctx())
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "{err:?}");

        let msg = err.to_string();
        assert!(
            msg.contains("BLOCKED:"),
            "要告诉模型下一步用 BLOCKED：{msg}"
        );
        assert!(
            msg.contains("你的仓库叫什么？"),
            "问题要留在记录里，使用者下次能补答：{msg}"
        );
        assert!(
            msg.contains("不要自己编"),
            "最坏的结局是模型自己编一个答案顶上去：{msg}"
        );
    }

    #[test]
    fn ask_user_reports_an_empty_answer() {
        // 人确实答了，但内容是空的。既不能当成"没人应答"，也不能把空字符串
        // 当成一个答案回灌给模型（那等于让模型对着空气继续推理）。
        let tool = AskUserTool::new(Box::new(|_q: &str, _o: &[String]| Some("   ".to_string())));
        let err = tool
            .call(&json!({ "question": "几点？" }), &mut tool_ctx())
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("空回答"), "{msg}");
        assert!(msg.contains("BLOCKED:"), "{msg}");
    }

    #[test]
    fn ask_user_rejects_a_bad_question_or_options() {
        let mut ctx = tool_ctx();
        let tool = AskUserTool::new(Box::new(|_q: &str, _o: &[String]| Some("x".into())));

        for bad in [
            json!({}),
            json!({ "question": "" }),
            json!({ "question": 42 }),
        ] {
            let err = tool.call(&bad, &mut ctx).unwrap_err();
            assert!(matches!(err, ToolError::BadArgs { .. }), "{bad}: {err:?}");
        }

        let err = tool
            .call(
                &json!({ "question": "选一个", "options": ["a", 2] }),
                &mut ctx,
            )
            .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs { .. }), "{err:?}");
    }

    #[test]
    fn ask_user_treats_an_empty_option_list_as_free_form() {
        // `"options": []` 和 `"options": null` 都是"没有选项"，不是错误：
        // 把可选字段填成空值很常见，拒掉只是白费一轮往返。
        let tool = AskUserTool::new(Box::new(|_q: &str, options: &[String]| {
            assert!(options.is_empty());
            Some("自由回答".to_string())
        }));
        let mut ctx = tool_ctx();
        for args in [
            json!({ "question": "随便说说", "options": [] }),
            json!({ "question": "随便说说", "options": null }),
        ] {
            let out = tool.call(&args, &mut ctx).expect("空选项应可接受");
            assert!(out.text.contains("自由回答"), "{}", out.text);
        }
    }

    // ---- 与审批门禁的配合 ----

    #[test]
    fn purely_local_tools_do_not_wait_for_a_human() {
        // `now` 和 `ask_user` 是"没人可问时最需要能跑"的两个工具：一个是时间
        // 基准，一个是缺信息时唯一的退路。默认策略下它们必须免问——否则
        // ask_user 会变成"先批准我问你一句"，而守护进程里连批准的人都没有。
        let ctx = tool_ctx();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(NowTool),
            Box::new(AskUserTool::new(Box::new(|_q: &str, _o: &[String]| {
                Some("ok".into())
            }))),
        ];
        for tool in &tools {
            let decision = gate(
                tool.as_ref(),
                &json!({}),
                &ToolPolicy::default(),
                &ctx,
                None,
            );
            assert!(
                decision.is_allow(),
                "{} 是纯本地只读，默认策略下该免问：{decision:?}",
                tool.name()
            );
        }

        // 但粒度仍然是可以写规则的：deny 优先于自动放行。
        let denied = ToolPolicy {
            allow: Vec::new(),
            deny: vec![Rule::scoped("ask_user", LOCAL_SCOPE)],
            inert_inside_cwd_is_free: true,
        };
        let decision = gate(tools[1].as_ref(), &json!({}), &denied, &ctx, None);
        assert!(decision.is_deny(), "deny 必须压过自动放行：{decision:?}");
    }

    #[test]
    fn run_command_is_never_auto_approved_by_being_readonly() {
        // 执行命令不是只读：默认策略下它必须走到决策层或问人。
        let decision = gate(
            &RunCommandTool,
            &json!({ "command": ["git", "status"] }),
            &ToolPolicy::default(),
            &tool_ctx(),
            None,
        );
        assert!(!decision.is_allow(), "执行命令不该免问：{decision:?}");
    }

    // ---- 小工具 ----

    #[test]
    fn head_truncates_by_chars_not_bytes() {
        // 按字节截断会把多字节字符劈成两半，拼出无效的 UTF-8。
        let s = "中文参数".repeat(100);
        let cut = head(&s, 10);
        assert_eq!(cut.chars().count(), 11, "10 个字符加一个省略号");
        assert!(cut.ends_with('…'));
        assert_eq!(head("短", 10), "短");
    }

    #[test]
    fn bad_args_echo_what_the_model_actually_sent() {
        // 不回显参数的话，"模型给错了什么"就只能靠猜；但回显必须截断——
        // 模型可以把一大段文本塞进参数里，报错信息不能因此变成新的膨胀源。
        let args = json!({ "command": "very long command line ".repeat(200) });
        let err = RunCommandTool.call(&args, &mut tool_ctx()).unwrap_err();
        let ToolError::BadArgs { detail } = err else {
            panic!("应为 BadArgs");
        };
        assert!(
            detail.contains("very long command line"),
            "回显要带上实际收到的内容：{detail}"
        );
        assert!(detail.contains('…'), "超长回显要被截断：{detail}");
        assert!(
            detail.chars().count() < 900,
            "报错不该被回显撑爆（{} 字符）",
            detail.chars().count()
        );
    }
}
