//! `session.settings` 的校验与应用（协议 v1 的运行期设置更新）。
//!
//! 只对**内核持有**的东西生效：`initialize.model` 交来的模型配置与会话压缩配置。
//! 设置对后续回合生效——正在跑的回合在开始时已快照了模型配置与工具声明，
//! 本次更新不会改变它已经发出去的那次请求（见 `docs/protocol-v1.md`）。
//!
//! 校验与写入分两段：任一字段被拒时一个字节都不改（原子），
//! 拒绝原因进错误响应的 `data.kind`，宿主据此决定是提示「下次会话生效」还是重试。

use omnicrawl_compaction::CompactionConfig;
use omnicrawl_config::models::llm::normalize_reasoning_effort;
use omnicrawl_ipc::bridge::{KernelCompactionConfig, KernelModelConfig, SessionSettingsParams};
use omnicrawl_ipc::frame::{error_code, ErrorObject};
use serde_json::{json, Value};

use crate::compaction::overlay_compaction_config;

/// 没有模型配置（宿主走 `model.reply` 代答）：模型段一律不可改。
pub const KIND_MODEL_UNAVAILABLE: &str = "model_unavailable";
/// 内核没有自持会话：压缩段不可改。
pub const KIND_SESSION_UNAVAILABLE: &str = "session_unavailable";
/// 两个段都没给。
pub const KIND_EMPTY: &str = "empty_settings";
/// 字段取值非法。
pub const KIND_INVALID: &str = "invalid_settings";

/// 一次被拒绝的设置更新。
#[derive(Debug)]
pub struct SettingsRejection {
    pub kind: &'static str,
    pub message: String,
}

impl SettingsRejection {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn to_error(&self) -> ErrorObject {
        ErrorObject::with_data(
            error_code::INVALID_PARAMS,
            self.message.clone(),
            json!({ "kind": self.kind }),
        )
    }
}

/// 应用一次设置更新，返回被改动的字段路径（如 `model.tools`）。
pub fn apply(
    model: &mut Option<KernelModelConfig>,
    compaction: Option<&mut CompactionConfig>,
    params: &SessionSettingsParams,
) -> Result<Vec<String>, SettingsRejection> {
    if params.model.is_none() && params.compaction.is_none() {
        return Err(SettingsRejection::new(KIND_EMPTY, "设置请求没有任何字段。"));
    }
    validate(model, compaction.is_some(), params)?;

    let mut applied: Vec<String> = Vec::new();
    if let (Some(settings), Some(config)) = (params.model.as_deref(), model.as_mut()) {
        if let Some(name) = settings.model.as_ref() {
            config.model = name.trim().to_string();
            applied.push("model.model".to_string());
        }
        if let Some(options) = settings.options.as_ref() {
            config.options = options.clone();
            applied.push("model.options".to_string());
        }
        if let Some(effort) = settings.reasoning_effort.as_ref() {
            // 校验阶段已确认可归一化；与 Python 的 `set_reasoning_effort` 一样存归一化值。
            let normalized = normalize_reasoning_effort(effort)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| effort.trim().to_string());
            // 只改这一个键：整体替换 options 会顺手把别的生成选项打回默认值。
            let mut options = match std::mem::take(&mut config.options) {
                Value::Object(map) => map,
                // 非对象（含 null）视作空段，与 initialize 侧「null 按默认」的容错一致。
                _ => serde_json::Map::new(),
            };
            options.insert("reasoning_effort".to_string(), Value::String(normalized));
            config.options = Value::Object(options);
            applied.push("model.reasoning_effort".to_string());
        }
        if let Some(prompt) = settings.system_prompt.as_ref() {
            config.system_prompt = prompt.clone();
            applied.push("model.system_prompt".to_string());
        }
        if let Some(messages) = settings.context_messages.as_ref() {
            config.context_messages = messages.clone();
            applied.push("model.context_messages".to_string());
        }
        if let Some(tools) = settings.tools.as_ref() {
            config.tools = tools.clone();
            applied.push("model.tools".to_string());
        }
        if let Some(tokens) = settings.context_window_tokens {
            config.context_window_tokens = tokens;
            applied.push("model.context_window_tokens".to_string());
        }
        // 渠道字段：只是字符串替换，空串按「不改」处理（宿主换渠道时才会带）。
        for (value, target, path) in [
            (&settings.provider, &mut config.provider, "model.provider"),
            (&settings.protocol, &mut config.protocol, "model.protocol"),
            (&settings.base_url, &mut config.base_url, "model.base_url"),
            (
                &settings.api_key_env,
                &mut config.api_key_env,
                "model.api_key_env",
            ),
        ] {
            let Some(text) = value.as_ref() else {
                continue;
            };
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            *target = trimmed.to_string();
            applied.push(path.to_string());
        }
    }
    if let (Some(settings), Some(config)) = (params.compaction.as_ref(), compaction) {
        applied.extend(
            overlay_compaction_config(config, &normalized_compaction(settings))
                .into_iter()
                .map(str::to_string),
        );
    }
    Ok(applied)
}

