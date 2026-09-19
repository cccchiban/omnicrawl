//! `omnicrawl/agent/controllers/session/settings.py` 的判定层，外加它调用的三个校验件
//! （`config/features/approval.py`、`llm/normalize_reasoning_effort`、
//! `config/features/{tools,subagents}.py`）。
//!
//! 宿主边界：setter 里「替换 `self.config` 字段 → 重建工具表 / Runtime / MCP Manager，
//! 失败回滚」这一整套事务留在 Python；这里只收校验、归一化与阈值换算。

use crate::error::AgentError;

pub const APPROVAL_MODE_AUTO: &str = "auto";

pub const APPROVAL_MODE_REVIEW: &str = "review";

pub const APPROVAL_MODE_MANUAL: &str = "manual";

pub const VALID_APPROVAL_MODES: [&str; 3] = [
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
];

const APPROVAL_MODE_ALIASES: [(&str, &str); 11] = [
    ("ask", APPROVAL_MODE_MANUAL),
    ("confirm", APPROVAL_MODE_MANUAL),
    ("manual", APPROVAL_MODE_MANUAL),
    ("off", APPROVAL_MODE_MANUAL),
    ("auto", APPROVAL_MODE_AUTO),
    ("auto_approve", APPROVAL_MODE_AUTO),
    ("approve", APPROVAL_MODE_AUTO),
    ("always", APPROVAL_MODE_AUTO),
    ("review", APPROVAL_MODE_REVIEW),
    ("auto_review", APPROVAL_MODE_REVIEW),
    ("reviewed", APPROVAL_MODE_REVIEW),
];

pub const VALID_REASONING_EFFORTS: [&str; 6] = ["high", "low", "max", "medium", "none", "xhigh"];

const REASONING_EFFORT_ALIASES: [(&str, &str); 14] = [
    ("", ""),
    ("disabled", "disabled"),
    ("off", "disabled"),
    ("none", "none"),
    ("low", "low"),
    ("medium", "medium"),
    ("med", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("x_high", "xhigh"),
    ("extra_high", "xhigh"),
    ("very_high", "xhigh"),
    ("max", "max"),
    ("maximum", "max"),
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
    let normalized = value.trim().to_lowercase().replace('-', "_");
    for (alias, mode) in APPROVAL_MODE_ALIASES {
        if alias == normalized {
            return Ok(mode);
        }
    }
    Err(AgentError::new(format!(
        "approval.mode 仅支持 {}，当前值：{value}。",
        allowed_values(&VALID_APPROVAL_MODES)
    )))
}

/// 推理强度归一化；空串是合法取值（表示未设置）。
pub fn normalize_reasoning_effort(value: &str) -> Result<&'static str, AgentError> {
    let normalized = value.trim().to_lowercase().replace(['-', ' '], "_");
    for (alias, effort) in REASONING_EFFORT_ALIASES {
        if alias == normalized {
            return Ok(effort);
        }
    }
    let mut allowed: Vec<&str> = VALID_REASONING_EFFORTS.to_vec();
    allowed.push("disabled");
    allowed.sort_unstable();
    Err(AgentError::new(format!(
        "llm.reasoning_effort 仅支持 {}，当前值：{value}。",
        allowed.join(", ")
    )))
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

fn allowed_values(values: &[&str]) -> String {
    let mut sorted: Vec<&str> = values.to_vec();
    sorted.sort_unstable();
    sorted.join(", ")
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
