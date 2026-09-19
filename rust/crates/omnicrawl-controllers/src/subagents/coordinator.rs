//! `omnicrawl/agent/subagents/coordinator.py` 的判定与投影面。
//!
//! Python 侧 Coordinator 把「参数校验、权限 profile 收窄、结果与事件投影」和线程池、
//! 模型运行时、Session 落盘混在同一个类里。本模块只收前者：需要线程、进程或模型运行时
//! 的地方由宿主注入，失败诊断的异常分类也由宿主按 `llm::errors` 的映射结果传入。

use serde_json::{json, Map, Value};

use omnicrawl_config::features::subagents::SubAgentConfig;

use crate::subagents::definitions::{AgentDefinition, AgentDefinitionRegistry};
use crate::subagents::read_only::READ_ONLY_COMMAND_TOOL_NAMES;
use crate::subagents::verify::VERIFY_COMMAND_TOOL_NAME;

/// delegated-read-only 可直接继承的只读工具。
pub const READ_ONLY_TOOL_NAMES: [&str; 9] = [
    "list",
    "find",
    "read",
    "read_image",
    "grep",
    "web_search",
    "memory_search",
    "memory_read",
    "memory_expand_related",
];

/// 只读 profile 硬拒绝的本地写入口与递归调度。
pub const READ_ONLY_BLOCKED_TOOL_NAMES: [&str; 5] =
    ["write_file", "Edit_file", "memory_write", "subagent", "git"];

/// standard 写 Agent 额外允许的写入与命令工具。
pub const STANDARD_WRITE_TOOL_NAMES: [&str; 4] = ["write_file", "Edit_file", "bash", "powershell"];

/// 任务级允许的字段。
pub const TASK_FIELDS: [&str; 4] = ["description", "prompt", "subagent_type", "context"];
/// 顶层允许的字段。
pub const TOP_LEVEL_FIELDS: [&str; 10] = [
    "action",
    "tasks",
    "max_concurrency",
    "fail_fast",
    "task_id",
    "batch_id",
    "branch",
    "strategy",
    "cleanup",
    "remove_branch",
];
/// worktree 控制动作。
pub const WORKTREE_CONTROL_ACTIONS: [&str; 3] =
    ["apply_worktree", "discard_worktree", "list_worktrees"];
/// worktree 控制动作允许的字段。
pub const WORKTREE_CONTROL_FIELDS: [&str; 8] = [
    "action",
    "task_id",
    "batch_id",
    "branch",
    "strategy",
    "cleanup",
    "remove_branch",
    "force",
];
/// 查询动作允许的字段。
pub const QUERY_FIELDS: [&str; 3] = ["action", "task_id", "batch_id"];

pub fn verify_tool_names() -> Vec<String> {
    let mut names: Vec<String> = READ_ONLY_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    names.push(VERIFY_COMMAND_TOOL_NAME.to_string());
    names
}

pub fn standard_tool_names() -> Vec<String> {
    let mut names: Vec<String> = READ_ONLY_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for name in STANDARD_WRITE_TOOL_NAMES {
        names.push(name.to_string());
    }
    names
}

/// 一次批量请求里已完成权限收窄的任务投影。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreparedTaskView {
    pub batch_id: String,
    pub task_id: String,
    pub description: String,
    pub agent_type: String,
    pub definition_source: String,
}

/// 工具表的收窄结果；宿主据此构造真正的工具对象。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSelection {
    /// 保留的工具名（已排序、去重）。
    pub names: Vec<String>,
    /// 需要套只读命令包装的工具名。
    pub wrap_read_only_command: Vec<String>,
    /// `git` 是否需要只读包装（`false` 表示按定义要求放开完整子命令）。
    pub wrap_read_only_git: bool,
    /// 需要清除审批要求的工具名。
    pub clear_confirmation: Vec<String>,
}

