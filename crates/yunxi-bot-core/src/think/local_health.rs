//! 本地小模型槽位的健康判定与兜底：**没就绪 / 加载失败 / 状态不认识 → 这一轮改走 Agnes**。
//!
//! ## 为什么需要这一层
//!
//! D128 把 provider→客户端那处映射修好之后，请求**真的**打到
//! `127.0.0.1:17872` 了——于是"它可能根本没在跑"第一次成了一个真问题。
//!
//! | 情况 | `/health` 说什么 | 后果 |
//! |---|---|---|
//! | sidecar 没起来 | 连接被拒 | 这一轮必然失败 |
//! | 正在加载 8 GB 权重 | `loading`（几十秒） | **第一句话必然失败**，而那是使用者刚打完字的时候 |
//! | 权重被空闲释放了 | `idle` | 权重不在显存里，这一轮要等它重新加载 |
//! | 上次加载失败 | `degraded` + `detail` | **一直**失败——它不重试 |
//!
//! 最后一种最要命：`_load_once` 见到 `_model_error` 就直接返回，所以那是一个
//! **不会自愈**的状态，而它看上去和"还没开始加载"一模一样。前几种靠等能好，
//! 这一种等多久都一样——使用者唯一能做的判断是"重启 sidecar"，而前提是
//! **他得先知道**。
//!
//! 要求因此很直接：**本地模型挂了 / 还没就绪 → 这一轮走 Agnes**，
//! 而且必须看得见。静默的替换正是 D128 那一类 bug 的形状：
//! 功能还在，只是退化成了另一个模型，看起来一切正常。
//!
//! > `idle` 也归到"还没就绪"这一边：sidecar 那边写着"下一个请求会自己把它
//! > 拉回来"，而那句"拉回来"**是这一轮要等几十秒**的意思——所以这一轮不等。
//!
//! ## 回落之后还得有人把模型拉回来，否则本地槽位是**静默死掉**的
//!
//! 空闲回收之后 `/health` 报 `idle`。只回落、不催热的话：这一轮走 Agnes，
//! 下一轮探还是 `idle`、于是再回落……**那个本地模型再也不会被用到**，
//! 而每一处显示都正常（端点换了、账也没多花、没有一处报错）。这正是 D128
//! 那一类：功能还在，只是退化了。
//!
//! 反方向（`idle` 就留在本地）同样不行：sidecar 的 `do_POST` 在
//! `_model is None` 时**同步加载**，8 GB 要十几秒——使用者盯着一个不动的
//! 光标，而那正是流式输出那一轮工作要消掉的东西。
//!
//! 所以走第三条：**这一轮走 Agnes（快），同时 fire-and-forget 地
//! `POST /warmup`**（sidecar 立刻答 202，权重在它自己的后台线程里加载），
//! **下一轮就回到本地**。该不该催由 [`LocalHealth::needs_warmup`] 判，
//! 和"这一轮走谁"一起从 [`decide`] 出来（见 [`LocalSlot::Fallback`] 的 `warm`）。
//!
//! ## 判定是纯的，探测只有薄薄一层
//!
//! 六种健康状态 →"留本地 / 改走谁"这件事**不碰任何 I/O**（[`decide`]），
//! 所以六种情况都能在单元测试里直接构造出来。真去连端口的只有 [`probe`]
//! 一个函数，300 ms 超时，复用 [`crate::decide::http`] 那个回环客户端
//! （它已经带了回环限制和回环专属的短连接超时）。
//!
//! ## 非本地槽位一律不动
//!
//! 健康状态说的是 `127.0.0.1:17872` 上那个 Python 进程。
//! DeepSeek / Agnes 的端点和它没有任何关系，所以 [`decide`] 对非本地 spec
//! **一律返回 [`LocalSlot::Keep`]**——哪怕状态是"连不上"。
//! **回落的判据不能溢出到别的槽位**，那会变成"本地没起来，于是连远端也不用"。

use crate::think::router::{ModelSpec, Routing};

/// `/health` 探测的超时。
///
/// ## 为什么是 300 ms
///
/// 这一跳在**每一轮对话的关键路径上**，而健康的 sidecar 大约 1 ms 就答完
/// （它只是读两个全局变量）。300 ms 只在"它在、但卡住了"这种情况下花掉，
/// 而那种情况和"没起来"一样该回落——**为它多等一秒，是让每一轮都白等一秒**。
///
/// 这个数还有一层来源：`decide::http` 里量出来 Windows 连一个没人监听的
/// 回环端口要**约 2070 ms** 才回"积极拒绝"，比默认超时还晚。所以本机服务
/// 的探测不该用默认超时，300 ms 足够区分"要么立刻接受、要么就是没起来"。
pub const HEALTH_TIMEOUT_MS: u64 = 300;

/// `/warmup` 那一脚的超时。
///
/// 和 [`HEALTH_TIMEOUT_MS`] 恰好同一个数，但**理由不同、所以是另一个常量**：
/// 那边是"健康状态要立刻答"，这边是"叫一声就走"。sidecar 收到就往后台线程
/// 一扔、立刻答 202（见它的 `_warmup()`），所以这 300 ms 只在"它在、但卡住了"
/// 的时候花掉——**催热是顺手做的事，不该比探测本身还贵**。
pub const WARMUP_TIMEOUT_MS: u64 = 300;

