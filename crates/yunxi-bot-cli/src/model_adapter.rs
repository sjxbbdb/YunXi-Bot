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
            "qwen38" | "qwen3.8" => (ModelSpec::LOCAL_QWEN38, "Qwen3.8-27B（本机）"),
            _ => (ModelSpec::LOCAL_GEMMA, "Gemma 4 26B-A4B（本机）"),
        };
        // Qwen3.8 的本地工具协议尚未实测；先走自动召回，测完再决定是否挂工具。
        let tools = spec.model == ModelSpec::LOCAL_GEMMA.model;
        Self {
            role: "companion",
            label,
            spec,
            local: true,
            tools,
        }
    }

    /// 工作代理：默认 DeepSeek Flash；设置 `YUNXI_WORK_BASE_URL` 和
    /// `YUNXI_WORK_MODEL` 时切到任意 OpenAI 兼容云端端点，密钥放
    /// `data/secrets/openai.key`。这样市面模型不必改 Handler。
    pub fn work() -> Self {
        let base = std::env::var("YUNXI_WORK_BASE_URL").ok();
        let model = std::env::var("YUNXI_WORK_MODEL").ok();
        if let (Some(base), Some(model)) = (base, model)
            && !base.trim().is_empty()
            && !model.trim().is_empty()
        {
            return Self::cloud_openai_compatible(base.trim(), model.trim());
        }
        Self {
            role: "work",
            label: "DeepSeek Flash（云端）",
            spec: ModelSpec::DEEPSEEK_FLASH,
            local: false,
            tools: true,
        }
    }

    /// 构造一个 OpenAI 兼容云端适配器；用于测试和动态配置。
    pub fn cloud_openai_compatible(base_url: &str, model: &str) -> Self {
        let base_url: &'static str = Box::leak(base_url.to_string().into_boxed_str());
        let model: &'static str = Box::leak(model.to_string().into_boxed_str());
        let label: &'static str = Box::leak(format!("{model}（OpenAI 兼容云端）").into_boxed_str());
        Self {
            role: "work",
            label,
            spec: ModelSpec {
                tier: yunxi_bot_core::think::router::Tier::Deep,
                provider: "openai-compatible",
                base_url,
                model,
                price: yunxi_bot_core::think::cost::PriceTable::DEEPSEEK_FLASH,
                rpm: 60,
                thinking: yunxi_bot_core::think::Thinking::Enabled,
                key_hint: "secrets/openai.key 或 YUNXI_BOT_HOME/secrets/openai.key",
            },
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
    fn custom_cloud_adapter_keeps_openai_compatible_metadata() {
        let adapter =
            ModelAdapter::cloud_openai_compatible("https://api.example.com/v1", "some-model");
        assert!(!adapter.local);
        assert!(adapter.tools);
        assert_eq!(adapter.spec.provider, "openai-compatible");
        assert_eq!(adapter.spec.model, "some-model");
        assert_eq!(adapter.spec.base_url, "https://api.example.com/v1");
    }

    #[test]
    fn work_is_cloud_deepseek_with_tools() {
        let adapter = ModelAdapter::work();
        assert!(!adapter.local);
        assert!(adapter.tools);
        assert_eq!(adapter.spec.provider, "deepseek");
    }
}
