//! `omnicrawl/agent/controllers/session/settings.py` 的判定层，外加它调用的三个校验件
//! （`config/features/approval.py`、`llm/normalize_reasoning_effort`、
//! `config/features/{tools,subagents}.py`）。
//!
//! 宿主边界：setter 里「替换 `self.config` 字段 → 重建工具表 / Runtime / MCP Manager，
//! 失败回滚」这一整套事务留在 Python；这里只收校验、归一化与阈值换算。

use crate::error::AgentError;
use omnicrawl_config::features::context_compaction::ContextCompactionConfig;
use omnicrawl_ipc::KernelCompactionConfig;

// 审批模式与推理强度的别名表只有配置域一份实现（Python 侧同样只有
// `config/features/approval.py` 与 `config/models/llm.py`），这里改为复用。
pub use omnicrawl_config::features::approval::{
    APPROVAL_MODE_AUTO, APPROVAL_MODE_MANUAL, APPROVAL_MODE_REVIEW,
};
pub use omnicrawl_config::models::llm::VALID_REASONING_EFFORTS;

pub const VALID_APPROVAL_MODES: [&str; 3] = [
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
];

pub const DEFAULT_CONTEXT_WINDOW_TOKENS: i64 = 128_000;

pub const TOOL_SWITCH_KEYS: [&str; 26] = [
    "list",
    "find",
    "read",
    "read_image",
    "grep",
    "web_search",
    "fetcher",
    "image_gen",
    "tts_synthesize",
    "Edit_file",
    "write_file",
    "bash",
    "powershell",
    "monitor",
    "recall_session_evidence",
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "subagent",
    "update_todos",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
];

pub const SUBAGENT_ADVANCED_SETTING_KEYS: [&str; 6] = [
    "max_concurrency",
    "max_tasks_per_batch",
    "default_timeout_seconds",
    "model_request_concurrency",
    "verify_command_timeout_seconds",
    "task_retention_minutes",
];

/// `(取值类型, 下限, 上限)`；`取值类型` 为 `int` 或 `number`。
pub const SUBAGENT_ADVANCED_SETTING_RULES: [(&str, &str, f64, f64); 6] = [
    ("max_concurrency", "int", 1.0, 4.0),
    ("max_tasks_per_batch", "int", 1.0, 4.0),
    ("default_timeout_seconds", "number", 1.0, 3600.0),
    ("model_request_concurrency", "int", 1.0, 4.0),
    ("verify_command_timeout_seconds", "int", 1.0, 360.0),
    ("task_retention_minutes", "int", 1.0, 10080.0),
];

/// 审批模式归一化：大小写、空格与 `-`/`_` 差异都收敛到闭集取值。
pub fn normalize_approval_mode(value: &str) -> Result<&'static str, AgentError> {
    omnicrawl_config::features::approval::normalize_approval_mode(value)
        .map_err(|error| AgentError::new(error.message()))
}

/// 推理强度归一化；空串是合法取值（表示未设置）。
pub fn normalize_reasoning_effort(value: &str) -> Result<&'static str, AgentError> {
    omnicrawl_config::models::llm::normalize_reasoning_effort(value)
        .map_err(|error| AgentError::new(error.message()))
}

/// 推理强度归一化后同步的思考类型开关。
pub fn thinking_type_for(effort: &str) -> &'static str {
    if effort == "none" || effort == "disabled" {
        "disabled"
    } else {
        "enabled"
    }
}

/// 按当前模型的上下文窗口百分比换算压缩阈值 Token。
pub fn context_compaction_trigger_tokens(context_window_tokens: i64, percent: i64) -> i64 {
    std::cmp::max(1, context_window_tokens * percent / 100)
}

/// `set_context_window_tokens` 的联动值：窗口变化后按既有百分比重算阈值。
pub fn tokens_after_window_change(new_window_tokens: i64, percent: Option<i64>) -> Option<i64> {
    percent.map(|value| context_compaction_trigger_tokens(new_window_tokens, value))
}

/// `set_context_compaction_trigger_percent` 的目标值；与现值相同则返回 `None`（不动作）。
pub fn percent_change_target(
    context_window_tokens: i64,
    percent: i64,
    current_tokens: i64,
    current_percent: Option<i64>,
) -> Option<i64> {
    let tokens = context_compaction_trigger_tokens(context_window_tokens, percent);
    if tokens == current_tokens && current_percent == Some(percent) {
        return None;
    }
    Some(tokens)
}

/// `set_context_compaction_trigger_tokens` 的目标值；与现值相同则返回 `None`（不动作）。
pub fn token_change_target(
    tokens: i64,
    current_tokens: i64,
    current_percent: Option<i64>,
) -> Option<i64> {
    if tokens == current_tokens && current_percent.is_none() {
        return None;
    }
    Some(tokens)
}

