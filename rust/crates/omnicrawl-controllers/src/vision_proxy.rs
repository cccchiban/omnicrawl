//! `omnicrawl/agent/runtime/vision_proxy.py` 的判定面：候选选择、模型标签、失败汇总与文本截断。
//!
//! 真正的模型调用在内核侧（`omnicrawl-cli/src/vision_proxy.rs`）：这里只放能脱离运行时验证的部分。

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::models::llm::ActiveModelRef;
use omnicrawl_config::models::vision::load_vision_configuration;
use omnicrawl_core::AgentLoopObservation;
use serde_json::{json, Value};

/// 视觉分析文本的字数上限（Python 侧 `VisionModelProxy` 的默认值）。
pub const VISION_MAX_OUTPUT_CHARS: usize = 6000;
/// 单个候选错误在汇总文案里的最大长度（Python 侧的 240 字符）。
pub const VISION_CANDIDATE_ERROR_CHARS: usize = 240;

pub const VISION_PROXY_DISABLED_ERROR: &str = "视觉模型代理未启用。";
pub const VISION_PROXY_NO_MODELS_ERROR: &str = "视觉模型代理已启用，但没有配置视觉模型。";
pub const VISION_PROXY_NO_IMAGES_ERROR: &str = "视觉模型代理没有收到图片附件。";
pub const VISION_PROXY_EMPTY_TEXT_ERROR: &str = "视觉模型返回了空文本。";
pub const VISION_PROXY_NO_REF_ERROR: &str = "视觉模型引用缺少可用的模型标识。";
pub const VISION_PROXY_NO_DETAIL: &str = "没有可用的错误详情";

/// 候选模型的请求选择串：custom 用 key，detected 用 `profile/model_id`。
pub fn model_ref_selection(reference: &ActiveModelRef) -> Result<String, String> {
    if reference.source == "custom" && !reference.key.is_empty() {
        return Ok(reference.key.clone());
    }
    if reference.source == "detected"
        && !reference.profile.is_empty()
        && !reference.model_id.is_empty()
    {
        return Ok(format!("{}/{}", reference.profile, reference.model_id));
    }
    Err(VISION_PROXY_NO_REF_ERROR.to_string())
}

/// 错误与展示里的模型标签。
pub fn model_ref_label(reference: &ActiveModelRef) -> String {
    if reference.source == "custom" {
        return if reference.key.is_empty() {
            "custom/unknown".to_string()
        } else {
            reference.key.clone()
        };
    }
    if !reference.profile.is_empty() {
        return if reference.model_id.is_empty() {
            reference.profile.clone()
        } else {
            format!("{}/{}", reference.profile, reference.model_id)
        };
    }
    if reference.model_id.is_empty() {
        "unknown".to_string()
    } else {
        reference.model_id.clone()
    }
}

/// 单个候选的失败摘要：`标签：消息`，消息按上限截断。
pub fn candidate_error(label: &str, message: &str) -> String {
    let trimmed = message.trim();
    let text = if trimmed.is_empty() {
        "未知错误"
    } else {
        trimmed
    };
    let bounded: String = text.chars().take(VISION_CANDIDATE_ERROR_CHARS).collect();
    format!("{label}：{bounded}")
}

/// 全部候选失败时的模型可见文案。
pub fn all_candidates_failed_error(errors: &[String]) -> String {
    let detail = if errors.is_empty() {
        VISION_PROXY_NO_DETAIL.to_string()
    } else {
        errors.join("；")
    };
    format!("视觉模型全部调用失败：{detail}")
}

/// 分析文本按上限截断（Python 的 `…视觉模型分析已截断。` 后缀）。
pub fn bound_text(text: &str, max_chars: usize) -> String {
    let limit = max_chars.max(1);
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n…视觉模型分析已截断。")
}

/// 视觉请求消息：一条 user 消息，正文是「文本 + 图片部件」。
pub fn vision_request_message(content: Value) -> Value {
    json!({"role": "user", "content": content})
}

/// 观察里的视觉观察正文：宿主注入的 user 消息，正文是数组且含 `image_url` 部件。
///
/// 宿主注入图片时用的就是这条形状（`omnicrawl-tui` 的 `vision_observation_messages`），
/// 内核据此判断「这批观察带不带图」。图片的去向不靠这条形状决定：原生视觉与独立视觉代理
/// 的优先级由 `initialize.model.native_vision` 加上 `[vision]` 配置共同决定。
pub fn vision_followup_content(observation: &AgentLoopObservation) -> Option<Value> {
    observation.followup_messages.iter().find_map(|message| {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            return None;
        }
        let content = message.get("content")?.as_array()?;
        let has_image = content
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some("image_url"));
        if !has_image {
            return None;
        }
        Some(Value::Array(content.clone()))
    })
}

/// `[vision]` 里是否配置了可用的视觉模型代理（已启用且有候选模型）。
///
/// 宿主据此决定要不要把图片交给内核（`native_vision` 为假时仍可能要走代理），内核据此
/// 决定要不要装配代理——两边必须是同一个判定，否则会出现「交了图却没人处理」。
pub fn vision_proxy_configured(env: &ConfigEnvironment) -> bool {
    load_vision_configuration(env, None)
        .map(|config| config.enabled && !config.models.is_empty())
        .unwrap_or(false)
}

