//! 执行循环：取可跑的步骤 → 执行 → 回填 → 再取，直到全部终态。
//!
//! ## 为什么并行是"设计好但先不接线"
//!
//! [`ready_steps`] 返回的是**一批**可以并行的步骤，不是一步。DAG 的形状、
//! 依赖检查、状态机都为并行准备好了。
//!
//! 但当前实现顺序执行，理由是这两条：
//!
//! 1. **台账是串行的。** `append` 要 `&mut Ledger` 且每条都 flush。并行写台账
//!    要么加锁排队（那并行就没意义了），要么破坏 append-only 的时序保证。
//! 2. **限流器是共享状态。** Agnes 免费档 10 RPM——并行发请求只会更快撞上 429，
//!    不会更快跑完。
//!
//! 所以并行的收益在**远端调用**上，而当前的瓶颈在**台账和配额**上。
//! 等真有并发需求（比如 DeepSeek 档上跑十几个独立步骤）再接，
//! 那时要改的只是"从 ready_steps 里取一批还是取一个"。
//!
//! ## 一个任务执行中途不换模型
//!
//! 每一步各自路由一次（因为每一步是独立的任务），但**某一步自己从开始到结束
//! 用一个模型**。中途换模型会把已经构建好的前缀缓存全部作废——
//! DeepSeek 的缓存单元必须完整匹配，参见 [`crate::think::router`] 的模块文档。

use std::collections::BTreeMap;

use crate::costlog::CallRecord;
use crate::decide::Decider;
use crate::ledger::{EventKind, Ledger, LedgerError, task_from_events};
use crate::think::{ModelRouter, ReasoningEffort, Routing, TaskKind, TaskProfile};

use super::decide::{TaskDecision, parse_options};
use super::model::{
    Budget, BudgetLedger, RoutingRecord, Step, StepState, Task, TaskError, TaskState,
    validate_dependencies,
};
use super::plan::parse_plan;

/// 单步输出上限。够一段正文或一个命令，又不至于让失控的输出吃掉预算。
pub const STEP_MAX_TOKENS: u32 = 1024;

/// 拆解输出的上限。步骤清单本身不长，但思考过程要占位置。
pub const PLAN_MAX_TOKENS: u32 = 2048;

/// **开了思考时，输出预算要额外加的余量。**
///
/// `max_tokens` 是"思考过程 + 正文"的总和，不是正文的额度。
/// 所以思考开着时，原来那个"够写正文"的数字就不够了。
///
/// 这是真机测出来的：拆解请求在 `max_tokens=2048` 时被截断
/// （`finish_reason=length`），思考吃掉 280~540 token，剩下的不够写完 JSON。
/// 而截断的表现是——一个残缺的 `{"steps":[{"id":"s1",...`，被解析器
/// 当成"没有 steps 字段"，**指向完全错误的方向**。
///
/// 同一批实测里 `max_tokens=4096` 正常结束（`finish=stop`）。
pub const THINKING_OUTPUT_HEADROOM: u32 = 2048;

/// 决策点生成选项的输出上限。
pub const OPTIONS_MAX_TOKENS: u32 = 512;

/// 决策点步骤的前缀。带这个前缀的步骤**不产生内容，只做选择**。
///
/// 用一个显式前缀而不是让模型自己说"这一步是决策"，是因为
/// **"哪一步需要人判断"必须是可检的、不依赖模型措辞的**。
pub const DECIDE_PREFIX: &str = "decide:";

/// 判据：决策点至少要有几个选项。少于两个就不叫选择。
pub const MIN_OPTIONS: usize = 2;

/// 步骤产出的成功标记。模型必须用它开头。
pub const OK_MARK: &str = "OK:";
/// 步骤产出的"做不了"标记。
pub const BLOCKED_MARK: &str = "BLOCKED:";

/// 一个步骤的产出该怎么判定。
///
/// ## 为什么需要这个
///
/// 真实运行里出现过这样一幕：三步任务，前两步模型都回了
/// "做不了。我没有联网能力，无法获取实时天气数据……"，而引擎把它们
/// **记成了成功**——因为在引擎眼里"模型返回了文本"就是成功。
/// 于是任务报告"完成 2 步"，而实际上什么都没办成。
///
/// **这是最坏的一种错**：报告说做完了，实际没做。所以步骤产出有了一个
/// 明确契约——模型必须用 `OK:` 或 `BLOCKED:` 开头。这不是靠关键词猜
/// （"做不了"这三个字有一万种说法），而是一条**格式约定**。
///
/// 没有标记时按"完成"处理而不是"失败"：内容确实在那儿，判失败会让它
/// 白重试一次。但记下"没标记"，让这种事在台账里看得见。
#[derive(Debug, Clone, PartialEq)]
pub enum StepVerdict {
    /// 做成了。
    Done(String),
    /// **明确说做不了。** 不该重试——模型说了缺什么，再试一次还是缺。
    Blocked(String),
    /// 没有标记。内容在，但没法确认完成。
    Unmarked(String),
}

impl<'a, H: TaskHandler, S: TaskStore> Engine<'a, H, S> {
    /// 这一步的输入：**声明过的依赖** + **指令里点名提到的步骤**，两者的结果。
    ///
    /// ## 为什么抽出来共用
    ///
    /// `run_step` 和 `run_decision` 都要它。各写一份的话必然漂移——
    /// 而漂移的表现是"某一条路有证据、另一条没有"，**最难查**
    /// （这个 session 里已经栽过好几次：两份来源迟早对不上）。
    ///
    /// ## 决策点尤其需要它
    ///
    /// 决策步此前**什么都不带**：`goal` 是空的、前置结果一个没有。
    /// 于是生成选项的模型只能凭那句话编，而 `Verdict`（双编码器，
    /// 按选项文本打分）更是一点证据都拿不到——**它是在猜**。
    ///
    /// 真机后果：决策步选了「门槛按 `>` 判定」，而前面的分析步骤
    /// 已经写着 `README` 明说「满减的边界条件写错了」。执行步骤
    /// 当场发现矛盾、拒绝照做，任务卡住。**那不是它拍错，
    /// 是我们没给它拍板所需的材料。**
    fn collect_inputs(
        &self,
        task_id: &str,
        step_id: &str,
    ) -> Result<Vec<(String, String)>, TaskError> {
        let task = self.load(task_id)?;
        let Some(step) = task.step(step_id) else {
            return Ok(Vec::new());
        };
        let mut wanted: Vec<String> = step.depends_on.clone();
        for id in mentioned_step_ids(&step.instruction, &task) {
            if !wanted.contains(&id) {
                wanted.push(id);
            }
        }
        Ok(wanted
            .iter()
            .filter_map(|d| {
                task.step(d)
                    .and_then(|s| s.result.clone())
                    .map(|r| (d.clone(), r))
            })
            .collect())
    }
}

/// 指令里**点名提到的**、且确实存在于这个任务里的其他步骤 id。
///
/// ## 为什么需要它
///
/// 真机上第 9 步的指令是「按 s3 的边界清单逐条核对」，而它的
/// `depends_on` 里没写 s3。引擎只把**声明过的**依赖当输入，
/// 于是模型手上没有 s3 的内容，只能如实回
/// 「无法获取 s3 的边界清单原文」——一步白做，后面依赖它的也被跳过。
///
/// 计划是模型写的：**它引用一个步骤，却忘了声明依赖，是很自然的事。**
/// 而"引用"这件事本身在指令文本里是明摆着的——照着它补上，
/// 比让这一步白跑一次要划算得多。
///
/// ## 只认真正存在的 id
///
/// 这样才叫"补一个漏写的依赖"而不是"猜"。指令里出现 `s3` 而任务里
/// 没有 s3 的话，什么也不补——那多半是模型在说别的（型号、变量名）。
///
/// 大小写不敏感（`S3` 也算），因为模型两种都会写。
fn mentioned_step_ids(instruction: &str, task: &Task) -> Vec<String> {
    let bytes = instruction.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // 找一个 `s` / `S`，且它前面不是字母数字（避免命中 `was3`、`xs3` 这类）
        if !bytes[i].eq_ignore_ascii_case(&b's') {
            i += 1;
            continue;
        }
        let prev_is_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if prev_is_word {
            i += 1;
            continue;
        }
        // 后面必须紧跟数字
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            i += 1;
            continue;
        }
        // 数字后面不能再接字母数字（`s3x` 不是步骤 id）
        let after_is_word =
            end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_');
        if !after_is_word {
            let candidate = &instruction[i..end];
            // **返回任务里真实的那个 id，不是指令里的写法。**
            //
            // 指令写 `S3`、任务里是 `s3` 时，返回 `S3` 会让下游按 id
            // 查结果时对不上——而那种对不上不会报错，只会**静默少一份输入**，
            // 于是模型又说一次"拿不到"。
            if let Some(real) = task
                .steps
                .iter()
                .find(|s| s.id.eq_ignore_ascii_case(candidate))
                .map(|s| s.id.clone())
                && !out.iter().any(|x: &String| x.eq_ignore_ascii_case(&real))
            {
                out.push(real);
            }
        }
        i = end;
    }
    out
}

/// 解析步骤产出。**大小写不敏感**，并容忍全角冒号与行首空白。
pub fn classify_step_output(raw: &str) -> StepVerdict {
    let t = raw.trim_start();
    let upper = t.to_ascii_uppercase();
    // 只在**开头**认标记：正文里出现 "BLOCKED:" 不算
    if upper.starts_with("BLOCKED:") || upper.starts_with("BLOCKED：") {
        return StepVerdict::Blocked(strip_mark(t, "BLOCKED"));
    }
    if upper.starts_with("OK:") || upper.starts_with("OK：") {
        return StepVerdict::Done(strip_mark(t, "OK"));
    }
    StepVerdict::Unmarked(raw.trim().to_string())
}

/// 去掉开头的标记与紧随其后的冒号/空白。
fn strip_mark(s: &str, mark: &str) -> String {
    let rest = &s[mark.len()..];
    rest.trim_start_matches([':', '：'])
        .trim_start()
        .to_string()
}

/// 模型侧的三个动作。引擎只负责调它们，不关心用的是哪个模型。
///
/// 拆出来是为了**执行循环可以完全离线测试**：给一个不碰网络的 handler，
/// 就能测出拓扑、预算、重试、卡住、升级人工这些逻辑。
///
/// `records` 由引擎传进来而不是 handler 自己存：handler 要同时持有
/// "可选模型"（可变）和"观测"（可变）两个东西，塞进同一个 struct 会让
/// 每次调用都撞借用检查。把观测当参数传，借用关系就一目了然。
pub trait TaskHandler {
    /// 把目标拆成步骤清单（模型的原始回复）。
    fn plan(
        &mut self,
        routing: &Routing,
        run: &PlanRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError>;

    /// 执行一步，返回这一步的产出。
    fn execute_step(
        &mut self,
        routing: &Routing,
        run: &StepRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError>;

    /// 为决策点生成选项（模型的原始回复）。
    fn observe(
        &mut self,
        routing: &Routing,
        run: &ObserveRequest,
        records: &mut Vec<CallRecord>,
    ) -> Result<String, TaskError>;
}

/// 拆解请求的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRequest {
    pub task_id: String,
    pub goal: String,
    /// 最多允许几个步骤。
    pub max_steps: u32,
}

/// 单步执行的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct StepRequest {
    pub task_id: String,
    pub goal: String,
    pub step_id: String,
    pub instruction: String,
    /// 已完成的依赖步骤 → 其结果。**只给依赖的结果**，不给全部历史：
    /// 无关步骤的输出既浪费 token 又会干扰。
    pub inputs: Vec<(String, String)>,
}