pub fn validate_positive_tokens(tokens: i64) -> Result<i64, AgentError> {
    if tokens <= 0 {
        return Err(AgentError::new("上下文长度必须是正整数 Token。"));
    }
    Ok(tokens)
}

pub fn validate_compaction_percent(percent: i64) -> Result<i64, AgentError> {
    if percent <= 0 {
        return Err(AgentError::new("上下文压缩阈值百分比必须是正整数。"));
    }
    Ok(percent)
}

pub fn validate_compaction_tokens(tokens: i64) -> Result<i64, AgentError> {
    if tokens <= 0 {
        return Err(AgentError::new("上下文压缩阈值必须是正整数 Token。"));
    }
    Ok(tokens)
}

/// 内置工具开关名校验；旧版按作用域拆分的记忆开关会被映射回统一的 `memory_*`。
pub fn validate_tool_switch_name(name: &str) -> Result<String, AgentError> {
    let normalized = name.trim();
    let normalized = legacy_tool_switch_alias(normalized).unwrap_or(normalized);
    if !TOOL_SWITCH_KEYS.contains(&normalized) {
        return Err(AgentError::new(format!(
            "tools 配置不支持工具：{normalized}。可用：{}。",
            TOOL_SWITCH_KEYS.join(", ")
        )));
    }
    Ok(normalized.to_string())
}

/// `project_memory_search` → `memory_search` 这类旧开关名。
fn legacy_tool_switch_alias(name: &str) -> Option<&'static str> {
    for prefix in ["project_", "session_", "user_"] {
        let Some(rest) = name.strip_prefix(prefix) else {
            continue;
        };
        let target = match rest {
            "memory_search" => "memory_search",
            "memory_read" => "memory_read",
            "memory_expand_related" => "memory_expand_related",
            "memory_write" => "memory_write",
            _ => continue,
        };
        return Some(target);
    }
    None
}

/// 开关一个内置工具后的禁用集合；`None` 表示无需改动（集合未变化）。
pub fn apply_tool_switch(
    disabled: &[String],
    name: &str,
    enabled: bool,
) -> Result<Option<Vec<String>>, AgentError> {
    let normalized = validate_tool_switch_name(name)?;
    let mut next: Vec<String> = disabled.to_vec();
    if enabled {
        next.retain(|item| item != &normalized);
    } else if !next.contains(&normalized) {
        next.push(normalized);
    } else {
        return Ok(None);
    }
    let mut current: Vec<String> = disabled.to_vec();
    current.sort();
    next.sort();
    if current == next {
        return Ok(None);
    }
    Ok(Some(next))
}

/// 设置面板开放的 SubAgent 资源参数校验；返回归一化后的数值。
pub fn validate_subagent_advanced_setting(
    name: &str,
    value: f64,
    is_int: bool,
) -> Result<f64, AgentError> {
    let Some((_, value_type, minimum, maximum)) = SUBAGENT_ADVANCED_SETTING_RULES
        .iter()
        .find(|(key, _, _, _)| *key == name)
    else {
        return Err(AgentError::new(format!(
            "设置面板不支持配置项 subagents.{name}。"
        )));
    };
    if *value_type == "int" && !is_int {
        return Err(AgentError::new(format!(
            "配置项 subagents.{name} 必须是整数。"
        )));
    }
    let normalized = value;
    if normalized < *minimum || normalized > *maximum {
        return Err(AgentError::new(format!(
            "配置项 subagents.{name} 必须在 {} 到 {} 之间，当前值：{}。",
            format_general(*minimum),
            format_general(*maximum),
            format_general(normalized)
        )));
    }
    Ok(normalized)
}