/// 清掉带图观察的 followup，返回被清理的观察数。
///
/// 主模型看不懂图、代理又装配不出来时，图片不能留在请求里：Python 的 `route_image_result`
/// 在同样条件下返回空 followup（非视觉主模型只收到图片元数据）。宿主没把图片交出来时
/// 这里无事可做。
pub fn strip_vision_followups(observations: &mut [AgentLoopObservation]) -> usize {
    let mut cleared = 0;
    for observation in observations.iter_mut() {
        if vision_followup_content(observation).is_some() {
            observation.followup_messages.clear();
            cleared += 1;
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::models::llm::ActiveModelRef;
    use omnicrawl_core::{ToolCall, ToolResult};

    fn reference(source: &str, key: &str, profile: &str, model_id: &str) -> ActiveModelRef {
        ActiveModelRef {
            source: source.to_string(),
            key: key.to_string(),
            profile: profile.to_string(),
            model_id: model_id.to_string(),
            protocol: String::new(),
        }
    }

    #[test]
    fn strips_only_image_followups() {
        let with_image = observation(vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
            ],
        })]);
        let text_only = observation(vec![json!({"role": "user", "content": "纯文本"})]);
        let mut items = vec![with_image, text_only];

        assert_eq!(strip_vision_followups(&mut items), 1);
        assert!(items[0].followup_messages.is_empty(), "图片观察应被清掉");
        assert_eq!(items[1].followup_messages.len(), 1, "纯文本观察不动");
        // 工具消息本身（模型看得到的文本）保持不变。
        assert_eq!(items[0].message["content"], "已读取图片");
    }

    #[test]
    fn vision_proxy_is_unconfigured_without_a_config_file() {
        // 隔离环境读不到 `[vision]`：按未配置处理，不 panic、不误判为可用。
        let env = ConfigEnvironment::new("bundle", "test");
        assert!(!vision_proxy_configured(&env));
    }

    #[test]
    fn selection_and_label_match_python_rules() {
        let custom = reference("custom", "my-vision", "", "");
        assert_eq!(model_ref_selection(&custom).unwrap(), "my-vision");
        assert_eq!(model_ref_label(&custom), "my-vision");

        let detected = reference("detected", "", "profile-a", "qwen-vl");
        assert_eq!(model_ref_selection(&detected).unwrap(), "profile-a/qwen-vl");
        assert_eq!(model_ref_label(&detected), "profile-a/qwen-vl");

        let bare = reference("detected", "", "", "qwen-vl");
        assert!(model_ref_selection(&bare).is_err());
        assert_eq!(model_ref_label(&bare), "qwen-vl");

        assert_eq!(
            model_ref_label(&reference("custom", "", "", "")),
            "custom/unknown"
        );
    }

    #[test]
    fn failures_are_summarized_with_bounded_candidates() {
        let long = "x".repeat(VISION_CANDIDATE_ERROR_CHARS + 20);
        let first = candidate_error("a/1", &long);
        assert_eq!(
            first.chars().count(),
            "a/1：".chars().count() + VISION_CANDIDATE_ERROR_CHARS
        );
        assert_eq!(candidate_error("a/1", "   "), "a/1：未知错误");

        let text = all_candidates_failed_error(&[first, "b/2：连接被拒绝".to_string()]);
        assert!(text.starts_with("视觉模型全部调用失败：a/1："), "{text}");
        assert!(text.contains("；b/2：连接被拒绝"), "{text}");
        assert_eq!(
            all_candidates_failed_error(&[]),
            "视觉模型全部调用失败：没有可用的错误详情"
        );
    }

    #[test]
    fn text_is_bounded_with_python_suffix() {
        assert_eq!(bound_text("短文本", 6000), "短文本");
        let bounded = bound_text(&"字".repeat(10), 4);
        assert_eq!(bounded, "字字字字\n…视觉模型分析已截断。");
        assert_eq!(bound_text("", 0), "");
    }

    fn observation(followups: Vec<Value>) -> AgentLoopObservation {
        AgentLoopObservation {
            tool_call: ToolCall {
                name: "read_image".to_string(),
                arguments: serde_json::Map::new(),
                id: "call-1".to_string(),
                function_name: "read_image".to_string(),
            },
            result: ToolResult {
                ok: true,
                output: "已读取图片".to_string(),
                full_output: "已读取图片".to_string(),
                error_code: None,
                retryable: false,
            },
            message: json!({"role": "tool", "tool_call_id": "call-1", "content": "已读取图片"}),
            followup_messages: followups,
        }
    }

    #[test]
    fn vision_content_is_detected_from_image_parts() {
        let image_followup = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "看看这张图"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA", "detail": "auto"}},
            ],
        });
        let content =
            vision_followup_content(&observation(vec![image_followup])).expect("应当识别");
        assert_eq!(content.as_array().expect("数组").len(), 2);

        // 纯文本 followup、assistant 消息、非数组正文都不算视觉观察。
        assert!(vision_followup_content(&observation(vec![
            json!({"role": "user", "content": "文本"})
        ]))
        .is_none());
        assert!(vision_followup_content(&observation(vec![json!({
            "role": "assistant",
            "content": [{"type": "image_url", "image_url": {"url": "x"}}],
        })]))
        .is_none());
        assert!(vision_followup_content(&observation(Vec::new())).is_none());
    }
}
