//! 工具调用结果的按需淘汰：把「刚变老」的那一批交决策模型裁决，无用的整组移出上下文。
//!
//! 术语（用户口径）：一次刚执行完的工具调用批次是「小登」——它还在被后续步骤使用，一律保留；
//! 当 Agent 又推进一批（产生新的工具调用）后，前一批变成「老登」，此时才问决策模型：
//! 这一批里的每次调用，其结果还有必要留在上下文里吗？
//!
//! 两条硬约束（用户指定）：
//! * **只淘汰刚变老的那一批**：淘汰点贴近上下文尾部，前面已发过的前缀逐字不变，前缀缓存不失效；
//! * **按整组淘汰**：一组 = 一次调用（请求 + 结果 + 拒绝，即 `tool_event_ids` 的粒度），
//!   移除时必须让 assistant 的 `tool_calls` 与其 `tool` 结果一起走，绝不留下半截协议。
//!
//! 本模块只放纯逻辑：送审形状（state / 每个调用一个 choice 提问）、裁决结果到调用 ID 的映射，
//! 以及「按调用 ID 整组剔除消息」。出站请求与答案解析留在宿主（决策通道只有一份实现）。

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// 单个调用的参数进请求前截断到的字符数。
pub const PRUNE_ARGUMENT_CHARS: usize = 300;
/// 单个调用的结果进请求前截断到的字符数（判断「还有没有用」不需要全文）。
pub const PRUNE_OUTPUT_CHARS: usize = 600;

/// 一次工具调用的裁决候选：请求侧 + 已执行的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneCandidate {
    /// 协议调用 ID；淘汰时按它整组移除 `tool_calls` 项与 `tool` 结果消息。
    pub call_id: String,
    pub tool: String,
    /// 公开投影后的参数摘要（已截断）。
    pub arguments: String,
    pub ok: bool,
    /// 模型可见的结果正文（已截断）。
    pub output: String,
}

impl PruneCandidate {
    /// 按送审预算收敛参数与结果。
    pub fn bounded(
        call_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: impl AsRef<str>,
        ok: bool,
        output: impl AsRef<str>,
    ) -> Self {
        Self {
            call_id: call_id.into(),
            tool: tool.into(),
            arguments: bound_chars(arguments.as_ref(), PRUNE_ARGUMENT_CHARS),
            ok,
            output: bound_chars(output.as_ref(), PRUNE_OUTPUT_CHARS),
        }
    }
}

/// 每个调用组一个提问：提问 ID 按组下标命名（`g0`、`g1`…），读答案时按下标还原。
pub fn prune_question_id(index: usize) -> String {
    format!("g{index}")
}

/// 候选项键：`keep` 保留、`drop` 淘汰（字面键，与审查通道的 approve / reject 同一习惯）。
pub const PRUNE_KEEP: &str = "keep";
/// 候选项键：该调用组已无用，可以从上下文里移除。
pub const PRUNE_DROP: &str = "drop";

/// 裁决上下文：这些调用都**不是**最新一批（最新一批是小登，从不送审）。
pub const PRUNE_STATE_NOTE: &str =
    "这些工具调用已经执行完毕，且不是最新一轮：最新一轮调用正在被使用，不在本裁定范围内。";
const PRUNE_QUESTION_INSTRUCTIONS: &str = "判断这一次工具调用的结果是否还有必要留在模型的上下文里。\
 只有后续步骤仍要引用它的内容时才保留；过程性探查、已被后续步骤消化、或内容已被别处覆盖的，都判为已无用。";
const PRUNE_KEEP_CRITERIA: &str = "该结果仍必要：后续步骤还要引用它的内容。";
const PRUNE_DROP_CRITERIA: &str = "已无用：过程性探查、已被后续步骤消化，或结果已在别处得到。";

/// 一次淘汰请求的 `state`：任务背景 + 待裁决的调用（都带组下标，便于与提问对齐）。
pub fn prune_state(task: &str, groups: &[PruneCandidate]) -> Value {
    let mut state = Map::new();
    let task = task.trim();
    if !task.is_empty() {
        state.insert("task".to_string(), Value::from(task.to_string()));
    }
    state.insert("note".to_string(), Value::from(PRUNE_STATE_NOTE));
    state.insert(
        "tool_calls".to_string(),
        Value::Array(
            groups
                .iter()
                .enumerate()
                .map(|(index, group)| group_payload(index, group))
                .collect(),
        ),
    );
    Value::Object(state)
}