/// sidecar 报出来的健康状态。
///
/// **可用/不可用的几种状态全都是 HTTP 200**（见 `sidecar/local_llm_server.py`
/// 的 `do_GET`）——状态在**正文里**，所以"能连上"远远不等于"能用"。
/// 只看状态码会把 `degraded` 当成健康，而那恰恰是最该回落的一种。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalHealth {
    /// `{"status":"ok"}`：权重在显存里，这一轮就用它。
    Ready,
    /// `{"status":"loading"}`：还在加载（几 GB、几十秒）。
    ///
    /// **不等。** 这一轮等它加载完，使用者看到的就是"打完字之后卡住几十秒"；
    /// 而换成 Agnes 这一轮立刻就有结果。慢不是这条路的预期——**失败才是**。
    Loading,
    /// `{"status":"idle"}`：曾经加载好，**权重被空闲释放了**（`/unload` 或空闲回收）。
    ///
    /// sidecar 那边写着"这不是错误，也不是在加载：下一个请求会自己把它拉回来"。
    /// 但那句"拉回来"是**这一轮要等几十秒**的意思——和 [`Self::Loading`]
    /// 对使用者是同一件事，所以判定也一样：这一轮走 Agnes，
    /// 同时由 [`Self::needs_warmup`] 决定**顺手踢一脚 `/warmup`**，
    /// 好让下一轮回到本地。
    ///
    /// **单独一支而不是并进 [`Self::Unexpected`]**：理由是给人看的。
    /// "状态不认识（idle）"会让操作者去查一个根本没坏的东西，
    /// 而真实情况（权重被释放了）指向的动作是"让 /warmup 把它热上"。
    ///
    /// 哪天要改"idle 该怎么办"，改的是 [`decide`] 里那一行和
    /// [`Self::needs_warmup`]，不是这里。
    Idle,
    /// `{"status":"degraded","detail":"..."}`：上一次加载失败了，而且不会重试。
    ///
    /// 带着 `detail`（比如 `ModuleNotFoundError: No module named 'torch'`），
    /// 因为**那是唯一能解释"为什么它一直不好"的东西**。
    Degraded(String),
    /// 连不上 / 超时 / 非 2xx：多半是 sidecar 没起来。
    Unreachable(String),
    /// 答了 200，但状态不是上面任何一种（或者根本不是 JSON）。
    ///
    /// **不认识就回落，不猜"大概没事"。** 猜"没事"的方向是这一轮必然失败，
    /// 而猜错的方向只需要多花几分钱——两边代价不对称，所以往能干活的那边倒。
    Unexpected(String),
}

impl LocalHealth {
    /// 这一种状态该不该顺手踢一脚 `/warmup`（[`WARMUP_TIMEOUT_MS`]，不等结果）。
    ///
    /// ## 为什么只有 `idle` / `loading`
    ///
    /// 这两种是**"权重不在显存里、而它没坏"**——催一下就有用：sidecar 起一个
    /// 后台线程把权重搬回去，下一轮 `/health` 就是 `ok` 了。
    ///
    /// 另外几种催了也没用，所以**不催**：
    ///
    /// - [`Self::Degraded`]：sidecar 的 `_load_once` 见到 `_model_error` 就
    ///   立刻返回，**它不重试**。踢一百次和踢一次一样，只是白等一次超时
    /// - [`Self::Unreachable`]：没有进程在监听，POST 出去只能等到超时
    /// - [`Self::Unexpected`]：**我们不知道那是什么**。对一个不认识的进程
    ///   动手，比不动手更坏
    ///
    /// ## 这一位为什么不能省
    ///
    /// `idle` 只回落不催的话，下一轮还是 `idle`、再下一轮还是——
    /// **本地槽位就静默地永远死了**：每一处显示都正常，只有那个模型
    /// 再也不会被用到。那是 D128 那一类的形状，而它没有任何症状。
    pub fn needs_warmup(&self) -> bool {
        matches!(self, LocalHealth::Idle | LocalHealth::Loading)
    }
}

/// 这一轮用哪个槽位。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalSlot {
    /// 本地就绪，照路由走。
    Keep,
    /// 本地用不了：改走 `to`，`reason` 是给操作者看的中文理由。
    ///
    /// `reason` **不含句号**——打印那一层自己补，这样它既能进 stderr
    /// 又能原样进台账/`Routing::reason`，不必有人去改标点。
    Fallback {
        to: ModelSpec,
        reason: String,
        /// 要不要顺手 `POST /warmup` 把权重热上（**不等它**）。
        ///
        /// 这一位是"idle 会不会把本地槽位永久关掉"的分水岭：`idle` / `loading`
        /// 只回落不催的话，下一轮探还是 `idle`、于是再回落——**那个模型再也
        /// 不会被用到，而所有显示都正常**（走的还是那个免费端点，账也没多花）。
        /// 判据见 [`LocalHealth::needs_warmup`]。
        warm: bool,
    },
}

/// 这条 spec 是不是那个本地槽位。
///
/// 判据用 `provider` 而不是 `base_url`：**`provider` 才是路由和台账用的身份**，
/// 而 `base_url` 是可以变的（换个端口、换个 sidecar 进程，还是同一个本地槽位）。
pub fn is_local(spec: &ModelSpec) -> bool {
    spec.provider == ModelSpec::LOCAL_QWEN.provider
}

