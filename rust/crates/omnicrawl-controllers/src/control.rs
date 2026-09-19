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

/// 关闭入口的守卫：已关闭或正在推迟关闭时直接返回。
pub fn close_guard(closed: bool, closing: bool) -> bool {
    closed || closing
}

/// 关闭流程的阶段序列：顺序即契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePhase {
    PluginHookBefore,
    SessionClosedEvent,
    PluginHookAfter,
    McpManager,
    MonitorManager,
    TempWorkspace,
    LlmClient,
    RuntimeManager,
    CloseCallbacks,
}

impl ClosePhase {
    /// 与对照数据集共用的阶段标签。
    pub fn label(self) -> &'static str {
        match self {
            ClosePhase::PluginHookBefore => "plugin_hook_before",
            ClosePhase::SessionClosedEvent => "session_closed_event",
            ClosePhase::PluginHookAfter => "plugin_hook_after",
            ClosePhase::McpManager => "mcp_manager",
            ClosePhase::MonitorManager => "monitor_manager",
            ClosePhase::TempWorkspace => "temp_workspace",
            ClosePhase::LlmClient => "llm_client",
            ClosePhase::RuntimeManager => "runtime_manager",
            ClosePhase::CloseCallbacks => "close_callbacks",
        }
    }
}

/// 关闭阶段：插件 Hook 与事件收尾成组，之后逐个关资源，最后跑回调。
///
/// 每步失败只记录、不中断后续步骤，全部走完后才把第一个错误抛给调用方。
pub const CLOSE_PHASES: [ClosePhase; 9] = [
    ClosePhase::PluginHookBefore,
    ClosePhase::SessionClosedEvent,
    ClosePhase::PluginHookAfter,
    ClosePhase::McpManager,
    ClosePhase::MonitorManager,
    ClosePhase::TempWorkspace,
    ClosePhase::LlmClient,
    ClosePhase::RuntimeManager,
    ClosePhase::CloseCallbacks,
];

/// 注册关闭回调的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseCallbackAction {
    /// 已经关闭：立即执行，不再排队。
    RunNow,
    /// 还没关闭：排队，等关闭时执行。
    Enqueue,
}

pub fn close_callback_action(closed: bool) -> CloseCallbackAction {
    if closed {
        CloseCallbackAction::RunNow
    } else {
        CloseCallbackAction::Enqueue
    }
}

/// 隔离工作区与 SubAgent worktree 的收尾结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationFinalize {
    /// 收尾摘要（非空条目用「；」连接）。
    pub summary: String,
    /// 是否满足回调条件（宿主另有 `on_finalized` 时才真正回调）。
    pub notify: bool,
}

pub const ISOLATION_FINALIZE_FAILED: &str = "隔离工作区收尾失败：";

pub const SUBAGENT_WORKTREE_FINALIZE_FAILED: &str = "SubAgent worktree 收尾失败：";

/// 拼接隔离收尾摘要，并给出是否触发回调。
///
/// `isolation` 只在会话存在时参与；`subagents` 的 `Ok` 值同时决定回调条件——
/// 即使摘要为空，只要有 SubAgent 收尾动作就要通知（Python 用的是原值而非过滤后的摘要）。
pub fn finalize_isolation_summary(
    session_present: bool,
    isolation: Option<Result<String, String>>,
    subagents: Result<String, String>,
) -> IsolationFinalize {
    let mut summaries: Vec<String> = Vec::new();
    if session_present {
        match isolation {
            Some(Ok(text)) => summaries.push(text),
            Some(Err(message)) => summaries.push(format!("{ISOLATION_FINALIZE_FAILED}{message}")),
            None => {}
        }
    }
    let mut sub_summary = String::new();
    match subagents {
        Ok(text) => {
            sub_summary = text;
            if !sub_summary.is_empty() {
                summaries.push(sub_summary.clone());
            }
        }
        Err(message) => summaries.push(format!("{SUBAGENT_WORKTREE_FINALIZE_FAILED}{message}")),
    }
    let summary = summaries
        .into_iter()
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join("；");
    IsolationFinalize {
        summary,
        notify: session_present || !sub_summary.is_empty(),
    }
}

/// 父 Session 切换失败时的固定文案。
pub const SESSION_TRANSITION_DRAIN_FAILED: &str =
    "父 Session 切换失败：仍有 SubAgent 子任务未在期限内退出，已保留当前会话和共享资源。";

/// 父 Session 切换前对旧会话子任务的排空处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionDrain {
    /// 没有子任务编排器：不做事。
    Skip,
    /// 排空成功：恢复接单能力。
    Resume,
    /// 取消过程抛异常：先恢复接单，再原样抛出原异常。
    ResumeAndReraise,
    /// 未在期限内排空：先恢复接单，再按固定文案拒绝切换。
    ResumeAndReject,
}

pub fn session_transition_drain(
    has_coordinator: bool,
    cancel_failed: bool,
    drained: bool,
) -> TransitionDrain {
    if !has_coordinator {
        return TransitionDrain::Skip;
    }
    if cancel_failed {
        return TransitionDrain::ResumeAndReraise;
    }
    if !drained {
        return TransitionDrain::ResumeAndReject;
    }
    TransitionDrain::Resume
}
