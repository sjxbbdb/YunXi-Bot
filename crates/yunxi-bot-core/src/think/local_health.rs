//! 本地模型健康检查与预热；不就绪时保留本地端点并报告原因。
//! 推理请求负责有界等待或返回错误，健康检查不再选择远端替代模型。

use crate::think::router::{ModelSpec, Routing};

/// 探测位于每轮对话的关键路径，短超时避免本机服务失联拖慢路由。
pub const HEALTH_TIMEOUT_MS: u64 = 300;

/// 预热仅提交后台加载请求，不在健康检查阶段等待权重加载。
pub const WARMUP_TIMEOUT_MS: u64 = 300;

/// HTTP 200 不代表模型就绪；必须检查响应正文里的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalHealth {
    /// `{"status":"ok"}`：权重在显存里，这一轮就用它。
    Ready,
    /// `{"status":"loading"}`：还在加载（几 GB、几十秒）。
    ///
    /// 健康检查不阻塞等待权重加载；
    /// 等待由本地推理请求的超时约束，不更换模型。
    Loading,
    /// `{"status":"idle"}`：曾经加载好，**权重被空闲释放了**（`/unload` 或空闲回收）。
    ///
    /// sidecar 那边写着"这不是错误，也不是在加载：下一个请求会自己把它拉回来"。
    /// 但那句"拉回来"是**这一轮要等几十秒**的意思——和 [`Self::Loading`]
    /// 对使用者是同一件事，所以判定也一样：保持本地端点，请求将等待加载或明确失败，
    /// 同时由 [`Self::needs_warmup`] 决定**顺手踢一脚 `/warmup`**，
    /// 让推理请求到来前开始加载。
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
    /// 未知状态必须报告，不推断模型已就绪，也不改用远端。
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
    pub fn needs_warmup(&self) -> bool {
        matches!(self, LocalHealth::Idle | LocalHealth::Loading)
    }
}

/// 本地槽位是否就绪、是否需要预热。
#[derive(Debug, Clone, PartialEq)]
pub enum LocalSlot {
    /// 本地就绪，照路由走。
    Keep,
    /// 本地未就绪：保留端点，报告原因；仅 idle/loading 会请求预热。
    ///
    /// `reason` **不含句号**——打印那一层自己补，这样它既能进 stderr
    /// 又能原样进台账/`Routing::reason`，不必有人去改标点。
    Unavailable {
        reason: String,
        /// 要不要顺手 `POST /warmup` 把权重热上（**不等它**）。
        ///
        /// 判据见 [`LocalHealth::needs_warmup`]。
        warm: bool,
    },
}

/// 这条 spec 是不是那个本地槽位。
///
/// 判据用 `provider` 而不是 `base_url`：**`provider` 才是路由和台账用的身份**，
/// 而 `base_url` 是可以变的（换个端口、换个 sidecar 进程，还是同一个本地槽位）。
pub fn is_local(spec: &ModelSpec) -> bool {
    spec.provider == ModelSpec::LOCAL.provider
}

/// 从本地槽位的 `base_url` 推出 `/health` 地址。
///
/// **这里不写端口号。** 端口已经有一份出处：`ModelSpec::LOCAL.base_url`
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

/// 健康检查只决定是否需要预热，不授予切换远端的权限。
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
        LocalHealth::Loading => {
            "还没就绪（loading，权重还在加载），保持本地端点，请求将等待加载或明确失败".to_string()
        }
        // `idle` 和 `loading` 对使用者是同一件事：权重都不在显存里，
        // 加载由 sidecar 完成，推理请求仍受自身超时约束。
        LocalHealth::Idle => {
            "权重被空闲释放了（idle），保持本地端点，请求将等待加载或明确失败".to_string()
        }
        LocalHealth::Degraded(detail) => {
            format!(
                "加载失败过（degraded：{}），保持本地端点，请求将等待加载或明确失败",
                snippet(detail)
            )
        }
        // 这里不再自己编"多半是没起来"：`HttpError` 的文案本来就带
        // 地址和该查什么（"连不上（回环地址 300 ms 内没接受连接）——多半是这个服务没起来"），
        // 再套一层括号只会把真正有用的那句埋掉——所以用冒号接，不套括号。
        LocalHealth::Unreachable(err) => {
            format!(
                "探测不通：{}，保持本地端点，请求将等待加载或明确失败",
                snippet(err)
            )
        }
        LocalHealth::Unexpected(raw) => format!(
            "状态不认识（{}），不敢当它可用，保持本地端点，请求将等待加载或明确失败",
            snippet(raw)
        ),
    };
    LocalSlot::Unavailable { reason, warm }
}