/// 决策点生成选项的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct ObserveRequest {
    pub task_id: String,
    pub goal: String,
    pub step_id: String,
    pub question: String,
    /// 前置步骤的结果。
    ///
    /// **决策点必须看到证据。** 在这之前这里什么都没有：`goal` 传的是
    /// 空字符串、前置结果一个都没带，于是生成选项的模型只能凭那句话编，
    /// 而 `Verdict`（双编码器，按选项文本打分）**更是一点证据都拿不到**——
    /// 它是在猜。
    ///
    /// 真机后果：决策步选了「门槛按 `>` 判定」，而前面的分析步骤已经
    /// 写着 `README` 明说「满减的边界条件写错了」。执行步骤当场发现矛盾、
    /// 拒绝照做，任务卡住。**那不是它拍错，是我们没给它拍板所需的材料。**
    pub inputs: Vec<(String, String)>,
}

/// 一次推进的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Advance {
    /// 又跑完一步，还能继续。
    Progress {
        settled: usize,
        total: usize,
        last: String,
    },
    /// 全部终态。
    Finished,
    /// 停下等人。**失败方向朝「不执行」**：预算、成环、弃权都走这里。
    Waiting { reason: String },
}

/// 一轮推进的汇总。
#[derive(Debug, Clone, PartialEq)]
pub struct EngineOutcome {
    pub task: Task,
    pub used_model_calls: u32,
    pub advance: Advance,
}

/// 任务执行引擎。
///
/// 泛型参数是两个 mock 点：`H` 模型调用、`S` 落盘。
/// 两个都不碰网络时，整个执行循环可以离线跑完。
/// 决策层用 `&dyn Decider`——它已经是一个 trait 对象可用的抽象，
/// 再套一层泛型只会让签名更长。
pub struct Engine<'a, H: TaskHandler, S: TaskStore> {
    pub router: &'a ModelRouter,
    pub decider: &'a dyn Decider,
    pub handler: &'a mut H,
    pub store: &'a mut S,
    pub effort: ReasoningEffort,
    budget: Budget,
}

impl<'a, H: TaskHandler, S: TaskStore> Engine<'a, H, S> {
    pub fn new(
        router: &'a ModelRouter,
        decider: &'a dyn Decider,
        handler: &'a mut H,
        store: &'a mut S,
        budget: Budget,
    ) -> Self {
        Self {
            router,
            decider,
            handler,
            store,
            effort: ReasoningEffort::Auto,
            budget,
        }
    }

    pub fn with_effort(mut self, effort: ReasoningEffort) -> Self {
        self.effort = effort;
        self
    }

    /// 推进一个任务直到完成 / 等人 / 预算用尽。
    ///
    /// **每轮都从台账重新投影**，不用内存里的状态推进。这样"状态"永远只是
    /// 台账的函数，不存在内存与台账不一致的可能。
    pub fn run(&mut self, task_id: &str) -> Result<EngineOutcome, TaskError> {
        let mut ledger = BudgetLedger::new(self.budget);
        // 本轮所有模型调用的用量。跑完一次性写台账——
        // **一次 flush 一条，而不是每次调用都写**：调用期间崩掉留下的
        // 是"钱花了没记上"，而记上的是既成事实，宁可重复不可丢失。
        let mut records: Vec<CallRecord> = Vec::new();
        let result = self.run_inner(task_id, &mut ledger, &mut records);
        self.flush_records(&mut records);
        result
    }

    fn run_inner(
        &mut self,
        task_id: &str,
        ledger: &mut BudgetLedger,
        records: &mut Vec<CallRecord>,
    ) -> Result<EngineOutcome, TaskError> {
        // 循环体里每个分支要么 return，要么 continue；这个哨兵让"循环不可达终点"
        // 这件事在类型层面成立，而不是靠一句注释。
        let mut outcome = None;
        // 回收只在开头做一次。每轮都做的话，一旦还有 Running 的步骤就会无限循环。
        let mut recovered = false;

        loop {
            let mut task = self.load(task_id)?;

            // 拆解阶段
            if task.state == TaskState::Planning {
                self.plan_task(&mut task, ledger, records)?;
                continue;
            }

            // 终态：收工
            if task.state.is_terminal() {
                let advance = match task.state {
                    TaskState::Done => Advance::Finished,
                    other => Advance::Waiting {
                        reason: format!("任务已{}", other.label()),
                    },
                };
                outcome = Some(EngineOutcome {
                    task,
                    used_model_calls: ledger.used(),
                    advance,
                });
            } else if task.state == TaskState::AwaitingHuman {
                // 有人等在人工上，就别动
                outcome = Some(EngineOutcome {
                    task,
                    used_model_calls: ledger.used(),
                    advance: Advance::Waiting {
                        reason: "任务在等人工介入".into(),
                    },
                });
            } else {
                // 依赖合法性只在第一次执行前查一次——成环必须在花钱之前查出来
                validate_dependencies(&task.steps)?;
                self.skip_doomed(&mut task)?;

                // **回收被打断的步骤。** 走到这里说明任务是闲置的，
                // 所以任何还停在"执行中"的步骤都是上一次跑到一半断掉的——
                // 当前没有任何进程在跑它。不回收的话它会永远停在 Running，
                // 而 Running 既不在 ready_steps 里也不是终态，任务就卡住了。
                if !recovered && self.recover_interrupted(task_id)? {
                    recovered = true;
                    continue;
                }

                task = self.load(task_id)?;

                if task.all_settled() {
                    // **"所有步骤都尘埃落定"不等于"目标达成了"。**
                    //
                    // `all_settled()` 的定义是"完成/失败/跳过**都算终态**"，
                    // 所以它回答的是"还有没有在跑的步骤"，**不是**
                    // "目标达成没有"。真机上抓到过（D93）：`s5` 因为
                    // "需要使用者拍板"而 BLOCKED（**那是对的行为**），
                    // 依赖它的全跳过，而任务状态报的是"**完成**"、
                    // 文件一个字没改。**12 次里出现 2 次。**
                    //
                    // **D18 那条契约管的是单步输出，管不到整条链。**
                    // 这里补的就是整条链那一层。
                    let broken: Vec<String> = task
                        .steps
                        .iter()
                        .filter(|s| {
                            matches!(
                                s.state,
                                crate::task::StepState::Failed | crate::task::StepState::Skipped
                            )
                        })
                        .map(|s| s.id.clone())
                        .collect();
                    if broken.is_empty() {
                        self.finish(task_id, TaskState::Done)?;
                        // `Done` **是**终态，所以下一轮会在 `is_terminal()`
                        // 那里提前返回——这里 `continue` 是对的。
                        continue;
                    }
                    // **这一段必须 `break`，不能 `continue`。**
                    //
                    // `Stalled` **不是终态**——`is_terminal()` 只认
                    // `Done | Failed | Cancelled`。所以回到循环顶之后
                    // `all_settled()` 仍然为真、`broken` 仍非空，
                    // 会**无限设 Stalled**。
                    //
                    // 上一版我写的就是 `continue`，理由是"假设下一轮会在
                    // `is_terminal()` 处退出"——**那个假设没验证就写进了代码**，
                    // 结果是测试套件挂死（D94）。
                    // **设完状态就把 outcome 交出去，不要指望循环顶去兜。**
                    self.set_state(task_id, TaskState::Stalled)?;
                    // `break` 要把 outcome 交出去——**这个 `loop` 的值就是
                    // 函数的返回值**，所以不能光 `break`，也不能靠 `continue`
                    // 回到循环顶去兜（`Stalled` 不是终态，回不去）。
                    break Ok(EngineOutcome {
                        task: self.load(task_id)?,
                        used_model_calls: ledger.used(),
                        advance: Advance::Waiting {
                            reason: TaskError::Stalled { remaining: broken }.to_string(),
                        },
                    });
                }

                let ready = ready_steps(&task);
                if ready.is_empty() {
                    let remaining: Vec<String> = task
                        .steps
                        .iter()
                        .filter(|s| !s.state.is_settled())
                        .map(|s| s.id.clone())
                        .collect();
                    self.set_state(task_id, TaskState::Stalled)?;
                    outcome = Some(EngineOutcome {
                        task: self.load(task_id)?,
                        used_model_calls: ledger.used(),
                        advance: Advance::Waiting {
                            reason: TaskError::Stalled { remaining }.to_string(),
                        },
                    });
                } else {
                    // 逐个跑。并行版本从这里改成"取一批"，其余不变（见模块文档）。
                    let mut stopped = false;
                    for step_id in ready {
                        match self.run_step(self.decider, task_id, &step_id, ledger, records) {
                            Ok(StepRun::Settled | StepRun::RetryLater) => {}
                            Ok(StepRun::NeedsHuman { reason }) => {
                                outcome = Some(EngineOutcome {
                                    task: self.load(task_id)?,
                                    used_model_calls: ledger.used(),
                                    advance: Advance::Waiting { reason },
                                });
                                stopped = true;
                            }
                            Err(e) if e.needs_human() => {
                                self.set_state(task_id, TaskState::AwaitingHuman)?;
                                outcome = Some(EngineOutcome {
                                    task: self.load(task_id)?,
                                    used_model_calls: ledger.used(),
                                    advance: Advance::Waiting {
                                        reason: e.to_string(),
                                    },
                                });
                                stopped = true;
                            }
                            Err(e) => return Err(e),
                        }
                        if stopped {
                            break;
                        }
                    }
                    if !stopped {
                        // 这一轮跑完了，**推进了一步**：回去重新投影再跑下一轮。
                        //
                        // 这里不能 return——任务是"循环到跑完"，不是"跑一步就交差"。
                        // 我第一版就是在这里 return 了，于是三步的线性任务只跑第一步
                        // 就报告 Progress。**测试抓到了它。**
                        continue;
                    }
                }
            }

            if let Some(o) = outcome.take() {
                return Ok(o);
            }
            // 走到这里说明既没有终态也没有可推进的步骤。
            // 不能静默返回——那会表现为"任务停在原地但没人知道为什么"。
            return Err(TaskError::Stalled {
                remaining: vec![task_id.to_string()],
            });
        }
    }

    /// 把本轮的模型用量写进观测台账。
    ///
    /// 每次调用都写一条：**崩在中间时丢的是记录，不是事实**。
    /// 反过来的设计（攒到最后一起写）会让"钱花了但台账里没有"，
    /// 而台账是唯一事实来源——它不能比现实少。
    fn flush_records(&mut self, records: &mut Vec<CallRecord>) {
        for r in records.drain(..) {
            // 写失败只报不中断：账已经花了，任务结果比记账重要
            if let Err(e) = self.store.record_cost(&r) {
                eprintln!("⚠ 成本记录写入失败（不影响任务）: {e}");
            }
        }
    }