/// Python `{value:g}` 的可用子集：整数不带小数点，其余走最短表示。
fn format_general(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e6 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

pub const MODEL_ID_REQUIRED: &str = "模型 ID 不能为空。";

/// `set_model` 的选择串：去空白后为空即拒绝；允许 key / alias / `profile/model_id` 形态。
pub fn model_selection(value: &str) -> Result<String, AgentError> {
    let selection = value.trim();
    if selection.is_empty() {
        return Err(AgentError::new(MODEL_ID_REQUIRED));
    }
    Ok(selection.to_string())
}

pub const NATIVE_VISION_INVALID: &str = "模型原生视觉必须是布尔值或未配置。";

pub const CURRENT_MODEL_MISSING: &str = "当前 Agent 缺少模型配置。";

pub const TOOL_SWITCH_INVALID: &str = "工具开关必须是布尔值。";

pub const SUBAGENT_SWITCH_INVALID: &str = "SubAgent 开关必须是布尔值。";

pub const SUBAGENTS_DISABLE_FAILED: &str = "SubAgent 关闭失败：仍有子任务未退出。";

pub const SUBAGENTS_DISABLE_REASON: &str = "SubAgent 功能即将关闭，当前子任务已取消。";

pub const PLUGIN_SWITCH_INVALID: &str = "Plugin 开关必须是布尔值。";

pub const PLUGIN_RUNTIME_MISSING: &str = "Plugin Runtime 未连接，无法即时切换插件。";

pub const MCP_SWITCH_INVALID: &str = "MCP 开关必须是布尔值。";

pub const MCP_CONFIG_INVALID: &str = "MCP 配置类型无效。";

pub fn model_switch_failed(cause: &str) -> AgentError {
    AgentError::new(format!("模型运行时切换失败：{cause}"))
}

pub fn mcp_apply_failed(cause: &str) -> AgentError {
    AgentError::new(format!("MCP 设置应用失败：{cause}"))
}

pub fn plugin_apply_failed(cause: &str) -> AgentError {
    AgentError::new(format!("Plugin 设置应用失败：{cause}"))
}

/// `[context_compaction]` 整段映射成内核协议的压缩策略字段。
///
/// 宿主必须在握手时下发：内核只认协议字段，缺字段一律回落到 `CompactionConfig::default()`
/// （摘要预算 2000 token、记忆回写关闭），于是用户的 `[context_compaction]` 就会「写了却不生效」。
/// 触发阈值优先按配置里的百分比用当前窗口换算（与 Python 运行期
/// `context_compaction_trigger_percent` 的联动一致），没有百分比才用固定的
/// `trigger_context_tokens`：两条口径都对应「窗口的百分之几时压缩」这一个配置关系。
pub fn kernel_compaction_settings(
    context_window_tokens: i64,
    config: &ContextCompactionConfig,
) -> KernelCompactionConfig {
    let trigger_context_tokens = match config.trigger_context_percent {
        Some(percent) => context_compaction_trigger_tokens(context_window_tokens, percent),
        None => config.trigger_context_tokens,
    };
    KernelCompactionConfig {
        recent_turns: Some(config.recent_turns),
        // 0 = 无摘要预算上限：必须原样传给内核，否则会被默认的 2000 顶掉。
        target_summary_tokens: Some(config.target_summary_tokens),
        next_user_reserve_tokens: Some(config.next_user_reserve_tokens),
        trigger_context_tokens: Some(trigger_context_tokens),
        context_window_tokens: Some(context_window_tokens),
        emergency_context_ratio: Some(config.emergency_context_ratio),
        reasoning_effort: Some(config.reasoning_effort.clone()),
        preserve_exact_evidence: Some(config.preserve_exact_evidence),
        archive_compacted_events: Some(config.archive_compacted_events),
        auto_memory_recall: Some(config.auto_memory_recall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置里写了的字段必须逐项落到协议字段上：漏一个就是「配置写了却不生效」。
    #[test]
    fn kernel_compaction_settings_carry_the_whole_section() {
        let config = ContextCompactionConfig {
            trigger_context_percent: Some(20),
            trigger_context_tokens: 999_999,
            next_user_reserve_tokens: 4096,
            emergency_context_ratio: 0.85,
            reasoning_effort: "low".to_string(),
            recent_turns: 6,
            target_summary_tokens: 0,
            preserve_exact_evidence: true,
            archive_compacted_events: true,
            auto_memory_recall: true,
            ..ContextCompactionConfig::default()
        };
        let settings = kernel_compaction_settings(1_024_000, &config);
        assert_eq!(
            settings.trigger_context_tokens,
            Some(204_800),
            "有百分比时按窗口换算，忽略可能过期的固定阈值"
        );
        assert_eq!(settings.context_window_tokens, Some(1_024_000));
        assert_eq!(settings.recent_turns, Some(6));
        assert_eq!(
            settings.target_summary_tokens,
            Some(0),
            "0 表示不设摘要预算上限，不能被默认值顶掉"
        );
        assert_eq!(settings.next_user_reserve_tokens, Some(4096));
        assert_eq!(settings.emergency_context_ratio, Some(0.85));
        assert_eq!(settings.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(settings.preserve_exact_evidence, Some(true));
        assert_eq!(settings.archive_compacted_events, Some(true));
        assert_eq!(settings.auto_memory_recall, Some(true));
    }

    /// 没有百分比时用固定的 `trigger_context_tokens`（Python 侧「精确恢复阈值」的等价语义）。
    #[test]
    fn kernel_compaction_settings_fall_back_to_fixed_tokens() {
        let config = ContextCompactionConfig {
            trigger_context_percent: None,
            trigger_context_tokens: 300_000,
            ..ContextCompactionConfig::default()
        };
        let settings = kernel_compaction_settings(1_024_000, &config);
        assert_eq!(settings.trigger_context_tokens, Some(300_000));
    }
}
