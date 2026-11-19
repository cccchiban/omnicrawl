//! 零散小工具：问题类型判定、待办快照、审批模式映射等。

use crate::service::*;

/// 提问种类：工具参数里的 `kind` 优先，缺省按有没有选项判。
pub(super) fn question_kind(arguments: &Map<String, Value>, options: &[String]) -> &'static str {
    match arguments.get("kind").and_then(Value::as_str) {
        Some("select") => "select",
        Some("confirm") => "confirm",
        Some("question") => "question",
        _ => {
            if options.is_empty() {
                "question"
            } else {
                "select"
            }
        }
    }
}
/// `update_todos` 的清单投影（最多 20 项，空步骤跳过）。
pub(super) fn todo_snapshots(raw: &Value) -> Vec<TodoSnapshot> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .take(20)
        .filter_map(|item| {
            let object = item.as_object()?;
            let step = object.get("step").and_then(Value::as_str)?.trim();
            if step.is_empty() {
                return None;
            }
            Some(TodoSnapshot {
                id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                step: step.to_string(),
                completed: object
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}
/// 配置值 → 宿主运行期的审批模式（启动与热应用共用同一套映射）。
///
/// `review`（自动审查）在这里保持 `Review`：宿主执行层的审查闸按它判定哪些调用要交给审查模型。
pub(super) fn runtime_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        APPROVAL_MODE_AUTO => Some(ApprovalMode::Auto),
        APPROVAL_MODE_REVIEW => Some(ApprovalMode::Review),
        APPROVAL_MODE_MANUAL => Some(ApprovalMode::Manual),
        _ => None,
    }
}
/// 内核回执里的 `unavailable` 标记。
pub(super) fn subagent_unavailable_in(value: &Value) -> bool {
    value
        .get("unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
/// 等内核经 stderr 报出的会话 ID；等不到按空值返回（ID 未知不影响回合）。
pub(super) fn wait_for_session_id(ready: &Arc<Mutex<String>>) -> String {
    for _ in 0..40 {
        if let Ok(guard) = ready.lock() {
            if !guard.is_empty() {
                return guard.clone();
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    String::new()
}
/// 非空字符串转 `Option`；空串按「不改」处理（协议语义与内核一致）。
pub(super) fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