    /// 拆解。
    fn plan_task(
        &mut self,
        task: &mut Task,
        ledger: &mut BudgetLedger,
        records: &mut Vec<CallRecord>,
    ) -> Result<(), TaskError> {
        // **拆解本身也是"一个任务"**，所以也走路由：它需要推理、而且它要产出多步。
        //
        // `step_count` 故意按"至少三步"报：拆解的产出是多步计划，按一步报会被
        // 判成轻量档（免费的那个），而免费档的思考模式是"服务端默认"——
        // 拆解不开思考，拆出来的步骤质量会明显掉。这是我在测试里发现接线错了的地方。
        let kind = TaskKind::Planning;
        let profile = TaskProfile {
            prompt_chars: task.goal.chars().count(),
            step_count: 3,
            explicit_multi: crate::think::detect_explicit_multi(&task.goal),
            has_code: crate::think::detect_code(&task.goal),
        };
        let routing =
            self.router
                .route(&task.goal, kind, &profile, self.effort, Some(self.decider));
        ledger.try_charge()?;
        let req = PlanRequest {
            task_id: task.id.clone(),
            goal: task.goal.clone(),
            max_steps: self.budget.max_steps,
        };
        let raw = self.handler.plan(&routing, &req, records)?;

        let planned = parse_plan(&raw)?;
        ledger.steps_allowed(planned.len())?;

        // 落成 Step：类型由模型给，没给就按文本兜底判
        let steps: Vec<Step> = planned
            .iter()
            .map(|p| {
                let kind = p.kind.unwrap_or_else(|| {
                    TaskKind::classify(&p.instruction).unwrap_or(TaskKind::Generation)
                });
                Step::new(p.id.clone(), p.instruction.clone(), kind)
                    .with_depends_on(p.depends_on.clone())
            })
            .collect();
        validate_dependencies(&steps)?;

        // 一次写完拆解结果：崩在中间不会留下"半个步骤清单"
        self.store.write(
            &task.id,
            EventKind::TaskPlanned,
            serde_json::json!({
                "task": task.id,
                "steps": steps,
                "routing": RoutingRecord::from_routing(&routing),
            }),
        )?;
        self.set_state(&task.id, TaskState::Running)?;
        Ok(())
    }

    /// 把停在"执行中"的步骤退回待执行。
    ///
    /// 只在任务闲置时调用。**这是"跨重启续跑"和"预算中途用尽"共用的恢复路径**：
    /// 两种情况留下的都是"标记为执行中、但实际没人在跑"的步骤。
    ///
    /// 退回而不是判失败，是因为**我们不知道它做没做完**——可能已经在远端产生了
    /// 副作用。交给下一步再试一次，比假装它失败了安全。
    ///
    /// 返回是否有步骤被回收。
    fn recover_interrupted(&mut self, task_id: &str) -> Result<bool, TaskError> {
        let task = self.load(task_id)?;
        let stuck: Vec<Step> = task
            .steps
            .iter()
            .filter(|s| s.state == StepState::Running)
            .cloned()
            .collect();
        if stuck.is_empty() {
            return Ok(false);
        }
        for step in &stuck {
            self.store.write(
                task_id,
                EventKind::StepPending,
                serde_json::json!({
                    "task": task_id,
                    "step": step.id,
                    "instruction": step.instruction,
                    "kind": step.kind,
                    "depends_on": step.depends_on,
                    "recovered": "上一次执行被打断，退回待执行",
                }),
            )?;
        }
        Ok(true)
    }

    /// 执行一个步骤。
    fn run_step(
        &mut self,
        decider: &dyn Decider,
        task_id: &str,
        step_id: &str,
        ledger: &mut BudgetLedger,
        records: &mut Vec<CallRecord>,
    ) -> Result<StepRun, TaskError> {
        let task = self.load(task_id)?;
        let Some(step) = task.step(step_id).cloned() else {
            return Err(TaskError::Stalled {
                remaining: vec![step_id.to_string()],
            });
        };
        if step.state.is_settled() || step.state == StepState::Running {
            return Ok(StepRun::Settled);
        }

        // **人的答案优先于一切重试计数。**
        //
        // 位置很要紧：必须在下面那个"重试次数用尽"的检查**之前**。
        //
        // 真机上踩过：决策步骤因为决策模型反复弃权，两次尝试用光成了 Failed；
        // 人随后用 `resume --answer` 给了答案，答案也记进台账了，
        // **可这一步还是被判失败**——因为引擎先看尝试次数，
        // 根本没走到"有没有人答过"那一步。
        //
        // 人已经拍板了，这一步就该收尾。**重试计数是给模型用的，
        // 不是给人的。**
        if step.instruction.trim_start().starts_with(DECIDE_PREFIX)
            && let Some(answer) = self.human_answer(task_id, step_id)
        {
            return self.settle_human_answer(task_id, &step, &answer);
        }

        // 重试次数用尽 → 这一步判失败
        if step.attempts >= self.budget.max_attempts_per_step {
            self.store.write(
                task_id,
                EventKind::StepFailed,
                serde_json::json!({
                    "task": task_id,
                    "step": step_id,
                    "error": format!("已尝试 {} 次仍未成功", step.attempts),
                }),
            )?;
            return Ok(StepRun::Settled);
        }

        // 路由：这一步自己的属性决定模型和思考模式
        let profile = TaskProfile {
            prompt_chars: step.instruction.chars().count(),
            step_count: 1,
            explicit_multi: crate::think::detect_explicit_multi(&step.instruction),
            has_code: crate::think::detect_code(&step.instruction),
        };
        let routing = self.router.route(
            &step.instruction,
            step.kind,
            &profile,
            self.effort,
            Some(decider),
        );
        self.store.write(
            task_id,
            EventKind::StepRouted,
            serde_json::json!({
                "task": task_id,
                "step": step_id,
                "routing": RoutingRecord::from_routing(&routing),
            }),
        )?;

        // **先落 Running 再扣预算**：如果崩在中间，投影会显示"执行中"而不是
        // "待执行"——模糊总比"看起来没跑过"安全（可能已经产生了副作用）。
        self.store.write(
            task_id,
            EventKind::StepRunning,
            serde_json::json!({ "task": task_id, "step": step_id }),
        )?;
        if let Err(e) = ledger.try_charge() {
            // 预算在"标记执行中"之后用尽：必须把它退回去，
            // 否则这一步会永远停在 Running——既不是终态也永远不会被选中。
            //
            // `fresh: false`：预算用尽**不是这一步的错**，次数要保留，
            // 否则一个步骤可以靠"反复撞预算"绕过重试上限。
            self.reset_step(task_id, &step, false)?;
            return Err(e);
        }

        // 决策点走另一条路
        if let Some(question) = step.instruction.strip_prefix(DECIDE_PREFIX) {
            let question = question.trim().to_string();
            // 人的答案已经在上面（重试计数之前）查过了——那里是唯一的检查点，
            // 两处各查一遍的话，"哪一处先生效"迟早漂移。
            return self.run_decision(self.decider, task_id, &step, &question, ledger, records);
        }

        let inputs = self.collect_inputs(task_id, &step.id)?;
        let req = StepRequest {
            task_id: task_id.to_string(),
            goal: task.goal.clone(),
            step_id: step_id.to_string(),
            instruction: step.instruction.clone(),
            inputs,
        };

        match self.handler.execute_step(&routing, &req, records) {
            Ok(out) => {
                // **产出要看它自己怎么说，不能"返回了文本就算成功"。**
                match classify_step_output(&out) {
                    StepVerdict::Done(text) => {
                        self.store.write(
                            task_id,
                            EventKind::StepSucceeded,
                            serde_json::json!({
                                "task": task_id, "step": step_id, "result": text, "marked": true,
                            }),
                        )?;
                    }
                    StepVerdict::Blocked(reason) => {
                        // 模型明确说做不了 → 判失败，**且不重试**：
                        // 它已经说了缺什么，再试一次还是缺。
                        self.store.write(
                            task_id,
                            EventKind::StepFailed,
                            serde_json::json!({
                                "task": task_id,
                                "step": step_id,
                                "error": reason,
                                "blocked": true,
                            }),
                        )?;
                    }
                    StepVerdict::Unmarked(text) => {
                        // 没标记：内容在，按完成算，但记下来让这事在台账里看得见
                        self.store.write(
                            task_id,
                            EventKind::StepSucceeded,
                            serde_json::json!({
                                "task": task_id, "step": step_id, "result": text, "marked": false,
                            }),
                        )?;
                    }
                }
                Ok(StepRun::Settled)
            }
            Err(e) => {
                // 单步失败不立刻判死，但**只有还能重试时才写 StepFailed**。
                // 再写一条 StepPending 会覆盖掉刚写下的失败原因，让投影里
                // 看不出这一步曾经失败过——排查时最需要的就是那条。
                // attempts 已经由 StepRunning 累加过，所以重试次数仍然是有限的。
                if step.attempts + 1 < self.budget.max_attempts_per_step {
                    self.reset_step(task_id, &step, false)?;
                    Ok(StepRun::RetryLater)
                } else {
                    self.store.write(
                        task_id,
                        EventKind::StepFailed,
                        serde_json::json!({
                            "task": task_id,
                            "step": step_id,
                            "error": e.to_string(),
                        }),
                    )?;
                    Ok(StepRun::Settled)
                }
            }
        }
    }

    /// 跑一个决策点。
    ///
    /// 决策层用 `&dyn Decider` 传进来而不是从 `self.decider` 取：
    /// `self` 同时被 `handler`（可变）和 `store`（可变）借着，
    /// 从 `self` 里再借一个字段出来会和它们打架。显式传参把借用关系摆明了。
    /// 人已经就这一步给过答案吗。**取最新的一条。**
    ///
    /// 取最新而不是第一条：人可能改主意，而"最后说的那句话"才算数。
    fn human_answer(&self, task_id: &str, step_id: &str) -> Option<String> {
        self.store
            .human_answer(task_id, step_id)
            .filter(|a| !a.trim().is_empty())
    }

    /// 用人的答案把这一步收尾。
    ///
    /// 产物形状**和决策模型选了某个选项时一致**，只是多了一个
    /// `source: "human"`——事后回看要能分清"这是模型选的"还是"人定的"，
    /// 那两种的可信度完全不同。
    fn settle_human_answer(
        &mut self,
        task_id: &str,
        step: &Step,
        answer: &str,
    ) -> Result<StepRun, TaskError> {
        let result = serde_json::json!({
            "choice": answer,
            "rationale": "人工给出",
            "alternatives": [],
            "source": "human",
        })
        .to_string();
        self.store.write(
            task_id,
            EventKind::StepSucceeded,
            serde_json::json!({ "task": task_id, "step": step.id, "result": result }),
        )?;
        Ok(StepRun::Settled)
    }