/// 从本地槽位的 `base_url` 推出 `/health` 地址。
///
/// **这里不写端口号。** 端口已经有一份出处：`ModelSpec::LOCAL_QWEN.base_url`
/// （`crates/yunxi-bot-cli/src/main.rs` 的 `LOCAL_MODEL_PORT` 是"把 sidecar
/// 拉起来"要用的那一份，两边注释互相指认）。再写一个数字就是第二份出处，
/// 改一处漏一处——**那正是 D128 的成因形状**。
///
/// `base_url` 以 `/v1` 结尾（`OpenAiThinker` 自己拼 `/chat/completions`），
/// 而 `/health` 挂在根上，所以这里砍掉最后那段路径再接上去。
pub fn health_endpoint(base_url: &str) -> String {
    format!("{}/health", slot_root(base_url))
}

/// 从本地槽位的 `base_url` 推出 `/warmup` 地址。
///
/// 和 [`health_endpoint`] **同源**（同一个 [`slot_root`]）：探的是哪台机器、
/// 催的就是哪台机器上的同一个进程，而端口仍然只有 `LOCAL_QWEN.base_url`
/// 一份出处——这里同样不写 17872。
pub fn warmup_endpoint(base_url: &str) -> String {
    format!("{}/warmup", slot_root(base_url))
}

/// `base_url` 里的"根"：砍掉尾部斜杠和 `/v1` 那段路径。
///
/// 两个端点地址都从它拼出来，所以"/v1 要砍掉"这条规矩**只有一份**——
/// 分开写两份的话，改了 `health_endpoint` 忘了 `warmup_endpoint`，
/// 表现是"催热 404"，而 404 在那条 fire-and-forget 的路上**没人会看见**。
fn slot_root(base_url: &str) -> &str {
    let base = base_url.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base)
}

/// 从 `ModelSpec` + 健康状态决定这一轮走谁。**纯函数：不碰网络、不碰台账。**
///
/// 纯是刻意的：这几种健康状态在真机上凑不齐（`degraded` 要先把 torch 弄坏），
/// 而这条判据恰恰是"本地挂了会不会静默降级"的守门人——
/// **它必须能在一个不联网的测试里被逐条钉住。**
///
/// 回落的结论里还带着 `warm`（要不要顺手催一下 `/warmup`）：
/// **"这一轮走谁"和"下一轮还能不能回到本地"是同一次判定的两半**，
/// 分开判必然漂移——而漏掉催热的那一半，就是本地槽位静默死掉。
pub fn decide(spec: &ModelSpec, health: &LocalHealth) -> LocalSlot {
    // 非本地槽位：**连看都不看健康状态**，原样放行
    if !is_local(spec) {
        return LocalSlot::Keep;
    }
    // 该不该催热和"这一轮走谁"是**同一条判据的两半**，所以一起从这里出去。
    // 分成两个函数（先 decide 再问 needs_warmup）的话，加状态时很容易只改一边——
    // 而漏掉的那一半正是"本地槽位静默死掉"。
    let warm = health.needs_warmup();
    let reason = match health {
        LocalHealth::Ready => return LocalSlot::Keep,
        LocalHealth::Loading => "还没就绪（loading，权重还在加载），这一轮走 Agnes".to_string(),
        // `idle` 和 `loading` 对使用者是同一件事：权重都不在显存里，
        // 这一轮要等几十秒。区别只在**谁去把它拉回来**——而那不是这一轮该等的。
        LocalHealth::Idle => "权重被空闲释放了（idle），这一轮走 Agnes".to_string(),
        LocalHealth::Degraded(detail) => {
            format!(
                "加载失败过（degraded：{}），这一轮走 Agnes",
                snippet(detail)
            )
        }
        // 这里不再自己编"多半是没起来"：`HttpError` 的文案本来就带
        // 地址和该查什么（"连不上（回环地址 300 ms 内没接受连接）——多半是这个服务没起来"），
        // 再套一层括号只会把真正有用的那句埋掉——所以用冒号接，不套括号。
        LocalHealth::Unreachable(err) => {
            format!("探测不通：{}，这一轮走 Agnes", snippet(err))
        }
        LocalHealth::Unexpected(raw) => format!(
            "状态不认识（{}），不敢当它可用，这一轮走 Agnes",
            snippet(raw)
        ),
    };
    LocalSlot::Fallback {
        // 兜底目标是 Agnes（免费档）：和 `ModelRouter::route` 里
        // "拿不准走免费的那个"同一条判据。**不挑 DeepSeek**——
        // 回落的理由是"本地这台机器上跑不了"，不是"这活变难了"。
        to: ModelSpec::AGNES_FLASH,
        reason,
        warm,
    }
}

