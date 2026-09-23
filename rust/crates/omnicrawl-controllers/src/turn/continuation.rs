//! `loop.py` 的 run_guard 续跑（Continue）状态机：判定面与载荷面。
//!
//! Python 把 Continue 写在 `run_stream` 的编排壳里：一段循环跑完后，如果模型只输出
//! reasoning 而没有文本/工具调用，或执行清单还有未完成项，就自动补发一轮「继续」，
//! 上限是 `[run_guard].continuation.max_auto_followups`。本模块只收判定与载荷，
//! 真实的模型调用、会话事件落盘与上下文拼装留在宿主（`omnicrawl-cli` 的内核回合循环）。
//!
//! 三条边界与 Python 一致：
//!
//! - 生效条件是 `run_guard.enabled` **且** `continuation.enabled`；任一为假时不存在续跑
//!   配置（Python 折成 `None`），`next_step` 一律返回 [`ContinueStep::Stop`]。
//! - `pause_work` 的主动暂停、用户取消、真实文本或工具调用都会终止自动路径。
//! - 上限用尽且仍未完成时写 `run_guard_continue_exhausted`，事件保留待续文本，
//!   用户下一次发「继续」仍能恢复原任务。

use serde_json::{json, Value};

use crate::tool_impl::TodoItem;

/// 自动续跑补发的用户消息（与 Python 逐字一致）。
pub const CONTINUE_PROMPT: &str =
    "请继续执行上一任务，不要停在计划或推理阶段；完成未完成的 Todo，或给出可执行的最终结果。";

/// 续跑理由：模型只输出 reasoning。
pub const REASON_REASONING_ONLY: &str = "reasoning_only";
/// 续跑理由：执行清单仍有未完成项。
pub const REASON_TODO_INCOMPLETE: &str = "todo_incomplete";

/// `run_guard_paused` 事件的理由（`pause_work` 主动暂停）。
pub const PAUSE_REASON: &str = "pause_work";

/// 续跑终态事件名：达到上限仍未完成。
pub const CONTINUE_EXHAUSTED_EVENT: &str = "run_guard_continue_exhausted";
/// 续跑事件名：自动补发一轮。
pub const CONTINUE_EVENT: &str = "run_guard_continue";
/// 暂停事件名：`pause_work` 主动暂停回合。
pub const PAUSED_EVENT: &str = "run_guard_paused";
/// 续跑上限用尽时的理由回落（Python 的 `continuation_reason or "todo_incomplete"`）。
pub const EXHAUSTED_REASON_FALLBACK: &str = REASON_TODO_INCOMPLETE;

/// 生效的续跑参数。
///
/// 只有 `run_guard.enabled` 与 `continuation.enabled` 同时为真时才存在这个值——与 Python
/// 把不生效的配置折成 `None` 同一语义，让「没配置」与「配置为 0 次」在调用点无差别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinueConfig {
    /// 每个用户回合最多自动补发几轮。
    pub max_auto_followups: usize,
}

impl ContinueConfig {
    /// 由配置段解析：总开关关闭、续跑开关关闭或上限非正时返回 `None`。
    pub fn resolve(
        run_guard_enabled: bool,
        continuation_enabled: bool,
        max_auto_followups: i64,
    ) -> Option<Self> {
        if !run_guard_enabled || !continuation_enabled || max_auto_followups <= 0 {
            return None;
        }
        Some(Self {
            max_auto_followups: max_auto_followups as usize,
        })
    }
}

/// 最后一条模型回复里与续跑有关的三项事实。
///
/// Python 直接读 `last_reply` 的 `reasoning` / `content` / `tool_calls`；内核把这三项
/// 归一成事实结构，判定面不依赖任何模型运行时类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastReplyFacts {
    pub has_reasoning: bool,
    pub content_empty: bool,
    pub no_tool_calls: bool,
}

impl LastReplyFacts {
    /// 从一条模型回复的原始字段构造（`tool_call_count` 为该回复的工具调用条数）。
    pub fn new(reasoning: &str, content: &str, tool_call_count: usize) -> Self {
        Self {
            has_reasoning: !reasoning.trim().is_empty(),
            content_empty: content.trim().is_empty(),
            no_tool_calls: tool_call_count == 0,
        }
    }
}

/// reasoning-only：只给了 reasoning，既没有文本也没有工具调用。
pub fn reasoning_only(last: Option<&LastReplyFacts>) -> bool {
    last.map(|facts| facts.has_reasoning && facts.content_empty && facts.no_tool_calls)
        .unwrap_or(false)
}