    fn run_decision(
        &mut self,
        decider: &dyn Decider,
        task_id: &str,
        step: &Step,
        question: &str,
        ledger: &mut BudgetLedger,
        records: &mut Vec<CallRecord>,
    ) -> Result<StepRun, TaskError> {
        // 决策点生成选项是**一次调用**，而且它必须推理（要理解这一步在纠结什么）。
        //
        // 所以这里**故意不把档位交给本地决策模型**：调用次数我们已经知道是 1，
        // "需要推理"也已经在 `TaskKind::Analysis` 里写明了。问一次纯属浪费一个
        // 本地调用，而且被问的模型并不比我们多任何信息。
        //
        // 档位先按"一次调用"落到默认档，再由路由里那条
        // **"需要推理但端点不支持思考 → 换到支持思考的端点"** 规则升上去。
        // 两个轴的判据各自只写在一处（router.rs），这里不重复实现一遍。
        let profile = TaskProfile {
            prompt_chars: question.chars().count(),
            step_count: 1,
            explicit_multi: false,
            has_code: false,
        };
        let routing = self
            .router
            .route(question, TaskKind::Analysis, &profile, self.effort, None);

        ledger.try_charge()?;
        let raw = self.handler.observe(
            &routing,
            &ObserveRequest {
                task_id: task_id.to_string(),
                // **目标要带上。** 原来这里是空的——而"这一步在纠结什么"
                // 只有放回整条任务的目标里才判得准。
                goal: self.load(task_id)?.goal.clone(),
                step_id: step.id.clone(),
                question: question.to_string(),
                // **前置证据也要带上**，理由见 `collect_inputs` 的文档。
                inputs: self.collect_inputs(task_id, &step.id)?,
            },
            records,
        )?;

        let (q, options) = parse_options(&raw)?;
        if options.len() < MIN_OPTIONS {
            return self.escalate(task_id, step, &q, &options, "选项少于两个");
        }

        // 判据的键必须与选项一一对应——**顺序也要一致**，
        // 否则本地决策模型看到的"选项"和它给回的 choice 对不上。
        //
        // 用 opt0/opt1 当键、选项原文当判据：本地决策模型（Verdict）是双编码器，
        // 它按**判据文本**打分，所以判据必须是能读懂的原话；而它回的是键。
        let criteria: Vec<(String, String)> = options
            .iter()
            .enumerate()
            .map(|(i, o)| (format!("opt{i}"), o.clone()))
            .collect();
        let criteria_ref: Vec<(&str, &str)> = criteria
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        match super::decide::decision_point(decider, "task_decision", &q, &criteria_ref)? {
            TaskDecision::Chosen {
                choice,
                rationale,
                alternatives,
            } => {
                // **键要换回原文再落盘。** 否则台账里存的是 "opt1" 这种占位符，
                // 事后回看"当时选了哪个做法"完全读不出来——留痕就白留了。
                let label = |key: &str| -> String {
                    key.strip_prefix("opt")
                        .and_then(|i| i.parse::<usize>().ok())
                        .and_then(|i| options.get(i))
                        .cloned()
                        .unwrap_or_else(|| key.to_string())
                };
                let result = serde_json::json!({
                    "choice": label(&choice),
                    "choice_key": choice,
                    "rationale": rationale,
                    "alternatives": alternatives.iter().map(|a| label(a)).collect::<Vec<_>>(),
                })
                .to_string();
                self.store.write(
                    task_id,
                    EventKind::StepSucceeded,
                    serde_json::json!({ "task": task_id, "step": step.id, "result": result }),
                )?;
                Ok(StepRun::Settled)
            }
            TaskDecision::NeedsHuman {
                question,
                options: opts,
            } => self.escalate(task_id, step, &question, &opts, "决策模型弃权"),
        }
    }

    /// 升级人工。**不确定就停，不猜。**
    fn escalate(
        &mut self,
        task_id: &str,
        step: &Step,
        question: &str,
        options: &[String],
        why: &str,
    ) -> Result<StepRun, TaskError> {
        let reason = format!(
            "{why}：步骤 {} 需要人工判断「{question}」；候选 = {options:?}",
            step.id
        );
        // **弃权也要进台账。**
        //
        // 决策模型**拍板**那条路已经留了痕（`StepSucceeded` 的 result 里
        // 有 choice / rationale / alternatives）。但**弃权这条路没有**——
        // `why` 只进了返回值，谁没接住就没了。
        //
        // 为什么这个缺口要紧（D78）：**决策模型接管之后，人从回路里退出了**。
        // 以前每个 `decide:` 都问人，人看到问题本身就是一道检查；
        // 现在它自己拍板，"它拍了什么、凭什么"只能靠台账。
        // 而**弃权是最需要解释的那一种**——"它为什么不敢定"直接说明
        // 那一类问题它判不了，那正是该去调提示词或补样本的地方。
        self.store.write(
            task_id,
            EventKind::DecisionAsked,
            serde_json::json!({
                "task": task_id,
                "step": step.id,
                "question": question,
                "options": options,
                // **说清是"谁"没定下来。** "决策模型弃权"和
                // "选项少于两个"是两种完全不同的原因，混在一起就查不出东西。
                "why": why,
            }),
        )?;
        // 步骤回到待执行，但任务状态转等人工——人处理完之后重新跑就能续上。
        // **`fresh: true`：弃权不该消耗重试次数。**
        self.reset_step(task_id, step, true)?;
        self.set_state(task_id, TaskState::AwaitingHuman)?;
        Ok(StepRun::NeedsHuman { reason })
    }

    /// 把一步退回待执行（保留 attempts，不重置重试计数）。
    /// 把步骤放回待执行。
    ///
    /// `fresh` 决定**要不要连重试次数一起清掉**：
    ///
    /// - `false`（普通重试）：次数保留，否则重试就成了无限次
    /// - `true`（升级人工后）：**次数清零**。弃权是"我需要人拍板"，
    ///   不是"这次尝试失败了"——不该消耗重试预算。
    ///   真机上决策模型弃权两次就把步骤硬判失败，任务从"等人工"
    ///   变成"卡住"，人再想答也没机会了。
    fn reset_step(&mut self, task_id: &str, step: &Step, fresh: bool) -> Result<(), TaskError> {
        self.store.write(
            task_id,
            EventKind::StepPending,
            serde_json::json!({
                "task": task_id,
                "step": step.id,
                "instruction": step.instruction,
                "kind": step.kind,
                "depends_on": step.depends_on,
                "fresh": fresh,
            }),
        )?;
        Ok(())
    }

    /// 依赖已失败的步骤：标记跳过。**跳过不是失败**——它自己没出错。
    fn skip_doomed(&mut self, task: &mut Task) -> Result<(), TaskError> {
        let doomed: Vec<String> = task
            .steps
            .iter()
            .filter(|s| {
                !s.state.is_settled()
                    && s.state != StepState::Running
                    && s.depends_on
                        .iter()
                        .any(|d| task.step(d).is_some_and(|x| x.state == StepState::Failed))
            })
            .map(|s| s.id.clone())
            .collect();
        for id in doomed {
            self.store.write(
                &task.id,
                EventKind::StepSkipped,
                serde_json::json!({
                    "task": task.id,
                    "step": id,
                    "reason": "依赖的步骤失败",
                }),
            )?;
        }
        Ok(())
    }

    fn set_state(&mut self, task_id: &str, to: TaskState) -> Result<(), TaskError> {
        self.store.write(
            task_id,
            EventKind::TaskStateChanged,
            serde_json::json!({ "task": task_id, "state": to }),
        )?;
        Ok(())
    }

    fn finish(&mut self, task_id: &str, to: TaskState) -> Result<(), TaskError> {
        self.store.write(
            task_id,
            EventKind::TaskFinished,
            serde_json::json!({ "task": task_id, "state": to }),
        )?;
        Ok(())
    }

    fn load(&self, task_id: &str) -> Result<Task, TaskError> {
        let set = self.store.project();
        set.get(task_id).cloned().ok_or_else(|| {
            TaskError::Core(crate::CoreError::Ledger(format!("找不到任务 {task_id}")))
        })
    }
}

/// 单步执行的结局。
enum StepRun {
    Settled,
    RetryLater,
    NeedsHuman { reason: String },
}

/// 任务落盘。抽象出来是为了执行循环能离线测试。
///
/// **成本记录也走这里**，而不是给引擎单独接一个 sink：引擎已经可变借着
/// `store`，再让它借第二个可变引用指向同一个台账对象，借用检查过不去——
/// 而这个限制是对的，它逼着"谁持有台账"只有一个答案。
pub trait TaskStore {
    /// 追加一条事件。
    fn write(
        &mut self,
        task_id: &str,
        kind: EventKind,
        data: serde_json::Value,
    ) -> Result<(), TaskError>;

    /// 从台账投影出全部任务。
    fn project(&self) -> crate::task::TaskSet;

    /// 记一次模型调用的用量。**默认不记**——不落盘的 store（比如干跑）不需要。
    fn record_cost(&mut self, _rec: &CallRecord) -> Result<(), TaskError> {
        Ok(())
    }

    /// **人已经就某个决策步骤给过答案吗。**
    ///
    /// 有默认实现（返回 `None`），于是不落盘的 store 不必实现它。
    /// 但真正落盘的那两个必须实现——**不实现就等于"人工介入是死路"**，
    /// 而那正是加这个方法的理由。
    fn human_answer(&self, _task_id: &str, _step_id: &str) -> Option<String> {
        None
    }
}

/// 基于台账的实现。**唯一的事实来源。**
pub struct LedgerTaskStore {
    ledger: Ledger,
}

impl LedgerTaskStore {
    pub fn new(ledger: Ledger) -> Self {
        Self { ledger }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// 可变访问台账。给 CLI 读明细用。**别拿它绕过 `write` 改任务状态。**
    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }
}

/// 从一串事件里找"人给这一步的答案"。**取最新的一条。**
///
/// 取最新而不是第一条：**人可能改主意，而最后说的那句才算数。**
/// 两个 store 共用这一个实现——两处各写一遍的话，
/// "取第一条还是最后一条"这种细节迟早漂移。
fn human_answer_in(
    events: &[crate::ledger::Event],
    task_id: &str,
    step_id: &str,
) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| {
            e.kind == EventKind::HumanAnswered
                && e.data.get("task").and_then(|v| v.as_str()) == Some(task_id)
                && e.data.get("step").and_then(|v| v.as_str()) == Some(step_id)
        })
        .and_then(|e| {
            e.data
                .get("answer")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        // **空白答案当"没答过"。** 过滤放在这里而不是调用方：
        // 两个 store 共用这一个实现，放调用方就会出现"一个过滤了一个没过滤"。
        // 而一条空答案如果真的生效，会把步骤钉死在一个空决定上。
        .filter(|a| !a.trim().is_empty())
}

impl TaskStore for LedgerTaskStore {
    fn human_answer(&self, task_id: &str, step_id: &str) -> Option<String> {
        human_answer_in(self.ledger.events(), task_id, step_id)
    }

    fn write(
        &mut self,
        _task_id: &str,
        kind: EventKind,
        data: serde_json::Value,
    ) -> Result<(), TaskError> {
        // 审计事件必须落在边界内——台账会拒绝边界外的审计事件。
        //
        // **这里原来写的是"任务执行框架目前没有审计事件（DecisionAsked
        // 由 decide 模块写），所以直接追加"。那句话当时是对的，D90 之后
        // 不成立了**：`Engine::escalate` 现在自己写 `DecisionAsked`，
        // 而 `DecisionAsked` 在"需要边界"的名单里（`ledger.rs:150`）。
        //
        // 后果很隐蔽：**真实 CLI 用的是 `LedgerTaskStore`（会拦），
        // 而引擎测试用的是 `MemoryTaskStore`（不拦）**。
        // 于是测试全绿，真机上每一次弃权都变成 `TaskError::Core`
        // 被抛出去——**不是"等人工"**。而 `Core::needs_human()` 是 false，
        // 所以它连升级人工都走不到。
        //
        // 这和 D93 那次（引擎测试的内存台账不检查、真机台账检查）
        // 是同一族：**测试用了一套和生产不同的器件**。
        //
        // 修法：**由持有台账的这一层自己开边界**。语义上也对——
        // 审计事件必须落在边界内，而"开边界"是这个 store 的职责，
        // 不该推给上层每个调用点。
        if kind.requires_span() && !self.ledger.has_open_span() {
            // **写入必须经过 `span.ledger()`。**
            // `begin_span` 已经把台账可变借走了，这里再借 `self.ledger`
            // 编译不过。这也正是审计边界的本意：边界内的写只能从边界进去
            // （`decide/mod.rs:300-315` 就是这么写的）。
            let mut span = self
                .ledger
                .begin_span()
                .map_err(|e| TaskError::Core(crate::CoreError::Ledger(e.to_string())))?;
            span.ledger()
                .append(kind, None, data)
                .map_err(|e: LedgerError| {
                    TaskError::Core(crate::CoreError::Ledger(e.to_string()))
                })?;
            // **显式关边界，把"关不上"也当失败报出来。**
            // 留一个永远开着的边界，后面所有审计事件都会落进它里面——
            // 那是一种更隐蔽的错。
            return span
                .close()
                .map(|_| ())
                .map_err(|e| TaskError::Core(crate::CoreError::Ledger(e.to_string())));
        }
        self.ledger
            .append(kind, None, data)
            .map(|_| ())
            .map_err(|e: LedgerError| TaskError::Core(crate::CoreError::Ledger(e.to_string())))
    }

    fn project(&self) -> crate::task::TaskSet {
        task_from_events(self.ledger.events())
    }

    fn record_cost(&mut self, rec: &CallRecord) -> Result<(), TaskError> {
        self.ledger
            .append(EventKind::ModelCalled, None, serde_json::json!(rec))
            .map(|_| ())
            .map_err(|e: LedgerError| TaskError::Core(crate::CoreError::Ledger(e.to_string())))
    }
}

/// 内存实现。**只用于测试和干跑**——不落盘就没有"跨重启存活"。
#[derive(Debug, Default)]
pub struct MemoryTaskStore {
    events: Vec<crate::ledger::Event>,
    seq: u64,
    /// 成本记录单独存：它们不是任务事件，混进 `events` 会污染投影。
    pub costs: Vec<CallRecord>,
}

impl MemoryTaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> &[crate::ledger::Event] {
        &self.events
    }
}

