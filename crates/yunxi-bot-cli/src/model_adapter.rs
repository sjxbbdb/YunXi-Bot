//! 双代理的模型适配器描述。
//!
//! 这里只做**模型选择与能力声明**，不负责启动 sidecar；真正的 OpenAI 兼容
//! 客户端仍由 `ModelSpec::thinker_config()` 构造。模型选项集中在一张表里，
//! 启动时校验权重目录和架构，避免“别名存在但模型已经删了”的静默失败。

use std::path::Path;

use yunxi_bot_core::think::ModelSpec;

#[derive(Debug, Clone)]
pub struct ModelAdapter {
    pub role: &'static str,
    pub label: &'static str,
    pub spec: ModelSpec,
    pub local: bool,
    pub tools: bool,
}

struct CompanionChoice {
    aliases: &'static [&'static str],
    /// 只用于映射到已验证的 `ModelSpec` 常量。
    spec: &'static str,
    label: &'static str,
    /// `home/models/<dir>`。
    dir: &'static str,
    /// 本地后端允许的 config.json model_type。
    model_type: &'static str,
    tools: bool,
}

const COMPANION_CHOICES: &[CompanionChoice] = &[
    CompanionChoice {
        aliases: &["gemma", "gemma4"],
        spec: "gemma",
        label: "Gemma 4 26B-A4B（本机）",
        dir: "gemma-4-26B-A4B-it-qat-4bit",
        model_type: "gemma4",
        tools: true,
    },
    CompanionChoice {
        aliases: &["qwen", "qwen3.5"],
        spec: "qwen",
        label: "Qwen3.5-9B（本机）",
        dir: "Qwen3.5-9B-4bit",
        model_type: "qwen3_5",
        tools: true,
    },
];

fn spec_for(kind: &str) -> ModelSpec {
    match kind {
        "gemma" => ModelSpec::LOCAL_GEMMA,
        "qwen" => ModelSpec::LOCAL_CHAT,
        other => unreachable!("未知的本地模型规格：{other}"),
    }
}

fn select_choice(env: &str) -> Option<&'static CompanionChoice> {
    let env = env.trim();
    if env.is_empty() {
        return COMPANION_CHOICES.first();
    }
    COMPANION_CHOICES
        .iter()
        .find(|choice| choice.aliases.contains(&env))
}

fn build(choice: &CompanionChoice) -> ModelAdapter {
    let spec = spec_for(choice.spec);
    ModelAdapter {
        role: "companion",
        label: choice.label,
        spec,
        local: true,
        tools: choice.tools,
    }
}

fn validate_local(home: &Path, choice: &CompanionChoice) -> Result<(), String> {
    let dir = home.join("models").join(choice.dir);
    let config_path = dir.join("config.json");
    let config = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("模型 {} 缺少 {}：{e}", choice.label, config_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&config)
        .map_err(|e| format!("模型 {} 的 config.json 无法解析：{e}", choice.label))?;
    let actual_type = value["model_type"]
        .as_str()
        .ok_or_else(|| format!("模型 {} 的 config.json 缺少 model_type", choice.label))?;
    if actual_type != choice.model_type {
        return Err(format!(
            "模型 {} 架构不匹配：需要 {}，实际 {}",
            choice.label, choice.model_type, actual_type
        ));
    }
    let has_weights = dir.join("model.safetensors").is_file()
        || std::fs::read_dir(&dir)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "safetensors")
                })
            })
            .unwrap_or(false);
    if !has_weights {
        return Err(format!(
            "模型 {} 缺少 safetensors 权重：{}",
            choice.label,
            dir.display()
        ));
    }
    Ok(())
}

impl ModelAdapter {
    /// 陪伴代理：默认 Gemma 4 26B-A4B；`YUNXI_COMPANION_MODEL=qwen` 切到 Qwen3.5-9B。
    ///
    /// 这个入口不校验磁盘，给测试和无 home 的调用方；ChatServer 使用
    /// [`ModelAdapter::companion_for`]，启动时校验权重和架构。
    pub fn companion() -> Self {
        let env = std::env::var("YUNXI_COMPANION_MODEL").unwrap_or_default();
        let choice = select_choice(&env).unwrap_or_else(|| unreachable!("companion 表为空"));
        build(choice)
    }

    /// 启动时使用的严格版本：别名不存在、权重缺失、架构不匹配都直接报错。
    pub fn companion_for(home: &Path) -> Result<Self, String> {
        let env = std::env::var("YUNXI_COMPANION_MODEL").unwrap_or_default();
        let choice = select_choice(&env).ok_or_else(|| {
            format!(
                "未知的陪伴模型别名 {env:?}；可用：{}",
                COMPANION_CHOICES
                    .iter()
                    .flat_map(|c| c.aliases)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        validate_local(home, choice)?;
        Ok(build(choice))
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
    fn companion_defaults_to_gemma() {
        let adapter = ModelAdapter::companion();
        assert!(adapter.local);
        assert!(adapter.tools);
        assert_eq!(adapter.spec.provider, "local");
        assert_eq!(adapter.spec.model, "gemma-4-26B-A4B-it-qat-4bit");
    }

    #[test]
    fn stale_aliases_are_not_accepted() {
        let home = std::env::temp_dir().join(format!("yunxi-adapter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let err = ModelAdapter::companion_for(&home).unwrap_err();
        assert!(
            err.contains("Gemma") || err.contains("模型"),
            "unexpected: {err}"
        );
        let _ = std::fs::remove_dir_all(&home);
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