/// 失败诊断的事实面；分类与映射由宿主持有，内核只做投影与脱敏。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureFacts {
    pub exception_type: String,
    pub category: String,
    pub retryable: bool,
    pub provider: String,
    pub status_code: Option<i64>,
    pub detail: String,
}

/// worktree 控制动作的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeControlRequest {
    List,
    Apply {
        key: String,
        strategy: String,
        cleanup: bool,
    },
    Discard {
        key: String,
        remove_branch: bool,
        force: bool,
    },
}

/// 查询动作的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryRequest {
    List,
    Get { task_id: String },
    Cancel { task_id: String, batch_id: String },
}

/// `available_agent_types`：当前配置下可以实际创建任务的角色名称。
pub fn available_agent_types(
    config: &SubAgentConfig,
    registry: &AgentDefinitionRegistry,
) -> Vec<String> {
    registry
        .list_all()
        .iter()
        .filter(|definition| unsupported_definition_reason(definition, config).is_empty())
        .map(|definition| definition.name.clone())
        .collect()
}

/// `_unsupported_definition_reason`：定义只能选择已实现且已显式开启的权限 profile。
pub fn unsupported_definition_reason(
    definition: &AgentDefinition,
    config: &SubAgentConfig,
) -> String {
    let mut unsupported: Vec<String> = Vec::new();
    match definition.permission_mode.as_str() {
        "delegated-read-only" => {
            if definition.background {
                unsupported.push("read_only profile 的 background 必须为 false".to_string());
            }
        }
        "explicit-command-allowlist" => {
            if !config.enable_verify_agent {
                unsupported.push(
                    "explicit-command-allowlist 需要 subagents.enable_verify_agent=true"
                        .to_string(),
                );
            }
        }
        "standard" => {
            if !config.allow_standard_agent {
                unsupported
                    .push("standard profile 需要 subagents.allow_standard_agent=true".to_string());
            }
            if definition.isolation == "shared" && !config.allow_shared_workspace_writes {
                unsupported.push(
                    "standard + isolation=shared 需要 \
subagents.allow_shared_workspace_writes=true；推荐改用 isolation=worktree"
                        .to_string(),
                );
            }
        }
        _ => unsupported.push("permissionMode 不受当前 SubAgent 阶段支持".to_string()),
    }
    if !matches!(definition.isolation.as_str(), "shared" | "worktree") {
        unsupported.push(format!("isolation 不受支持：{}", definition.isolation));
    } else if definition.isolation == "worktree" && !config.allow_worktree {
        unsupported.push("isolation=worktree 需要 subagents.allow_worktree=true".to_string());
    }
    if !definition.skills.is_empty() {
        unsupported.push("当前阶段尚不注入 skills".to_string());
    }
    if !definition.mcp_servers.is_empty() {
        unsupported.push("当前阶段尚不开放 mcpServers".to_string());
    }
    if unsupported.is_empty() {
        return String::new();
    }
    let source = definition
        .source_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| definition.source.clone());
    format!(
        "Agent 定义超出当前 SubAgent 能力：{}（{source}）。",
        unsupported.join("; ")
    )
}

