//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;
use omnicrawl_ipc::bridge::SessionEventsParams;
use omnicrawl_session::SessionEvent;
use super::model_port::{read_api_key};



use super::turn::{Conn, compaction_progress, evicted_prefix, post_compaction_context_tokens};

/// 显式压缩当前会话：摘要与压缩结果回到 `{summary, compacted}`。
pub(super) fn respond_compact(conn: &Rc<RefCell<Conn>>, id: Id) {
    let target = {
        let connection = conn.borrow();
        connection
            .session
            .as_ref()
            .map(|session| (session.session_id.clone(), connection.model.clone()))
    };
    let Some((_session_id, model)) = target else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "当前没有可压缩的活动 Session。",
            ),
        );
        return;
    };
    let Some(model) = model else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "内核缺少模型配置，无法生成摘要。",
            ),
        );
        return;
    };
    let api_key = read_api_key(&model).unwrap_or_default();
    let snapshot = {
        let connection = conn.borrow();
        let usage = connection.usage.borrow();
        usage.last_request_messages.clone()
    };
    let last_request_messages = evicted_prefix(conn, &snapshot);
    let outcome = {
        let mut connection = conn.borrow_mut();
        // 摘要在 session 的不可变借用上跑，因此先把会话搬出来；进度回调要再借
        // 同一条连接，留在借用里会撞上 RefCell 的运行时借用检查。
        let mut session = match connection.session.take() {
            Some(session) => session,
            None => return,
        };
        let outcome = compact_now(
            &session,
            &model,
            &api_key,
            &last_request_messages,
            compaction_progress(conn),
        );
        if let Ok(report) = outcome.as_ref() {
            match report.history.clone() {
                Some(history) => session.history = history,
                None => {
                    // `reload_history` 就地重建运行期历史；失败只记日志，压缩结论照旧回给宿主。
                    if let Err(detail) = session.reload_history() {
                        eprintln!("[kernel] 压缩后重建运行期历史失败：{detail}");
                    }
                }
            }
        }
        connection.session = Some(session);
        outcome
    };

    match outcome {
        Ok(report) => {
            for notice in report.notices.clone() {
                // 显式压缩的边界也走会话区：命令回执本身是另一条提示（「已压缩当前会话…」），
                // 这条是计量边界，Python 侧两者也是各占一行。
                conn.borrow_mut()
                    .notify(HostEvent::Notice(MessagePayload { message: notice }));
            }
            // 压缩后大小随回执给宿主：显式压缩同样不会有后续模型请求来修正遥测。
            let post_compaction_context_tokens =
                conn.borrow().session.as_ref().and_then(|session| {
                    post_compaction_context_tokens(conn, session.history.as_slice())
                });
            conn.borrow_mut().respond(
                id,
                json!({
                    "summary": report.summary,
                    "compacted": report.compacted,
                    "post_compaction_context_tokens": post_compaction_context_tokens,
                }),
            );
        }
        Err(detail) => {
            conn.borrow_mut()
                .respond_error(id, ErrorObject::new(error_code::INVALID_REQUEST, detail));
        }
    }
}

/// 撤销最近一轮：恢复工作区副作用并回退会话逻辑，结果作为命令响应返回。
pub(super) fn respond_undo(conn: &Rc<RefCell<Conn>>, id: Id) {
    let target = {
        let connection = conn.borrow();
        connection.session.as_ref().map(|session| {
            (
                Arc::clone(&session.store),
                session.session_id.clone(),
                session.workspace.clone(),
            )
        })
    };
    let Some((store, session_id, workspace)) = target else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "当前会话不受内核持有，无法撤销。",
            ),
        );
        return;
    };
    let Some(workspace) = workspace else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                "会话未绑定工作区，无法回滚副作用。",
            ),
        );
        return;
    };
    match crate::undo::undo_last_turn(&store, &session_id, std::path::Path::new(&workspace)) {
        Ok(outcome) => {
            // 回退后运行期历史要跟着会话重建，否则下一轮请求还会带上被撤回的消息。
            if let Some(session) = conn.borrow_mut().session.as_mut() {
                if let Err(detail) = session.reload_history() {
                    eprintln!("[kernel] 撤销后重建运行期历史失败：{detail}");
                }
            }
            // 回退后的历史一并回给宿主：它据此重放对话视图（撤回的消息与工具卡不能留着）。
            let history = conn
                .borrow()
                .session
                .as_ref()
                .map(|session| Value::Array(session.history.clone()))
                .unwrap_or(Value::Null);
            conn.borrow_mut().respond(
                id,
                json!({
                    "kind": outcome.kind,
                    "message_count": outcome.message_count,
                    "side_effects_reverted": outcome.side_effects_reverted,
                    "unrestorable": outcome.unrestorable,
                    "history": history,
                }),
            );
        }
        Err(detail) => {
            conn.borrow_mut()
                .respond_error(id, ErrorObject::new(error_code::INVALID_REQUEST, detail));
        }
    }
}