impl TaskStore for MemoryTaskStore {
    fn human_answer(&self, task_id: &str, step_id: &str) -> Option<String> {
        human_answer_in(&self.events, task_id, step_id)
    }

    fn write(
        &mut self,
        _task_id: &str,
        kind: EventKind,
        data: serde_json::Value,
    ) -> Result<(), TaskError> {
        self.seq += 1;
        let seq = self.seq;
        self.events.push(crate::ledger::Event {
            seq,
            at: seq,
            kind,
            span: None,
            job: None,
            data,
        });
        Ok(())
    }

    fn project(&self) -> crate::task::TaskSet {
        task_from_events(&self.events)
    }

    fn record_cost(&mut self, rec: &CallRecord) -> Result<(), TaskError> {
        self.costs.push(rec.clone());
        Ok(())
    }
}

/// 记录一个新任务。**只写 TaskCreated**，拆解由 [`Engine::run`] 做。
pub fn create_task<S: TaskStore>(
    store: &mut S,
    task_id: &str,
    goal: &str,
) -> Result<(), TaskError> {
    store.write(
        task_id,
        EventKind::TaskCreated,
        serde_json::json!({ "task": task_id, "goal": goal }),
    )?;
    store.write(
        task_id,
        EventKind::TaskPlanning,
        serde_json::json!({ "task": task_id }),
    )
}

/// 现在就能跑的步骤：状态是待执行，且**所有依赖都已成功**。
///
/// 依赖失败的情况不在这里处理（那由 [`Engine::skip_doomed`] 转成跳过），
/// 因为"能不能跑"和"该不该跳过"是两个判断。
pub fn ready_steps(task: &Task) -> Vec<String> {
    task.steps
        .iter()
        .filter(|s| s.state == StepState::Pending)
        .filter(|s| {
            s.depends_on.iter().all(|d| {
                task.step(d)
                    .is_some_and(|x| x.state == StepState::Succeeded)
            })
        })
        .map(|s| s.id.clone())
        .collect()
}

/// 汇总各步骤结果，拼成给人类看的答复。
///
/// **失败的步骤也要出现**，不能只报成功的部分——那会让人以为任务做完了。
pub fn summarize(task: &Task) -> String {
    let (done, failed, skipped) = task.tally();
    let mut out = format!(
        "任务「{}」{}：完成 {} / 失败 {} / 跳过 {}\n",
        task.goal,
        task.state.label(),
        done,
        failed,
        skipped
    );
    for s in &task.steps {
        let mark = match s.state {
            StepState::Succeeded => "✓",
            StepState::Failed => "✗",
            StepState::Skipped => "—",
            _ => "·",
        };
        out.push_str(&format!("{mark} [{}] {}", s.id, s.instruction));
        if let Some(r) = &s.result {
            let head: String = r.chars().take(120).collect();
            out.push_str(&format!("\n    {head}"));
        }
        out.push('\n');
    }
    out
}

/// 从台账投影里取一个任务。给 CLI 用。
pub fn find_task(set: &crate::task::TaskSet, id: &str) -> Option<Task> {
    set.get(id).cloned()
}

/// 供 CLI 展示用：把一个任务的步骤状态整理成表。
pub fn step_table(task: &Task) -> Vec<(String, &'static str, String)> {
    task.steps
        .iter()
        .map(|s| {
            let deps = if s.depends_on.is_empty() {
                "-".to_string()
            } else {
                s.depends_on.join(",")
            };
            (s.id.clone(), s.state.label(), deps)
        })
        .collect()
}

/// 报表用的空映射辅助（避免调用点到处写类型标注）。
pub fn empty_inputs() -> Vec<(String, String)> {
    BTreeMap::<String, String>::new().into_iter().collect()
}

#[cfg(test)]
mod human_answer_tests {
    use super::*;
    use serde_json::json;

    fn answer(store: &mut MemoryTaskStore, task: &str, step: &str, text: &str) {
        store
            .write(
                task,
                EventKind::HumanAnswered,
                json!({ "task": task, "step": step, "answer": text }),
            )
            .unwrap();
    }

    #[test]
    fn a_recorded_answer_is_found() {
        // **这一步是最小复现。** 真机上人给了答案、台账里也有
        // `human_answered` 事件，可 `human_answer()` 返回 None，
        // 于是步骤还是被判失败。
        let mut store = MemoryTaskStore::default();
        answer(&mut store, "t1", "s4", "用 >= 而不是 >");
        assert_eq!(
            store.human_answer("t1", "s4").as_deref(),
            Some("用 >= 而不是 >"),
            "记进去的答案必须能查出来"
        );
    }

    #[test]
    fn an_answer_for_another_step_is_not_confused() {
        let mut store = MemoryTaskStore::default();
        answer(&mut store, "t1", "s4", "给 s4 的");
        assert!(store.human_answer("t1", "s5").is_none(), "不该串到别的步骤");
        assert!(store.human_answer("t2", "s4").is_none(), "不该串到别的任务");
    }

    #[test]
    fn the_latest_answer_wins() {
        // 人可能改主意，**最后说的那句才算数**
        let mut store = MemoryTaskStore::default();
        answer(&mut store, "t1", "s4", "先这么定");
        answer(&mut store, "t1", "s4", "改成这样");
        assert_eq!(store.human_answer("t1", "s4").as_deref(), Some("改成这样"));
    }

    #[test]
    fn an_empty_answer_does_not_count() {
        // 空字符串当"没答过"——不然一次误操作会把步骤钉死
        let mut store = MemoryTaskStore::default();
        answer(&mut store, "t1", "s4", "   ");
        assert!(store.human_answer("t1", "s4").is_none());
    }

    #[test]
    fn nothing_recorded_means_none() {
        let store = MemoryTaskStore::default();
        assert!(store.human_answer("t1", "s4").is_none());
    }
}

#[cfg(test)]
mod mentioned_step_ids_tests {
    use super::*;
    use crate::task::Step;

    /// 一个有三步的任务：s1 / s2 / s3。
    fn task() -> Task {
        let mut t = Task::new("t1", "目标", 0);
        for id in ["s1", "s2", "s3"] {
            t.steps.push(Step::new(id, "做什么", TaskKind::Generation));
        }
        t
    }

    #[test]
    fn a_mentioned_id_that_exists_is_returned() {
        // **真机上就是这一种。** 指令写着「按 s3 的边界清单」，而 depends_on
        // 里没写 s3——模型拿不到内容，只能回"无法获取"。
        assert_eq!(
            mentioned_step_ids("按 s3 的边界清单逐条核对", &task()),
            vec!["s3"]
        );
    }

    #[test]
    fn several_mentions_are_all_returned() {
        let got = mentioned_step_ids("先看 s1，再对照 s3", &task());
        assert_eq!(got, vec!["s1", "s3"]);
    }

    #[test]
    fn a_repeated_mention_is_returned_once() {
        assert_eq!(
            mentioned_step_ids("s2 的内容，s2 的结论", &task()),
            vec!["s2"]
        );
    }

    #[test]
    fn an_id_that_does_not_exist_is_not_invented() {
        // **这才叫"补漏写的依赖"而不是"猜"。**
        // 任务里没有 s9，指令里提到它也不能凭空造一个。
        assert!(mentioned_step_ids("按 s9 的做法", &task()).is_empty());
    }

    #[test]
    fn ordinary_words_are_not_mistaken_for_step_ids() {
        // `was3` / `xs1` 里的 s 不是步骤引用——前面接着字母就不该认
        assert!(mentioned_step_ids("the was3 value and xs1", &task()).is_empty());
        // `s3x` 同理，后面接着字母也不算
        assert!(mentioned_step_ids("s3x 这个变量", &task()).is_empty());
    }

    #[test]
    fn uppercase_mentions_work_too() {
        // 模型两种大小写都会写
        assert_eq!(mentioned_step_ids("按 S3 的清单", &task()), vec!["s3"]);
    }

    #[test]
    fn a_plain_word_starting_with_s_is_not_a_reference() {
        // `s` 后面不跟数字就不算
        assert!(mentioned_step_ids("see the source", &task()).is_empty());
    }

    #[test]
    fn text_without_any_mention_returns_nothing() {
        assert!(mentioned_step_ids("读一下 src/billing.py", &task()).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::StubDecider;
    use crate::think::ModelRouter;

    /// 剧本化 handler：按调用顺序返回预设回复，全程不碰网络。
    #[derive(Debug, Default)]
    struct ScriptedHandler {
        plans: Vec<String>,
        steps: Vec<Result<String, TaskError>>,
        observes: Vec<String>,
        plan_calls: usize,
        step_calls: usize,
        observe_calls: usize,
        /// 记录每次调用拿到的路由，用来断言"复杂走 DeepSeek、简单走 Agnes"
        seen_providers: Vec<String>,
        seen_thinking: Vec<bool>,
        seen_kinds: Vec<TaskKind>,
        /// 每次执行步骤时拿到的输入。用来断言"指令里点名的步骤，结果真的给了"。
        seen_inputs: Vec<Vec<(String, String)>>,
    }

    impl ScriptedHandler {
        fn with_plan(plan: &str) -> Self {
            Self {
                plans: vec![plan.to_string()],
                ..Default::default()
            }
        }

        fn push_step(mut self, out: Result<&str, TaskError>) -> Self {
            self.steps.push(out.map(str::to_string));
            self
        }
    }

    impl TaskHandler for ScriptedHandler {
        fn plan(
            &mut self,
            routing: &Routing,
            _run: &PlanRequest,
            records: &mut Vec<CallRecord>,
        ) -> Result<String, TaskError> {
            self.plan_calls += 1;
            self.seen_providers.push(routing.spec.provider.to_string());
            self.seen_thinking.push(routing.thinking);
            self.seen_kinds.push(routing.kind);
            records.push(
                CallRecord::new(
                    routing.spec.provider,
                    routing.spec.model,
                    crate::costlog::peak_now(),
                )
                .with_thinking(routing.thinking),
            );
            Ok(self.plans.remove(0))
        }

        fn execute_step(
            &mut self,
            routing: &Routing,
            run: &StepRequest,
            records: &mut Vec<CallRecord>,
        ) -> Result<String, TaskError> {
            let i = self.step_calls;
            self.step_calls += 1;
            self.seen_providers.push(routing.spec.provider.to_string());
            self.seen_thinking.push(routing.thinking);
            self.seen_kinds.push(routing.kind);
            self.seen_inputs.push(run.inputs.clone());
            records.push(
                CallRecord::new(
                    routing.spec.provider,
                    routing.spec.model,
                    crate::costlog::peak_now(),
                )
                .with_thinking(routing.thinking),
            );
            if i < self.steps.len() {
                match &self.steps[i] {
                    Ok(s) => Ok(s.clone()),
                    Err(e) => Err(match e {
                        TaskError::StepFailed { step, reason } => TaskError::StepFailed {
                            step: step.clone(),
                            reason: reason.clone(),
                        },
                        other => TaskError::Core(crate::CoreError::Ledger(other.to_string())),
                    }),
                }
            } else {
                Ok(format!("step-{i} 完成"))
            }
        }

        fn observe(
            &mut self,
            routing: &Routing,
            _run: &ObserveRequest,
            records: &mut Vec<CallRecord>,
        ) -> Result<String, TaskError> {
            let i = self.observe_calls;
            self.observe_calls += 1;
            self.seen_providers.push(routing.spec.provider.to_string());
            self.seen_thinking.push(routing.thinking);
            self.seen_kinds.push(routing.kind);
            records.push(
                CallRecord::new(
                    routing.spec.provider,
                    routing.spec.model,
                    crate::costlog::peak_now(),
                )
                .with_thinking(routing.thinking),
            );
            Ok(self.observes.get(i).cloned().unwrap_or_else(|| {
                r#"{"question":"怎么办？","options":["方案甲","方案乙"]}"#.to_string()
            }))
        }
    }

    fn engine<'a>(
        router: &'a ModelRouter,
        decider: &'a StubDecider,
        handler: &'a mut ScriptedHandler,
        store: &'a mut MemoryTaskStore,
        budget: Budget,
    ) -> Engine<'a, ScriptedHandler, MemoryTaskStore> {
        Engine::new(router, decider, handler, store, budget)
    }