/// `_tools_for_definition`：按 profile 选择工具，再应用定义自身的白名单与黑名单。
pub fn select_profile_tools(
    definition: &AgentDefinition,
    parent_tools: &[String],
    verify_tools: &[String],
) -> ToolSelection {
    if definition.permission_mode == "explicit-command-allowlist" {
        let mut available: Vec<String> = parent_tools.to_vec();
        for name in verify_tools {
            if !available.contains(name) {
                available.push(name.clone());
            }
        }
        return filter_profile_tools(definition, &available, &verify_tool_names(), true);
    }
    if definition.permission_mode == "standard" {
        return filter_profile_tools(definition, parent_tools, &standard_tool_names(), false);
    }

    let profile: Vec<String> = parent_tools
        .iter()
        .filter(|name| !READ_ONLY_BLOCKED_TOOL_NAMES.contains(&name.as_str()))
        .cloned()
        .collect();
    let mut selection = filter_profile_tools(definition, parent_tools, &profile, false);
    for name in selection.names.clone() {
        if READ_ONLY_TOOL_NAMES.contains(&name.as_str())
            && !selection.clear_confirmation.contains(&name)
        {
            selection.clear_confirmation.push(name);
        }
    }
    for name in selection.names.clone() {
        if READ_ONLY_COMMAND_TOOL_NAMES.contains(&name.as_str())
            && !selection.wrap_read_only_command.contains(&name)
        {
            // 包装器同时把审批要求关掉（只读代理由运行前判定负责）。
            selection.clear_confirmation.push(name.clone());
            selection.wrap_read_only_command.push(name);
        }
    }
    if definition.tools.iter().any(|name| name == "git")
        && parent_tools.iter().any(|name| name == "git")
    {
        selection.wrap_read_only_git = definition.git_mode != "full";
        if selection.wrap_read_only_git
            && !selection.clear_confirmation.contains(&"git".to_string())
        {
            selection.clear_confirmation.push("git".to_string());
        }
        if !selection.names.iter().any(|name| name == "git") {
            selection.names.push("git".to_string());
            selection.names.sort();
        }
    }
    selection
}

fn filter_profile_tools(
    definition: &AgentDefinition,
    available: &[String],
    profile: &[String],
    clear_confirmation: bool,
) -> ToolSelection {
    let base: Vec<String> = if definition.tools.is_empty() {
        profile.to_vec()
    } else {
        definition.tools.clone()
    };
    let mut names: Vec<String> = base
        .into_iter()
        .filter(|name| profile.contains(name))
        .filter(|name| !definition.disallowed_tools.contains(name))
        .filter(|name| available.contains(name))
        .collect();
    names.sort();
    names.dedup();
    let clear_confirmation = if clear_confirmation {
        names.clone()
    } else {
        Vec::new()
    };
    ToolSelection {
        names,
        wrap_read_only_command: Vec::new(),
        wrap_read_only_git: false,
        clear_confirmation,
    }
}

