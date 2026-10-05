//! 网络类工具：抓网页、搜索。
//!
//! ## `web_fetch` 用 Jina Reader，但必须留回退
//!
//! 实测（2026-10-05）：`https://r.jina.ai/<url>` 不要 key，返回带
//! `Title` / `URL Source` / `Markdown Content` 的纯文本，直连与走代理都通。
//! 省掉自己写 HTML→Markdown。
//!
//! **但它是个第三方免费服务。** 挂了要有回退：自己抓 HTML 再剥标签。
//! 依赖一个免费服务而不写回退，等于把可用性押在别人身上。
//!
//! ## `web_search` 的 provider 必须可换
//!
//! 实测：**不要 key 又能出网页结果的只有 DuckDuckGo lite 一条**。
//! SearXNG 公共实例普遍关掉了 JSON 接口（实测返回 `text/html`），
//! 要 JSON 得自建。Brave / Tavily / Google CSE / Serper 都要 key。
//!
//! DuckDuckGo lite 是**抓 HTML**——改版或反爬就失效。
//! 所以：**这个方案一定会烂，不是"可能"。** 默认零配置那条的价值是
//! "今天能用"，不是"长期可靠"。provider 必须做成配置项，
//! 哪天失效填个 key 就能换掉，不用改代码。
//!
//! ## 能力是 [`Capability::Network`]，不是只读
//!
//! 抓一个网页会泄露请求内容（URL 本身可能就是隐私），返回的内容还是
//! **不可信数据**，会进模型上下文。把它并进只读会让所有网络访问自动放行。
//! Claude Code 对 `WebFetch`/`WebSearch` 也都标了"需要权限"。

// 待实现：见 docs/工具层调研与设计.md 阶段一。