/// 解析 `/health` 的响应体。**纯函数**——几种 200 的区分只在这里做。
///
/// 认得 `ok` / `loading` / `idle` / `degraded`（`sidecar/local_llm_server.py`
/// 目前报的就是这四种）。**认不出来的一律 [`LocalHealth::Unexpected`]**：
/// sidecar 加了新状态时，那是"我们没见过它"，不是"它一定没事"——
/// 回落，并把原话打出来。
pub fn parse_health(body: &str) -> LocalHealth {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return LocalHealth::Unexpected(snippet(body));
    };
    match v.get("status").and_then(|s| s.as_str()) {
        Some("ok") => LocalHealth::Ready,
        Some("loading") => LocalHealth::Loading,
        Some("idle") => LocalHealth::Idle,
        Some("degraded") => LocalHealth::Degraded(match v.get("detail").and_then(|d| d.as_str()) {
            // sidecar 没给 detail 时**别编一句"加载失败"**：原样把正文
            // 带出去，读的人至少能看出它答了什么
            Some(d) => d.to_string(),
            None => format!("sidecar 没给 detail，原样回答：{}", snippet(body)),
        }),
        // 有 status 但不是我们认识的值：把那个值本身带出去
        Some(other) => LocalHealth::Unexpected(other.to_string()),
        // 连 status 都没有（比如答的是 HTML）：带正文片段
        None => LocalHealth::Unexpected(snippet(body)),
    }
}

/// 本地槽位那条 I/O 缝：**探一次健康**，以及**催一次加载**。
///
/// 做成可替换的函数对象（和 `tool::runner::DeltaSink` 同一个写法）是为了让
/// **"该回落时回落"能在测试里发生**：真起一个 4B sidecar 要几十秒和几 GB 内存，
/// 那种测试没人会跑第二遍——于是回落逻辑等于没测，而它恰恰是
/// "本地挂了会不会静默降级"的守门人。
///
/// ## 为什么两个动作绑在同一个东西上
///
/// 它们是同一件事的两半：`/health` 回答"现在能不能用"，`/warmup` 说"下回能不能用"。
/// 拆成两个可以分别注入的口子，测试里换掉了探测、却漏掉催热，那一下就会真的
/// 打到 `127.0.0.1:17872`——同一份代码在两台机器上答案不同（一台起了 sidecar、
/// 一台没起），而且还可能把同事机器上正闲着的模型热起来。
/// 绑成一个结构就没这个口子：**换掉探测，催热必然一起被换掉。**
pub struct Probe {
    health_fn: std::sync::Arc<dyn Fn(&str) -> LocalHealth + Send + Sync>,
    warm_fn: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
}

impl Probe {
    /// 两个动作都自己定。**测试用这个**——理由是上面那段。
    pub fn new(
        health: impl Fn(&str) -> LocalHealth + Send + Sync + 'static,
        warm: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            health_fn: std::sync::Arc::new(health),
            warm_fn: std::sync::Arc::new(warm),
        }
    }

    /// 生产用的那一个：真去 GET `/health`，真去 POST `/warmup`。
    ///
    /// **默认值就是它**（`ChatHandler::new` 和 CLI 那几条路都不显式换）。
    /// 默认换成"永远就绪"会让没接上的链路安静地退化成"本地永远可用"，
    /// 而那种退化看起来和正常一模一样——D128 就是这么来的。
    pub fn real() -> Self {
        Self::new(probe, warm_up)
    }

    /// 探一次健康。地址由调用方从 `LOCAL_QWEN.base_url` 推出来
    /// （[`health_endpoint`]）。
    pub fn health(&self, endpoint: &str) -> LocalHealth {
        (self.health_fn)(endpoint)
    }

    /// 催一次加载。**结果一律丢掉**——理由见 [`warm_up`]。
    pub fn warm(&self, endpoint: &str) {
        (self.warm_fn)(endpoint)
    }
}

/// 真去叫一次 `/warmup`：**发出去、不等、不看结果**。
///
/// 三条都重要：
///
/// 1. **不等。** sidecar 收到就答 202，加载在它自己的后台线程里——
///    真要等的是那十几秒，而那正是 D106 那条要躲的东西
/// 2. **300 ms 就放弃**（[`WARMUP_TIMEOUT_MS`]）。这一下是"顺手叫一声"，
///    不该比探测本身还贵
/// 3. **结果一律丢掉。** 成功没有后续动作，失败也没有——这一轮已经改走 Agnes 了，
///    在终端上多打一行"催热失败"只会是噪音；而它为什么没热起来，
///    下一轮的 `/health` 说得比这里准（`degraded` 会带上 `detail`）
pub fn warm_up(endpoint: &str) {
    // 正文给一个空 JSON 对象：sidecar 的 `/warmup` 不读正文，
    // 但 `Content-Type: application/json` 下面总得有个合法的东西。
    let _ = crate::decide::http::post_json(endpoint, "{}", WARMUP_TIMEOUT_MS);
}

/// 真去 GET 一次 `/health`，[`HEALTH_TIMEOUT_MS`] 超时。
///
/// 走 [`crate::decide::http::get_json`] 而不是自己开 socket：那个客户端
/// **只允许回环地址**（本地槽位只可能在 `127.0.0.1` 上），而且回环连接
/// 用它量出来的短超时。非 2xx 在它那里就是 `Err`——正好，
/// "答了 500"和"连不上"一样该回落。
pub fn probe(endpoint: &str) -> LocalHealth {
    match crate::decide::http::get_json(endpoint, HEALTH_TIMEOUT_MS) {
        Ok(body) => parse_health(&body),
        Err(e) => LocalHealth::Unreachable(e.to_string()),
    }
}