/// `_validate_arguments`：返回 `None` 表示通过，否则是稳定的 `(code, message)`。
pub fn validate_arguments(
    arguments: &Value,
    config: &SubAgentConfig,
    registry: &AgentDefinitionRegistry,
) -> Option<(String, String)> {
    let Some(object) = arguments.as_object() else {
        return Some((
            "AGENT_DEFINITION_INVALID".to_string(),
            "subagent 参数必须是对象。".to_string(),
        ));
    };
    let mut unknown_top_level: Vec<String> = object
        .keys()
        .filter(|key| !TOP_LEVEL_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown_top_level.is_empty() {
        unknown_top_level.sort();
        return Some((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            format!(
                "当前 SubAgent 阶段不允许参数：{}。",
                unknown_top_level.join(", ")
            ),
        ));
    }
    if !matches!(
        object.get("action").and_then(Value::as_str),
        Some("run") | Some("spawn")
    ) {
        return Some((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            "仅支持 action=run 或 action=spawn。".to_string(),
        ));
    }
    let Some(tasks) = object.get("tasks").and_then(Value::as_array) else {
        return Some((
            "SUBAGENT_LIMIT_EXCEEDED".to_string(),
            "tasks 必须是数组。".to_string(),
        ));
    };
    let task_limit = config.max_tasks_per_batch;
    if tasks.is_empty() || tasks.len() as i64 > task_limit {
        return Some((
            "SUBAGENT_LIMIT_EXCEEDED".to_string(),
            format!("当前 SubAgent 阶段每批必须包含 1 到 {task_limit} 个任务。"),
        ));
    }

    let max_concurrency = object
        .get("max_concurrency")
        .cloned()
        .unwrap_or_else(|| Value::from(config.max_concurrency));
    let concurrency_ok = match &max_concurrency {
        Value::Number(number) if number.is_i64() => {
            matches!(number.as_i64(), Some(1..=4))
        }
        _ => false,
    };
    if !concurrency_ok {
        return Some((
            "SUBAGENT_LIMIT_EXCEEDED".to_string(),
            "max_concurrency 必须是 1 到 4 的整数。".to_string(),
        ));
    }
    let fail_fast = object
        .get("fail_fast")
        .cloned()
        .unwrap_or(Value::Bool(false));
    if !fail_fast.is_boolean() {
        return Some((
            "AGENT_DEFINITION_INVALID".to_string(),
            "fail_fast 必须是布尔值。".to_string(),
        ));
    }

    for (index, task) in tasks.iter().enumerate() {
        let Some(task) = task.as_object() else {
            return Some((
                "AGENT_DEFINITION_INVALID".to_string(),
                format!("tasks[{index}] 必须是对象。"),
            ));
        };
        let mut unknown_task_fields: Vec<String> = task
            .keys()
            .filter(|key| !TASK_FIELDS.contains(&key.as_str()))
            .cloned()
            .collect();
        if !unknown_task_fields.is_empty() {
            unknown_task_fields.sort();
            return Some((
                "SUBAGENT_PERMISSION_DENIED".to_string(),
                format!(
                    "当前 SubAgent 阶段不允许任务字段：{}。",
                    unknown_task_fields.join(", ")
                ),
            ));
        }
        let description = task.get("description").and_then(Value::as_str);
        let prompt = task.get("prompt").and_then(Value::as_str);
        let agent_type = task.get("subagent_type").and_then(Value::as_str);
        let context = task
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("fresh");
        if description
            .map(|text| text.trim().is_empty())
            .unwrap_or(true)
        {
            return Some((
                "AGENT_DEFINITION_INVALID".to_string(),
                format!("tasks[{index}].description 必须是非空字符串。"),
            ));
        }
        if prompt.map(|text| text.trim().is_empty()).unwrap_or(true) {
            return Some((
                "AGENT_DEFINITION_INVALID".to_string(),
                format!("tasks[{index}].prompt 必须是非空字符串。"),
            ));
        }
        if agent_type
            .map(|text| text.trim().is_empty())
            .unwrap_or(true)
        {
            return Some((
                "AGENT_DEFINITION_INVALID".to_string(),
                format!("tasks[{index}].subagent_type 必须是非空字符串。"),
            ));
        }
        let normalized = agent_type.unwrap_or_default().trim().to_lowercase();
        let Some(definition) = registry.get(&normalized) else {
            let available = available_agent_types(config, registry).join(", ");
            let available = if available.is_empty() {
                "无".to_string()
            } else {
                available
            };
            return Some((
                "AGENT_TYPE_NOT_FOUND".to_string(),
                format!("未找到 Agent 定义：{normalized}。当前可用：{available}。"),
            ));
        };
        let unsupported = unsupported_definition_reason(definition, config);
        if !unsupported.is_empty() {
            return Some(("AGENT_DEFINITION_INVALID".to_string(), unsupported));
        }
        if !matches!(context, "fresh" | "fork") {
            return Some((
                "AGENT_DEFINITION_INVALID".to_string(),
                "context 仅支持 fresh 或 fork。".to_string(),
            ));
        }
        if context == "fork" && !config.allow_fork {
            return Some((
                "SUBAGENT_PERMISSION_DENIED".to_string(),
                "Fork SubAgent 默认关闭，请在配置中显式设置 allow_fork=true。".to_string(),
            ));
        }
    }
    None
}

/// `_worktree_control_action` 的参数判定与请求投影。
pub fn worktree_control_request(
    arguments: &Value,
) -> Result<WorktreeControlRequest, (String, String)> {
    let Some(object) = arguments.as_object() else {
        return Err((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            "worktree 控制动作包含不允许的字段：".to_string(),
        ));
    };
    let mut unknown: Vec<String> = object
        .keys()
        .filter(|key| !WORKTREE_CONTROL_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            format!("worktree 控制动作包含不允许的字段：{}", unknown.join(", ")),
        ));
    }
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "list_worktrees" {
        return Ok(WorktreeControlRequest::List);
    }

    let key = match object.get("task_id").and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => value,
        _ => match object.get("branch").and_then(Value::as_str) {
            Some(value) if !value.trim().is_empty() => value,
            _ => {
                return Err((
                    "AGENT_DEFINITION_INVALID".to_string(),
                    format!("{action} 需要 task_id 或 branch。"),
                ))
            }
        },
    };
    let key = key.trim().to_string();

    if action == "apply_worktree" {
        let strategy = object
            .get("strategy")
            .and_then(Value::as_str)
            .unwrap_or("checkout")
            .to_string();
        if !matches!(strategy.as_str(), "checkout" | "merge") {
            return Err((
                "AGENT_DEFINITION_INVALID".to_string(),
                "strategy 仅支持 checkout 或 merge。".to_string(),
            ));
        }
        let cleanup = object
            .get("cleanup")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return Ok(WorktreeControlRequest::Apply {
            key,
            strategy,
            cleanup,
        });
    }

    let remove_branch = object
        .get("remove_branch")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let force = object
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(WorktreeControlRequest::Discard {
        key,
        remove_branch,
        force,
    })
}