/// 解析 `/health` 的响应体。**纯函数**——几种 200 的区分只在这里做。
///
/// 认得 `ok` / `loading` / `idle` / `degraded`（`sidecar/local_llm_server.py`
/// 目前报的就是这四种）。**认不出来的一律 [`LocalHealth::Unexpected`]**：
/// sidecar 加了新状态时，那是"我们没见过它"，不是"它一定没事"——
/// 报告原话，不把未知状态当成就绪。
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

/// 健康检查和预热一起注入，测试替身不会意外唤醒真实模型。
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

/// 预热由侧车后台完成；本次失败由推理请求与下一次健康检查报告。
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
/// "答了 500"和"连不上"都按未就绪报告。
pub fn probe(endpoint: &str) -> LocalHealth {
    match crate::decide::http::get_json(endpoint, HEALTH_TIMEOUT_MS) {
        Ok(body) => parse_health(&body),
        Err(e) => LocalHealth::Unreachable(e.to_string()),
    }
}

/// 未就绪时说明原因并请求预热；模型、端点与思考模式保持不变。
pub fn prepare_local(routing: &mut Routing, probe: &Probe) -> Option<String> {
    if !is_local(&routing.spec) {
        return None;
    }
    let health = probe.health(&health_endpoint(routing.spec.base_url));
    let LocalSlot::Unavailable { reason, warm } = decide(&routing.spec, &health) else {
        return None;
    };
    if warm {
        probe.warm(&warmup_endpoint(routing.spec.base_url));
    }
    routing.reason.push_str(&format!("；{reason}"));
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
    #[test]
    fn local_failure_never_changes_the_model_or_endpoint() {
        use crate::think::{ModelRouter, ReasoningEffort, TaskKind, TaskProfile};
        for health in [
            LocalHealth::Idle,
            LocalHealth::Loading,
            LocalHealth::Degraded("load failed".into()),
            LocalHealth::Unreachable("offline".into()),
            LocalHealth::Unexpected("unknown".into()),
        ] {
            let mut route = ModelRouter::new(vec![ModelSpec::LOCAL], ModelSpec::LOCAL.tier).route(
                "你好",
                TaskKind::Conversation,
                &TaskProfile::default(),
                ReasoningEffort::Auto,
                None,
            );
            let before = route.spec.clone();
            let probe = Probe::new(move |_| health.clone(), |_| {});
            assert!(prepare_local(&mut route, &probe).is_some());
            assert_eq!(route.spec, before);
        }
    }
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
    fn the_health_table_distinguishes_ready_from_unavailable() {
        let local = ModelSpec::LOCAL;
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
                    LocalSlot::Unavailable { reason, .. } => {
                        assert!(!reason.is_empty(), "{health:?} 的回落必须带理由");
                    }
                    LocalSlot::Keep => panic!("{health:?} 不该当成可用——那是静默降级"),
                }
            }
        }
    }

    #[test]
    fn ready_is_the_only_healthy_state() {
        // 反过来钉一遍：**除了 ok，没有任何一种状态算"可用"**。
        // 这条防的是将来有人加状态时手滑把默认值写成 `Keep`。
        let local = ModelSpec::LOCAL;
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
        match decide(&ModelSpec::LOCAL, &LocalHealth::Idle) {
            LocalSlot::Unavailable { reason, .. } => {
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
        let local = ModelSpec::LOCAL;
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
                LocalSlot::Unavailable { warm, .. } => {
                    assert_eq!(warm, wants, "{health:?} 的催热判据不对");
                }
                // 只有 `ok` 走这里，而它的期望值就是"不催"
                LocalSlot::Keep => assert!(!wants, "{health:?} 既没回落也不催热"),
            }
        }
    }

    #[test]
    fn preparing_local_warms_idle_without_touching_remote_slots() {
        use crate::think::router::{TaskKind, Tier, TokenEstimate};

        /// 一条"本来是本地槽位"的路由。
        fn local_routing() -> Routing {
            Routing {
                tier: Tier::Cheap,
                kind: TaskKind::Conversation,
                spec: ModelSpec::LOCAL,
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
        let reason = prepare_local(&mut routing, &probe).expect("idle 该回落");
        assert_eq!(routing.spec.provider, "local", "保留本地模型");
        assert_eq!(routing.tier, Tier::Cheap, "档位保持不变");
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
        remote.spec = ModelSpec::DEEPSEEK_FLASH;
        remote.tier = ModelSpec::DEEPSEEK_FLASH.tier;
        let before = remote.clone();
        assert!(prepare_local(&mut remote, &probe).is_none());
        assert_eq!(
            probed.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "远端端点走它自己的路，本地那个进程的健康与它无关"
        );
        assert_eq!(remote, before, "非本地槽位不该被改动");
    }

    #[test]
    fn an_unrecognised_status_is_reported_as_unavailable() {
        // **不认识就往能干活的那边倒，不是往"大概没事"倒。**
        //
        // sidecar 加了新状态时，我们并不知道它是什么意思——猜"没事"的代价是
        // 这一轮必然失败，猜错的代价只是多花几分钱。
        let got = decide(
            &ModelSpec::LOCAL,
            &LocalHealth::Unexpected("hibernating".into()),
        );
        match got {
            LocalSlot::Unavailable { reason, .. } => {
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
        for spec in [ModelSpec::DEEPSEEK_FLASH] {
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
        match decide(&ModelSpec::LOCAL, &health) {
            LocalSlot::Unavailable { reason, .. } => {
                assert!(
                    reason.contains("ModuleNotFoundError: No module named 'torch'"),
                    "detail 必须原样进理由——那是操作者唯一的线索: {reason}"
                );
                assert!(
                    reason.contains("保持本地端点"),
                    "要说清本轮端点不变: {reason}"
                );
            }
            LocalSlot::Keep => panic!("degraded 被当成了可用"),
        }
    }

    #[test]
    fn an_unreachable_reason_says_what_failed() {
        // 连不上时理由要带**底层那句话**：`decide::http` 的文案里有地址和
        // "多半是这个服务没起来"，那正是操作者要知道的下一步。
        match decide(
            &ModelSpec::LOCAL,
            &LocalHealth::Unreachable(
                "连接失败: 127.0.0.1:17872 连不上——多半是这个服务没起来".into(),
            ),
        ) {
            LocalSlot::Unavailable { reason, .. } => {
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
                decide(&ModelSpec::LOCAL, &parse_health(body)),
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
        match decide(&ModelSpec::LOCAL, &h) {
            LocalSlot::Unavailable { reason, .. } => {
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
        match decide(&ModelSpec::LOCAL, &h) {
            LocalSlot::Unavailable { reason, .. } => {
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
        match decide(&ModelSpec::LOCAL, &cn) {
            LocalSlot::Unavailable { reason, .. } => assert!(reason.contains('很')),
            LocalSlot::Keep => panic!(),
        }
    }

    #[test]
    fn the_health_endpoint_is_derived_from_the_slot_itself() {
        // **端口只能有一份出处。** 这条钉的是"我们从 spec 推地址"，
        // 而不是"我们在第二个地方写了 17872"：
        // `LOCAL_QWEN.base_url` 一改，探测地址跟着改。
        let ep = health_endpoint(ModelSpec::LOCAL.base_url);
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
        let warm = warmup_endpoint(ModelSpec::LOCAL.base_url);
        assert_eq!(warm, "http://127.0.0.1:17872/warmup");
        assert!(warm.contains("17872"), "端口必须来自 base_url: {warm}");
        // 同一条规矩：`/v1` 要砍掉（`/v1/warmup` 是 404）
        assert!(!warm.contains("/v1"), "{warm}");
        // 探的和催的必须是同一台机器上的同一个进程：两个地址除了最后一段
        // 路径之外必须逐字相同
        let health = health_endpoint(ModelSpec::LOCAL.base_url);
        assert_eq!(
            health.rsplit_once('/').expect("有斜杠").0,
            warm.rsplit_once('/').expect("有斜杠").0,
            "探测地址和催热地址不是同一个服务"
        );
    }

    #[test]
    fn only_the_local_provider_counts_as_local() {
        // 判据用 provider，不用 base_url：换个端口、换个 sidecar 进程
        // 还是同一个本地槽位；而一个恰好也在回环上的别的槽位不该被当成它。
        assert!(is_local(&ModelSpec::LOCAL));
        assert!(!is_local(&ModelSpec::DEEPSEEK_FLASH));
    }
}
