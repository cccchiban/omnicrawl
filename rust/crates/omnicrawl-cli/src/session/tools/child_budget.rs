//! 子代理回合的上下文降载：在消息列表里就地裁掉最旧的工具批次。
//!
//! 子回合与主回合的差别在于**没有会话转录**：子任务的消息列表只活在这次调用里，
//! 不落盘、不进 `SessionStore`。因此主回合那套压缩链（读事件 → 摘要模型 → 归档 →
//! 写记忆）在这里全部用不上，能做的只有就地裁剪。
//!
//! 裁剪口径刻意保守：
//! * 用户任务消息（子任务的 `<subagent_task>` 指令）永远保留——丢了它，子代理就不知道
//!   自己在做什么；
//! * 最近若干轮原样保留——这些正是子代理当前据以推进的观察；
//! * 只从最旧的一端裁剪，且以「一整批工具调用 + 它的结果」为单位，绝不切在调用与结果之间。
//!
//! 触发点有两处：请求前的预算检查（到达声明的窗口比例）与上游明确回报上下文超限后。

use serde_json::Value;

use omnicrawl_controllers::context_compaction::estimate_value_tokens;

/// 请求前触发裁剪的窗口占比：与 `[context_compaction].trigger_context_percent` 的默认档一致。
pub const DEFAULT_TRIGGER_PERCENT: f64 = 80.0;

/// 至少保留的最近消息条数（按「一轮 = 一条 assistant + 其后的 tool 结果」计）。
pub const DEFAULT_KEEP_RECENT_TURNS: usize = 4;

/// 裁剪结果：新消息列表与本轮裁掉的条数。
#[derive(Debug, Clone, PartialEq)]
pub struct TrimOutcome {
    pub messages: Vec<Value>,
    pub dropped: usize,
}

impl TrimOutcome {
    pub fn trimmed(&self) -> bool {
        self.dropped > 0
    }
}

/// 按声明的窗口给子代理折算裁剪阈值。
///
/// `window` 非正数时返回 0，表示「没有可用的窗口声明」，调用方据此跳过请求前裁剪——
/// 拿一个猜出来的窗口去裁，只会把本来够用的上下文白白丢掉。
pub fn trigger_tokens(window: i64, percent: f64) -> i64 {
    if window <= 0 {
        return 0;
    }
    let ratio = if percent > 0.0 && percent <= 100.0 {
        percent / 100.0
    } else {
        DEFAULT_TRIGGER_PERCENT / 100.0
    };
    (window as f64 * ratio) as i64
}

/// 估算整个请求体的输入规模：系统提示、上下文消息、工具声明与历史一起算。
///
/// 与主回合同一口径（`estimate_value_tokens`），不另立一套估算，否则同一条消息
/// 在主回合与子回合会得到两个价格。
pub fn request_tokens(
    system_prompt: &str,
    context_messages: &[Value],
    tools: &[Value],
    messages: &[Value],
) -> i64 {
    let mut total = estimate_value_tokens(&Value::String(system_prompt.to_string()));
    total += estimate_value_tokens(&Value::Array(context_messages.to_vec()));
    total += estimate_value_tokens(&Value::Array(tools.to_vec()));
    total += estimate_value_tokens(&Value::Array(messages.to_vec()));
    total
}

/// 从最旧的一端裁掉工具批次，直到估算值回到 `target_tokens` 以下。
///
/// `target_tokens <= 0` 时不做任何裁剪；已经低于目标时同样原样返回。
pub fn trim_to_budget(
    messages: &[Value],
    system_prompt: &str,
    context_messages: &[Value],
    tools: &[Value],
    target_tokens: i64,
    keep_recent_turns: usize,
) -> TrimOutcome {
    let unchanged = |dropped| TrimOutcome {
        messages: messages.to_vec(),
        dropped,
    };
    if target_tokens <= 0 {
        return unchanged(0);
    }
    let keep = keep_recent_turns.max(1);
    let protected = protected_indexes(messages, keep);

    let mut current = messages.to_vec();
    let mut dropped = 0;
    while request_tokens(system_prompt, context_messages, tools, &current) > target_tokens {
        let Some(index) = first_droppable(&current, &protected) else {
            break;
        };
        current.remove(index);
        dropped += 1;
    }
    if dropped == 0 {
        return unchanged(0);
    }
    TrimOutcome {
        messages: current,
        dropped,
    }
}

/// 上游明确回报上下文超限后的降载：砍掉一半可裁部分，必要时连最近轮次也一起让位。
///
/// 与请求前裁剪的区别是**目标不再是某个阈值，而是必须腾出空间**：上游已经把话说死，
/// 再发同一份消息只会再失败一次。代价是最新观察可能被裁掉，因此保留最后一条消息
/// （通常是刚拿到的工具结果或用户补充），避免子代理失去当前进展。
pub fn reduce_after_overflow<'a>(
    messages: &'a [Value],
    system_prompt: &str,
    context_messages: &[Value],
    tools: &[Value],
) -> TrimOutcome {
    // 先按常规口径裁：保住最近几轮，只丢最旧的历史。
    let total = request_tokens(system_prompt, context_messages, tools, messages);
    let half = (total / 2).max(1);
    let relaxed = trim_to_budget(
        messages,
        system_prompt,
        context_messages,
        tools,
        half,
        DEFAULT_KEEP_RECENT_TURNS,
    );
    if relaxed.trimmed() {
        return relaxed;
    }

    // 常规口径腾不出空间：只剩「任务指令 + 最后一条」。工具配对由出站前的
    // `repair_tool_pairing` 收尾，因此这里允许切在批次中间。
    let protected = protected_indexes(messages, 0);
    let mut current = messages.to_vec();
    let mut dropped = 0;
    while current.len() > protected.len().max(1) {
        let Some(index) = first_droppable(&current, &protected) else {
            break;
        };
        current.remove(index);
        dropped += 1;
    }
    let _ = tools;
    TrimOutcome {
        messages: current,
        dropped,
    }
}

