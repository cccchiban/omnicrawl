//! 流式响应还原：占位符感知的尾部挂起缓冲与结构化还原。
//!
//! 语义基准是 Python `omnicrawl/llm/desensitization/stream.py`。文本分片可能把占位符
//! 切在中间（`…｛Desensitized:1` + `2｝…`）；朴素替换会漏还原或误还原。这里只挂起
//! 「可能是占位符前缀 / 未闭合占位符」的尾串（长度有界），其余照常输出；完整占位符
//! 出现即还原。工具调用参数增量走同一套尾部挂起缓冲（跨分片的占位符同样要还原），
//! 参数已解析成结构值之后做递归还原（键与结构件不动）。
//!
//! 与 Python 的差异（都不出现在真实语料里）：`\s` / `str.isspace()` 含 `\x1c`–`\x1f`
//! 这类控制分隔符，Rust 的 `char::is_whitespace` 不含；占位符里的序号只认 ASCII 数字，
//! Python 的 `\d` 还认 Unicode 数字。

use omnicrawl_protocol::ProviderWarning;
use serde_json::Value;

use super::{
    char_offsets, find_placeholders, has_open_brace, match_placeholder, matches_marker,
    skip_whitespace, DesensitizationError, DesensitizationStats, PlaceholderCycle,
    FULLWIDTH_OPEN_BRACE, PLACEHOLDER_MARKER,
};

/// Provider 以「正常结束」形态表达输出被截断的 finish_reason 值。
///
/// 与 `omnicrawl/agent/runtime/llm_protocol.py::_TRUNCATED_FINISH_REASONS` 同一份清单：
/// 这些值必须进重试 / 回滚路径，周期不能按成功注销（否则半截回复会被当正常完成）。
pub const TRUNCATED_FINISH_REASONS: [&str; 5] = [
    "length",
    "incomplete",
    "max_tokens",
    "content_filter",
    "failed",
];

const MARKER_WORD: &str = "desensitized";
const TEXT_CHANNEL: &str = "text";
const REASONING_CHANNEL: &str = "reasoning";
const ARGS_CHANNEL_PREFIX: &str = "args:";
const STOP_FINISH_REASON: &str = "stop";

/// 占位符最长合理长度（防止「疑似前缀」在异常输入下无限挂起）。
const MAX_HOLD_CHARS: usize = 64;

/// 单次流式响应的还原状态机（每个尝试一个实例）。
///
/// 还原只查传入周期的注册集合：未注册的序号保留原样 + 告警（`strict` 时中止），
/// 绝不把占位符猜成原文。
pub struct StreamRestorer<'a> {
    cycle: &'a PlaceholderCycle,
    stats: &'a mut DesensitizationStats,
    strict: bool,
    /// 文本 / 推理 / 各工具参数通道的挂起缓冲，按通道首次出现顺序排列（对齐 Python dict 的插入序）。
    buffers: Vec<(String, String)>,
    pending_warnings: Vec<ProviderWarning>,
    warned_unresolved: bool,
    warned_malformed: bool,
    pub saw_text: bool,
    pub saw_reasoning: bool,
    pub saw_tool_call: bool,
    pub finish_reason: String,
}

impl<'a> StreamRestorer<'a> {
    pub fn new(
        cycle: &'a PlaceholderCycle,
        stats: &'a mut DesensitizationStats,
        strict: bool,
    ) -> Self {
        Self {
            cycle,
            stats,
            strict,
            buffers: vec![
                (TEXT_CHANNEL.to_string(), String::new()),
                (REASONING_CHANNEL.to_string(), String::new()),
            ],
            pending_warnings: Vec::new(),
            warned_unresolved: false,
            warned_malformed: false,
            saw_text: false,
            saw_reasoning: false,
            saw_tool_call: false,
            finish_reason: STOP_FINISH_REASON.to_string(),
        }
    }

    // ── 文本还原（尾部挂起缓冲） ─────────────────────────────────────────

    pub fn feed_text(&mut self, text: &str) -> Result<String, DesensitizationError> {
        self.feed(TEXT_CHANNEL, text)
    }