/// 写入前的压缩段：`reasoning_effort` 按别名归一化（与 Python 的 `set_reasoning_effort`
/// 一致地存归一化值）；校验阶段已确认它可归一化，这里拿不到归一化结果就保持原值。
fn normalized_compaction(settings: &KernelCompactionConfig) -> KernelCompactionConfig {
    let mut copy = settings.clone();
    if let Some(effort) = settings.reasoning_effort.as_deref() {
        if let Ok(value) = normalize_reasoning_effort(effort) {
            copy.reasoning_effort = Some(value.to_string());
        }
    }
    copy
}

/// 只校验不改动；失败时调用方不会写入任何字段。
fn validate(
    model: &Option<KernelModelConfig>,
    has_session: bool,
    params: &SessionSettingsParams,
) -> Result<(), SettingsRejection> {
    if let Some(settings) = params.model.as_deref() {
        if model.is_none() {
            return Err(SettingsRejection::new(
                KIND_MODEL_UNAVAILABLE,
                "内核未持有模型配置（宿主走 model.reply 代答），无法更新模型设置。",
            ));
        }
        if let Some(name) = settings.model.as_ref() {
            if name.trim().is_empty() {
                return Err(SettingsRejection::new(KIND_INVALID, "模型 ID 不能为空。"));
            }
        }
        if let Some(options) = settings.options.as_ref() {
            if !options.is_object() && !options.is_null() {
                return Err(SettingsRejection::new(
                    KIND_INVALID,
                    "model.options 必须是对象。",
                ));
            }
        }
        if let Some(effort) = settings.reasoning_effort.as_ref() {
            normalize_reasoning_effort(effort)
                .map_err(|error| SettingsRejection::new(KIND_INVALID, error.message()))?;
        }
        if let Some(tools) = settings.tools.as_ref() {
            if tools.iter().any(|tool| !tool.is_object()) {
                return Err(SettingsRejection::new(
                    KIND_INVALID,
                    "model.tools 的每一项都必须是对象。",
                ));
            }
        }
        if let Some(tokens) = settings.context_window_tokens {
            if tokens <= 0 {
                return Err(SettingsRejection::new(
                    KIND_INVALID,
                    "上下文长度必须是正整数 Token。",
                ));
            }
        }
    }
    if let Some(settings) = params.compaction.as_ref() {
        if !has_session {
            return Err(SettingsRejection::new(
                KIND_SESSION_UNAVAILABLE,
                "内核未持有会话，无法更新压缩设置。",
            ));
        }
        check_positive(settings.trigger_context_tokens, "上下文压缩阈值")?;
        check_positive(settings.context_window_tokens, "上下文长度")?;
        check_positive(settings.target_summary_tokens, "摘要目标 Token 数")?;
        if settings.recent_turns.is_some_and(|value| value < 0) {
            return Err(SettingsRejection::new(
                KIND_INVALID,
                "保留最近回合数不能为负数。",
            ));
        }
        if settings
            .next_user_reserve_tokens
            .is_some_and(|value| value < 0)
        {
            return Err(SettingsRejection::new(
                KIND_INVALID,
                "下一轮用户输入预留 Token 数不能为负数。",
            ));
        }
        if let Some(ratio) = settings.emergency_context_ratio {
            if !(ratio > 0.0 && ratio <= 1.0) {
                return Err(SettingsRejection::new(
                    KIND_INVALID,
                    "应急压缩比例必须落在 (0, 1] 区间。",
                ));
            }
        }
        if let Some(effort) = settings.reasoning_effort.as_ref() {
            normalize_reasoning_effort(effort)
                .map_err(|error| SettingsRejection::new(KIND_INVALID, error.message()))?;
        }
    }
    Ok(())
}

fn check_positive(value: Option<i64>, label: &str) -> Result<(), SettingsRejection> {
    match value {
        Some(number) if number <= 0 => Err(SettingsRejection::new(
            KIND_INVALID,
            format!("{label}必须是正整数 Token。"),
        )),
        _ => Ok(()),
    }
}