/// 不可裁剪的下标集合：子任务指令与最近若干轮。
fn protected_indexes(messages: &[Value], keep_recent_turns: usize) -> Vec<usize> {
    let mut protected: Vec<usize> = Vec::new();
    // 任务指令：第一条 user 消息。子回合的 `messages` 以后者开头，但 fork 上下文
    // 可能是父会话快照，因此按「第一条 user」而不是固定下标取。
    if let Some(index) = messages
        .iter()
        .position(|message| role_of(message) == Some("user"))
    {
        protected.push(index);
    }
    // 最近 `keep_recent_turns` 条消息原样保留：它们是子代理此刻正在用的观察。
    if keep_recent_turns > 0 {
        let start = messages.len().saturating_sub(keep_recent_turns);
        protected.extend(start..messages.len());
    }
    protected.sort_unstable();
    protected.dedup();
    protected
}

/// 第一条可裁的消息下标：跳过受保护项，且始终从最旧的一端取。
fn first_droppable(messages: &[Value], protected: &[usize]) -> Option<usize> {
    (0..messages.len()).find(|index| !protected.contains(index))
}

fn role_of(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant_call(id: &str, text: &str) -> Value {
        json!({
            "role": "assistant",
            "content": text,
            "tool_calls": [{"id": id, "type": "function",
                            "function": {"name": "read", "arguments": "{}"}}]
        })
    }

    fn tool_result(id: &str, text: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": text})
    }

    /// 一条任务 + 若干轮工具往返；每轮正文足够长，便于把预算调到一个确定的档。
    fn conversation(turns: usize) -> Vec<Value> {
        let mut messages = vec![user("任务：把 a.py 的返回值改对")];
        for index in 0..turns {
            let id = format!("call-{index}");
            messages.push(assistant_call(&id, "先读文件"));
            messages.push(tool_result(&id, &"内容".repeat(200)));
        }
        messages
    }

    #[test]
    fn zero_window_disables_trimming() {
        let messages = conversation(6);
        let outcome = trim_to_budget(&messages, "sys", &[], &[], 0, 2);
        assert_eq!(outcome.dropped, 0);
        assert_eq!(outcome.messages, messages);
    }

    #[test]
    fn under_budget_is_untouched() {
        let messages = conversation(2);
        let outcome = trim_to_budget(&messages, "sys", &[], &[], i64::MAX, 2);
        assert!(!outcome.trimmed());
        assert_eq!(outcome.messages, messages);
    }

    #[test]
    fn task_instruction_survives_trimming() {
        let messages = conversation(8);
        let outcome = trim_to_budget(&messages, "sys", &[], &[], 400, 2);
        assert!(outcome.trimmed(), "预算收紧后应真的裁掉内容");
        // 第一条仍是子任务指令，否则子代理会忘记自己在做什么。
        assert_eq!(outcome.messages.first(), Some(&messages[0]));
    }

    #[test]
    fn trims_from_the_oldest_end() {
        let messages = conversation(8);
        let outcome = trim_to_budget(&messages, "sys", &[], &[], 1200, 2);
        assert!(outcome.trimmed());
        // 尾部保留：最后两条一定还在。
        let tail = &messages[messages.len() - 2..];
        assert!(outcome.messages.ends_with(tail));
    }

    #[test]
    fn overflow_reduction_always_frees_space() {
        // 常规口径腾不开（只有任务 + 最近两条，keep_recent_turns 把它们全保护住），
        // 超限降载必须退到「只保任务 + 最后一条」，不能原样返回。
        let messages = conversation(2);
        let outcome = reduce_after_overflow(&messages, "sys", &[], &[]);
        assert!(outcome.trimmed(), "超限后必须真的少掉内容");
        assert_eq!(outcome.messages.first(), Some(&messages[0]));
        assert!(outcome.messages.len() < messages.len());
    }

    #[test]
    fn trigger_tokens_follows_window() {
        assert_eq!(trigger_tokens(0, 80.0), 0);
        assert_eq!(trigger_tokens(-1, 80.0), 0);
        assert_eq!(trigger_tokens(100_000, 80.0), 80_000);
        // 非法百分比回落到默认档，而不是把窗口整个让掉。
        assert_eq!(trigger_tokens(100_000, 0.0), 80_000);
        assert_eq!(trigger_tokens(100_000, 300.0), 80_000);
    }

    #[test]
    fn request_tokens_counts_system_and_tools() {
        let messages = vec![user("hi")];
        let bare = request_tokens("", &[], &[], &messages);
        let with_system = request_tokens("很长的系统提示", &[], &[], &messages);
        assert!(with_system > bare);
    }
}