    pub fn feed_reasoning(&mut self, text: &str) -> Result<String, DesensitizationError> {
        self.feed(REASONING_CHANNEL, text)
    }

    /// 工具参数增量：与文本同一套尾部挂起缓冲，跨分片的占位符同样要还原。
    pub fn feed_tool_arguments(
        &mut self,
        call_id: &str,
        text: &str,
    ) -> Result<String, DesensitizationError> {
        self.feed(&args_channel(call_id), text)
    }

    /// 流结束：冲刷各工具参数的挂起缓冲，返回 `(call_id, 需补发的文本)` 列表。
    pub fn flush_tool_arguments(&mut self) -> Result<Vec<(String, String)>, DesensitizationError> {
        let channels: Vec<String> = self
            .buffers
            .iter()
            .filter(|(channel, _)| channel.starts_with(ARGS_CHANNEL_PREFIX))
            .map(|(channel, _)| channel.clone())
            .collect();
        let mut tails = Vec::new();
        for channel in channels {
            let buffered = self.channel(channel.as_str()).to_string();
            self.buffers.retain(|(name, _)| name != &channel);
            let (emitted, _) = self.fold(&buffered, true)?;
            if !emitted.is_empty() {
                tails.push((channel[ARGS_CHANNEL_PREFIX.len()..].to_string(), emitted));
            }
        }
        Ok(tails)
    }

    /// 流结束：冲刷两路挂起缓冲（未闭合前缀按原样保留 + 告警）。
    pub fn flush(&mut self) -> Result<(String, String), DesensitizationError> {
        let text_buffer = self.channel(TEXT_CHANNEL).to_string();
        let reasoning_buffer = self.channel(REASONING_CHANNEL).to_string();
        let (text, _) = self.fold(&text_buffer, true)?;
        let (reasoning, _) = self.fold(&reasoning_buffer, true)?;
        self.channel_mut(TEXT_CHANNEL).clear();
        self.channel_mut(REASONING_CHANNEL).clear();
        Ok((text, reasoning))
    }

    fn feed(&mut self, channel: &str, chunk: &str) -> Result<String, DesensitizationError> {
        if chunk.is_empty() {
            return Ok(String::new());
        }
        // 常见情形：上次没有挂起尾串。此时不必「读出旧缓冲 + 拼上分片」再整体扫描，
        // 直接对分片本身走一次折叠（分片通常也正是无花括号的纯文本，命中快路径）。
        if self.channel(channel).is_empty() {
            let (emitted, hold) = self.fold(chunk, false)?;
            if !hold.is_empty() {
                *self.channel_mut(channel) = hold;
            }
            return Ok(emitted);
        }
        let mut combined = self.channel(channel).to_string();
        combined.push_str(chunk);
        let (emitted, hold) = self.fold(&combined, false)?;
        *self.channel_mut(channel) = hold;
        Ok(emitted)
    }

    /// 把缓冲折叠为「可安全输出的文本 + 需继续挂起的尾串」。
    ///
    /// 绝大多数分片既不含完整占位符、也不以「疑似前缀」结尾，此时输出就是输入本身
    /// （原样借用，不再复制）。因此这里先做一次廉价判定：不含花括号、且不是疑似前缀尾
    /// 的文本直接整体外发，跳过「拼串 → 逐字符建表 → 扫描」的整条重活。
    fn fold(
        &mut self,
        buffer: &str,
        flush: bool,
    ) -> Result<(String, String), DesensitizationError> {
        if !flush && !has_open_brace(buffer) {
            return Ok((buffer.to_string(), String::new()));
        }
        let mut emitted = String::new();
        let mut rest = buffer;
        while let Some((start, end, seq)) = find_first_placeholder(rest) {
            emitted.push_str(&self.emit_literal(&rest[..start])?);
            emitted.push_str(&self.resolve(seq, &rest[start..end])?);
            rest = &rest[end..];
        }
        if let Some(index) = partial_start_index(rest) {
            if !flush && rest[index..].chars().count() <= MAX_HOLD_CHARS {
                emitted.push_str(&self.emit_literal(&rest[..index])?);
                return Ok((emitted, rest[index..].to_string()));
            }
        }
        emitted.push_str(&self.emit_literal(rest)?);
        Ok((emitted, String::new()))
    }