/// 当前内核持有的会话存储与 id；没有自持会话时给出同一条明确拒绝。
pub(super) fn session_handle(conn: &Conn) -> Option<(Arc<SessionStore>, String)> {
    conn.session
        .as_ref()
        .map(|session| (Arc::clone(&session.store), session.session_id.clone()))
}

/// 会话的**只读**查询：`session.list`、`session.history` 与 `session.events`。
///
/// 三条都只看会话目录（索引、提示历史与转录文件），与正在跑的回合不共享可变状态，所以回合内
/// 也可应答：宿主侧 `/sessions`、`/archives`、`/history` 是「立即命令」，生成期间照常
/// 执行，若内核只在空闲时应答，它们就只能撞上「未知方法」。
#[derive(Debug, Clone)]
pub(super) enum SessionQuery {
    List(SessionListParams),
    History(SessionHistoryParams),
    /// 回放用的有效事件流：只读内核当前持有的会话。
    Events(SessionEventsParams),
}

impl SessionQuery {
    /// 解帧；`method` 从帧里取，非法负载按协议报错。
    pub(super) fn from_frame(frame: &Frame) -> Result<Self, ErrorObject> {
        let params = frame.params.clone().unwrap_or_else(|| json!({}));
        let invalid = |error: serde_json::Error| {
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                format!("{} 负载不符：{error}", frame.method().unwrap_or_default()),
            )
        };
        match frame.method() {
            Some(method::SESSION_LIST) => {
                Ok(Self::List(serde_json::from_value(params).map_err(invalid)?))
            }
            Some(method::SESSION_HISTORY) => Ok(Self::History(
                serde_json::from_value(params).map_err(invalid)?,
            )),
            Some(method::SESSION_EVENTS) => Ok(Self::Events(
                serde_json::from_value(params).map_err(invalid)?,
            )),
            other => Err(ErrorObject::new(
                error_code::METHOD_NOT_FOUND,
                format!("未知方法 {}。", other.unwrap_or_default()),
            )),
        }
    }

    pub(super) fn respond(&self, conn: &Conn) -> Result<Value, ErrorObject> {
        let Some((store, current)) = session_handle(conn) else {
            return Err(no_session_error());
        };
        match self {
            Self::List(params) => {
                // 不按工作区过滤：与 Python「列出全部会话，不再绑当前工作区」一致。
                let query = SessionListQuery {
                    workspace_root: None,
                    project_path: None,
                    limit: params.limit as usize,
                    include_archived: false,
                    archived_only: params.archived,
                };
                let entries = store.list_sessions_filtered(&query).map_err(|error| {
                    ErrorObject::new(
                        error_code::INTERNAL_ERROR,
                        format!("会话列表读取失败：{}", error.message()),
                    )
                })?;
                let items: Vec<Value> = entries.iter().map(|entry| entry.to_dict()).collect();
                Ok(json!({ "sessions": items, "current_session_id": current }))
            }
            Self::History(params) => {
                let entries = store
                    .prompt_history()
                    .search(None, None, &params.query, i64::from(params.limit))
                    .map_err(|error| {
                        ErrorObject::new(
                            error_code::INTERNAL_ERROR,
                            format!("提示历史读取失败：{}", error.message()),
                        )
                    })?;
                let items: Vec<Value> = entries.iter().map(|entry| entry.to_dict()).collect();
                Ok(json!({ "entries": items }))
            }
            Self::Events(params) => {
                // 回退投影后的有效事件流（`turn_undone` 撤掉的轮次不出现），与
                // Python `current_session_events()` 读的是同一条视图，UI 回放据此
                // 重建消息、工具卡与 SubAgent 进度树。
                let events = store
                    .read_active_events(&current)
                    .map_err(|error| {
                        ErrorObject::new(
                            error_code::INTERNAL_ERROR,
                            format!("会话事件读取失败：{}", error.message()),
                        )
                    })?;
                let total = events.len();
                let (selected, has_more) = select_event_window(events, params);
                let selected: Vec<Value> = selected
                    .into_iter()
                    .map(|event| event.to_dict())
                    .collect();
                Ok(json!({
                    "session_id": current,
                    "events": selected,
                    "has_more": has_more,
                    "total": total,
                }))
            }
        }
    }
}

