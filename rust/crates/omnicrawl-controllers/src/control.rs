//! `omnicrawl/agent/controllers/session/control.py` 的判定层。
//!
//! 收进来的是可判定的部分：插件子系统状态文案、退出时的会话收尾决策、关闭/停用
//! SubAgent 前的排空决策。真正的资源关闭（MCP Manager、Monitor、临时目录、Runtime、
//! 关闭回调）与隔离区收尾仍是宿主的活。

use crate::error::AgentError;
use crate::settings::{SUBAGENTS_DISABLE_FAILED, SUBAGENTS_DISABLE_REASON};
use crate::shared::SUBAGENT_LIFECYCLE_WAIT_SECONDS;

pub const SUBAGENTS_DISABLED_ERROR: &str = "SubAgent 功能未启用。";

pub const AGENT_CLOSE_REASON: &str = "Agent 正在关闭，当前子任务已取消。";

/// `/plugins` 展示用的一行 Worker 状态（对应 `manager.list_status()` 的元素）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginWorkerRow {
    pub name: String,
    pub version: String,
    pub scope: String,
    pub active: bool,
    pub circuit_open: bool,
    pub dev_mode: bool,
    pub handlers: Vec<String>,
    pub last_error: String,
}

/// 插件子系统只读状态；`None` 表示宿主未注入 Plugin Runtime。
pub fn plugins_status_text(manager: Option<&(bool, Vec<PluginWorkerRow>)>) -> String {
    let Some((enabled, rows)) = manager else {
        return "插件子系统：未注入 PluginManager（无插件模式）。\n\
                管理命令：ocl plugin doctor / list / install ..."
            .to_string();
    };

    let mut lines = vec![format!(
        "插件系统：{}",
        if *enabled {
            "已启用"
        } else {
            "已关闭（plugins.enabled=false）"
        }
    )];
    if rows.is_empty() {
        lines.push("当前工作区没有已加载的插件 Worker。".to_string());
        lines.push("管理命令：ocl plugin list".to_string());
        return lines.join("\n");
    }

    lines.push(format!("已加载 Worker：{}", rows.len()));
    for row in rows {
        let name = if row.name.is_empty() { "?" } else { &row.name };
        let version = if row.version.is_empty() {
            "?"
        } else {
            &row.version
        };
        let scope = if row.scope.is_empty() {
            "?"
        } else {
            &row.scope
        };
        let active = if row.active { "active" } else { "inactive" };
        let circuit = if row.circuit_open {
            " circuit-open"
        } else {
            ""
        };
        let dev = if row.dev_mode { " [dev]" } else { "" };
        lines.push(format!(
            "  - {name}@{version} ({scope}) {active}{circuit}{dev}"
        ));
        if !row.handlers.is_empty() {
            lines.push(format!("    handlers: {}", row.handlers.join(", ")));
        }
        let last_error = row.last_error.trim();
        if !last_error.is_empty() {
            let head: String = last_error.chars().take(160).collect();
            lines.push(format!("    lastError: {head}"));
        }
    }
    lines.push(
        "管理命令：ocl plugin list|info|enable|disable|install|update|rollback|uninstall ..."
            .to_string(),
    );
    lines.join("\n")
}

/// 正常退出时对当前会话的收尾动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCloseAction {
    /// 没有会话：什么都不做。
    None,
    /// 只丢弃没有真实内容的启动占位。
    Discard,
    /// 先补写 `session_closed` 事件，再丢弃空会话。
    AppendAndDiscard,
}

/// 按最后一次事件类型决定退出收尾动作。
pub fn session_closed_action(last_event_type: Option<&str>) -> SessionCloseAction {
    let Some(last_event_type) = last_event_type else {
        return SessionCloseAction::None;
    };
    match last_event_type {
        "session_closed" => SessionCloseAction::Discard,
        "session_interrupted" => SessionCloseAction::None,
        _ => SessionCloseAction::AppendAndDiscard,
    }
}
/// 关闭 Agent 前的排空结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStep {
    /// 子任务已排空（或没有协调器）：继续关闭资源。
    Proceed,
    /// 仍有子任务未退出：推迟关闭，等最后一个子任务收尾后重试。
    Defer,
}

/// 关闭前是否要把资源关闭推迟到子任务真正退出之后。
pub fn close_step(has_coordinator: bool, drained: bool) -> CloseStep {
    if has_coordinator && !drained {
        return CloseStep::Defer;
    }
    CloseStep::Proceed
}

/// 停用 SubAgent 前的排空结论；`Err` 表示仍有子任务未退出，必须拒绝停用。
pub fn subagents_disable_step(has_coordinator: bool, drained: bool) -> Result<(), AgentError> {
    if has_coordinator && !drained {
        return Err(AgentError::new(SUBAGENTS_DISABLE_FAILED));
    }
    Ok(())
}

/// 关闭 / 停用 SubAgent 时使用的取消原因与等待上限。
pub fn subagent_cancel_reason(closing_agent: bool) -> &'static str {
    if closing_agent {
        AGENT_CLOSE_REASON
    } else {
        SUBAGENTS_DISABLE_REASON
    }
}

pub fn subagent_cancel_timeout() -> f64 {
    SUBAGENT_LIFECYCLE_WAIT_SECONDS
}
