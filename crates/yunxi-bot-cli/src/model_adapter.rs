//! 双代理的模型适配器描述。
//!
//! 当前先覆盖两条已验证的链路：
//! - 陪伴：Apple Silicon 本地 MLX（Qwen3.5-9B / Gemma 4 26B）
//! - 工作：DeepSeek Flash（云端）
//!
//! 这里只做能力声明与选择，不负责启动 sidecar；真正的 OpenAI 兼容
//! 客户端仍由 `ModelSpec::thinker_config()` 构造。后续接入市面模型时，
//! 扩展 `spec` 的来源即可，不要把模型名散落到 Handler 里。

use yunxi_bot_core::think::ModelSpec;

#[derive(Debug, Clone)]
pub struct ModelAdapter {
    pub role: &'static str,
    pub label: &'static str,
    pub spec: ModelSpec,
    pub local: bool,
    pub tools: bool,
}

impl ModelAdapter {
    /// 陪伴代理：默认 Gemma 4 26B-A4B；`YUNXI_COMPANION_MODEL=qwen` 切到 Qwen3.5-9B。
    pub fn companion() -> Self {
        let env = std::env::var("YUNXI_COMPANION_MODEL").unwrap_or_default();
        let (spec, label) = match env.as_str() {
            "qwen" | "qwen3.5" => (ModelSpec::LOCAL_CHAT, "Qwen3.5-9B（本机）"),
            _ => (ModelSpec::LOCAL_GEMMA, "Gemma 4 26B-A4B（本机）"),
        };
        let tools = spec.model == ModelSpec::LOCAL_GEMMA.model;
        Self {
            role: "companion",
            label,
            spec,
            local: true,
            tools,
        }
    }

    /// 工作代理：DeepSeek Flash。只允许云端；本地模型不承担任务。
    pub fn work() -> Self {
        Self {
            role: "work",
            label: "DeepSeek Flash（云端）",
            spec: ModelSpec::DEEPSEEK_FLASH,
            local: false,
            tools: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_defaults_to_local_gemma() {
        let adapter = ModelAdapter::companion();
        assert!(adapter.local);
        assert!(adapter.tools);
        assert_eq!(adapter.spec.provider, "local");
        assert_eq!(adapter.spec.model, "gemma-4-26B-A4B-it-qat-4bit");
    }

    #[test]
    fn work_is_cloud_deepseek_with_tools() {
        let adapter = ModelAdapter::work();
        assert!(!adapter.local);
        assert!(adapter.tools);
        assert_eq!(adapter.spec.provider, "deepseek");
    }
}