/// 探一次、判一次、**就地改写路由**；回落了就返回理由（`None` = 一个字都没动）。
///
/// ## 为什么"改写"这一步也在 core 里
///
/// 对话那条链路（`ChatHandler::route_for`）和任务引擎（`task::engine`）
/// 都要做同一件事：本地槽位不可用 → 换端点、换档位、把理由接进 `reason`。
/// **两份实现必然漂移**，而漂移的方向是"有一条链路悄悄不回落了"——
/// 那正是 D128 的形状（功能还在，只是退化成了一个看不出来的样子）。
/// 所以判据和落地都只留这一份，两个调用方各自只管"怎么把它说出来"。
///
/// ## 非本地槽位一个字节都不动
///
/// [`is_local`] 不成立时**连探都不探**：远端端点的健康和 17872 上那个进程
/// 没有关系，白探一次不只是多花最多 300 ms——探失败时还会把"本地没起来"
/// 记到一次本来和它无关的路由上。
///
/// 返回的理由**不含句号**（和 [`LocalSlot::Fallback`] 一致），
/// 调用方要打印就自己补。
pub fn apply_fallback(routing: &mut Routing, probe: &Probe) -> Option<String> {
    if !is_local(&routing.spec) {
        return None;
    }
    let health = probe.health(&health_endpoint(routing.spec.base_url));
    let LocalSlot::Fallback { to, reason, warm } = decide(&routing.spec, &health) else {
        return None;
    };
    if warm {
        // **回落和催热必须一起发生。** 只回落就是把本地槽位静默关掉；
        // 只催热就是让这一轮白等几十秒。原设计是"这一轮走 Agnes，
        // 本地在后台热着，下一轮回到本地"——两半缺一不可。
        //
        // 地址从**同一个** `base_url` 推（见 `warmup_endpoint`），
        // 所以探的和催的必然是同一台机器上的同一个进程。
        //
        // 这一下是**同步发出**的（不是扔给一个线程就回来），而且是刻意的：
        // 能走到这里说明 `/health` 刚刚**及时答过话**——"卡住"的 sidecar
        // 探出来是 `Unreachable`，压根不会催。所以真实代价是一毫秒级的 202，
        // 而反过来（起线程）会在"进程马上就退出"的那条路上把这一脚丢掉，
        // 于是本地槽位还是回不来——**那正是这次要修的 bug**。
        probe.warm(&warmup_endpoint(routing.spec.base_url));
    }
    routing.reason.push_str(&format!("；{reason}"));
    // `tier` 跟着 `spec` 一起换：兜底之后这一轮**真的**是标准档（Agnes），
    // 留着原来那一档会让 `Routing` 自己和自己对不上，而台账、终端标签、
    // 成本统计读的正是这两个字段。
    routing.tier = to.tier;
    routing.spec = to;
    Some(reason)
}

