//! `omnicrawl/agent/controllers/turn/loop.py` 的回合文本判定与收尾消息。
//!
//! 这里收的是纯文本规则：短「继续/重试」要还原成上一轮的真实任务、被取消的回合要在历史里
//! 留下可延续的摘要、助手消息要不要带推理文本。真正的回合执行（事件落盘、插件钩子、模型
//! 请求）仍在宿主，见 [`crate::turn::turn_loop`]。

use serde_json::{json, Value};

use crate::shared::CONTINUE_LAST_TASK_TEXTS;

/// 短「继续/重试」的识别：去掉空白与中英文句读后按小写比对固定文案集合。
pub fn is_continue_last_task_request(text: &str) -> bool {
    CONTINUE_LAST_TASK_TEXTS.contains(&normalize_continue_text(text).as_str())
}

fn normalize_continue_text(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|ch| {
            !(ch.is_whitespace() || matches!(ch, '，' | '。' | '.' | '!' | '！' | '?' | '？'))
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// 把短「继续/重试」还原成上一轮未完成的真实任务；没有待续任务时原样返回。
pub fn resolve_continue_request(text: &str, pending_text: Option<&str>) -> String {
    if !is_continue_last_task_request(text) {
        return text.to_string();
    }
    let pending = pending_text.unwrap_or("").trim();
    if pending.is_empty() {
        return text.to_string();
    }
    format!(
        "继续上一轮未完成任务。上一轮任务内容如下，请不要要求用户重复说明，直接基于这个任务继续执行或重试：\n{pending}"
    )
}

/// 被取消回合的历史摘要：按首次出现顺序统计已执行工具，重复出现的写成 `<名>×<次数>`。
///
/// `None` 表示这一轮没有起点快照（纯读/纯对话），与「有快照但没执行工具」是两种文案。
pub fn cancelled_turn_summary(executed_tools: Option<&[String]>) -> String {
    let Some(tools) = executed_tools else {
        return "（上一回合被取消，未生成最终回复）".to_string();
    };
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for name in tools {
        match counts
            .iter_mut()
            .find(|(existing, _)| *existing == name.as_str())
        {
            Some((_, count)) => *count += 1,
            None => counts.push((name.as_str(), 1)),
        }
    }
    if counts.is_empty() {
        return "（上一回合被取消，未生成最终回复，未执行任何工具）".to_string();
    }
    let summary = counts
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name}×{count}")
            } else {
                (*name).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("，");
    format!("（上一回合被取消，未生成最终回复）已执行工具：{summary}")
}

/// 助手消息：有推理文本时附带 `reasoning_content`，键序与 Python 侧一致。
pub fn assistant_message(content: &str, reasoning: &str) -> Value {
    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::from(reasoning);
    }
    message
}