    /// 还原一个完整占位符；未注册序号保留原样 + 告警。
    fn resolve(&mut self, seq: u64, matched: &str) -> Result<String, DesensitizationError> {
        if let Some(value) = self.cycle.lookup(seq) {
            self.stats.restore_hits += 1;
            return Ok(value);
        }
        self.stats.restore_unresolved += 1;
        if !self.warned_unresolved {
            self.warned_unresolved = true;
            self.pending_warnings.push(ProviderWarning::new(
                "desensitization_unresolved",
                format!("模型返回了未注册的脱敏占位符（序号 {seq}），已按原样保留。"),
            ));
        }
        if self.strict {
            return Err(DesensitizationError::new(format!(
                "还原遇到未注册的脱敏占位符（序号 {seq}）。"
            )));
        }
        Ok(matched.to_string())
    }

    /// 输出不含完整占位符的文本；残留的疑似畸形前缀保留 + 告警。
    fn emit_literal(&mut self, text: &str) -> Result<String, DesensitizationError> {
        if text.is_empty() {
            return Ok(String::new());
        }
        // 没有花括号就不可能有疑似成形的占位符：直接外发，跳过逐字符扫描。
        if !has_open_brace(text) {
            return Ok(text.to_string());
        }
        let count = count_placeholder_prefixes(text);
        if count > 0 {
            self.stats.restore_malformed += count as u64;
            if !self.warned_malformed {
                self.warned_malformed = true;
                self.pending_warnings.push(ProviderWarning::new(
                    "desensitization_malformed",
                    format!("检测到 {count} 处疑似畸形脱敏占位符，已按原样保留。"),
                ));
            }
            if self.strict {
                return Err(DesensitizationError::new(
                    "还原遇到畸形脱敏占位符（strict_restore）。",
                ));
            }
        }
        Ok(text.to_string())
    }

    // ── 结构化还原（工具调用参数） ───────────────────────────────────────

    /// 替换字符串中所有完整占位符（子串位置还原）。
    pub fn restore_string(&mut self, text: &str) -> Result<String, DesensitizationError> {
        if text.is_empty() {
            return Ok(String::new());
        }
        let matches = find_placeholders(text);
        if matches.is_empty() {
            return Ok(text.to_string());
        }
        let mut restored = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end, seq) in matches {
            restored.push_str(&text[cursor..start]);
            restored.push_str(&self.resolve(seq, &text[start..end])?);
            cursor = end;
        }
        restored.push_str(&text[cursor..]);
        Ok(restored)
    }

    /// 工具调用参数：递归还原字符串值（键与结构件不动）。
    pub fn restore_arguments(&mut self, value: &Value) -> Result<Value, DesensitizationError> {
        match value {
            Value::String(text) => Ok(Value::String(self.restore_string(text)?)),
            Value::Array(items) => {
                let mut restored = Vec::with_capacity(items.len());
                for item in items {
                    restored.push(self.restore_arguments(item)?);
                }
                Ok(Value::Array(restored))
            }
            Value::Object(entries) => {
                let mut restored = serde_json::Map::new();
                for (key, item) in entries {
                    restored.insert(key.clone(), self.restore_arguments(item)?);
                }
                Ok(Value::Object(restored))
            }
            other => Ok(other.clone()),
        }
    }

    // ── 生命周期标记与告警 ──────────────────────────────────────────────

    pub fn note_text(&mut self, text: &str) {
        if !text.is_empty() {
            self.saw_text = true;
        }
    }

    pub fn note_reasoning(&mut self, text: &str) {
        if !text.is_empty() {
            self.saw_reasoning = true;
        }
    }

    pub fn note_tool_call(&mut self) {
        self.saw_tool_call = true;
    }

    pub fn note_completed(&mut self, finish_reason: &str) {
        self.finish_reason = if finish_reason.is_empty() {
            STOP_FINISH_REASON.to_string()
        } else {
            finish_reason.to_string()
        };
    }

    /// 回复可用（有内容 / 工具调用 / 推理且未被截断）→ 周期按成功注销。
    pub fn reply_usable(&self) -> bool {
        if TRUNCATED_FINISH_REASONS.contains(&self.finish_reason.as_str()) {
            return false;
        }
        self.saw_text || self.saw_reasoning || self.saw_tool_call
    }

    /// 取走待外发告警（取走后清空）。
    pub fn take_warnings(&mut self) -> Vec<ProviderWarning> {
        std::mem::take(&mut self.pending_warnings)
    }

    /// 审计计数快照（只到「数量级」粒度，不含任何原文）。
    pub fn stats(&self) -> &DesensitizationStats {
        self.stats
    }

    fn channel(&self, channel: &str) -> &str {
        self.buffers
            .iter()
            .find(|(name, _)| name == channel)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }

    fn channel_mut(&mut self, channel: &str) -> &mut String {
        let index = match self.buffers.iter().position(|(name, _)| name == channel) {
            Some(index) => index,
            None => {
                self.buffers.push((channel.to_string(), String::new()));
                self.buffers.len() - 1
            }
        };
        &mut self.buffers[index].1
    }
}