/// 把一段可能很长的文本压到能打在一行里的长度。
///
/// 按**字符**截而不是按字节：这些文本是中文和 Python 异常混着的，
/// 按字节切会切出半个汉字。上限取 200，和 `parse_response` 里
/// 报错误正文用的是同一个数——**日志里的东西要能被一眼看完**。
fn snippet(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= 200 {
        return t.to_string();
    }
    let head: String = t.chars().take(200).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::think::router::ModelSpec;

    /// sidecar 真的会发出来的几种正文（照抄 `do_GET` 那一支）。
    const OK_BODY: &str = r#"{"status":"ok","model":"Qwen3-4B-Instruct-2507"}"#;
    const LOADING_BODY: &str = r#"{"status":"loading","model":"Qwen3-4B-Instruct-2507"}"#;
    const DEGRADED_BODY: &str = r#"{"status":"degraded","model":"unloaded","detail":"ModuleNotFoundError: No module named 'torch'"}"#;

    /// 六种状态各一条：**这条表就是"本地挂了怎么办"的全部判据**。
    ///
    /// 表驱动而不是六个测试函数，是因为加一种新状态时，
    /// 漏掉一行比漏掉一个函数更容易被发现——左边的长度是写死的。
    #[test]
    fn the_health_table_maps_to_keep_or_fallback() {
        let local = ModelSpec::LOCAL_QWEN;
        let cases: [(LocalHealth, bool); 6] = [
            (LocalHealth::Ready, true),
            (LocalHealth::Loading, false),
            (LocalHealth::Idle, false),
            (LocalHealth::Degraded("爆炸".into()), false),
            (LocalHealth::Unreachable("连接被拒绝".into()), false),
            (LocalHealth::Unexpected("hibernating".into()), false),
        ];
        for (health, keeps) in cases {
            let got = decide(&local, &health);
            if keeps {
                assert_eq!(got, LocalSlot::Keep, "{health:?} 该留着本地槽位");
            } else {
                match got {
                    LocalSlot::Fallback { to, reason, .. } => {
                        assert_eq!(to.provider, "agnes", "{health:?} 该回落到 Agnes");
                        assert!(!reason.is_empty(), "{health:?} 的回落必须带理由");
                    }
                    LocalSlot::Keep => panic!("{health:?} 不该当成可用——那是静默降级"),
                }
            }
        }
    }

    #[test]
    fn ready_is_the_only_state_that_keeps_the_local_slot() {
        // 反过来钉一遍：**除了 ok，没有任何一种状态算"可用"**。
        // 这条防的是将来有人加状态时手滑把默认值写成 `Keep`。
        let local = ModelSpec::LOCAL_QWEN;
        let not_ready = [
            LocalHealth::Loading,
            LocalHealth::Idle,
            LocalHealth::Degraded(String::new()),
            LocalHealth::Unreachable(String::new()),
            LocalHealth::Unexpected(String::new()),
        ];
        for h in not_ready {
            assert_ne!(decide(&local, &h), LocalSlot::Keep, "{h:?}");
        }
        assert_eq!(decide(&local, &LocalHealth::Ready), LocalSlot::Keep);
    }

    #[test]
    fn an_idle_sidecar_says_the_weights_were_released() {
        // `idle` 是"曾经加载好、权重被空闲释放了"。**判定和 `loading` 一样是回落**
        // （权重都不在显存里，这一轮要等几十秒），但**理由要说得准**：
        // 说成"状态不认识"会让操作者去查一个根本没坏的东西，
        // 而它该做的动作是让 `/warmup` 把模型热上。
        assert_eq!(
            parse_health(r#"{"status":"idle","model":"Qwen3-4B-Instruct-2507"}"#),
            LocalHealth::Idle
        );
        match decide(&ModelSpec::LOCAL_QWEN, &LocalHealth::Idle) {
            LocalSlot::Fallback { to, reason, .. } => {
                assert_eq!(to.provider, "agnes");
                assert!(reason.contains("idle"), "要把 sidecar 的原话带上: {reason}");
                assert!(
                    !reason.contains("不认识"),
                    "idle 是认得的（sidecar 自己写明了语义），不该说成不认识: {reason}"
                );
            }
            LocalSlot::Keep => panic!("权重不在显存里，这一轮不该留在本地"),
        }
    }

    #[test]
    fn only_the_not_ready_states_ask_for_a_warmup() {
        // **"idle 死锁"的机器可读版本。**
        //
        // 六种状态各要不要顺手踢一脚 `/warmup`：
        //
        // - `idle` / `loading`：**踢**。权重不在显存里，而 sidecar 会自己在
        //   后台把它搬回去——不踢的话**下一轮还是 idle**、于是每一轮都回落，
        //   本地槽位就这么静默地永远死了（所有显示都正常）
        // - `degraded`：不踢。`_load_once` 见到 `_model_error` 立刻返回，
        //   它**不重试**——踢一百次和踢一次一样，只是白等一次超时
        // - `unreachable`：不踢。没有进程在听，POST 只能等到超时
        // - `unexpected`：不踢。**我们不知道那是什么**，对不认识的进程动手
        //   比不动手更坏
        let local = ModelSpec::LOCAL_QWEN;
        let cases: [(LocalHealth, bool); 6] = [
            (LocalHealth::Ready, false),
            (LocalHealth::Loading, true),
            (LocalHealth::Idle, true),
            (LocalHealth::Degraded("爆炸".into()), false),
            (LocalHealth::Unreachable("连接被拒绝".into()), false),
            (LocalHealth::Unexpected("hibernating".into()), false),
        ];
        for (health, wants) in cases {
            match decide(&local, &health) {
                LocalSlot::Fallback { warm, .. } => {
                    assert_eq!(warm, wants, "{health:?} 的催热判据不对");
                }
                // 只有 `ok` 走这里，而它的期望值就是"不催"
                LocalSlot::Keep => assert!(!wants, "{health:?} 既没回落也不催热"),
            }
        }
    }

    #[test]
    fn applying_the_fallback_also_kicks_the_warmup_but_never_touches_remote_slots() {
        use crate::think::router::{TaskKind, Tier, TokenEstimate};

        /// 一条"本来是本地槽位"的路由。
        fn local_routing() -> Routing {
            Routing {
                tier: Tier::Cheap,
                kind: TaskKind::Conversation,
                spec: ModelSpec::LOCAL_QWEN,
                thinking: false,
                reason: "这条话判成轻量".into(),
                estimated_calls: 1,
                used_decider: false,
                estimate: TokenEstimate::default(),
            }
        }

        // —— idle：换端点 **并且** 催一脚，两半缺一不可 ——
        let kicked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&kicked);
        let probe = Probe::new(
            |_: &str| LocalHealth::Idle,
            move |endpoint: &str| sink.lock().expect("锁").push(endpoint.to_string()),
        );
        let mut routing = local_routing();
        let reason = apply_fallback(&mut routing, &probe).expect("idle 该回落");
        assert_eq!(routing.spec.provider, "agnes", "这一轮改走 Agnes");
        assert_eq!(routing.tier, Tier::Standard, "档位要跟着一起换");
        assert!(
            routing.reason.contains(&reason),
            "理由接进 reason 才会进台账: {}",
            routing.reason
        );
        let kicked = kicked.lock().expect("锁").clone();
        assert_eq!(
            kicked,
            vec!["http://127.0.0.1:17872/warmup".to_string()],
            "不催的话下一轮还是 idle——本地槽位永远回不来"
        );

        // —— 非本地槽位：**连探都不探**，路由一个字节都不动 ——
        let probed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&probed);
        let probe = Probe::new(
            move |_: &str| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                LocalHealth::Unreachable("不该被调到".into())
            },
            |_: &str| panic!("远端槽位不该被催热"),
        );
        let mut remote = local_routing();
        remote.spec = ModelSpec::AGNES_FLASH;
        remote.tier = ModelSpec::AGNES_FLASH.tier;
        let before = remote.clone();
        assert!(apply_fallback(&mut remote, &probe).is_none());
        assert_eq!(
            probed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "远端端点走它自己的路，本地那个进程的健康与它无关"
        );
        assert_eq!(remote, before, "非本地槽位不该被改动");
    }

    #[test]
    fn an_unrecognised_status_falls_back() {
        // **不认识就往能干活的那边倒，不是往"大概没事"倒。**
        //
        // sidecar 加了新状态时，我们并不知道它是什么意思——猜"没事"的代价是
        // 这一轮必然失败，猜错的代价只是多花几分钱。
        let got = decide(
            &ModelSpec::LOCAL_QWEN,
            &LocalHealth::Unexpected("hibernating".into()),
        );
        match got {
            LocalSlot::Fallback { to, reason, .. } => {
                assert_eq!(to.provider, "agnes");
                assert!(
                    reason.contains("不认识"),
                    "要说明白是「不认识」，而不是别的什么原因: {reason}"
                );
                assert!(reason.contains("hibernating"), "要把原话带上: {reason}");
            }
            LocalSlot::Keep => panic!("不认识的状态被当成了可用"),
        }
    }

    #[test]
    fn a_non_local_spec_is_never_altered() {
        // 健康状态说的是 17872 上那个进程。**远端端点的路由不该被它碰到**——
        // 否则本地没起来会顺带把 DeepSeek 那条路也改掉。
        let healths = [
            LocalHealth::Ready,
            LocalHealth::Loading,
            LocalHealth::Idle,
            LocalHealth::Degraded("torch 没了".into()),
            LocalHealth::Unreachable("连接被拒绝".into()),
            LocalHealth::Unexpected("???".into()),
        ];
        for spec in [ModelSpec::AGNES_FLASH, ModelSpec::DEEPSEEK_FLASH] {
            for h in &healths {
                assert_eq!(
                    decide(&spec, h),
                    LocalSlot::Keep,
                    "{} 槽位被 {h:?} 影响了",
                    spec.provider
                );
            }
        }
    }

    #[test]
    fn the_sidecar_detail_reaches_the_operator_message() {
        // **这条是"能不能查出为什么"的全部。**
        //
        // `degraded` 是唯一不会自愈的状态，而 detail 是唯一解释它的东西
        // （D106 那次查了很久，就是因为 /health 只说了"没加载"）。
        // 理由里丢掉 detail，操作者就只能自己去翻 sidecar 的 stderr。
        let health = parse_health(DEGRADED_BODY);
        match &health {
            LocalHealth::Degraded(d) => {
                assert!(d.contains("ModuleNotFoundError"), "detail 没解析出来: {d}")
            }
            other => panic!("{DEGRADED_BODY} 该解析成 degraded，实际 {other:?}"),
        }
        match decide(&ModelSpec::LOCAL_QWEN, &health) {
            LocalSlot::Fallback { reason, .. } => {
                assert!(
                    reason.contains("ModuleNotFoundError: No module named 'torch'"),
                    "detail 必须原样进理由——那是操作者唯一的线索: {reason}"
                );
                assert!(reason.contains("Agnes"), "要说清这一轮改走了谁: {reason}");
            }
            LocalSlot::Keep => panic!("degraded 被当成了可用"),
        }
    }

    #[test]
    fn an_unreachable_reason_says_what_failed() {
        // 连不上时理由要带**底层那句话**：`decide::http` 的文案里有地址和
        // "多半是这个服务没起来"，那正是操作者要知道的下一步。
        match decide(
            &ModelSpec::LOCAL_QWEN,
            &LocalHealth::Unreachable(
                "连接失败: 127.0.0.1:17872 连不上——多半是这个服务没起来".into(),
            ),
        ) {
            LocalSlot::Fallback { reason, .. } => {
                assert!(reason.contains("17872"), "要带地址: {reason}");
                assert!(reason.contains("没起来"), "要带下一步: {reason}");
            }
            LocalSlot::Keep => panic!("连不上被当成了可用"),
        }
    }

    #[test]
    fn the_200_bodies_are_told_apart() {
        // **这几种状态都是 HTTP 200**——只看状态码会把 degraded 当成健康。
        // 这条表就是"状态在正文里"这件事的机器可读版本。
        assert_eq!(parse_health(OK_BODY), LocalHealth::Ready);
        assert_eq!(parse_health(LOADING_BODY), LocalHealth::Loading);
        assert_eq!(
            parse_health(r#"{"status":"idle","model":"Qwen3-4B-Instruct-2507"}"#),
            LocalHealth::Idle
        );
        assert!(matches!(
            parse_health(DEGRADED_BODY),
            LocalHealth::Degraded(_)
        ));
    }

    #[test]
    fn a_body_that_is_not_json_is_unexpected_not_ready() {
        // 端口上蹲着的可能是**别的**服务（比如 17870 那个、或者一个代理页）。
        // 它答 200 + HTML 时，"解析失败"绝不能被当成"健康"。
        for body in ["", "not json", "<html>hello</html>", "{}"] {
            assert!(
                matches!(parse_health(body), LocalHealth::Unexpected(_)),
                "{body:?} 该是「不认识」"
            );
            assert_ne!(
                decide(&ModelSpec::LOCAL_QWEN, &parse_health(body)),
                LocalSlot::Keep,
                "{body:?} 不该留下本地槽位"
            );
        }
    }

    #[test]
    fn a_degraded_body_without_detail_still_carries_the_body() {
        // sidecar 理论上总会给 detail，但真不给的时候**也不能只报一句
        // "加载失败"**——那句话正是 D106 让人查了很久的东西。
        let h = parse_health(r#"{"status":"degraded","model":"unloaded"}"#);
        match decide(&ModelSpec::LOCAL_QWEN, &h) {
            LocalSlot::Fallback { reason, .. } => {
                assert!(reason.contains("degraded"), "{reason}");
                assert!(reason.contains("没给 detail"), "要说清缺了什么: {reason}");
            }
            LocalSlot::Keep => panic!(),
        }
    }

    #[test]
    fn a_huge_detail_cannot_flood_the_line() {
        // detail 是别人（Python 异常）给的，长度不由我们定。
        // 一行几万字的 stderr 会把真正有用的那几行冲掉。
        let long = "x".repeat(5000);
        let h = LocalHealth::Degraded(long);
        match decide(&ModelSpec::LOCAL_QWEN, &h) {
            LocalSlot::Fallback { reason, .. } => {
                assert!(
                    reason.chars().count() < 400,
                    "理由被截断了: {}",
                    reason.len()
                );
            }
            LocalSlot::Keep => panic!(),
        }
        // 截断按字符来，不能切出半个汉字
        let cn = LocalHealth::Degraded("很".repeat(500));
        match decide(&ModelSpec::LOCAL_QWEN, &cn) {
            LocalSlot::Fallback { reason, .. } => assert!(reason.contains('很')),
            LocalSlot::Keep => panic!(),
        }
    }

    #[test]
    fn the_health_endpoint_is_derived_from_the_slot_itself() {
        // **端口只能有一份出处。** 这条钉的是"我们从 spec 推地址"，
        // 而不是"我们在第二个地方写了 17872"：
        // `LOCAL_QWEN.base_url` 一改，探测地址跟着改。
        let ep = health_endpoint(ModelSpec::LOCAL_QWEN.base_url);
        assert_eq!(ep, "http://127.0.0.1:17872/health");
        assert!(
            ep.contains("17872"),
            "端口必须来自 LOCAL_QWEN.base_url: {ep}"
        );
        // `/v1` 那段要砍掉：`/health` 挂在根上，拼成 `/v1/health` 会 404，
        // 而 404 会被当成"服务返回 404"——一个查不出原因的错
        assert!(!ep.contains("/v1"), "{ep}");
    }

    #[test]
    fn the_warmup_endpoint_is_derived_from_the_same_source_as_health() {
        // 和 `/health` 同一条规矩：**端口只能有一份出处**。
        // 这里再写一个 17872 的话，改端口时漏掉它——而漏掉的表现是
        // "催热打到没人监听的端口上"，那条路是 fire-and-forget 的，
        // **没有任何地方会报出来**。
        let warm = warmup_endpoint(ModelSpec::LOCAL_QWEN.base_url);
        assert_eq!(warm, "http://127.0.0.1:17872/warmup");
        assert!(warm.contains("17872"), "端口必须来自 base_url: {warm}");
        // 同一条规矩：`/v1` 要砍掉（`/v1/warmup` 是 404）
        assert!(!warm.contains("/v1"), "{warm}");
        // 探的和催的必须是同一台机器上的同一个进程：两个地址除了最后一段
        // 路径之外必须逐字相同
        let health = health_endpoint(ModelSpec::LOCAL_QWEN.base_url);
        assert_eq!(
            health.rsplit_once('/').expect("有斜杠").0,
            warm.rsplit_once('/').expect("有斜杠").0,
            "探测地址和催热地址不是同一个服务"
        );
    }

    #[test]
    fn the_fallback_target_is_the_free_remote_slot() {
        // 兜底挑 Agnes：免费档。**不挑 DeepSeek**——回落的理由是
        // "本地这台机器上跑不了"，不是"这活变难了"。
        match decide(
            &ModelSpec::LOCAL_QWEN,
            &LocalHealth::Unreachable("连接被拒绝".into()),
        ) {
            LocalSlot::Fallback { to, .. } => {
                assert_eq!(to.provider, "agnes");
                assert_eq!(to.model, ModelSpec::AGNES_FLASH.model);
                // 回落之后**不能再是本地**，否则等于没落
                assert_ne!(to.provider, ModelSpec::LOCAL_QWEN.provider);
            }
            LocalSlot::Keep => panic!(),
        }
    }

    #[test]
    fn only_the_local_provider_counts_as_local() {
        // 判据用 provider，不用 base_url：换个端口、换个 sidecar 进程
        // 还是同一个本地槽位；而一个恰好也在回环上的别的槽位不该被当成它。
        assert!(is_local(&ModelSpec::LOCAL_QWEN));
        assert!(!is_local(&ModelSpec::AGNES_FLASH));
        assert!(!is_local(&ModelSpec::DEEPSEEK_FLASH));
    }
}
