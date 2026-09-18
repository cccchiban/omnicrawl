//! `omnicrawl/agent/controllers/advisor.py` 的判定层。
//!
//! 顾问是一次旁路单轮补全：不产生转录、无工具、无审批。这里收「能不能咨询」「转发什么
//! 消息」「结果信封长什么样」；模型选择、Runtime 引导、协议调用与用量回调留给宿主。

use crate::error::AgentError;
use crate::types::ToolResult;
use serde_json::{json, Map, Value};
use std::path::Path;

pub const ADVISOR_TOOL_NAME: &str = "advisor";

pub const ADVISOR_SYSTEM_TEMPLATE_NAME: &str = "advisor_system.md";

pub const ADVISOR_NUDGE_TEXT: &str = "请基于以上执行者工作情况给出 plan/correction/stop 指导。";

pub const ADVISOR_EMPTY_ERROR: &str = "顾问连续两次返回空响应，请稍后重试。";

pub const ADVISOR_DISABLED_ERROR: &str = "顾问未启用：请先通过 /advisor 选择顾问模型。";

pub const ADVISOR_BLACKLISTED_ERROR: &str = "当前执行者模型在顾问黑名单中，advisor 不可用。";

pub const ADVISOR_NO_CONTEXT_ERROR: &str = "当前没有可评审的工作上下文。";

pub const ADVISOR_NO_LLM_ERROR: &str = "当前运行态不支持顾问模型（缺少完整 LLM 配置）。";

/// 读取内置顾问系统提示模板。
pub fn advisor_system_prompt(templates_dir: &Path) -> Result<String, AgentError> {
    let path = templates_dir.join(ADVISOR_SYSTEM_TEMPLATE_NAME);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| AgentError::new(format!("读取顾问系统提示模板失败：{error}")))?;
    Ok(text.trim().to_string())
}

/// `advisor` 是否进入工具表：显式启用且未命中当前模型黑名单。
pub fn advisor_is_active(active: bool, blacklisted: bool) -> bool {
    active && !blacklisted
}

/// 当前执行者模型是否命中 `disabled_for_models` 黑名单（大小写不敏感的子串匹配）。
pub fn advisor_blacklisted(
    disabled_for_models: &[String],
    catalog_key: &str,
    profile_id: &str,
    model: &str,
) -> bool {
    if disabled_for_models.is_empty() {
        return false;
    }
    let haystack = [catalog_key, profile_id, model]
        .iter()
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    disabled_for_models
        .iter()
        .map(|item| item.trim().to_lowercase())
        .filter(|item| !item.is_empty())
        .any(|item| haystack.contains(&item))
}

/// 剥掉尾部 assistant 消息里 `name=advisor` 的孤儿 toolCall（转发前必须剥掉）。
pub fn strip_inflight_advisor_call(messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }
    let mut cleaned = messages;
    for index in (0..cleaned.len()).rev() {
        let message = &cleaned[index];
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let tool_calls = match message.get("tool_calls") {
            Some(Value::Array(calls)) if !calls.is_empty() => calls.clone(),
            _ => break,
        };
        let mut remaining: Vec<Value> = Vec::new();
        let mut stripped = false;
        for call in &tool_calls {
            let function_name = call
                .get("function")
                .filter(|value| !value.is_null())
                .and_then(|function| function.get("name"))
                .map(crate::undo::python_str)
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            if function_name == ADVISOR_TOOL_NAME {
                stripped = true;
                continue;
            }
            remaining.push(call.clone());
        }
        if stripped {
            let mut rebuilt = message.clone();
            if let Value::Object(map) = &mut rebuilt {
                if remaining.is_empty() {
                    map.remove("tool_calls");
                } else {
                    map.insert("tool_calls".to_string(), Value::Array(remaining));
                }
            }
            cleaned[index] = rebuilt;
        }
        break;
    }
    cleaned
}

