//! Rich `Text` 的最小对映：分段样式文本与按字符的样式视图。
//!
//! Python 侧 HUD、轮播与消息流用 `rich.text.Text` 逐段拼装（`append` /
//! `append_text`，样式是 Rich 风格串），再交给 Textual 渲染。Rust 侧用同形状的
//! [`StyledText`] 承载，样式串保持原样，渲染时由
//! [`crate::ui::fullscreen::terminal::theme::rich_style`] 解析成 ratatui 样式。

use ratatui::text::Span;

use crate::ui::fullscreen::terminal::theme::rich_style;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    /// Rich 风格串；空串表示继承所在文本的基样式。
    pub style: String,
}

/// 对映 `rich.text.Text`：基样式 + 依次追加的分段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledText {
    spans: Vec<StyledSpan>,
    base_style: String,
}

impl StyledText {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对映 `Text(text, style=...)`。
    pub fn styled(text: &str, style: &str) -> Self {
        let mut rendered = Self::new();
        rendered.push(text, style);
        rendered
    }

    /// 对映 `Text(style=...)`：空内容、只有基样式。
    pub fn with_base_style(style: &str) -> Self {
        Self {
            spans: Vec::new(),
            base_style: style.to_string(),
        }
    }

    pub fn base_style(&self) -> &str {
        &self.base_style
    }

    /// 对映 `Text.append(text, style=...)`。
    pub fn push(&mut self, text: &str, style: &str) {
        if text.is_empty() {
            return;
        }
        self.spans.push(StyledSpan {
            text: text.to_string(),
            style: style.to_string(),
        });
    }

    /// 对映 `Text.append_text(other)`：追加对方的分段，内容与样式原样保留。
    pub fn append_text(&mut self, other: &StyledText) {
        self.spans.extend(other.spans.iter().cloned());
    }

    /// 对映 `Text.plain`。
    pub fn plain(&self) -> String {
        let mut out = String::with_capacity(self.plain_bytes());
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    /// 全部段的字节数（拼整串前的容量预判，省掉反复扩容）。
    pub fn plain_bytes(&self) -> usize {
        self.spans.iter().map(|span| span.text.len()).sum()
    }

    /// 字符总数（不拼整串）。
    pub fn plain_chars(&self) -> usize {
        self.spans.iter().map(|span| span.text.chars().count()).sum()
    }

    pub fn spans(&self) -> &[StyledSpan] {
        &self.spans
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|span| span.text.is_empty())
    }

    /// 对映 `_text_styles`：每个字符位置的样式，分段样式优先、其余回落基样式。
    pub fn char_styles(&self) -> Vec<String> {
        let mut styles: Vec<String> = self
            .plain()
            .chars()
            .map(|_| self.base_style.clone())
            .collect();
        let mut offset = 0usize;
        for span in &self.spans {
            let length = span.text.chars().count();
            let style = if span.style.is_empty() {
                self.base_style.clone()
            } else {
                span.style.clone()
            };
            for index in offset..offset + length {
                if let Some(slot) = styles.get_mut(index) {
                    *slot = style.clone();
                }
            }
            offset += length;
        }
        styles
    }

    pub fn display_width(&self) -> usize {
        use unicode_width::UnicodeWidthChar;
        // 不经过 `plain()`：整段拼接一次字符串只为数字符宽度，纯属白拷贝。
        self.spans
            .iter()
            .flat_map(|span| span.text.chars())
            .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
            .sum()
    }

    /// 是否只有空白（对映 `not text.strip()`）：逐段判定，不拼整串。
    pub fn is_blank(&self) -> bool {
        self.spans
            .iter()
            .all(|span| span.text.chars().all(char::is_whitespace))
    }

    /// 对映 `Text.split("\n")`：按换行切分并保留各段样式。
    pub fn split_lines(&self) -> Vec<StyledText> {
        let mut lines: Vec<StyledText> = Vec::new();
        let mut current = StyledText::with_base_style(&self.base_style);
        for span in &self.spans {
            let mut parts = span.text.split('\n');
            if let Some(first) = parts.next() {
                if !first.is_empty() {
                    current.push(first, &span.style);
                }
            }
            for part in parts {
                lines.push(std::mem::take(&mut current));
                current = StyledText::with_base_style(&self.base_style);
                if !part.is_empty() {
                    current.push(part, &span.style);
                }
            }
        }
        lines.push(current);
        lines
    }