/// `_query_action` 的参数判定与请求投影。
pub fn query_request(arguments: &Value) -> Result<QueryRequest, (String, String)> {
    let Some(object) = arguments.as_object() else {
        return Err((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            "查询动作包含不允许的字段。".to_string(),
        ));
    };
    let unknown: Vec<String> = object
        .keys()
        .filter(|key| !QUERY_FIELDS.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err((
            "SUBAGENT_PERMISSION_DENIED".to_string(),
            "查询动作包含不允许的字段。".to_string(),
        ));
    }
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let task_id = object
        .get("task_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let batch_id = object
        .get("batch_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match action {
        "list" => Ok(QueryRequest::List),
        "get" => {
            if task_id.is_empty() {
                return Err((
                    "AGENT_DEFINITION_INVALID".to_string(),
                    "get 需要 task_id。".to_string(),
                ));
            }
            Ok(QueryRequest::Get { task_id })
        }
        _ => {
            if task_id.is_empty() && batch_id.is_empty() {
                return Err((
                    "AGENT_DEFINITION_INVALID".to_string(),
                    "cancel 需要 task_id 或 batch_id。".to_string(),
                ));
            }
            Ok(QueryRequest::Cancel { task_id, batch_id })
        }
    }
}

/// `_failure_payload`：失败任务的公开结果。
pub fn failure_payload(
    task: &PreparedTaskView,
    code: &str,
    message: &str,
    diagnostic: Option<&Value>,
) -> Value {
    let mut error = Map::new();
    error.insert("code".to_string(), Value::String(code.to_string()));
    error.insert(
        "message".to_string(),
        Value::String(omnicrawl_session::redaction::redact_sensitive_text(message)),
    );
    if let Some(diagnostic) = diagnostic {
        error.insert("diagnostic".to_string(), diagnostic.clone());
    }
    json!({
        "task_id": task.task_id,
        "description": task.description,
        "agent_type": task.agent_type,
        "definition_source": task.definition_source,
        "status": "failed",
        "summary": "",
        "evidence": [],
        "artifacts": [],
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "cached_input_tokens": 0,
            "model_turns": 0,
            "tool_calls": 0,
        },
        "error": Value::Object(error),
    })
}

