//! `omnicrawl/agent/controllers/session/store.py` 的事件投影编排。
//!
//! 会话门面本身是薄委托，真正属于内核的是「追加的事件如何参与运行期投影」这条规则：
//! 落盘事件的 payload 已经做过值级脱敏，运行期投影必须改用未脱敏的原始 payload，
//! 否则同一会话内的多轮对话与已发送前缀不再逐字一致（前缀缓存失效，模型也会看到
//! 自己的推理被改写）；会话未启用、事件不落盘时构造只在内存中参与投影的等价事件。
//! 门面转发、项目列表与文件系统访问仍留在宿主。

use serde_json::{json, Map, Value};

use omnicrawl_session::{SessionEvent, SessionStoreError};

/// 会话未启用、事件不落盘时使用的占位会话标识。
///
/// 必须是满足事件 ID 格式校验的合法形状：事件构造失败会中断整个回合，
/// 所以这里用全零而非语义化字符串。
pub const EPHEMERAL_SESSION_ID: &str = "00000000-000000-000000";

/// 事件追加后如何喂给运行期投影器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionFeed {
    /// 没有投影器（该会话不做轨迹投影）：不喂。
    Skip,
    /// 事件已落盘：用未脱敏的原始 payload 投影。
    Persisted,
    /// 事件未落盘：构造只在内存中参与投影的事件。
    Ephemeral,
}

/// 按「是否有投影器」与「事件是否落盘」决定投影方式。
pub fn projection_feed(has_projector: bool, persisted: bool) -> ProjectionFeed {
    if !has_projector {
        return ProjectionFeed::Skip;
    }
    if persisted {
        ProjectionFeed::Persisted
    } else {
        ProjectionFeed::Ephemeral
    }
}

/// 推进内存事件序号：Python 侧先自增再拿它构造 ID。
pub fn next_sequence(current: u32) -> u32 {
    current.saturating_add(1)
}

/// 内存事件的 ID：`memory-event-<序号>-<事件类型>`。
pub fn ephemeral_event_id(sequence: u32, event_type: &str) -> String {
    format!("memory-event-{sequence}-{event_type}")
}

/// 构造只在内存中参与投影的事件；`current_sequence` 是宿主已记录的序号，函数内部自增。
///
/// 事件 ID 带单调序号：既不与真实 JSONL 事件冲突，也保证同一轮内多条事件顺序稳定。
/// 投影器只依赖 `event_id` / `type` / `payload` 三个字段，其余字段走与落盘事件同一套校验。
pub fn ephemeral_event(
    current_sequence: u32,
    session_id: Option<&str>,
    event_type: &str,
    payload: &Map<String, Value>,
    created_at: &str,
) -> Result<SessionEvent, SessionStoreError> {
    SessionEvent::from_dict(&json!({
        "version": 1,
        "session_id": session_id.unwrap_or(EPHEMERAL_SESSION_ID),
        "event_id": ephemeral_event_id(next_sequence(current_sequence), event_type),
        "type": event_type,
        "created_at": created_at,
        "payload": Value::Object(payload.clone()),
    }))
}