    /// 对映 `Text("\n").join(lines)`。
    pub fn join_lines(lines: &[StyledText], separator: &str) -> StyledText {
        let mut rendered = StyledText::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                rendered.push(separator, "");
            }
            rendered.append_text(line);
        }
        rendered
    }

    /// 转成 ratatui 分段：样式串逐段解析。
    pub fn to_spans(&self) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(self.spans.len());
        for span in &self.spans {
            let style = if span.style.is_empty() {
                rich_style(&self.base_style)
            } else {
                rich_style(&span.style)
            };
            spans.push(Span::styled(span.text.clone(), style));
        }
        spans
    }
}

/// 逐字符样式累积器：把连续同样式的字符合成一段再交给 [`StyledText`]。
///
/// 解密扫描类特效逐字决定样式（每个字可能是乱码、可能是原字），若每个字符都单独
/// `push`，一份文本会退化成上千个单字符段：之后的样式解析、折行与写屏都要陪着一起碎。
/// 这里把相邻同一样式的字符合成一段，输出段数与颜色段数同阶。
#[derive(Default)]
pub struct StyleRuns {
    text: StyledText,
    buffer: String,
    style: String,
}

impl StyleRuns {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_str(&mut self, chunk: &str, style: &str) {
        if chunk.is_empty() {
            return;
        }
        if self.buffer.is_empty() {
            self.style.clear();
            self.style.push_str(style);
        } else if self.style != style {
            self.flush();
            self.style.clear();
            self.style.push_str(style);
        }
        self.buffer.push_str(chunk);
    }

    pub fn push_char(&mut self, ch: char, style: &str) {
        let mut single = [0u8; 4];
        self.push_str(ch.encode_utf8(&mut single), style);
    }

    fn flush(&mut self) {
        if !self.buffer.is_empty() {
            let chunk = std::mem::take(&mut self.buffer);
            self.text.push(&chunk, &self.style);
            self.buffer = chunk;
            self.buffer.clear();
        }
    }

    pub fn finish(mut self) -> StyledText {
        self.flush();
        self.text
    }
}

/// 一段文本逐字符的样式（借用，不复制样式串）；空样式回落到基样式。
pub fn span_char_styles(text: &StyledText) -> Vec<&str> {
    let mut styles: Vec<&str> = Vec::with_capacity(text.plain_chars());
    for span in text.spans() {
        let style = if span.style.is_empty() {
            text.base_style()
        } else {
            span.style.as_str()
        };
        for _ in span.text.chars() {
            styles.push(style);
        }
    }
    styles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_appends_spans_and_plain_concatenates() {
        let mut rendered = StyledText::new();
        rendered.push("CTX ", "dim");
        rendered.push("1.2K", "default");
        assert_eq!(rendered.plain(), "CTX 1.2K");
        assert_eq!(rendered.spans().len(), 2);
        rendered.push("", "default");
        assert_eq!(rendered.spans().len(), 2);
        assert!(!rendered.is_empty());
        assert!(StyledText::new().is_empty());
    }

    #[test]
    fn append_text_keeps_other_spans() {
        let mut rendered = StyledText::new();
        rendered.push("⁕", "bright_black");
        rendered.append_text(&StyledText::styled("MDL", "default bold"));
        assert_eq!(rendered.plain(), "⁕MDL");
        assert_eq!(rendered.spans()[1].style, "default bold");
    }

    #[test]
    fn char_styles_fall_back_to_base_style() {
        let mut rendered = StyledText::new();
        rendered.base_style = "dim".to_string();
        rendered.push("ab", "default");
        rendered.push("cd", "");
        assert_eq!(
            rendered.char_styles(),
            vec!["default", "default", "dim", "dim"]
        );
    }

    #[test]
    fn display_width_counts_cjk_columns() {
        assert_eq!(StyledText::styled("中文ab", "default").display_width(), 6);
    }
}