/// 设置更新的响应负载：内核实际改动的字段。
pub fn applied_result(applied: &[String]) -> Value {
    json!({ "applied": applied })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_ipc::bridge::{KernelCompactionConfig, SessionModelSettings};

    fn model_config(tools: usize) -> KernelModelConfig {
        KernelModelConfig {
            model: "gpt-a".to_string(),
            provider: String::new(),
            protocol: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            user_agent: String::new(),
            system_prompt: "系统".to_string(),
            context_messages: Vec::new(),
            tools: (0..tools)
                .map(|index| json!({"type": "function", "function": {"name": format!("t{index}")}}))
                .collect(),
            options: json!({"reasoning_effort": "low"}),
            request_timeout_seconds: None,
            context_window_tokens: 128_000,
            prompt_cache_capable: false,
            prompt_cache_identity: Default::default(),
            native_vision: false,
            request_retry_count: 1,
        }
    }

    fn params(
        model: Option<SessionModelSettings>,
        compaction: Option<KernelCompactionConfig>,
    ) -> SessionSettingsParams {
        SessionSettingsParams {
            model: model.map(Box::new),
            compaction,
        }
    }

    #[test]
    fn empty_request_is_rejected() {
        let mut model = Some(model_config(1));
        let error = apply(&mut model, None, &params(None, None)).expect_err("空请求必须被拒绝");
        assert_eq!(error.kind, KIND_EMPTY);
    }

    #[test]
    fn model_settings_require_model_config() {
        let mut none: Option<KernelModelConfig> = None;
        let request = params(
            Some(SessionModelSettings {
                model: Some("gpt-b".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        let error = apply(&mut none, None, &request).expect_err("没有模型配置时不可改");
        assert_eq!(error.kind, KIND_MODEL_UNAVAILABLE);
    }

    #[test]
    fn compaction_settings_require_session() {
        let mut model = Some(model_config(1));
        let request = params(
            None,
            Some(KernelCompactionConfig {
                trigger_context_tokens: Some(50_000),
                ..KernelCompactionConfig::default()
            }),
        );
        let error = apply(&mut model, None, &request).expect_err("没有会话时不可改压缩");
        assert_eq!(error.kind, KIND_SESSION_UNAVAILABLE);
    }

    #[test]
    fn invalid_field_leaves_everything_untouched() {
        let mut model = Some(model_config(2));
        let mut config = CompactionConfig::default();
        let before_options = model.as_ref().map(|config| config.options.clone());
        let before_trigger = config.trigger_context_tokens;
        let request = params(
            Some(SessionModelSettings {
                options: Some(json!({"reasoning_effort": "max"})),
                context_window_tokens: Some(0),
                ..SessionModelSettings::default()
            }),
            Some(KernelCompactionConfig {
                trigger_context_tokens: Some(11_000),
                ..KernelCompactionConfig::default()
            }),
        );
        let error = apply(&mut model, Some(&mut config), &request).expect_err("非法值必须被拒绝");
        assert_eq!(error.kind, KIND_INVALID);
        assert_eq!(
            model.as_ref().map(|config| config.options.clone()),
            before_options,
            "非法请求不得写入已通过校验的字段"
        );
        assert_eq!(config.trigger_context_tokens, before_trigger);
    }

    #[test]
    fn reasoning_effort_merges_into_options_without_resetting_others() {
        let mut model = Some(model_config(1));
        if let Some(config) = model.as_mut() {
            config.options = json!({"temperature": 0.5, "max_output_tokens": 4096});
        }
        let request = params(
            Some(SessionModelSettings {
                reasoning_effort: Some("X-High".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        let applied = apply(&mut model, None, &request).expect("别名应被接受");

        assert_eq!(applied, vec!["model.reasoning_effort".to_string()]);
        let options = model
            .as_ref()
            .map(|config| config.options.clone())
            .unwrap_or_default();
        assert_eq!(options["reasoning_effort"], json!("xhigh"), "存归一化值");
        assert_eq!(options["temperature"], json!(0.5), "不碰其它生成选项");
        assert_eq!(options["max_output_tokens"], json!(4096));
    }

    #[test]
    fn reasoning_effort_fills_empty_options_object() {
        let mut model = Some(model_config(1));
        if let Some(config) = model.as_mut() {
            config.options = Value::Null;
        }
        let request = params(
            Some(SessionModelSettings {
                reasoning_effort: Some("none".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        apply(&mut model, None, &request).expect("空 options 应能写入");

        assert_eq!(
            model.as_ref().map(|config| config.options.clone()),
            Some(json!({"reasoning_effort": "none"}))
        );
    }

    #[test]
    fn unknown_reasoning_effort_in_model_segment_is_rejected() {
        let mut model = Some(model_config(1));
        let request = params(
            Some(SessionModelSettings {
                reasoning_effort: Some("ultra".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        let error = apply(&mut model, None, &request).expect_err("未知档位必须被拒绝");
        assert_eq!(error.kind, KIND_INVALID);
    }

    #[test]
    fn channel_fields_replace_only_when_given() {
        let mut model = Some(model_config(1));
        let request = params(
            Some(SessionModelSettings {
                model: Some("gpt-b".to_string()),
                provider: Some("anthropic".to_string()),
                protocol: Some("anthropic_messages".to_string()),
                base_url: Some("https://api.example.com".to_string()),
                api_key_env: Some("EXAMPLE_KEY".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        let applied = apply(&mut model, None, &request).expect("渠道字段应被接受");

        assert_eq!(
            applied,
            vec![
                "model.model".to_string(),
                "model.provider".to_string(),
                "model.protocol".to_string(),
                "model.base_url".to_string(),
                "model.api_key_env".to_string(),
            ]
        );
        let config = model.expect("仍有模型配置");
        assert_eq!(config.model, "gpt-b");
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.protocol, "anthropic_messages");
        assert_eq!(config.base_url, "https://api.example.com");
        assert_eq!(config.api_key_env, "EXAMPLE_KEY");
    }

    #[test]
    fn blank_channel_field_is_ignored() {
        let mut model = Some(model_config(1));
        if let Some(config) = model.as_mut() {
            config.provider = "openai".to_string();
        }
        let request = params(
            Some(SessionModelSettings {
                base_url: Some("   ".to_string()),
                ..SessionModelSettings::default()
            }),
            None,
        );
        let applied = apply(&mut model, None, &request).expect("空串按不改处理");

        assert!(applied.is_empty(), "空串不改任何字段：{applied:?}");
        assert_eq!(
            model.as_ref().map(|config| config.provider.as_str()),
            Some("openai")
        );
    }

    #[test]
    fn tools_and_threshold_apply_and_report_paths() {
        let mut model = Some(model_config(3));
        let mut config = CompactionConfig::default();
        let request = params(
            Some(SessionModelSettings {
                tools: Some(vec![json!({"type": "function"})]),
                context_window_tokens: Some(200_000),
                ..SessionModelSettings::default()
            }),
            Some(KernelCompactionConfig {
                trigger_context_tokens: Some(160_000),
                context_window_tokens: Some(200_000),
                ..KernelCompactionConfig::default()
            }),
        );
        let applied = apply(&mut model, Some(&mut config), &request).expect("合法请求应生效");

        assert_eq!(
            applied,
            vec![
                "model.tools".to_string(),
                "model.context_window_tokens".to_string(),
                "compaction.trigger_context_tokens".to_string(),
                "compaction.context_window_tokens".to_string(),
            ]
        );
        assert_eq!(model.as_ref().map(|config| config.tools.len()), Some(1));
        assert_eq!(
            model.as_ref().map(|config| config.context_window_tokens),
            Some(200_000)
        );
        assert_eq!(config.trigger_context_tokens, 160_000);
        assert_eq!(config.context_window_tokens, 200_000);
        // 未给出的字段保持原值：模型名与默认保留回合数都没被动过。
        assert_eq!(
            model.as_ref().map(|config| config.model.as_str()),
            Some("gpt-a")
        );
        assert_eq!(
            config.recent_turns,
            CompactionConfig::default().recent_turns
        );
    }

    #[test]
    fn reasoning_effort_alias_is_normalized_into_compaction() {
        let mut model = Some(model_config(1));
        let mut config = CompactionConfig::default();
        let request = params(
            None,
            Some(KernelCompactionConfig {
                reasoning_effort: Some("X-High".to_string()),
                ..KernelCompactionConfig::default()
            }),
        );
        let applied = apply(&mut model, Some(&mut config), &request).expect("别名应被接受");

        assert_eq!(applied, vec!["compaction.reasoning_effort".to_string()]);
        assert_eq!(config.reasoning_effort, "xhigh");
    }

    #[test]
    fn unknown_reasoning_effort_is_rejected() {
        let mut model = Some(model_config(1));
        let mut config = CompactionConfig::default();
        let request = params(
            None,
            Some(KernelCompactionConfig {
                reasoning_effort: Some("ultra".to_string()),
                ..KernelCompactionConfig::default()
            }),
        );
        let error = apply(&mut model, Some(&mut config), &request).expect_err("未知档位必须被拒绝");
        assert_eq!(error.kind, KIND_INVALID);
        assert!(error.message.contains("reasoning_effort"));
    }
}