/// 执行清单仍有未完成项（空清单不算未完成）。
pub fn todo_incomplete(todos: &[TodoItem]) -> bool {
    !todos.is_empty() && todos.iter().any(|item| !item.completed)
}

/// 本次收尾的续跑理由；不需要续跑时为 `None`。
///
/// reasoning-only 优先于清单判定（Python 的三元选择）。
pub fn continue_reason(last: Option<&LastReplyFacts>, todos: &[TodoItem]) -> Option<&'static str> {
    if reasoning_only(last) {
        return Some(REASON_REASONING_ONLY);
    }
    if todo_incomplete(todos) {
        return Some(REASON_TODO_INCOMPLETE);
    }
    None
}

/// 一次循环收尾后的下一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinueStep {
    /// 结束回合：不再补发。
    Stop,
    /// 补发一轮；`followup` 是 1-based 次数（事件与提示文案都用它）。
    Continue {
        followup: usize,
        reason: &'static str,
    },
}

/// 判定是否再补发一轮。
///
/// `used` 是本回合已经补发过的次数，`paused` 是 `loop_result.paused || pause_requested()`
/// （主动暂停与取消都算），`todos` 是当前执行清单投影。
pub fn next_step(
    config: Option<&ContinueConfig>,
    used: usize,
    paused: bool,
    last: Option<&LastReplyFacts>,
    todos: &[TodoItem],
) -> ContinueStep {
    if paused {
        return ContinueStep::Stop;
    }
    let Some(config) = config else {
        return ContinueStep::Stop;
    };
    if used >= config.max_auto_followups {
        return ContinueStep::Stop;
    }
    match continue_reason(last, todos) {
        None => ContinueStep::Stop,
        Some(reason) => ContinueStep::Continue {
            followup: used + 1,
            reason,
        },
    }
}

/// 续跑上限已用尽且仍未完成：写 `run_guard_continue_exhausted`。
///
/// 与 Python 一致，只有「没暂停、有续跑配置、真的补发过、且最后一次仍未完成」才成立。
pub fn continuation_exhausted(
    config: Option<&ContinueConfig>,
    used: usize,
    paused: bool,
    last: Option<&LastReplyFacts>,
    todos: &[TodoItem],
) -> bool {
    if paused {
        return false;
    }
    let Some(config) = config else {
        return false;
    };
    used >= config.max_auto_followups
        && used > 0
        && (reasoning_only(last) || todo_incomplete(todos))
}

/// 续跑提示文案（对应 Python 的 `report_retry_status`）。
pub fn continue_status_text(followup: usize) -> String {
    format!("任务仍未完成，正在自动继续（第{followup}次）")
}

/// 执行清单的落盘投影：`[{id, step, completed}]`，与 `user_message` / `run_guard_*` 事件同形。
pub fn todo_items_value(todos: &[TodoItem]) -> Value {
    Value::Array(
        todos
            .iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "step": item.step,
                    "completed": item.completed,
                })
            })
            .collect(),
    )
}

/// `run_guard_continue` 事件载荷。
pub fn continue_event_payload(followup: usize, reason: &str, pending_user_text: &str) -> Value {
    json!({
        "followup": followup,
        "reason": reason,
        "pending_user_text": pending_user_text,
    })
}

/// `run_guard_paused` 事件载荷（`pause_work` 主动暂停）。
pub fn paused_event_payload(user_text: &str, pending_user_text: &str, todos: &[TodoItem]) -> Value {
    json!({
        "user_text": user_text,
        "pending_user_text": pending_user_text,
        "reason": PAUSE_REASON,
        "todo_items": todo_items_value(todos),
    })
}

/// `run_guard_continue_exhausted` 事件载荷。
pub fn exhausted_event_payload(
    followups: usize,
    pending_user_text: &str,
    reason: &str,
    todos: &[TodoItem],
) -> Value {
    let reason = if reason.trim().is_empty() {
        EXHAUSTED_REASON_FALLBACK
    } else {
        reason
    };
    json!({
        "followups": followups,
        "pending_user_text": pending_user_text,
        "reason": reason,
        "todo_items": todo_items_value(todos),
    })
}