/// `session.events` 单次窗口的条数上限。
const EVENT_WINDOW_MAX_LIMIT: usize = 5_000;
/// 窗口载荷字节预算：一条工具输出/图片事件就可能几百 KB，按字节兜底更稳。
const EVENT_WINDOW_BYTE_BUDGET: usize = 8 * 1024 * 1024;
/// 尾部窗口必须保留的状态事件：它们决定清单、暂停与压缩边界，不能因为落在窗口外就丢。
const TAIL_STATE_EVENT_TYPES: [&str; 6] = [
    "run_guard_continue",
    "run_guard_continue_exhausted",
    "run_guard_paused",
    "turn_cancelled",
    "session_interrupted",
    "compact_summary",
];

/// 从有效事件流里截出尾部窗口。
///
/// `limit = None` 且不带 `before_event_id` 时原样返回全部事件，保持旧客户端语义；
/// 宿主回放只取最后 `limit` 条，并把窗口外的最近状态事件补到前面。
fn select_event_window(
    events: Vec<SessionEvent>,
    params: &SessionEventsParams,
) -> (Vec<SessionEvent>, bool) {
    let end = params
        .before_event_id
        .as_deref()
        .and_then(|id| events.iter().position(|event| event.event_id == id))
        .unwrap_or(events.len());
    let limit = params
        .limit
        .filter(|limit| *limit > 0)
        .map(|limit| (limit as usize).min(EVENT_WINDOW_MAX_LIMIT))
        .unwrap_or(0);
    if limit == 0 && params.before_event_id.is_none() {
        return (events, false);
    }

    let mut start = end;
    let mut bytes = 0usize;
    while start > 0 && (limit == 0 || end - start < limit) {
        let size = event_bytes(&events[start - 1]);
        if limit != 0 && end - start > 0 && bytes.saturating_add(size) > EVENT_WINDOW_BYTE_BUDGET {
            break;
        }
        bytes = bytes.saturating_add(size);
        start -= 1;
    }

    let mut indices: Vec<usize> = (start..end).collect();
    if start > 0 {
        let mut state_indices: Vec<usize> = Vec::new();
        for event_type in TAIL_STATE_EVENT_TYPES {
            if events[start..end]
                .iter()
                .any(|event| event.event_type == event_type)
            {
                continue;
            }
            if let Some(index) = events[..start]
                .iter()
                .rposition(|event| event.event_type == event_type)
            {
                state_indices.push(index);
            }
        }
        if !events[start..end].iter().any(is_todo_call) {
            if let Some(index) = events[..start].iter().rposition(is_todo_call) {
                state_indices.push(index);
            }
        }
        state_indices.sort_unstable();
        state_indices.dedup();
        state_indices.extend(indices);
        indices = state_indices;
    }

    let wanted: std::collections::HashSet<usize> = indices.into_iter().collect();
    let selected = events
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| wanted.contains(&index).then_some(event))
        .collect();
    (selected, start > 0)
}

/// `update_todos` 的工具请求事件：TUI 重放时靠它恢复清单。
fn is_todo_call(event: &SessionEvent) -> bool {
    event.event_type == "tool_call_requested"
        && event.payload.get("tool").and_then(Value::as_str) == Some(TODO_TOOL_NAME)
}

/// 事件载荷的近似字节数；只用于窗口预算，不做序列化。
fn event_bytes(event: &SessionEvent) -> usize {
    event.event_type.len()
        + event.session_id.len()
        + event.event_id.len()
        + event.parent_id.as_deref().map_or(0, str::len)
        + event
            .payload
            .iter()
            .map(|(key, value)| key.len() + json_bytes(value) + 4)
            .sum::<usize>()
        + 64
}

fn json_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(number) => number.to_string().len(),
        Value::String(text) => text.len() + 2,
        Value::Array(items) => {
            2 + items.len() + items.iter().map(json_bytes).sum::<usize>()
        }
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(key, value)| key.len() + json_bytes(value) + 4)
                .sum::<usize>()
        }
    }
}