/// `_cancelled_payload`：取消任务的公开结果。
pub fn cancelled_payload(task: &PreparedTaskView, message: &str) -> Value {
    let mut payload = failure_payload(task, "SUBAGENT_CANCELLED", message, None);
    if let Some(object) = payload.as_object_mut() {
        object.insert("status".to_string(), Value::String("cancelled".to_string()));
    }
    payload
}

/// `_top_level_error`：整批失败时的顶层结果。
pub fn top_level_error(code: &str, message: &str) -> Value {
    json!({
        "batch_id": Value::Null,
        "status": "failed",
        "results": [],
        "error": {"code": code, "message": message},
    })
}

/// `_json_result` 的文本面：`json.dumps(payload, ensure_ascii=False, indent=2)`。
pub fn json_result_text(payload: &Value) -> String {
    crate::json::python_dumps(payload, 2)
}

/// `_emit_task_event` 的 payload。
pub fn task_event_payload(task: &PreparedTaskView, status: &str) -> Value {
    let public_status = if status == "started" {
        "running"
    } else {
        status
    };
    json!({
        "batch_id": task.batch_id,
        "task_id": task.task_id,
        "agent_type": task.agent_type,
        "description": task.description,
        "definition_source": task.definition_source,
        "status": public_status,
    })
}

/// `_emit_terminal_event` 的 payload。
pub fn terminal_event_payload(task: &PreparedTaskView, result: &Value) -> Value {
    json!({
        "batch_id": task.batch_id,
        "task_id": task.task_id,
        "agent_type": task.agent_type,
        "description": task.description,
        "definition_source": task.definition_source,
        "status": result.get("status").cloned().unwrap_or(Value::Null),
        "summary": result.get("summary").cloned().unwrap_or(Value::String(String::new())),
        "artifacts": result.get("artifacts").cloned().unwrap_or_else(|| json!([])),
        "usage": result.get("usage").cloned().unwrap_or_else(|| json!({})),
        "error": result.get("error").cloned().unwrap_or(Value::Null),
    })
}

/// `_build_failure_diagnostics`：把宿主给出的失败事实投影为可持久化诊断。
pub fn build_failure_diagnostics(
    facts: Option<&FailureFacts>,
    model: &str,
    wire_model: &str,
) -> Value {
    use omnicrawl_session::redaction::redact_sensitive_text;

    let mut diagnostic = Map::new();
    let facts = facts.cloned().unwrap_or_default();
    let category = if facts.category.is_empty() {
        "UNKNOWN".to_string()
    } else {
        facts.category.clone()
    };
    diagnostic.insert("category".to_string(), Value::String(category));
    diagnostic.insert(
        "exception_type".to_string(),
        Value::String(if facts.exception_type.is_empty() {
            "Exception".to_string()
        } else {
            facts.exception_type.clone()
        }),
    );
    diagnostic.insert("retryable".to_string(), Value::Bool(facts.retryable));
    if !facts.provider.is_empty() {
        diagnostic.insert(
            "provider".to_string(),
            Value::String(redact_sensitive_text(&facts.provider)),
        );
    }
    if let Some(status_code) = facts.status_code {
        diagnostic.insert("status_code".to_string(), Value::from(status_code));
    }
    if !facts.detail.is_empty() {
        let detail = redact_sensitive_text(&facts.detail);
        diagnostic.insert(
            "detail".to_string(),
            Value::String(take_chars(&detail, 400)),
        );
    }
    if !model.is_empty() {
        let safe_selection = redact_sensitive_text(model);
        diagnostic.insert("model".to_string(), Value::String(safe_selection.clone()));
        diagnostic.insert("model_selection".to_string(), Value::String(safe_selection));
    }
    if !wire_model.is_empty() {
        diagnostic.insert(
            "wire_model".to_string(),
            Value::String(redact_sensitive_text(wire_model)),
        );
    }
    Value::Object(diagnostic)
}

fn take_chars(text: &str, limit: usize) -> String {
    if text.chars().count() > limit {
        return text.chars().take(limit).collect();
    }
    text.to_string()
}
