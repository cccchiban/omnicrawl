//! Telegram 文本分段与流式显示裁剪。
//!
//! 语义基准是 Python `omnicrawl/connectors/telegram.py` 的 `_split_message` /
//! `_truncate_for_stream` / `_abort_stream` / `_finalize_stream`：长度一律按**字符**计，
//! 与 Python 的 `len(str)` / 切片一致，多字节字符不会被切坏。

/// sendMessage 单条消息上限（4096）留出的余量。
pub const MAX_MESSAGE_LEN: usize = 4000;

/// 流式编辑最小间隔（秒）：Telegram 对同一条消息的编辑频率约 1 次/秒。
pub const STREAM_EDIT_INTERVAL_SECONDS: f64 = 0.9;

/// 流式中途的显示上限：预留前缀与定型空间，避免编辑超限。
const STREAM_LIMIT: usize = MAX_MESSAGE_LEN - 20;

const INTERRUPTED_SUFFIX: &str = "…（输出中断）";
const FULL_NEXT_SUFFIX: &str = "…（完整内容见下一条）";

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn skip_chars(text: &str, count: usize) -> String {
    text.chars().skip(count).collect()
}

/// 按行优先、字符兜底把长文本切成不超过 `limit` 的分段。
///
/// - 分段内保留原有换行，不会把相邻两行粘连成一行；
/// - 行边界处切分时在段尾保留换行分隔符，下一段从新行开始；
/// - 单行超长时硬切，切点插入换行分隔符（占下一段 1 字符预算）。
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    if char_len(text) <= limit {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.split('\n') {
        let separator = if current.is_empty() { 0 } else { 1 };
        if char_len(&current) + separator + char_len(line) > limit {
            if !current.is_empty() {
                parts.push(format!("{current}\n"));
            }
            current = line.to_string();
        } else if current.is_empty() {
            current.push_str(line);
        } else {
            current.push('\n');
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    let mut final_parts: Vec<String> = Vec::new();
    for part in parts {
        let mut rest = part;
        while char_len(&rest) > limit {
            final_parts.push(take_chars(&rest, limit));
            rest = format!("\n{}", skip_chars(&rest, limit));
        }
        final_parts.push(rest);
    }
    final_parts
}

/// 流式中途的显示截断：预留前缀/定型空间，避免编辑超限。
pub fn truncate_for_stream(text: &str) -> String {
    truncate_for_stream_with_limit(text, STREAM_LIMIT)
}

pub fn truncate_for_stream_with_limit(text: &str, limit: usize) -> String {
    if char_len(text) <= limit {
        return text.to_string();
    }
    format!("{}…", take_chars(text, limit.saturating_sub(1)))
}

/// 任务异常/取消时的显示收尾：已流式输出的部分定型，错误单独一条消息。
#[derive(Debug, Clone, PartialEq)]
pub struct AbortPlan {
    /// 需要就地编辑的流式消息正文；`None` 表示没有可保留的部分输出。
    pub edit_head: Option<String>,
    /// 需要单独发送的错误文本。
    pub send_text: String,
}

/// 把已输出内容截断并追加中断标记；错误文本原样传给调用方（脱敏由调用方负责）。
pub fn plan_abort(has_stream_message: bool, deltas: &str, error_text: &str) -> AbortPlan {
    let edit_head = if has_stream_message && !deltas.is_empty() {
        let budget = MAX_MESSAGE_LEN.saturating_sub(char_len(INTERRUPTED_SUFFIX) + 1);
        Some(format!(
            "{}{}",
            take_chars(deltas, budget),
            INTERRUPTED_SUFFIX
        ))
    } else {
        None
    };
    AbortPlan {
        edit_head,
        send_text: error_text.to_string(),
    }
}

/// 定型流式消息的三种处置：短文本就地编辑、超长先编辑再补发、无流式消息直接发送。
#[derive(Debug, Clone, PartialEq)]
pub enum FinalizePlan {
    /// 短文本：编辑同一条流式消息定型。
    EditFull(String),
    /// 超长：流式消息显示截断头，完整内容另发一条。
    EditHeadThenSend { head: String, full: String },
    /// 没有流式消息：直接发送。
    SendFull(String),
}

/// 定型流式消息：`has_stream_message` 表示本次回合是否已创建过流式消息。
pub fn plan_finalize(has_stream_message: bool, text: &str) -> FinalizePlan {
    if has_stream_message && char_len(text) <= MAX_MESSAGE_LEN.saturating_sub(8) {
        return FinalizePlan::EditFull(text.to_string());
    }
    if has_stream_message {
        let budget = MAX_MESSAGE_LEN.saturating_sub(char_len(FULL_NEXT_SUFFIX) + 1);
        let head = format!("{}{}", take_chars(text, budget), FULL_NEXT_SUFFIX);
        return FinalizePlan::EditHeadThenSend {
            head,
            full: text.to_string(),
        };
    }
    FinalizePlan::SendFull(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_single_part() {
        assert_eq!(split_message("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn long_line_is_hard_split_with_separator() {
        let parts = split_message("abcdefghij", 4);
        assert_eq!(parts, vec!["abcd", "\nefg", "\nhij"]);
    }

    #[test]
    fn multibyte_is_never_cut_in_half() {
        let parts = split_message("中文汉字测试", 3);
        assert!(parts.iter().all(|part| part.chars().count() <= 3));
        let joined: String = parts.concat();
        assert!(joined.contains('测'));
    }
}