fn args_channel(call_id: &str) -> String {
    format!("{ARGS_CHANNEL_PREFIX}{call_id}")
}

/// 第一个占位符的 `(起始字节, 结束字节, 序号)`；解析语义与还原集合扫描共用父模块的实现。
fn find_first_placeholder(text: &str) -> Option<(usize, usize, u64)> {
    let characters: Vec<char> = text.chars().collect();
    let offsets = char_offsets(&characters);
    let mut index = 0;
    while index < characters.len() {
        if let Some((end, seq)) = match_placeholder(&characters, index) {
            return Some((offsets[index], offsets[end], seq));
        }
        index += 1;
    }
    None
}

/// 最后一个「可能成为占位符前缀」的开花括号位置；其后文本需继续挂起。
fn partial_start_index(buffer: &str) -> Option<usize> {
    let index = buffer
        .char_indices()
        .rev()
        .find(|(_, character)| *character == '{' || *character == FULLWIDTH_OPEN_BRACE)
        .map(|(offset, _)| offset)?;
    is_partial_prefix(&buffer[index..]).then_some(index)
}

/// 判断尾串是否可能是完整占位符的前缀（等待后续分片补全）。
fn is_partial_prefix(tail: &str) -> bool {
    let characters: Vec<char> = tail.chars().collect();
    let mut index = skip_whitespace(&characters, 1);
    let word: Vec<char> = MARKER_WORD.chars().collect();
    let mut matched = 0;
    while matched < word.len()
        && index < characters.len()
        && lower_equals(characters[index], word[matched])
    {
        index += 1;
        matched += 1;
    }
    if matched < word.len() {
        // 单词未匹配完：只有「已到串尾」才可能续接，否则永久无效。
        return index >= characters.len();
    }
    index = skip_whitespace(&characters, index);
    if index >= characters.len() {
        return true;
    }
    if !matches!(characters[index], ':' | '\u{ff1a}') {
        return false;
    }
    index = skip_whitespace(&characters, index + 1);
    while characters
        .get(index)
        .is_some_and(|character| character.is_ascii_digit())
    {
        index += 1;
    }
    index = skip_whitespace(&characters, index);
    index >= characters.len()
}

/// 疑似占位符前缀的出现次数（`[｛{]\s*desensitized`，最左优先、互不重叠）。
fn count_placeholder_prefixes(text: &str) -> usize {
    let characters: Vec<char> = text.chars().collect();
    let mut count = 0;
    let mut index = 0;
    while index < characters.len() {
        if matches!(characters[index], '{' | FULLWIDTH_OPEN_BRACE) {
            let start = skip_whitespace(&characters, index + 1);
            if matches_marker(&characters, start) {
                count += 1;
                index = start + PLACEHOLDER_MARKER.len();
                continue;
            }
        }
        index += 1;
    }
    count
}

/// 单字符小写比较：Python 的 `str.lower()` 可能展开成多个字符，那种情况不算相等。
fn lower_equals(character: char, expected: char) -> bool {
    let mut lowered = character.to_lowercase();
    matches!(lowered.next(), Some(value) if value == expected && lowered.next().is_none())
}