/// 内核没有自持会话时的统一拒绝（会话读写都要求 `initialize.session`）。
pub(super) fn no_session_error() -> ErrorObject {
    ErrorObject::new(
        error_code::INVALID_REQUEST,
        "当前会话不受内核持有，无法执行该会话操作。",
    )
}

/// 把一次只读会话查询的结果写回（空闲状态下的分发入口）。
pub(super) fn respond_session_query(conn: &Rc<RefCell<Conn>>, id: Id, query: &SessionQuery) {
    let outcome = query.respond(&conn.borrow());
    match outcome {
        Ok(value) => conn.borrow_mut().respond(id, value),
        Err(error) => conn.borrow_mut().respond_error(id, error),
    }
}

pub(super) fn respond_no_session(conn: &Rc<RefCell<Conn>>, id: Id) {
    conn.borrow_mut().respond_error(id, no_session_error());
}

/// `session.rename`：重命名当前会话。
pub(super) fn respond_session_rename(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionRenameParams) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    match store.rename_session(&current, &params.title, utc_now()) {
        Ok(entry) => conn
            .borrow_mut()
            .respond(id, json!({ "session": entry.to_dict() })),
        Err(error) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_REQUEST,
                format!("重命名会话失败：{}", error.message()),
            ),
        ),
    }
}

/// `session.archive`：归档当前会话并立即开启一个新的空会话。
pub(super) fn respond_session_archive(conn: &Rc<RefCell<Conn>>, id: Id) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    let archived = match store.archive_session(&current, utc_now()) {
        Ok(entry) => entry,
        Err(error) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("归档会话失败：{}", error.message()),
                ),
            );
            return;
        }
    };
    // 归档完成后再开新会话：新开的会话成为当前会话，归档的那条从默认列表隐藏。
    let started = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.start_new());
    let mut result = json!({ "session": archived.to_dict() });
    match started {
        Some(Ok(new_session_id)) => {
            result["new_session_id"] = json!(new_session_id);
        }
        Some(Err(detail)) => {
            eprintln!("[kernel] 归档后新建会话失败：{detail}");
        }
        None => {}
    }
    conn.borrow_mut().respond(id, result);
}

/// `session.new`：清空当前对话并开启新会话。
pub(super) fn respond_session_new(conn: &Rc<RefCell<Conn>>, id: Id) {
    let started = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.start_new());
    match started {
        None => respond_no_session(conn, id),
        Some(Ok(session_id)) => conn
            .borrow_mut()
            .respond(id, json!({ "session_id": session_id })),
        Some(Err(detail)) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INTERNAL_ERROR,
                format!("新建会话失败：{detail}"),
            ),
        ),
    }
}

/// `session.resume`：切到指定会话，用转录重建历史；归档会话先解除归档。
pub(super) fn respond_session_resume(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionResumeParams) {
    let Some((store, current)) = session_handle(&conn.borrow()) else {
        respond_no_session(conn, id);
        return;
    };
    let target = params.session_id.trim();
    if target.is_empty() {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                "session.resume 需要 session_id。",
            ),
        );
        return;
    }
    // 先证明目标会话有效，再做任何切换动作。
    let entry = match store.list_sessions() {
        Ok(entries) => entries.into_iter().find(|entry| entry.session_id == target),
        Err(error) => {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INTERNAL_ERROR,
                    format!("会话列表读取失败：{}", error.message()),
                ),
            );
            return;
        }
    };
    let Some(entry) = entry else {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_REQUEST, format!("未找到会话：{target}")),
        );
        return;
    };
    // 归档会话恢复后还要继续写入，先解除归档（失败不阻断切换）。
    if entry.archived_at.is_some() {
        if let Err(error) = store.unarchive_session(target, utc_now()) {
            conn.borrow_mut().respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("恢复归档会话失败：{}", error.message()),
                ),
            );
            return;
        }
    }
    // 切换走之前丢掉当前的空占位会话（切回同一会话时不动）。
    if current != target {
        let _ = store.discard_empty_session(&current);
    }
    let reopened = conn
        .borrow_mut()
        .session
        .as_mut()
        .map(|session| session.reopen(target));
    match reopened {
        Some(Ok(())) => {
            // 历史只在调用方明确要时复制：TUI 的 /resume 用 session.events 重放，
            // 不需要把整份历史再克隆、序列化一遍。
            let history = if params.include_history {
                conn.borrow()
                    .session
                    .as_ref()
                    .map(|session| session.history.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // 回包带上索引条目与历史：宿主据此展示标题/消息数并重放对话视图。
            conn.borrow_mut().respond(
                id,
                json!({
                    "session_id": target,
                    "session": entry.to_dict(),
                    "history": history,
                }),
            );
        }
        Some(Err(detail)) => conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(
                error_code::INTERNAL_ERROR,
                format!("恢复会话失败：{detail}"),
            ),
        ),
        None => respond_no_session(conn, id),
    }
}

