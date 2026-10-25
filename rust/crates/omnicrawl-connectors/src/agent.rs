//! 连接器与 Agent 宿主之间的边界。
//!
//! 连接器只做平台 I/O 与显示映射，回合、工具与命令由宿主侧实现：最终形态是 Rust 内核的
//! 协议 v1 宿主对端（`rust/docs/protocol-v1.md`），过渡期也可以由 Python 宿主承担。
//! [`TurnEvent`] 的形状与协议 v1 的同名通知一致，两侧共用一套映射，不再各写一份。

use std::sync::Arc;

use serde_json::Value;

/// 一次工具调用：名称、参数与内核侧标识。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
}

impl ToolCall {
    /// 按协议 v1 的 `ToolCall` 负载构造；缺字段回落默认值，参数非对象时为空对象。
    pub fn from_value(value: &Value) -> ToolCall {
        let arguments = match value.get("arguments") {
            Some(Value::Object(_)) => value.get("arguments").cloned().unwrap_or(Value::Null),
            Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        ToolCall {
            name: string_field(value, "name").unwrap_or_else(|| "?".to_string()),
            arguments,
            id: string_field(value, "id"),
        }
    }

    /// 工具参数的只读视图（脱敏与摘要展示用）。
    pub fn parameters(&self) -> &Value {
        &self.arguments
    }

    /// 参数以对象形式返回：非对象按空对象处理，便于下游统一取值。
    pub fn arguments_object(&self) -> serde_json::Map<String, Value> {
        match &self.arguments {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        }
    }
}

/// 一次工具执行结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
    pub error_code: Option<String>,
    pub retryable: bool,
}

impl ToolResult {
    pub fn from_value(value: &Value) -> ToolResult {
        let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(true);
        ToolResult {
            ok,
            output: string_field(value, "output").unwrap_or_default(),
            error_code: string_field(value, "error_code"),
            retryable: value
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// 回合结束时的结果，对应协议 v1 的 `turn.finished`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnOutcome {
    pub final_text: String,
    pub reasoning: String,
    pub model_turns: u64,
    pub tool_calls: u64,
    pub paused: bool,
}

/// 回合内事件：与协议 v1 的内核 → 宿主通知一一对应。
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    Delta(String),
    ReasoningDelta(String),
    Status(String),
    RetryStatus(String),
    ProtocolWait,
    StreamRollback,
    TokenUsage {
        input_tokens: i64,
        output_tokens: i64,
        cached_input_tokens: i64,
    },
    ToolStarted {
        step: u64,
        call: ToolCall,
    },
    ToolFinished {
        call: ToolCall,
        result: ToolResult,
    },
    ToolOutputUpdate {
        call: ToolCall,
        result: ToolResult,
    },
    SubagentEvent {
        name: String,
        payload: Value,
    },
    TodoUpdate {
        todos: Value,
    },
    Finished(TurnOutcome),
}

/// 回合失败：取消或错误；错误文案由宿主给出，连接器负责脱敏后回传。
#[derive(Debug, Clone, PartialEq)]
pub enum TurnError {
    Cancelled,
    Failed(String),
}

/// 宿主看到的 Agent 状态（`/status` 与 `/session` 用）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentStatus {
    pub workspace_root: String,
    pub session_id: String,
}

/// 工作区切换结果：切换后的根目录与持久化说明（说明可为空）。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSwitch {
    pub workspace_root: String,
    pub note: String,
}

/// 工具确认桥：宿主在执行敏感工具前调用连接器，等用户 `/approve` 或 `/reject`。
pub trait ConfirmHandler: Send + Sync {
    fn confirm(&self, tool_name: &str, arguments: &Value) -> bool;
}

/// 提问桥：宿主在 `ask_user` 时调用连接器，等用户选择；返回 None 表示未取得答案。
pub trait AskUserHandler: Send + Sync {
    fn ask(&self, request: &Value) -> Option<String>;
}

/// 宿主侧 Agent：连接器只通过它驱动回合、执行命令与取状态。
pub trait AgentDriver: Send + Sync + 'static {
    fn status(&self) -> AgentStatus;

    /// 开启新会话（清空对话历史）。
    fn reset_conversation(&self) -> Result<(), String>;

    /// 切换工作区。持久化失败不阻断切换，原因写在 [`WorkspaceSwitch::note`]。
    fn switch_workspace(&self, path: &str) -> Result<WorkspaceSwitch, String>;

    /// 交给宿主的斜杠命令注册表；`Ok(None)` 表示不是宿主认识的管理命令。
    fn handle_command(&self, text: &str, channel: &str) -> Result<Option<String>, String>;

    /// 当前已启用的主 Agent 模式（未启用时为空串）。
    ///
    /// 模式提示词由宿主装配进 system prompt，连接器只读标识做展示。
    fn active_mode(&self) -> String {
        String::new()
    }

    /// 启用主 Agent 模式（连接器默认开启简洁回复模式时用）。
    fn activate_mode(&self, _mode: &str) -> Result<String, String> {
        Err("当前宿主没有提示词装配，无法切换模式。".to_string())
    }

    /// 关闭当前模式，返回被关闭的模式标识。
    fn deactivate_mode(&self) -> Result<String, String> {
        Err("当前宿主没有提示词装配，无法切换模式。".to_string())
    }

    /// 发布到平台命令菜单的命令表（`(命令名, 一句说明)`）。
    ///
    /// 返回空表表示该宿主没有可发布的菜单，连接器就不去动平台侧设置。菜单只是客户端
    /// 提示，发布失败不影响命令分发。
    fn command_menu(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// 执行一个回合：事件经 `events` 外发，返回最终回答文本。
    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError>;

    /// 请求取消当前回合（建议性，宿主在下一个模型或工具批次边界检查）。
    fn request_cancel(&self);

    /// `.agent_tmp` 根目录：连接器把收到的文件分类存到这里。
    fn temp_root(&self) -> std::path::PathBuf;

    /// 关闭宿主并行回收资源；默认无操作。
    fn shutdown(&self) -> Result<(), String> {
        Ok(())
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>);

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>);
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}