/// 保证消息尾部是 user 角色：部分模型拒绝 assistant 结尾的请求。
pub fn ensure_user_tail(messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return vec![json!({"role": "user", "content": ADVISOR_NUDGE_TEXT})];
    }
    let mut cleaned = messages;
    if cleaned
        .last()
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        == Some("user")
    {
        return cleaned;
    }
    cleaned.push(json!({"role": "user", "content": ADVISOR_NUDGE_TEXT}));
    cleaned
}

/// 转发给顾问的消息分支：剥孤儿调用 + 保证 user 尾。
pub fn build_advisor_branch(messages: Vec<Value>) -> Vec<Value> {
    ensure_user_tail(strip_inflight_advisor_call(messages))
}

/// 从工具表生成 `## Available Executor Tools` 清单（按键排序、稳定序列化）。
pub fn executor_tool_inventory(tools: &[(String, String)]) -> String {
    let mut lines = vec!["## Available Executor Tools".to_string()];
    let mut entries: Vec<&(String, String)> = tools.iter().collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, description) in entries {
        let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(format!("- {name}: {collapsed}"));
    }
    lines.join("\n")
}

/// 顾问请求的超时：取执行者与顾问两侧的上限较小值。
pub fn advisor_request_timeout(parent_timeout_seconds: i64, frozen_timeout_seconds: i64) -> i64 {
    std::cmp::min(parent_timeout_seconds, frozen_timeout_seconds)
}

/// 顾问请求的前缀缓存身份（键序固定，供 Provider 缓存对齐）。
pub fn advisor_prompt_cache_identity(workspace: &str, selection: &str) -> Value {
    let mut map = Map::new();
    map.insert("workspace".to_string(), Value::from(workspace));
    map.insert("advisor".to_string(), Value::from("system"));
    map.insert("model".to_string(), Value::from(selection));
    Value::Object(map)
}

pub fn advisor_status_text(model_key: &str, effort: &str) -> String {
    format!("正在咨询顾问（{model_key}，effort={effort}）…")
}

pub fn model_resolve_failed(cause: &str) -> ToolResult {
    advisor_error_result(&format!("顾问模型无法解析：{cause}"))
}

pub fn config_invalid(cause: &str) -> ToolResult {
    advisor_error_result(&format!("顾问模型配置无效：{cause}"))
}

pub fn runtime_init_failed(cause: &str) -> ToolResult {
    advisor_error_result(&format!("顾问模型 Runtime 初始化失败：{cause}"))
}

pub fn request_failed(cause: &str) -> ToolResult {
    advisor_error_result(&format!("顾问请求失败：{cause}"))
}

pub fn advisor_error_result(message: &str) -> ToolResult {
    ToolResult {
        ok: false,
        output: message.to_string(),
        full_output: message.to_string(),
        ..ToolResult::default()
    }
}

/// 顾问成功结果信封：纯文本 + `ui_artifact.advisor` 元数据。
pub fn advisor_success_result(
    text: &str,
    selection: &str,
    effort: &str,
    usage: Option<(i64, i64, i64)>,
) -> ToolResult {
    let mut details = Map::new();
    details.insert("advisor_model".to_string(), Value::from(selection));
    details.insert("effort".to_string(), Value::from(effort));
    if let Some((input, output, cached_input)) = usage {
        let mut usage_map = Map::new();
        usage_map.insert("input_tokens".to_string(), Value::from(input));
        usage_map.insert("output_tokens".to_string(), Value::from(output));
        usage_map.insert("cached_input_tokens".to_string(), Value::from(cached_input));
        details.insert("usage".to_string(), Value::Object(usage_map));
    }
    let mut artifact = Map::new();
    artifact.insert("advisor".to_string(), Value::Object(details));
    ToolResult {
        ok: true,
        output: text.to_string(),
        full_output: text.to_string(),
        ui_artifact: Value::Object(artifact),
        ..ToolResult::default()
    }
}