pub(super) fn respond_session_append(conn: &Rc<RefCell<Conn>>, id: Id, params: &SessionAppendParams) {
    let role = params.role.trim().to_lowercase();
    if role != "assistant" {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::with_data(
                error_code::INVALID_PARAMS,
                "session.append 只支持 role=assistant。",
                json!({ "kind": "invalid_settings" }),
            ),
        );
        return;
    }
    let content = params.content.trim();
    if content.is_empty() {
        conn.borrow_mut().respond(id, json!({ "appended": false }));
        return;
    }
    let mut borrowed = conn.borrow_mut();
    let Some(session) = borrowed.session.as_mut() else {
        drop(borrowed);
        respond_no_session(conn, id);
        return;
    };
    let message = json!({"role": "assistant", "content": content});
    // 先落盘再进运行期历史：读取端（本次响应）看到的与下一轮请求带上的一致。
    let appended = match session.append("assistant_message", json!({ "content": content })) {
        Ok(()) => true,
        Err(detail) => {
            eprintln!("[kernel] 追加 assistant 消息失败：{detail}");
            false
        }
    };
    if appended {
        session.history.push(message);
    }
    drop(borrowed);
    conn.borrow_mut()
        .respond(id, json!({ "appended": appended }));
}

#[cfg(test)]
mod event_window_tests {
    use super::*;

    fn event(id: &str, event_type: &str, payload: Value) -> SessionEvent {
        let mut event = SessionEvent::create(
            "20260101-000000-abcdef",
            event_type,
            payload.as_object().cloned().unwrap_or_default(),
            None,
            chrono::Utc::now(),
        )
        .expect("事件可构造");
        event.event_id = id.to_string();
        event
    }

    fn ids(events: &[SessionEvent]) -> Vec<&str> {
        events.iter().map(|event| event.event_id.as_str()).collect()
    }

    #[test]
    fn no_limit_returns_everything() {
        let events = vec![
            event("e1", "user_message", serde_json::json!({"content": "问"})),
            event("e2", "assistant_message", serde_json::json!({"content": "答"})),
        ];
        let (selected, has_more) =
            select_event_window(events, &SessionEventsParams::default());
        assert_eq!(ids(&selected), vec!["e1", "e2"]);
        assert!(!has_more);
    }

    #[test]
    fn limit_keeps_the_tail_and_prepends_state_events() {
        let events = vec![
            event("e1", "run_guard_paused", serde_json::json!({"content": "继续"})),
            event("e2", "user_message", serde_json::json!({"content": "问"})),
            event("e3", "assistant_message", serde_json::json!({"content": "答"})),
        ];
        let params = SessionEventsParams {
            limit: Some(1),
            before_event_id: None,
        };
        let (selected, has_more) = select_event_window(events, &params);
        assert_eq!(ids(&selected), vec!["e1", "e3"]);
        assert!(has_more);
    }

    #[test]
    fn todo_tool_call_is_preserved_outside_the_window() {
        let events = vec![
            event(
                "e1",
                "tool_call_requested",
                serde_json::json!({"tool": TODO_TOOL_NAME, "arguments": {"todos": []}}),
            ),
            event("e2", "assistant_message", serde_json::json!({"content": "答"})),
        ];
        let params = SessionEventsParams {
            limit: Some(1),
            before_event_id: None,
        };
        let (selected, _) = select_event_window(events, &params);
        assert_eq!(ids(&selected), vec!["e1", "e2"]);
    }

    #[test]
    fn before_event_id_pages_towards_older_events() {
        let events = vec![
            event("e1", "user_message", serde_json::json!({"content": "一"})),
            event("e2", "assistant_message", serde_json::json!({"content": "二"})),
            event("e3", "user_message", serde_json::json!({"content": "三"})),
        ];
        let params = SessionEventsParams {
            limit: Some(2),
            before_event_id: Some("e3".to_string()),
        };
        let (selected, has_more) = select_event_window(events, &params);
        assert_eq!(ids(&selected), vec!["e1", "e2"]);
        assert!(!has_more);
    }
}
