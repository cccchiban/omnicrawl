//! 最近一轮回退的稳定事件计划：对齐 Python `SessionStore._build_undo_plan` 与其返回的
//! `SessionUndoPlan`。
//!
//! 计划是「不落盘」的中间产物：先由存储层算出最近一轮包含哪些事件，调用方做完文件系统副作用预检，
//! 再回存储层提交（追加 `turn_undone`）。提交时会重新算一遍并比对事件 id，防止回退期间又插进新的回合。

use crate::error::SessionStoreError;
use crate::event::SessionEvent;
use crate::naming::MESSAGE_EVENT_TYPES;

/// 完整回合：用户消息 + 助手回复。
pub const UNDO_KIND_COMPLETE: &str = "complete";
/// 未完成回合：只有用户消息，可能被取消或中断。
pub const UNDO_KIND_INCOMPLETE: &str = "incomplete";

/// 完整回合中「跟随在回复之后、仍属于本回合」的事件类型。
const TRAILING_TURN_EVENT_TYPES: &[&str] = &[
    "compact_summary",
    "turn_snapshot",
    "turn_cancelled",
    "session_interrupted",
    "run_guard_paused",
    "run_guard_continue_exhausted",
];

/// 未完成回合的终止事件类型。
const INCOMPLETE_TERMINAL_EVENT_TYPES: &[&str] = &["turn_cancelled", "session_interrupted"];

/// 最近一轮的稳定事件集合，供副作用预检后原子提交回退。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionUndoPlan {
    pub session_id: String,
    pub event_ids: Vec<String>,
    pub events: Vec<SessionEvent>,
    pub user_event_id: String,
    pub assistant_event_id: Option<String>,
    pub kind: String,
}

impl SessionUndoPlan {
    /// `turn_undone` 事件里的 `message_count`：未完成回合只撤回一条消息。
    pub fn message_count(&self) -> u64 {
        if self.kind == UNDO_KIND_INCOMPLETE {
            1
        } else {
            2
        }
    }
}

/// 从「已过滤回退事件」的活跃事件流里算出最近一轮的事件集合。
pub fn build_undo_plan(
    session_id: &str,
    active_events: &[SessionEvent],
) -> Result<SessionUndoPlan, SessionStoreError> {
    let last_user_index = active_events
        .iter()
        .rposition(|event| event.event_type == "user_message");
    let Some(last_user_index) = last_user_index else {
        return Err(SessionStoreError::new("当前会话没有可回退的对话轮次。"));
    };

    let assistant_index = active_events[last_user_index + 1..]
        .iter()
        .position(|event| event.event_type == "assistant_message")
        .map(|offset| last_user_index + 1 + offset);

    let (turn_events, undo_kind) = match assistant_index {
        None => {
            let terminal_index = active_events
                .iter()
                .rposition(|event| {
                    INCOMPLETE_TERMINAL_EVENT_TYPES.contains(&event.event_type.as_str())
                })
                .filter(|index| *index > last_user_index)
                .unwrap_or(active_events.len() - 1);
            (
                active_events[last_user_index..=terminal_index].to_vec(),
                UNDO_KIND_INCOMPLETE,
            )
        }
        Some(assistant_index) => {
            let mut turn_events = active_events[last_user_index..=assistant_index].to_vec();
            turn_events.extend(
                active_events[assistant_index + 1..]
                    .iter()
                    .filter(|event| TRAILING_TURN_EVENT_TYPES.contains(&event.event_type.as_str()))
                    .cloned(),
            );
            (turn_events, UNDO_KIND_COMPLETE)
        }
    };

    let message_events: Vec<SessionEvent> = turn_events
        .iter()
        .filter(|event| MESSAGE_EVENT_TYPES.contains(&event.event_type.as_str()))
        .cloned()
        .collect();
    let expected_message_count = if undo_kind == UNDO_KIND_INCOMPLETE {
        1
    } else {
        2
    };
    if message_events.len() != expected_message_count {
        return Err(SessionStoreError::new(
            "当前会话最后一轮结构异常，无法安全回退。",
        ));
    }

    Ok(SessionUndoPlan {
        session_id: session_id.to_string(),
        event_ids: turn_events
            .iter()
            .map(|event| event.event_id.clone())
            .collect(),
        events: turn_events,
        user_event_id: message_events[0].event_id.clone(),
        assistant_event_id: message_events.get(1).map(|event| event.event_id.clone()),
        kind: undo_kind.to_string(),
    })
}