    #[test]
    fn cost_records_reach_the_store() {
        // 记账必须真的接上——否则花了多少没人知道
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &["a"])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "两步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        assert_eq!(store.costs.len(), 3, "拆解 1 次 + 两步，共 3 次调用");
        assert_eq!(store.costs[0].provider, "deepseek", "拆解走深度档");
        assert!(store.costs[0].thinking, "拆解要开思考");
    }

    #[test]
    fn cost_records_do_not_pollute_the_task_projection() {
        // 成本事件不是任务事件，不该出现在任务投影里
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        let t = store.project().get("t1").cloned().unwrap();
        assert_eq!(t.state, TaskState::Done);
        assert_eq!(t.steps.len(), 1);
        assert!(!store.costs.is_empty());
    }

    fn plan_json(ids: &[(&str, &[&str])]) -> String {
        let steps: Vec<serde_json::Value> = ids
            .iter()
            .map(|(id, deps)| {
                serde_json::json!({
                    "id": id,
                    "instruction": format!("做 {id}"),
                    "depends_on": deps,
                    "kind": "generation",
                })
            })
            .collect();
        serde_json::json!({ "steps": steps }).to_string()
    }

    #[test]
    fn a_linear_task_runs_to_completion() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler =
            ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "把三件事依次做了").unwrap();

        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        assert_eq!(outcome.task.state, TaskState::Done);
        assert_eq!(outcome.task.tally(), (3, 0, 0));
        assert_eq!(outcome.advance, Advance::Finished);
        // 拆解 1 次 + 3 步
        assert_eq!(outcome.used_model_calls, 4);
    }

    #[test]
    fn independent_steps_all_get_run() {
        // 三个互不依赖的步骤：ready_steps 一次返回三个，顺序执行也要全跑完
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler =
            ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &[]), ("c", &[])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "三件独立的事").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.state, TaskState::Done);
        assert_eq!(outcome.task.tally().0, 3);
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let mut t = Task::new("t", "g", 0);
        t.steps = vec![
            Step::new("a", "A", TaskKind::Generation),
            Step::new("b", "B", TaskKind::Generation).with_depends_on(vec!["a".into()]),
        ];
        assert_eq!(ready_steps(&t), vec!["a"], "b 在 a 成功前不能跑");

        t.step_mut("a").unwrap().state = StepState::Succeeded;
        assert_eq!(ready_steps(&t), vec!["b"]);
    }

    #[test]
    fn blocked_step_is_not_ready() {
        let mut t = Task::new("t", "g", 0);
        let mut s = Step::new("a", "A", TaskKind::Generation);
        s.state = StepState::Blocked;
        t.steps = vec![s];
        assert!(ready_steps(&t).is_empty(), "等批准的步骤不能被自动选中");
    }

    #[test]
    fn a_cycle_is_caught_before_spending_anything() {
        // 成环必须在花钱之前查出来。注意 handler 一次都没被调用。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &["b"]), ("b", &["a"])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "互相依赖").unwrap();
        let r = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1");
        assert!(
            matches!(r, Err(TaskError::CircularDependency { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn budget_exhaustion_stops_instead_of_retrying() {
        // 预算用尽 → 停下等人，不是"再试一次"
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        // 5 步但预算只有 3 次调用：拆解用掉 1 次，只剩 2 次
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["b"]),
            ("d", &["c"]),
            ("e", &["d"]),
        ]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "五步任务").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_model_calls: 3,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.used_model_calls, 3, "不该超预算");
        assert_eq!(outcome.task.state, TaskState::AwaitingHuman);
        match outcome.advance {
            Advance::Waiting { reason } => assert!(reason.contains("预算"), "{reason}"),
            other => panic!("应停下等人，实际 {other:?}"),
        }
    }

    #[test]
    fn too_many_steps_is_rejected_before_running_any() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[
            ("a", &[]),
            ("b", &[]),
            ("c", &[]),
            ("d", &[]),
        ]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "四步").unwrap();
        let r = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_steps: 3,
                ..Default::default()
            },
        )
        .run("t1");
        assert!(
            matches!(r, Err(TaskError::TooManySteps { got: 4, limit: 3 })),
            "{r:?}"
        );
    }

    #[test]
    fn unparsable_plan_stops_and_names_the_raw_text() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan("我觉得可以这么做：先这样再那样");
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "目标").unwrap();
        let r = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1");
        match r {
            Err(TaskError::UnparsablePlan { raw, .. }) => {
                assert!(raw.contains("我觉得"), "必须带上原文: {raw}")
            }
            other => panic!("应报无法解析，实际 {other:?}"),
        }
    }

    #[test]
    fn a_failed_step_makes_dependents_skipped_not_failed() {
        // 跳过不是失败：它自己没出错
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &["a"])]))
            .push_step(Err(TaskError::StepFailed {
                step: "a".into(),
                reason: "超时".into(),
            }));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "两步").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                // 只允许试 1 次，避免测试里来回重试
                max_attempts_per_step: 1,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.step("a").unwrap().state, StepState::Failed);
        assert_eq!(
            outcome.task.step("b").unwrap().state,
            StepState::Skipped,
            "依赖失败应跳过而不是也判失败"
        );
        assert_eq!(outcome.task.tally(), (0, 1, 1));
    }

    #[test]
    fn a_failing_step_is_retried_up_to_the_limit() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]))
            .push_step(Err(TaskError::StepFailed {
                step: "a".into(),
                reason: "第一次超时".into(),
            }))
            .push_step(Ok("第二次成了"));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一步").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_attempts_per_step: 2,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();
        let s = outcome.task.step("a").unwrap();
        assert_eq!(s.state, StepState::Succeeded);
        assert_eq!(s.attempts, 2, "两次尝试都该被记上");
        assert_eq!(s.result.as_deref(), Some("第二次成了"));
    }

    #[test]
    fn a_task_where_every_step_succeeded_is_still_done() {
        // **改"有步骤失败就不叫完成"的时候，最容易顺手把正常路径弄坏。**
        //
        // 这个对照测试盯的就是它：每一步都成了，就必须还是 `Done`——
        // 否则"修复"变成了"所有任务都报卡住"，那比原来的 bug 更坏
        // （它会让所有人都不再相信"完成"这个状态）。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &["a"])]));
        handler.steps = vec![Ok("OK: 甲".into()), Ok("OK: 乙".into())];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "做两件事").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(
            outcome.task.state,
            TaskState::Done,
            "每一步都成了就该是完成"
        );
    }

    #[test]
    fn a_step_that_keeps_failing_settles_as_failed() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]))
            .push_step(Err(TaskError::StepFailed {
                step: "a".into(),
                reason: "一直失败".into(),
            }))
            .push_step(Err(TaskError::StepFailed {
                step: "a".into(),
                reason: "还是失败".into(),
            }));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一步").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_attempts_per_step: 2,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.step("a").unwrap().state, StepState::Failed);
        // **这条断言原来写的是 `Done`，理由"全部终态就是收工"——
        // 它把 bug 写成了规格。**
        //
        // `all_settled()` 的定义是"完成/失败/跳过**都算终态**"，
        // 所以它回答的是"还有没有在跑的步骤"，**不是"目标达成没有"**。
        //
        // 按旧断言，一步都没成、任务却报"完成"——**那正是"报告说做完了
        // 而实际没做"**（真机上 12 次里出现 2 次，D93）。
        //
        // **这不是"断言放宽"，是旧断言把 bug 当成了正确行为**，
        // 和 D50 那次 `calls_without_a_name_are_skipped` 是同一类。
        assert_eq!(
            outcome.task.state,
            TaskState::Stalled,
            "有步骤失败就不叫完成——就算别的步骤都尘埃落定了"
        );
    }

    #[test]
    fn decision_step_uses_the_local_decider() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding().with_choice("task_decision", "opt1");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        // 让这一步成为决策点
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个方案？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        handler.observes =
            vec![r#"{"question":"选哪个方案？","options":["保守方案","激进方案"]}"#.to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择的任务").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        assert_eq!(outcome.task.step("d").unwrap().state, StepState::Succeeded);
        let result = outcome.task.step("d").unwrap().result.clone().unwrap();
        assert!(result.contains("激进方案"), "应记录选中的那个: {result}");
        // **这个数字从 1 变成了 3。**
        //
        // 原来写"恰好一次"，前提是路由"确定性信号有把握就不问决策模型"。
        // 改成**每次都问**之后，拆解路由和步骤路由也各问一次，
        // 于是这里是 3。**这是设计变更的直接后果，不是回归。**
        //
        // **但它保护的意图必须守住**，所以把"至少问过一次"和
        // "选项生成只调一次模型"分开断言——后者原来就是独立的
        // 一条（`observe_calls`），现在仍然是精确的 `== 1`。
        //
        // 顺带记一条债：如果"每次会话输入问一次"就够了，
        // 那拆解和步骤路由这两次是**多余的**，该在入口处判一次然后复用。
        // 这一轮没做。
        assert!(decider.calls() >= 1, "决策点必须问本地决策模型");
        assert_eq!(handler.observe_calls, 1);
        // 落盘的是选项原文，不是 opt0/opt1 这种占位键
        assert!(!result.contains("opt0"), "台账里不该出现占位键: {result}");
    }

    #[test]
    fn decision_step_escalates_when_the_decider_abstains() {
        // 弃权 → 等人工，绝不默认选第一个
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding().with_choice("task_decision", "不在选项里");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.state, TaskState::AwaitingHuman);
        assert_eq!(
            outcome.task.step("d").unwrap().state,
            StepState::Pending,
            "弃权时步骤应退回待执行"
        );
        match outcome.advance {
            Advance::Waiting { reason } => assert!(reason.contains("人工"), "{reason}"),
            other => panic!("应升级人工，实际 {other:?}"),
        }
    }

    #[test]
    fn a_human_answer_settles_the_step_without_calling_any_model() {
        // **这条是 D44 的最小复现。**
        //
        // 真机上：决策步骤因为反复弃权把尝试次数用光成了 Failed，
        // 人给了答案、台账里也有 `human_answered`，可这一步还是被判失败。
        //
        // 这里从**干净的初始状态**验完整链路：人答过 → 步骤直接收尾，
        // 而且**一次模型调用都不花**（不用再生成选项、也不用问决策模型）。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 阈值含不含等于？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择").unwrap();
        store
            .write(
                "t1",
                EventKind::HumanAnswered,
                serde_json::json!({ "task": "t1", "step": "d", "answer": "含等于，用 >=" }),
            )
            .unwrap();

        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        let st = outcome.task.step("d").unwrap();
        assert_eq!(
            st.state,
            StepState::Succeeded,
            "人已经拍板了，这一步就该收尾"
        );
        let result = st.result.clone().unwrap_or_default();
        assert!(result.contains("含等于"), "产物里要有人的决定: {result}");
        assert!(
            result.contains("human"),
            "要标出这是人定的，不是模型选的: {result}"
        );
        // **真正该证的是"没去生成选项"。**
        //
        // 一开始我断言的是 `used_model_calls == 0`，它挂了——但那 1 次是
        // **拆解**花的（引擎先拆解再执行），不是决策步骤花的。
        // 断言写宽了会把正确行为判成错的，而这里要盯的是
        // `observe`：决策步骤靠它生成选项，人答过之后**一次都不该调**。
        assert_eq!(
            handler.observe_calls, 0,
            "人答过之后不该再去生成选项——那正是死循环里被反复浪费的调用"
        );
    }

    #[test]
    fn a_human_answer_revives_a_step_whose_retries_ran_out() {
        // **真机上就是这一种。** 位置很要紧：检查必须在"重试次数用尽"
        // **之前**——否则人给了答案，这一步还是被判失败，
        // 因为引擎先看尝试次数，根本走不到"有没有人答过"。
        //
        // **重试计数是给模型用的，不是给人的。**
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 阈值含不含等于？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择").unwrap();

        // 把尝试次数耗光：模拟"决策模型反复弃权"
        let budget = Budget {
            max_attempts_per_step: 2,
            ..Default::default()
        };
        for _ in 0..2 {
            let _ = engine(&router, &decider, &mut handler, &mut store, budget).run("t1");
            let _ = store.write(
                "t1",
                EventKind::StepPending,
                serde_json::json!({
                    "task": "t1", "step": "d",
                    "instruction": "decide: 阈值含不含等于？",
                    "kind": "analysis", "depends_on": [],
                }),
            );
        }

        // 现在人给出答案
        store
            .write(
                "t1",
                EventKind::HumanAnswered,
                serde_json::json!({ "task": "t1", "step": "d", "answer": "含等于" }),
            )
            .unwrap();
        store
            .write(
                "t1",
                EventKind::StepPending,
                serde_json::json!({
                    "task": "t1", "step": "d",
                    "instruction": "decide: 阈值含不含等于？",
                    "kind": "analysis", "depends_on": [],
                }),
            )
            .unwrap();

        // **任务此时停在 AwaitingHuman，要先放回执行中**——
        // 这正是 CLI 里 `resume` 做的那一步。测试里漏了它，
        // 引擎根本不会去跑任何步骤，于是"步骤还是 Pending"。
        store
            .write(
                "t1",
                EventKind::TaskStateChanged,
                serde_json::json!({ "task": "t1", "state": "running" }),
            )
            .unwrap();

        let outcome = engine(&router, &decider, &mut handler, &mut store, budget)
            .run("t1")
            .unwrap();
        let st = outcome.task.step("d").unwrap();
        assert_eq!(
            st.state,
            StepState::Succeeded,
            "人答过之后，尝试次数用尽也不该判失败"
        );
        assert!(
            st.result.clone().unwrap_or_default().contains("含等于"),
            "产物里要有人的决定"
        );
    }

    #[test]
    fn a_step_gets_the_result_of_a_step_its_instruction_names() {
        // **真机上踩到的**：第 9 步的指令写着「按 s3 的边界清单逐条核对」，
        // 而 `depends_on` 里没写 s3。引擎只把**声明过的**依赖当输入，
        // 于是模型手上没有 s3 的内容，只能如实回「无法获取 s3 的边界清单原文」——
        // **一步白做，后面依赖它的也被跳过。**
        //
        // 计划是模型写的：它引用一个步骤却忘了声明依赖，是很自然的事。
        // 指令里点名了谁就把谁的结果给它——这是补一个漏写的依赖。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan("");
        handler.plans = vec![serde_json::json!({
            "steps": [
                { "id": "s1", "instruction": "查一下有哪些测试", "depends_on": [], "kind": "lookup" },
                // **注意 depends_on 是空的**，但指令点名了 s1
                { "id": "s2", "instruction": "按 s1 的清单逐条核对", "depends_on": [], "kind": "analysis" }
            ]
        })
        .to_string()];
        handler.steps = vec![Ok("OK: 清单是：a、b".into()), Ok("OK: 核对完了".into())];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "核对").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        assert_eq!(handler.seen_inputs.len(), 2, "两步都该被执行");
        let s2_inputs = &handler.seen_inputs[1];
        assert!(
            s2_inputs.iter().any(|(id, _)| id == "s1"),
            "指令点名了 s1，s1 的结果就必须在输入里——否则模型只能回「拿不到」：{s2_inputs:?}"
        );
        assert!(
            s2_inputs
                .iter()
                .any(|(id, r)| id == "s1" && r.contains("清单是")),
            "给的是 s1 的真实产出：{s2_inputs:?}"
        );
    }

    #[test]
    fn an_abstention_survives_the_real_ledger_store() {
        // **这条测试盯着一个真机上的 bug，而我原来的测试抓不到它。**
        //
        // `DecisionAsked` 在"必须落在审计边界内"的名单里
        // （`ledger.rs:150`），而任务路径上**没有任何 `begin_span`**。
        // `Engine::escalate` 直接写它 → `LedgerTaskStore::write` 拦下 →
        // `TaskError::Core` 被抛出去，**连"等人工"都走不到**。
        //
        // 为什么原来的测试全绿：**引擎测试用的是 `MemoryTaskStore`，
        // 它不检查边界**；而真实 CLI 用的是 `LedgerTaskStore`（`main.rs:550`）。
        // **测试和生产用了两套不同的器件**——和 D93 那次同一族。
        //
        // 所以这条测试的价值不在断言本身，**在它用的是真台账**。
        let dir = std::env::temp_dir().join(format!("yunxi-span-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ledger.jsonl");
        let _ = std::fs::remove_file(&path);
        let ledger = crate::ledger::Ledger::open(&path).expect("开台账");
        let mut store = LedgerTaskStore::new(ledger);

        let router = ModelRouter::default();
        let decider = StubDecider::succeeding().with_choice("task_decision", "不在选项里");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        handler.steps = vec![Ok("OK: 完成了".into())];

        create_task(&mut store, "t1", "需要选择").expect("建任务");
        // **不用 `engine()` 那个助手**——它的返回类型写死了
        // `MemoryTaskStore`，而这条测试的要害恰恰是**换成真台账**。
        // 这正是那条 bug 藏了这么久的原因。
        let mut eng = Engine::new(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        );
        let out = eng.run("t1").expect("弃权不该让引擎报错——它该转成等待人工");

        // 弃权 = 等人工。**不是 Core 错误**。
        assert!(
            matches!(out.advance, Advance::Waiting { .. }),
            "弃权后该是等待，实际 {:?}",
            out.advance
        );
        // 而且台账里那条审计事件真的写进去了
        let ledger = crate::ledger::Ledger::open(&path).expect("重开台账");
        let asked = ledger
            .events()
            .iter()
            .filter(|e| e.kind == EventKind::DecisionAsked)
            .count();
        assert_eq!(asked, 1, "弃权该留下一条 DecisionAsked，而且是在真台账里");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_abstention_is_recorded_in_the_ledger() {
        // **决策模型接管之后，人从回路里退出了。**
        //
        // 以前每个 `decide:` 都问人，人看到问题本身就是一道检查；
        // 现在它自己拍板，"它拍了什么、凭什么"只能靠台账。
        //
        // 拍板那条路本来就留了痕（`StepSucceeded` 的 result 里有
        // choice / rationale）。**弃权这条没有**——`why` 只进了返回值，
        // 谁没接住就没了。而弃权恰恰是最需要解释的那一种：
        // "它为什么不敢定"直接说明那一类问题它判不了。
        let router = ModelRouter::default();
        // **"选了不在选项里的东西"就是弃权那条路。**
        // 用 `failing` 不行——那是"决策器坏了"，走的是降级，
        // 而这里要测的是"决策器能跑，但它不敢定"。
        let decider = StubDecider::succeeding().with_choice("task_decision", "不在选项里");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        // **步骤的指令必须真的以 `decide:` 开头**——否则引擎根本不走
        // 决策那条路，测试会挂在一个和被测行为无关的地方。
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        handler.steps = vec![Ok("OK: 完成了".into())];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "做一个决定").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        let events = store.events();
        let asked = events
            .iter()
            .filter(|e| e.kind == EventKind::DecisionAsked)
            .collect::<Vec<_>>();
        assert_eq!(asked.len(), 1, "弃权该留下一条 DecisionAsked");
        let d = &asked[0].data;
        assert_eq!(d["task"], serde_json::json!("t1"));
        assert_eq!(d["step"], serde_json::json!("d"));
        // **问题原文要在。** 只记"某一步弃权了"等于没记——
        // 事后要看的是"它当时在纠结什么"。
        assert!(
            d["question"].as_str().is_some_and(|q| !q.is_empty()),
            "问题原文必须留下：{d}"
        );
        // **候选也要在**：是"没有好选项"还是"选项都差不多"，从候选能看出来
        assert!(
            d["options"].as_array().is_some_and(|a| !a.is_empty()),
            "{d}"
        );
        // **说清是"谁"没定下来。** "决策模型弃权"和"选项少于两个"
        // 是两种完全不同的原因，混在一起就查不出东西。
        assert!(
            d["why"].as_str().is_some_and(|w| !w.is_empty()),
            "要说清为什么升级人工：{d}"
        );
    }

    #[test]
    fn abstaining_does_not_burn_retry_attempts() {
        // **这是真机上任务变成"卡住"的根因。**
        //
        // 决策模型弃权后，步骤回到待执行，但**尝试次数照样累加**。
        // 弃权两次之后次数用光 → 步骤硬判失败 → 依赖它的全跳过 →
        // 任务从"等人工"变成"卡住"，人再想答也没机会了。
        //
        // **弃权是"我需要人拍板"，不是"这次尝试失败了"。**
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding().with_choice("task_decision", "不在选项里");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择").unwrap();

        let budget = Budget {
            max_attempts_per_step: 2,
            ..Default::default()
        };
        // 连着弃权 3 次——**超过重试上限**。每次都该是"等人工"，
        // 而不是把步骤判死。
        for round in 1..=3 {
            let outcome = engine(&router, &decider, &mut handler, &mut store, budget)
                .run("t1")
                .unwrap();
            assert_eq!(
                outcome.task.state,
                TaskState::AwaitingHuman,
                "第 {round} 次弃权后该是等人工，实际 {}",
                outcome.task.state.label()
            );
            assert_eq!(
                outcome.task.step("d").unwrap().state,
                StepState::Pending,
                "第 {round} 次弃权后步骤该退回待执行"
            );
        }
    }

    #[test]
    fn decision_step_escalates_when_the_decider_errors() {
        let router = ModelRouter::default();
        let decider = StubDecider::failing("sidecar 没起来");
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("d", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "d", "instruction": "decide: 选哪个？", "depends_on": [], "kind": "analysis" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "需要选择").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.state, TaskState::AwaitingHuman);
    }

    // ---- 路由在任务框架里的接线 ----

    #[test]
    fn simple_steps_go_to_agnes_complex_steps_go_to_deepseek() {
        // 使用者的要求：复杂任务给 DeepSeek，简单任务给 Agnes
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let plan = serde_json::json!({
            "steps": [
                { "id": "s1", "instruction": "查一下今天的日程", "depends_on": [], "kind": "lookup" },
                { "id": "s2", "instruction": "分析这周的邮件并对比上周的变化趋势，评估是否需要调整",
                  "depends_on": ["s1"], "kind": "analysis" }
            ]
        })
        .to_string();
        let mut handler = ScriptedHandler::with_plan(&plan);
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "先查日程再分析邮件").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        // 第一次调用是拆解，之后依次是 s1、s2
        assert_eq!(handler.seen_kinds[1], TaskKind::Lookup);
        assert_eq!(handler.seen_kinds[2], TaskKind::Analysis);
        // **原来是 `"agnes"`。** 用户改了规格：**真实任务一律走 DeepSeek**，
        // 哪怕这一步只是简单查找。代价要说明白——这一步现在要花钱了。
        //
        // 动机有真机证据：Agnes 免费档 10 RPM，一次真实任务连着几步
        // 必然撞限流（`e2e_allmodules` 那次连续 9 次"等 N ms 后重发"，
        // 然后任务卡在 s10 推不动）。
        assert_eq!(
            handler.seen_providers[1], "deepseek",
            "真实任务一律走 DeepSeek（{:?}）",
            handler.seen_providers
        );
        assert!(!handler.seen_thinking[1], "查找类不该开思考");
        assert_eq!(
            handler.seen_providers[2], "deepseek",
            "分析类需要多次调用 → DeepSeek（{:?}）",
            handler.seen_providers
        );
        assert!(handler.seen_thinking[2], "分析类必须开思考");
    }

    #[test]
    fn planning_itself_routes_to_a_reasoning_capable_model() {
        // 拆解要推理，不能落在"服务端默认"的免费档上
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一件简单的事").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(handler.seen_kinds[0], TaskKind::Planning);
        assert!(
            handler.seen_thinking[0],
            "拆解必须开思考，否则拆不出像样的步骤"
        );
        assert_eq!(handler.seen_providers[0], "deepseek");
    }

    #[test]
    fn step_routing_is_recorded_in_the_ledger() {
        // 留痕是为了事后能解释账单
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        let t = store.project().get("t1").cloned().unwrap();
        let rec = t.step("a").unwrap().routing.clone().expect("应记录路由");
        assert!(!rec.reason.is_empty(), "理由必须能解释账单");
        assert!(!rec.model.is_empty());
    }

    #[test]
    fn state_survives_a_restart() {
        // 台账是唯一事实来源：换个 store 实例重放事件，状态必须一样。
        // 预算掐在中间，制造一个"没跑完"的任务。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let plan = plan_json(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let mut handler = ScriptedHandler::with_plan(&plan);
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "三步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_model_calls: 2,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();

        let before = store.project().get("t1").cloned().unwrap();
        assert_eq!(before.state, TaskState::AwaitingHuman);
        assert_eq!(before.step("a").unwrap().state, StepState::Succeeded);

        // "重启"：只用事件流重新投影
        let after = task_from_events(store.events()).get("t1").cloned().unwrap();
        assert_eq!(after, before, "重放事件必须得到同一个任务");
    }

    #[test]
    fn resume_continues_from_where_it_stopped() {
        // 续跑：把预算放宽，同一个事件流接着跑应该能到完成
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let plan = plan_json(&[("a", &[]), ("b", &["a"]), ("c", &["b"])]);
        let mut handler = ScriptedHandler::with_plan(&plan);
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "三步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget {
                max_model_calls: 2,
                ..Default::default()
            },
        )
        .run("t1")
        .unwrap();

        // 续跑前把状态从"等人工"放回执行中（模拟人处理完了）
        store
            .write(
                "t1",
                EventKind::TaskStateChanged,
                serde_json::json!({ "task": "t1", "state": "running" }),
            )
            .unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.state, TaskState::Done);
        assert_eq!(outcome.task.tally().0, 3);
        // a 不该被重跑：成功过的步骤不回到待执行
        assert_eq!(outcome.task.step("a").unwrap().attempts, 1);
    }

    #[test]
    fn a_stalled_task_says_stalled_not_running() {
        // 没有可跑步骤又没全部终态 → 卡住。不能表现成"还在跑"。
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        handler.plans = vec![serde_json::json!({
            "steps": [{ "id": "a", "instruction": "做 a", "depends_on": [], "kind": "generation" }]
        })
        .to_string()];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "一步").unwrap();
        engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        // 手工把 a 置回 Blocked：既不是终态，也不在 ready 里
        store
            .write(
                "t1",
                EventKind::StepRunning,
                serde_json::json!({ "task": "t1", "step": "a" }),
            )
            .unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        // Running 的步骤不该被抢着再跑一遍
        assert_eq!(outcome.task.step("a").unwrap().state, StepState::Running);
    }

    #[test]
    fn summarize_reports_failures_not_just_successes() {
        // 只报成功的部分会让人以为任务做完了
        let mut t = Task::new("t", "目标", 0);
        let mut a = Step::new("a", "做 a", TaskKind::Generation);
        a.state = StepState::Succeeded;
        a.result = Some("成了".into());
        let mut b = Step::new("b", "做 b", TaskKind::Generation);
        b.state = StepState::Failed;
        b.result = Some("超时".into());
        t.steps = vec![a, b];
        let s = summarize(&t);
        assert!(s.contains("失败 1"), "{s}");
        assert!(s.contains("超时"), "失败原因必须出现: {s}");
        assert!(s.contains("成了"), "{s}");
    }

    #[test]
    fn step_table_shows_dependencies() {
        let mut t = Task::new("t", "g", 0);
        t.steps = vec![
            Step::new("a", "A", TaskKind::Generation),
            Step::new("b", "B", TaskKind::Generation).with_depends_on(vec!["a".into()]),
        ];
        let table = step_table(&t);
        assert_eq!(table.len(), 2);
        assert_eq!(table[0].2, "-", "无依赖应显示为 -");
        assert_eq!(table[1].2, "a");
    }

    #[test]
    fn empty_inputs_is_empty() {
        assert!(empty_inputs().is_empty());
    }

    #[test]
    fn thresholds_are_used_for_step_profiles() {
        // 步骤画像复用路由的阈值，不要在任务框架里另起一套
        const { assert!(crate::think::thresholds::MULTI_CALL >= 2) };
        let long = "x".repeat(crate::think::thresholds::SHORT_PROMPT_CHARS + 10);
        assert_eq!(TaskKind::classify(&long), None);
    }

    // ---- 步骤产出判定 ----

    #[test]
    fn ok_mark_is_done_and_stripped() {
        assert_eq!(
            classify_step_output("OK: 北京今天 22℃"),
            StepVerdict::Done("北京今天 22℃".into())
        );
        // 大小写、全角冒号、行首空白都要容忍
        assert_eq!(
            classify_step_output("  ok：做完了"),
            StepVerdict::Done("做完了".into())
        );
    }

    #[test]
    fn blocked_mark_is_blocked() {
        assert_eq!(
            classify_step_output("BLOCKED: 我没有联网能力"),
            StepVerdict::Blocked("我没有联网能力".into())
        );
        assert_eq!(
            classify_step_output("blocked：缺 API key"),
            StepVerdict::Blocked("缺 API key".into())
        );
    }

    #[test]
    fn a_mark_in_the_middle_does_not_count() {
        // 只在开头认标记。正文里提到 BLOCKED 这个词不算"这一步被阻塞"
        match classify_step_output("先说明一下。BLOCKED: 这是文档里的示例") {
            StepVerdict::Unmarked(_) => {}
            other => panic!("正文里的标记不该被当成开头标记: {other:?}"),
        }
    }

    #[test]
    fn unmarked_output_is_not_treated_as_failure() {
        // 内容确实在那儿，判失败会让它白重试一次
        match classify_step_output("北京今天 22℃") {
            StepVerdict::Unmarked(t) => assert_eq!(t, "北京今天 22℃"),
            other => panic!("无标记应按未标记处理: {other:?}"),
        }
    }

    /// 回归：**模型说"做不了"的时候，不能记成成功。**
    ///
    /// 真实运行里的三步任务，前两步模型都回了"做不了。我没有联网能力……"，
    /// 而引擎把它们记成成功，任务报告"完成 2 步"——实际什么都没办成。
    /// 报告说做完了而实际没做，是最坏的一种错。
    #[test]
    fn a_blocked_step_is_recorded_as_failed_not_succeeded() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[]), ("b", &["a"])]));
        handler.steps = vec![
            Ok("BLOCKED: 我没有联网能力，无法获取实时天气".to_string()),
            Ok("不该跑到这里".to_string()),
        ];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "查天气再比较").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();

        let a = outcome.task.step("a").unwrap();
        assert_eq!(a.state, StepState::Failed, "说了做不了就不能算成功");
        assert!(
            a.result.as_deref().unwrap_or("").contains("联网"),
            "失败原因要留下模型的原话: {:?}",
            a.result
        );
        // 依赖它的步骤应该被跳过，而不是拿着"做不了"当输入硬跑
        assert_eq!(outcome.task.step("b").unwrap().state, StepState::Skipped);
        assert_eq!(outcome.task.tally(), (0, 1, 1));
        assert!(
            handler.step_calls == 1,
            "被阻塞的步骤不该重试（模型已经说了缺什么）"
        );
    }

    #[test]
    fn an_unmarked_step_still_succeeds_but_is_flagged() {
        // 老模型不认标记也不该把任务全判死；但台账里要看得见"没标记"
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        handler.steps = vec![Ok("北京今天 22℃".to_string())];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "查天气").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        assert_eq!(outcome.task.step("a").unwrap().state, StepState::Succeeded);

        let flagged = store.events().iter().any(|e| {
            e.kind == EventKind::StepSucceeded
                && e.data.get("marked").and_then(|v| v.as_bool()) == Some(false)
        });
        assert!(flagged, "无标记的产出要在台账里留痕");
    }

    #[test]
    fn a_marked_ok_step_records_the_stripped_text() {
        let router = ModelRouter::default();
        let decider = StubDecider::succeeding();
        let mut handler = ScriptedHandler::with_plan(&plan_json(&[("a", &[])]));
        handler.steps = vec![Ok("OK: 22℃".to_string())];
        let mut store = MemoryTaskStore::new();
        create_task(&mut store, "t1", "查天气").unwrap();
        let outcome = engine(
            &router,
            &decider,
            &mut handler,
            &mut store,
            Budget::default(),
        )
        .run("t1")
        .unwrap();
        // 标记本身不该进结果正文——它只是协议
        assert_eq!(
            outcome.task.step("a").unwrap().result.as_deref(),
            Some("22℃")
        );
    }
}