/// 把一段循环的收尾事实并进本回合累计（对应 Python 的三行累加）。
///
/// 空文本不进累计——最终回复按 `"\n\n"` 拼接，多一个空段会多出一个空行。
pub fn record_episode(
    reply_parts: &mut Vec<String>,
    reasoning_parts: &mut Vec<String>,
    content_streamed_seen: &mut bool,
    final_text: &str,
    reasoning: &str,
    content_streamed: bool,
) {
    if !final_text.is_empty() {
        reply_parts.push(final_text.to_string());
    }
    if !reasoning.is_empty() {
        reasoning_parts.push(reasoning.to_string());
    }
    *content_streamed_seen = *content_streamed_seen || content_streamed;
}

/// 最终回复：`"\n\n"` 拼接后去首尾空白（Python `"\n\n".join(parts).strip()`）。
pub fn joined_reply(parts: &[String]) -> String {
    parts.join("\n\n").trim().to_string()
}

/// 合并后的推理文本：`"\n"` 拼接后去首尾空白。
pub fn joined_reasoning(parts: &[String]) -> String {
    parts.join("\n").trim().to_string()
}

/// 最终回复是否需要补发一次增量（有文本但整回合都没流式发过）。
pub fn needs_final_delta(parts: &[String], content_streamed_seen: bool) -> bool {
    parts.iter().any(|part| !part.is_empty()) && !content_streamed_seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todos(items: &[(&str, bool)]) -> Vec<TodoItem> {
        items
            .iter()
            .map(|(step, completed)| TodoItem {
                id: step.to_string(),
                step: step.to_string(),
                completed: *completed,
            })
            .collect()
    }

    #[test]
    fn config_requires_both_switches() {
        assert!(ContinueConfig::resolve(true, true, 3).is_some());
        assert!(ContinueConfig::resolve(false, true, 3).is_none());
        assert!(ContinueConfig::resolve(true, false, 3).is_none());
        assert!(ContinueConfig::resolve(true, true, 0).is_none());
        assert!(
            ContinueConfig::resolve(false, false, 0).is_none(),
            "总开关关闭时续跑不存在"
        );
    }

    #[test]
    fn reasoning_only_needs_reasoning_without_text_or_tools() {
        let only_reasoning = LastReplyFacts::new("我在想", "   ", 0);
        assert!(reasoning_only(Some(&only_reasoning)));

        // 有文本就不是 reasoning-only。
        assert!(!reasoning_only(Some(&LastReplyFacts::new(
            "我在想",
            "结论",
            0
        ))));
        // 有工具调用也不是。
        assert!(!reasoning_only(Some(&LastReplyFacts::new("我在想", "", 1))));
        // 没有 reasoning 也不是。
        assert!(!reasoning_only(Some(&LastReplyFacts::new("", "", 0))));
        // 没有回复也不是。
        assert!(!reasoning_only(None));
    }

    #[test]
    fn todo_incomplete_ignores_empty_list() {
        assert!(!todo_incomplete(&[]));
        assert!(!todo_incomplete(&todos(&[("a", true), ("b", true)])));
        assert!(todo_incomplete(&todos(&[("a", true), ("b", false)])));
    }

    #[test]
    fn reason_prefers_reasoning_only() {
        let only_reasoning = LastReplyFacts::new("r", "", 0);
        assert_eq!(
            continue_reason(Some(&only_reasoning), &todos(&[("a", false)])),
            Some(REASON_REASONING_ONLY)
        );
        assert_eq!(
            continue_reason(
                Some(&LastReplyFacts::new("", "有文本", 0)),
                &todos(&[("a", false)])
            ),
            Some(REASON_TODO_INCOMPLETE)
        );
        assert_eq!(
            continue_reason(
                Some(&LastReplyFacts::new("", "有文本", 0)),
                &todos(&[("a", true)])
            ),
            None
        );
    }

    #[test]
    fn next_step_stops_on_pause_disable_and_limit() {
        let config = ContinueConfig {
            max_auto_followups: 2,
        };
        let only_reasoning = LastReplyFacts::new("r", "", 0);

        // 主动暂停立即终止自动路径。
        assert_eq!(
            next_step(Some(&config), 0, true, Some(&only_reasoning), &[]),
            ContinueStep::Stop
        );
        // 没有续跑配置。
        assert_eq!(
            next_step(None, 0, false, Some(&only_reasoning), &[]),
            ContinueStep::Stop
        );
        // 上限用尽。
        assert_eq!(
            next_step(Some(&config), 2, false, Some(&only_reasoning), &[]),
            ContinueStep::Stop
        );
        // 真实文本 + 已完成清单。
        assert_eq!(
            next_step(
                Some(&config),
                0,
                false,
                Some(&LastReplyFacts::new("", "完成", 0)),
                &todos(&[("a", true)])
            ),
            ContinueStep::Stop
        );
        // 上限内且仍未完成：次数是 1-based。
        assert_eq!(
            next_step(Some(&config), 0, false, Some(&only_reasoning), &[]),
            ContinueStep::Continue {
                followup: 1,
                reason: REASON_REASONING_ONLY
            }
        );
        assert_eq!(
            next_step(Some(&config), 1, false, None, &todos(&[("a", false)])),
            ContinueStep::Continue {
                followup: 2,
                reason: REASON_TODO_INCOMPLETE
            }
        );
    }

    #[test]
    fn exhausted_requires_used_budget_and_unfinished_last_episode() {
        let config = ContinueConfig {
            max_auto_followups: 2,
        };
        let only_reasoning = LastReplyFacts::new("r", "", 0);

        assert!(continuation_exhausted(
            Some(&config),
            2,
            false,
            Some(&only_reasoning),
            &[]
        ));
        assert!(continuation_exhausted(
            Some(&config),
            2,
            false,
            None,
            &todos(&[("a", false)])
        ));
        // 没真的补发过。
        assert!(!continuation_exhausted(
            Some(&config),
            0,
            false,
            Some(&only_reasoning),
            &[]
        ));
        // 上限还没到。
        assert!(!continuation_exhausted(
            Some(&config),
            1,
            false,
            Some(&only_reasoning),
            &[]
        ));
        // 最后一次其实完成了。
        assert!(!continuation_exhausted(
            Some(&config),
            2,
            false,
            Some(&LastReplyFacts::new("", "完成", 0)),
            &todos(&[("a", true)])
        ));
        // 暂停回合不写终态。
        assert!(!continuation_exhausted(
            Some(&config),
            2,
            true,
            Some(&only_reasoning),
            &[]
        ));
        // 没有续跑配置。
        assert!(!continuation_exhausted(
            None,
            2,
            false,
            Some(&only_reasoning),
            &[]
        ));
    }

    #[test]
    fn payloads_match_session_projection_keys() {
        let items = todos(&[("写测试", false)]);
        let payload = continue_event_payload(2, REASON_TODO_INCOMPLETE, "原任务");
        assert_eq!(payload["followup"], json!(2));
        assert_eq!(payload["reason"], json!(REASON_TODO_INCOMPLETE));
        assert_eq!(payload["pending_user_text"], json!("原任务"));

        let paused = paused_event_payload("继续", "原任务", &items);
        assert_eq!(paused["user_text"], json!("继续"));
        assert_eq!(paused["pending_user_text"], json!("原任务"));
        assert_eq!(paused["reason"], json!(PAUSE_REASON));
        assert_eq!(
            paused["todo_items"],
            json!([{"id": "写测试", "step": "写测试", "completed": false}])
        );

        let exhausted = exhausted_event_payload(3, "原任务", "", &items);
        assert_eq!(exhausted["followups"], json!(3));
        assert_eq!(exhausted["reason"], json!(EXHAUSTED_REASON_FALLBACK));
        assert_eq!(exhausted["todo_items"][0]["completed"], json!(false));
    }

    #[test]
    fn status_text_counts_from_one() {
        assert_eq!(
            continue_status_text(1),
            "任务仍未完成，正在自动继续（第1次）"
        );
        assert_eq!(
            continue_status_text(3),
            "任务仍未完成，正在自动继续（第3次）"
        );
    }

    #[test]
    fn episode_accumulation_and_final_text() {
        let mut replies: Vec<String> = Vec::new();
        let mut reasonings: Vec<String> = Vec::new();
        let mut streamed = false;
        record_episode(&mut replies, &mut reasonings, &mut streamed, "", "想", true);
        record_episode(
            &mut replies,
            &mut reasonings,
            &mut streamed,
            "第一段",
            "",
            false,
        );
        record_episode(
            &mut replies,
            &mut reasonings,
            &mut streamed,
            "第二段",
            "",
            false,
        );
        assert_eq!(joined_reply(&replies), "第一段\n\n第二段");
        assert_eq!(joined_reasoning(&reasonings), "想");
        // 只要有一段流式发过，就不再补发最终回复。
        assert!(!needs_final_delta(&replies, streamed));

        let mut never_streamed = false;
        let mut parts = Vec::new();
        record_episode(
            &mut parts,
            &mut Vec::new(),
            &mut never_streamed,
            "只此一段",
            "",
            false,
        );
        assert!(needs_final_delta(&parts, never_streamed));
        assert!(!needs_final_delta(&[], false), "没有文本就不补发");
    }
}