/// 一次淘汰请求的 `questions`：每个调用组一个二元 choice 提问。
pub fn prune_questions(groups: &[PruneCandidate]) -> Value {
    let mut questions = Map::new();
    for (index, group) in groups.iter().enumerate() {
        let mut criteria = Map::new();
        criteria.insert(
            PRUNE_KEEP.to_string(),
            Value::from(PRUNE_KEEP_CRITERIA.to_string()),
        );
        criteria.insert(
            PRUNE_DROP.to_string(),
            Value::from(format!(
                "{PRUNE_DROP_CRITERIA}（本次调用：{}，状态：{}）",
                group.tool,
                if group.ok { "成功" } else { "失败" }
            )),
        );
        questions.insert(
            prune_question_id(index),
            serde_json::json!({
                "type": "choice",
                "instructions": PRUNE_QUESTION_INSTRUCTIONS,
                "criteria": criteria,
            }),
        );
    }
    Value::Object(questions)
}

/// 组在 `state` 里的形状：下标、工具、参数、状态与结果。
fn group_payload(index: usize, group: &PruneCandidate) -> Value {
    let arguments = group.arguments.trim();
    serde_json::json!({
        "index": index,
        "tool": group.tool,
        "arguments": if arguments.is_empty() { "（无参数）" } else { arguments },
        "status": if group.ok { "成功" } else { "失败" },
        "result": group.output,
    })
}

/// 裁决结果（要淘汰的组下标）映射成调用 ID；越界下标忽略。
pub fn evicted_call_ids(groups: &[PruneCandidate], picked: &[usize]) -> Vec<String> {
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    picked
        .iter()
        .filter(|index| seen.insert(**index))
        .filter_map(|index| groups.get(*index))
        .map(|group| group.call_id.clone())
        .collect()
}

/// `tool_call_evicted` 事件的载荷：记下被淘汰的调用 ID，投影据此在同一份逻辑里剔除消息。
pub fn evicted_event_payload(call_ids: &[String], groups: &[PruneCandidate]) -> Value {
    let evicted: BTreeSet<&str> = call_ids.iter().map(String::as_str).collect();
    serde_json::json!({
        "evicted_call_ids": call_ids,
        "evicted_tool_names": groups
            .iter()
            .filter(|group| evicted.contains(group.call_id.as_str()))
            .map(|group| group.tool.clone())
            .collect::<Vec<String>>(),
    })
}

/// 淘汰落盘的事件类型。
pub const EVICTED_EVENT_TYPE: &str = "tool_call_evicted";

/// 按字符截断到 `limit`，超出时以省略号收尾。
fn bound_chars(text: &str, limit: usize) -> String {
    let text = text.trim();
    let characters: Vec<char> = text.chars().collect();
    if characters.len() <= limit {
        return text.to_string();
    }
    let head: String = characters[..limit].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(call_id: &str, tool: &str, output: &str) -> PruneCandidate {
        PruneCandidate::bounded(call_id, tool, "{}", true, output)
    }

    /// 送审形状：任务背景、说明与逐条调用都在 state 里，每个调用一个提问。
    #[test]
    fn state_and_questions_cover_every_candidate() {
        let groups = vec![
            candidate("c0", "bash", "输出一"),
            candidate("c1", "grep", "输出二"),
        ];
        let state = prune_state("  把脚本整理一下  ", &groups);
        assert_eq!(state["task"], "把脚本整理一下");
        assert_eq!(state["note"], PRUNE_STATE_NOTE);
        assert_eq!(state["tool_calls"][0]["tool"], "bash");
        assert_eq!(state["tool_calls"][1]["result"], "输出二");
        assert_eq!(state["tool_calls"][0]["index"], 0);

        let questions = prune_questions(&groups);
        assert_eq!(questions["g0"]["type"], "choice");
        assert_eq!(questions["g1"]["type"], "choice");
        assert!(questions["g0"]["criteria"][PRUNE_KEEP].is_string());
        assert!(questions["g0"]["criteria"][PRUNE_DROP].is_string());

        // 没有任务背景时不写空字段。
        let bare = prune_state("   ", &groups);
        assert!(bare.get("task").is_none());
    }

    /// 裁决下标 → 调用 ID；越界与重复都收口。
    #[test]
    fn picked_indexes_map_back_to_call_ids() {
        let groups = vec![
            candidate("c0", "bash", ""),
            candidate("c1", "grep", ""),
            candidate("c2", "read", ""),
        ];
        assert_eq!(evicted_call_ids(&groups, &[1, 1, 9]), vec!["c1".to_string()]);
        assert!(evicted_call_ids(&groups, &[]).is_empty());
    }

    /// 送审正文按预算截断，空参数不落空字段。
    #[test]
    fn candidates_are_bounded_before_the_request() {
        let long = "x".repeat(PRUNE_OUTPUT_CHARS + 50);
        let group = PruneCandidate::bounded("c0", "bash", "", true, &long);
        assert_eq!(group.arguments, "");
        assert_eq!(group.output.chars().count(), PRUNE_OUTPUT_CHARS + 1);
        assert!(group.output.ends_with('…'));

        let payload = group_payload(0, &group);
        assert_eq!(payload["arguments"], "（无参数）");
    }
}
