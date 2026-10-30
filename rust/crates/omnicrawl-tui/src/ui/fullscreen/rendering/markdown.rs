//! Markdown → 样式文本（对映 Python 侧 Rich `Markdown` 的渲染）。
//!
//! Python 用 `rich.markdown.Markdown`（RichMarkdown）把 AI 回复与思考内容渲染成带样式的
//! 行；Rust 侧用 `pulldown-cmark` 解析 CommonMark 事件流，再映射到与本项目同族的样式串。
//! Rich 的 Markdown 主题细节（各段的精确色值、标题色相）不复刻，取色沿用主题令牌，
//! 差异记录在 crate README。

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthChar;

use super::highlight::{highlight_line, ScanState};
use crate::ui::fullscreen::terminal::theme::REASONING_BACKGROUND;
use crate::ui::fullscreen::text::StyledText;

/// 行内代码：青色前景 + 代码块同款灰底（Rich 默认 `markdown.code` 为青底黑字同族）。
fn inline_code_style() -> String {
    format!("cyan on {REASONING_BACKGROUND}")
}

/// 代码块：整块灰色背景（与思考区视觉一致）。
fn code_block_style() -> String {
    format!("on {REASONING_BACKGROUND}")
}

/// 有序列表序号宽度对齐用不到补白，直接 `N. ` 前缀；无序用 `• `。
const BULLET: &str = "• ";
const LIST_INDENT: &str = "  ";
/// 分隔线字形：知道可用列宽时铺满整行，不知道时给固定 16 列。
const RULE_GLYPH: char = '─';
const RULE_WIDTH: usize = 16;
const RULE_MIN_WIDTH: usize = 8;
const RULE_MAX_WIDTH: usize = 200;
const QUOTE_PREFIX: &str = "│ ";
/// 表格列间竖线；表头下面铺一行横线。
const TABLE_SEPARATOR: &str = " │ ";
const TABLE_JOINT: &str = "─┼─";
/// 单列最小宽度：收紧后仍要能看清是哪一列。
const TABLE_MIN_COLUMN: usize = 5;
/// 表格收紧的迭代上限：每轮按最宽列均摊，正常一两次就收敛。
const TABLE_SHRINK_ROUNDS: usize = 32;
const TASK_DONE: &str = "☑ ";
const TASK_TODO: &str = "☐ ";

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 宽字符（CJK/全角）：两侧都是宽字符时段内换行不插空格（与 GitHub、富文本排版一致）。
fn is_wide(ch: char) -> bool {
    char_width(ch) >= 2
}

/// 分隔线：`width` 为可用列宽，`usize::MAX` 表示宽度未知。
fn rule_line(width: usize) -> String {
    let columns = match width {
        usize::MAX => RULE_WIDTH,
        columns => columns.clamp(RULE_MIN_WIDTH, RULE_MAX_WIDTH),
    };
    RULE_GLYPH.to_string().repeat(columns)
}

/// 行首结构前缀的单个词元：引用竖线、无序圆点、任务框、有序序号（`12. `）。
fn structure_token(text: &str) -> Option<(&str, &str)> {
    for token in [QUOTE_PREFIX, BULLET, TASK_DONE, TASK_TODO] {
        if let Some(tail) = text.strip_prefix(token) {
            return Some((token, tail));
        }
    }
    let digits = text.len() - text.trim_start_matches(|ch: char| ch.is_ascii_digit()).len();
    if digits > 0 {
        if let Some(tail) = text[digits..].strip_prefix(". ") {
            return Some((&text[..digits + 2], tail));
        }
    }
    None
}

/// 一行的续行缩进列数（见 [`continuation_prefix`]）。
pub fn hanging_indent(text: &str) -> usize {
    text_width(&continuation_prefix(text))
}

/// 渲染后的一行是否是代码块正文（整行带灰底、且不是行内代码）。
///
/// 代码里的 `• ` 或 `1. ` 是内容，不是列表标记，不能据此给它加悬挂缩进。
/// 行内代码的样式也是「青字 + 同一块灰底」，所以必须排除只有 `cyan` 前景的那一种，
/// 否则「以行内代码开头」的列表项会被误判成代码行而丢掉悬挂缩进。
pub fn is_code_line(line: &StyledText) -> bool {
    line.spans().first().is_some_and(|span| {
        span.style.contains(REASONING_BACKGROUND) && !span.style.contains("cyan")
    })
}

/// 续行前缀：引用竖线原样保留，列表标记与序号换成等宽空白。
///
/// 折行后的续行必须停在同一缩进列上，否则引用的竖线与列表的悬挂缩进会在折行处断掉。
pub fn continuation_prefix(text: &str) -> String {
    let mut prefix = String::new();
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        prefix.push_str(&rest[..rest.len() - trimmed.len()]);
        let Some((token, tail)) = structure_token(trimmed) else {
            break;
        };
        if token == QUOTE_PREFIX {
            prefix.push_str(QUOTE_PREFIX);
        } else {
            prefix.push_str(&" ".repeat(text_width(token)));
        }
        rest = tail;
    }
    prefix
}

/// 一级标题最重（加下划线），二级只加粗，三级及以下再加暗色，形成可读的层级差。
fn heading_style(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "bold underline",
        HeadingLevel::H2 => "bold",
        _ => "bold dim",
    }
}

/// 代码块：整块灰底，已知语言另外逐 token 着色（见 [`super::highlight`]）。
///
/// `scan` 在同一个代码块内跨行复用，用来承接块注释与三引号字符串。
fn push_code_block(
    rendered: &mut StyledText,
    text: &str,
    at_line_start: &mut bool,
    depth: usize,
    language: &str,
    scan: &mut ScanState,
) {
    let base = code_block_style();
    let indent = LIST_INDENT.repeat(depth);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            rendered.push("\n", "");
            *at_line_start = true;
        }
        if !line.is_empty() {
            if !indent.is_empty() {
                rendered.push(&indent, &base);
            }
            for (piece, style) in highlight_line(language, line, scan, &base) {
                rendered.push(&piece, &style);
            }
            *at_line_start = false;
        }
    }
}

/// 把 Markdown 渲染成样式文本；换行只表达逻辑行，折行交给渲染层。
pub fn render_markdown(markdown: &str) -> StyledText {
    render_markdown_with_width(markdown, usize::MAX)
}

/// 同上，但知道可用列宽：分隔线按它铺满、表格按它分配列宽。
///
/// `width` 传 `usize::MAX` 表示宽度未知（分隔线给固定宽度、表格只按内容撑开）。
pub fn render_markdown_with_width(markdown: &str, width: usize) -> StyledText {
    let mut renderer = Renderer {
        width,
        ..Renderer::default()
    };
    renderer.run(markdown);
    renderer.finish()
}

/// 渲染器：跨事件累积的修饰符栈、列表栈、引用深度与表格缓冲。
struct Renderer {
    rendered: StyledText,
    modifiers: Vec<&'static str>,
    lists: Vec<Option<u64>>,
    counters: Vec<u64>,
    link_depth: usize,
    in_code_block: bool,
    code_language: String,
    code_scan: ScanState,
    pending_prefix: Option<String>,
    quote_depth: usize,
    at_line_start: bool,
    /// 段内换行待定：是否补空格要看它的前后字符（见 [`Renderer::flush_soft_break`]）。
    pending_soft_break: bool,
    width: usize,
    table: Option<TableState>,
}

/// 表格累积：行 → 单元格 → 该单元格的样式文本。
#[derive(Default)]
struct TableState {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<StyledText>>,
    header_rows: usize,
    row: Vec<StyledText>,
    cell: Option<StyledText>,
    /// 表头单元格额外加粗。
    in_header: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            rendered: StyledText::new(),
            modifiers: Vec::new(),
            lists: Vec::new(),
            counters: Vec::new(),
            link_depth: 0,
            in_code_block: false,
            code_language: String::new(),
            code_scan: ScanState::default(),
            pending_prefix: None,
            quote_depth: 0,
            at_line_start: true,
            pending_soft_break: false,
            width: usize::MAX,
            table: None,
        }
    }
}

impl Renderer {
    fn run(&mut self, markdown: &str) {
        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TASKLISTS);
        options.insert(Options::ENABLE_TABLES);

        for event in Parser::new_ext(markdown, options) {
            self.event(event);
        }
    }

    fn finish(mut self) -> StyledText {
        self.push_newline();
        self.rendered
    }

    /// 当前写入目标：表格单元格内写单元格，其余写整段输出。
    fn target(&mut self) -> &mut StyledText {
        match self.table.as_mut().and_then(|table| table.cell.as_mut()) {
            Some(cell) => cell,
            None => &mut self.rendered,
        }
    }

    /// 是否正在写表格单元格（单元格里没有引用竖线与软换行的位置）。
    fn in_cell(&self) -> bool {
        self.table.as_ref().is_some_and(|table| table.cell.is_some())
    }

    /// 结构块之间收行：同一行的内容不再重复换行。
    ///
    /// 表格单元格里没有「行」的概念（列宽由 [`render_table`] 统一排），直接跳过。
    fn push_newline(&mut self) {
        self.pending_soft_break = false;
        if self.at_line_start || self.in_cell() {
            return;
        }
        self.rendered.push("\n", "");
        self.at_line_start = true;
    }

    /// 空行：段落与块级元素之间留一行。
    fn push_blank_line(&mut self) {
        self.pending_soft_break = false;
        if self.in_cell() {
            return;
        }
        self.rendered.push("\n", "");
        self.at_line_start = true;
    }

    fn style(&self) -> String {
        let mut parts = self.modifiers.clone();
        if self.link_depth > 0 {
            parts.push("underline");
            parts.push("blue");
        }
        parts.join(" ")
    }

    /// 已输出正文的最后一个字符（判断软换行两侧是否为宽字符）。
    fn last_char(&self) -> Option<char> {
        let spans = match self.table.as_ref().and_then(|table| table.cell.as_ref()) {
            Some(cell) => cell.spans(),
            None => self.rendered.spans(),
        };
        spans.iter().rev().find_map(|span| span.text.chars().next_back())
    }

    /// 段内换行落定：两侧都是宽字符时不留空格，否则补一个空格。
    fn flush_soft_break(&mut self, next: Option<char>) {
        if !self.pending_soft_break {
            return;
        }
        self.pending_soft_break = false;
        let keep = match (self.last_char(), next) {
            (Some(left), Some(right)) => !(is_wide(left) && is_wide(right)),
            _ => true,
        };
        if keep {
            self.target().push(" ", "");
        }
    }

    /// 正文落盘：先补行首结构前缀（列表圆点/序号/任务框），表头单元格再加粗。
    fn push_text(&mut self, text: &str, style: &str) {
        if let Some(prefix) = self.pending_prefix.take() {
            let prefix_style = if self.table.is_some() { "" } else { style };
            self.target().push(&prefix, prefix_style);
        }
        // 非表格路径保持借用，不为每段正文多分配一个样式串。
        if self.table.as_ref().is_some_and(|table| table.in_header) {
            let header = format!("{style} bold").trim().to_string();
            self.target().push(text, &header);
        } else {
            self.target().push(text, style);
        }
        self.at_line_start = false;
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    let depth = self.quote_depth;
                    push_code_block(
                        &mut self.rendered,
                        &text,
                        &mut self.at_line_start,
                        depth,
                        &self.code_language,
                        &mut self.code_scan,
                    );
                    return;
                }
                self.flush_soft_break(text.chars().next());
                let style = self.style();
                if self.quote_depth > 0 && self.at_line_start && !self.in_cell() {
                    self.rendered
                        .push(&QUOTE_PREFIX.repeat(self.quote_depth), "dim");
                }
                self.push_text(&text, &style);
            }
            Event::Code(text) => {
                self.flush_soft_break(text.chars().next());
                self.push_text(&text, &inline_code_style());
            }
            Event::TaskListMarker(checked) => {
                let indent = LIST_INDENT.repeat(self.lists.len().saturating_sub(1));
                self.pending_prefix = Some(format!(
                    "{indent}{}",
                    if checked { TASK_DONE } else { TASK_TODO }
                ));
            }
            // CommonMark 把段内单换行当可折叠空白；是否补空格留到下一个字符到达时判定。
            Event::SoftBreak => self.pending_soft_break = true,
            Event::HardBreak => self.push_newline(),
            Event::Rule => {
                self.push_newline();
                let rule = rule_line(self.width);
                self.rendered.push(&rule, "dim");
                self.push_blank_line();
            }
            // 块级 HTML（`<div>`、`<br>` 之类）自成一块；行内 HTML 只当弱化文本。
            Event::Html(text) => {
                self.push_newline();
                self.push_text(text.trim_end(), "dim");
                self.push_newline();
            }
            Event::InlineHtml(text) => {
                self.flush_soft_break(text.chars().next());
                self.push_text(&text, "dim");
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.push_newline(),
            Tag::Heading { level, .. } => {
                self.push_newline();
                self.modifiers.push(heading_style(level));
            }
            Tag::CodeBlock(kind) => {
                self.push_newline();
                // 列表项里直接放代码块时没有正文事件来消费行首前缀，先把它铺出去。
                if let Some(prefix) = self.pending_prefix.take() {
                    let style = self.style();
                    self.rendered.push(&prefix, &style);
                }
                self.in_code_block = true;
                self.code_language = fence_language(&kind);
                self.code_scan = ScanState::default();
            }
            Tag::List(start) => {
                self.push_newline();
                self.lists.push(start);
                self.counters.push(start.unwrap_or(1));
            }
            Tag::Item => {
                self.push_newline();
                let indent = LIST_INDENT.repeat(self.lists.len().saturating_sub(1));
                let prefix = match self.lists.last() {
                    Some(Some(_)) => {
                        let number = self.counters.last().copied().unwrap_or(1);
                        if let Some(counter) = self.counters.last_mut() {
                            *counter = number + 1;
                        }
                        format!("{indent}{number}. ")
                    }
                    _ => format!("{indent}{BULLET}"),
                };
                self.pending_prefix = Some(prefix);
            }
            Tag::BlockQuote(_) => {
                self.push_newline();
                self.quote_depth += 1;
            }
            Tag::Table(aligns) => {
                self.push_newline();
                self.table = Some(TableState {
                    aligns,
                    ..TableState::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell = Some(StyledText::new());
                }
            }
            Tag::Emphasis => self.modifiers.push("italic"),
            Tag::Strong => self.modifiers.push("bold"),
            Tag::Strikethrough => self.modifiers.push("strike"),
            Tag::Link { .. } => self.link_depth += 1,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.push_blank_line(),
            TagEnd::Heading(_) => {
                self.push_newline();
                self.push_blank_line();
                self.modifiers.pop();
            }
            TagEnd::CodeBlock => {
                self.push_newline();
                self.push_blank_line();
                self.in_code_block = false;
            }
            TagEnd::Item => self.push_newline(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.counters.pop();
                if self.lists.is_empty() {
                    self.push_blank_line();
                }
            }
            TagEnd::BlockQuote(_) => {
                self.push_newline();
                self.push_blank_line();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let block = render_table(&table, self.width);
                    self.rendered.append_text(&block);
                    self.at_line_start = false;
                    self.push_newline();
                    self.push_blank_line();
                }
            }
            TagEnd::TableHead => {
                // 表头行本身就是 `TableHead` 节点（pulldown-cmark 把解析出的表格行直接改名为
                // `TableHead`，不再额外发 `TableRow`），因此在这里收行并记下表头行数。
                if let Some(table) = self.table.as_mut() {
                    if !table.row.is_empty() {
                        let row = std::mem::take(&mut table.row);
                        table.rows.push(row);
                    }
                    table.header_rows = table.rows.len();
                    table.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    if !table.row.is_empty() {
                        let row = std::mem::take(&mut table.row);
                        table.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    if let Some(cell) = table.cell.take() {
                        table.row.push(cell);
                    }
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.modifiers.pop();
            }
            TagEnd::Link => self.link_depth = self.link_depth.saturating_sub(1),
            _ => {}
        }
    }
}

/// 围栏信息串 → 语言名：只看首个词元（```rust,ignore / ```sh 都取第一段）。
fn fence_language(kind: &CodeBlockKind<'_>) -> String {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split(|ch: char| ch.is_whitespace() || ch == ',')
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        CodeBlockKind::Indented => String::new(),
    }
}

/// 表格：按内容定列宽，知道窗口宽度时按比例收紧，超宽单元格以 `…` 收尾。
///
/// 表格骨架（`│` 与表头下划线）用弱化色，单元格内容保留自己的样式（表头加粗）。
fn render_table(table: &TableState, width: usize) -> StyledText {
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut rendered = StyledText::new();
    if columns == 0 {
        return rendered;
    }
    let mut widths = vec![1usize; columns];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            if index < columns {
                widths[index] = widths[index].max(cell.display_width());
            }
        }
    }
    if width != usize::MAX {
        let separators = text_width(TABLE_SEPARATOR) * columns.saturating_sub(1);
        let budget = width
            .saturating_sub(separators)
            .max(columns * TABLE_MIN_COLUMN);
        shrink_columns(&mut widths, budget);
    }
    for (row_index, row) in table.rows.iter().enumerate() {
        if row_index > 0 {
            rendered.push("\n", "");
        }
        for (index, column) in widths.iter().enumerate() {
            if index > 0 {
                rendered.push(TABLE_SEPARATOR, "dim");
            }
            push_cell(
                &mut rendered,
                row.get(index),
                *column,
                table.aligns.get(index).copied(),
            );
        }
        // 表头下面铺一行横线，把表头与数据行分开。
        if row_index + 1 == table.header_rows {
            rendered.push("\n", "");
            for (index, column) in widths.iter().enumerate() {
                if index > 0 {
                    rendered.push(TABLE_JOINT, "dim");
                }
                rendered.push(&RULE_GLYPH.to_string().repeat(*column), "dim");
            }
        }
    }
    rendered
}

/// 列宽收紧：每轮把超出预算的部分均摊到还能再窄的列上，收敛一次就停。
fn shrink_columns(widths: &mut [usize], budget: usize) {
    for _ in 0..TABLE_SHRINK_ROUNDS {
        let total: usize = widths.iter().sum();
        if total <= budget {
            return;
        }
        let shrinkable: Vec<usize> = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > TABLE_MIN_COLUMN)
            .map(|(index, _)| index)
            .collect();
        if shrinkable.is_empty() {
            return;
        }
        let excess = total - budget;
        let each = (excess / shrinkable.len()).max(1);
        for index in shrinkable {
            let next = widths[index].saturating_sub(each).max(TABLE_MIN_COLUMN);
            widths[index] = next;
        }
    }
}

/// 一个单元格：按列宽截断并补齐到整列，对齐方式取自分隔行。
fn push_cell(
    rendered: &mut StyledText,
    cell: Option<&StyledText>,
    column: usize,
    align: Option<Alignment>,
) {
    let used = cell.map(StyledText::display_width).unwrap_or(0);
    let (left, right) = match align {
        Some(Alignment::Right) => (column.saturating_sub(used), 0),
        Some(Alignment::Center) => {
            let pad = column.saturating_sub(used);
            (pad / 2, pad - pad / 2)
        }
        _ => (0, column.saturating_sub(used)),
    };
    if left > 0 {
        rendered.push(&" ".repeat(left), "");
    }
    if let Some(cell) = cell {
        // 左右补白已经占掉一部分列宽，正文只剩中间这段；超宽时由 `push_clipped` 补 `…`。
        push_clipped(rendered, cell, column.saturating_sub(left + right));
    }
    if right > 0 {
        rendered.push(&" ".repeat(right), "");
    }
}

/// 单元格内容压进 `budget` 列：超宽时留一格给 `…`，避免整行被列宽撑破。
fn push_clipped(rendered: &mut StyledText, cell: &StyledText, budget: usize) {
    let limit = if cell.display_width() > budget {
        budget.saturating_sub(1)
    } else {
        budget
    };
    let mut used = 0usize;
    let mut truncated = false;
    for span in cell.spans() {
        let style = if span.style.is_empty() {
            cell.base_style()
        } else {
            span.style.as_str()
        };
        let mut chunk = String::new();
        for ch in span.text.chars() {
            let cells = char_width(ch);
            if used + cells > limit {
                truncated = true;
                break;
            }
            used += cells;
            chunk.push(ch);
        }
        if !chunk.is_empty() {
            rendered.push(&chunk, style);
        }
        if truncated {
            break;
        }
    }
    if truncated {
        rendered.push("…", "dim");
    }
}

/// 对映 `_UniformGrayMarkdown`：保留 Markdown 结构，把全部前景统一为思考区灰阶。
pub fn uniform_gray(markdown: &str) -> StyledText {
    uniform_gray_with_width(markdown, usize::MAX)
}

/// 同上，但知道可用列宽（分隔线与表格按它排版）。
pub fn uniform_gray_with_width(markdown: &str, width: usize) -> StyledText {
    let body = render_markdown_with_width(markdown, width);
    let mut rendered = StyledText::new();
    for span in body.spans() {
        if span.text.is_empty() {
            continue;
        }
        let style = if span.style.is_empty() {
            "bright_black".to_string()
        } else {
            format!("{} bright_black", span.style)
        };
        rendered.push(&span.text, &style);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paragraph_becomes_one_line() {
        let rendered = render_markdown("你好，世界");
        assert_eq!(rendered.plain().trim_end(), "你好，世界");
    }

    #[test]
    fn heading_is_bold_with_blank_line() {
        let rendered = render_markdown("# 标题");
        assert_eq!(rendered.plain(), "标题\n\n");
        assert!(rendered.spans()[0].style.contains("bold"));
    }

    #[test]
    fn heading_levels_are_distinguishable() {
        // 一级标题带下划线，二级只加粗，三级再加暗色：层级在终端里要看得出来。
        let h1 = render_markdown("# 一");
        let h2 = render_markdown("## 二");
        let h3 = render_markdown("### 三");
        assert!(h1.spans()[0].style.contains("underline"));
        assert!(!h2.spans()[0].style.contains("underline"));
        assert!(h2.spans()[0].style.contains("bold"));
        assert!(h3.spans()[0].style.contains("dim"));
    }

    #[test]
    fn strong_and_emphasis_carry_modifiers() {
        let rendered = render_markdown("普通 **粗** 与 *斜* 结束");
        assert_eq!(rendered.plain().trim_end(), "普通 粗 与 斜 结束");
        let styles: Vec<&str> = rendered
            .spans()
            .iter()
            .map(|span| span.style.as_str())
            .collect();
        assert!(styles.contains(&"bold"));
        assert!(styles.contains(&"italic"));
    }

    #[test]
    fn inline_code_uses_code_background() {
        let rendered = render_markdown("调用 `kb_search` 工具");
        assert!(rendered.plain().contains("kb_search"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style == inline_code_style()));
    }

    #[test]
    fn fenced_code_block_keeps_lines_with_background() {
        let rendered = render_markdown("```rust\nlet a = 1;\nlet b = 2;\n```");
        let plain = rendered.plain();
        assert!(plain.contains("let a = 1;"));
        assert!(plain.contains("let b = 2;"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style == code_block_style()));
    }

    #[test]
    fn fenced_code_block_highlights_tokens_by_language() {
        let rendered = render_markdown("```rust\nlet a = 1; // 注释\n```");
        let styles: Vec<&str> = rendered
            .spans()
            .iter()
            .map(|span| span.style.as_str())
            .collect();
        assert!(
            styles.iter().any(|style| style.contains("#66d9ef")),
            "关键字应当着色"
        );
        assert!(
            styles.iter().any(|style| style.contains("#ae81ff")),
            "数字应当着色"
        );
        assert!(
            styles.iter().any(|style| style.contains("#75715e")),
            "注释应当着色"
        );
    }

    #[test]
    fn unknown_fence_language_keeps_the_plain_code_style() {
        let rendered = render_markdown("```\nlet a = 1;\n```");
        let code = rendered
            .spans()
            .iter()
            .find(|span| span.text.contains("let a = 1;"))
            .expect("代码行应当被渲染");
        assert_eq!(
            code.style,
            code_block_style(),
            "无语言信息时不应逐 token 着色"
        );
    }

    #[test]
    fn python_fence_keeps_triple_quote_state_across_lines() {
        let rendered = render_markdown("```python\ntext = \"\"\"第一行\n第二行\"\"\"\n```");
        let second = rendered
            .spans()
            .iter()
            .find(|span| span.text.contains("第二行"))
            .expect("第二行应当被渲染");
        assert!(
            second.style.contains("#e6db74"),
            "三引号状态应当跨行保留，第二行整行是字符串色"
        );
    }

    #[test]
    fn bullet_and_ordered_lists_get_prefixes() {
        let bullet = render_markdown("- 第一项\n- 第二项");
        assert!(bullet.plain().contains("• 第一项"));
        assert!(bullet.plain().contains("• 第二项"));

        let ordered = render_markdown("1. 甲\n2. 乙");
        assert!(ordered.plain().contains("1. 甲"));
        assert!(ordered.plain().contains("2. 乙"));
    }

    #[test]
    fn nested_list_indents_by_depth() {
        let rendered = render_markdown("- 外层\n  - 内层");
        assert!(rendered.plain().contains("• 外层"));
        assert!(rendered.plain().contains("  • 内层"));
    }

    #[test]
    fn task_list_markers_use_checkbox_glyphs() {
        let rendered = render_markdown("- [x] 完成项\n- [ ] 未完成项");
        let plain = rendered.plain();
        assert!(plain.contains("☑ 完成项"));
        assert!(plain.contains("☐ 未完成项"));
    }

    #[test]
    fn soft_break_between_cjk_keeps_no_extra_space() {
        // 中文排版里「甲\n乙」不该被拼成「甲 乙」：多出来的词距在窄屏上很显眼。
        let soft = render_markdown("第一行\n第二行");
        assert_eq!(soft.plain().trim_end(), "第一行第二行");
        // 英文仍保留可折叠空白。
        let latin = render_markdown("first\nsecond");
        assert_eq!(latin.plain().trim_end(), "first second");
        // 中英混排保留英文那一侧的空格。
        let mixed = render_markdown("中文\nlatin");
        assert_eq!(mixed.plain().trim_end(), "中文 latin");
        let hard = render_markdown("第一行  \n第二行");
        assert_eq!(hard.plain().trim_end(), "第一行\n第二行");
    }

    #[test]
    fn links_are_underlined_and_blue() {
        let rendered = render_markdown("见 [文档](https://example.com)");
        assert!(rendered.plain().contains("文档"));
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style.contains("underline") && span.style.contains("blue")));
    }

    #[test]
    fn rule_renders_a_horizontal_line() {
        let rendered = render_markdown("上面\n\n---\n\n下面");
        assert!(rendered.plain().contains(&RULE_GLYPH.to_string().repeat(RULE_WIDTH)));
        assert!(rendered.plain().contains("上面"));
        assert!(rendered.plain().contains("下面"));
    }

    #[test]
    fn rule_fills_the_available_width() {
        // 分隔线要跟着窗口走，固定 16 列在宽窗口里看着像断了一截。
        let narrow = render_markdown_with_width("---", 20);
        let wide = render_markdown_with_width("---", 60);
        let line_of = |text: &StyledText| {
            text.plain()
                .lines()
                .find(|line| line.starts_with(RULE_GLYPH))
                .map(|line| line.chars().count())
                .unwrap_or(0)
        };
        assert_eq!(line_of(&narrow), 20);
        assert_eq!(line_of(&wide), 60);
        // 极窄时不低于下限，极宽时封顶。
        assert_eq!(line_of(&render_markdown_with_width("---", 3)), RULE_MIN_WIDTH);
        assert_eq!(
            line_of(&render_markdown_with_width("---", 9999)),
            RULE_MAX_WIDTH
        );
    }

    #[test]
    fn blockquote_lines_carry_prefix() {
        let rendered = render_markdown("> 引用内容");
        assert!(rendered.plain().contains("│ 引用内容"));
    }

    #[test]
    fn blockquote_second_paragraph_keeps_the_bar() {
        // 引用里换段的第二个段落也要带竖线，否则看起来像引用已经结束。
        let rendered = render_markdown("> 第一段\n>\n> 第二段");
        let plain = rendered.plain();
        assert!(plain.contains("│ 第一段"), "{plain:?}");
        assert!(plain.contains("│ 第二段"), "{plain:?}");
    }

    #[test]
    fn tables_render_as_aligned_columns() {
        let rendered = render_markdown("| 名称 | 值 |\n| --- | --- |\n| a | 1 |\n| bb | 22 |");
        let plain = rendered.plain();
        let lines: Vec<&str> = plain.lines().filter(|line| !line.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "表头 + 分隔 + 两行数据：{plain:?}");
        assert!(lines[0].contains("名称"), "{plain:?}");
        assert!(lines[0].contains("值"), "{plain:?}");
        assert!(lines[1].contains('─'), "表头下要有横线：{plain:?}");
        // 同一列的内容要对齐到相同的起始列。
        let column_of = |line: &str| line.find('│').map(|index| index);
        assert_eq!(
            column_of(lines[2]),
            column_of(lines[3]),
            "数据行的列位置应当一致：{plain:?}"
        );
        assert!(lines[2].contains('a') && lines[2].contains('1'), "{plain:?}");
    }

    #[test]
    fn table_header_is_bold_and_body_is_not() {
        let rendered = render_markdown("| h |\n| --- |\n| b |");
        let header = rendered
            .spans()
            .iter()
            .find(|span| span.text.contains('h'))
            .expect("表头应被渲染");
        assert!(header.style.contains("bold"), "表头要加粗：{header:?}");
        let body = rendered
            .spans()
            .iter()
            .find(|span| span.text.contains('b'))
            .expect("数据行应被渲染");
        assert!(!body.style.contains("bold"), "数据行不加粗：{body:?}");
    }

    #[test]
    fn table_shrinks_to_the_window_and_clips_wide_cells() {
        let markdown = "| 项目 | 说明 |\n| --- | --- |\n| 甲 | 一段非常长的说明文字 |";
        let wide = render_markdown_with_width(markdown, 80);
        let narrow = render_markdown_with_width(markdown, 24);
        let widest = |text: &StyledText| {
            text.plain()
                .lines()
                .map(text_width)
                .max()
                .unwrap_or(0)
        };
        assert!(widest(&wide) <= 80);
        assert!(widest(&narrow) <= 24, "窄窗口下表格要收进来");
        assert!(
            narrow.plain().contains('…'),
            "超宽单元格要截断：{:?}",
            narrow.plain()
        );
        // 收进来之后各行列数必须一致，否则列关系就断了（末尾换行切出的空行不算表格行）。
        let widths: Vec<usize> = narrow
            .plain()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(text_width)
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "各行应等宽：{widths:?}"
        );
    }

    #[test]
    fn table_columns_are_padded_to_equal_width() {
        // 同列内容长短不同也要补齐成同一列宽，否则竖线会歪。
        let rendered = render_markdown("| a | b |\n| --- | --- |\n| 甲 | 1 |\n| 乙乙 | 22 |");
        let rows: Vec<String> = rendered
            .plain()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        assert!(rows.len() >= 4, "{rows:?}");
        // 竖线所在列按**显示列**算，不能按字节偏移（中文是多字节）。
        let separator_column = |row: &str| {
            row.chars()
                .take_while(|ch| *ch != '│')
                .map(char_width)
                .sum::<usize>()
        };
        let columns: Vec<usize> = rows
            .iter()
            .filter(|row| row.contains('│'))
            .map(|row| separator_column(row))
            .collect();
        assert!(columns.len() >= 3, "{rows:?}");
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "竖线应逐行对齐在同一列：{rows:?} -> {columns:?}"
        );
        // 各行显示宽度也必须一致（补齐到整列）。
        let widths: Vec<usize> = rows.iter().map(|row| text_width(row)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "各行应等宽：{rows:?} -> {widths:?}"
        );
    }

    #[test]
    fn table_respects_column_alignment() {
        // 左对齐列的补白在右、右对齐列的补白在左：同一行里能看出两列朝向不同。
        let rendered = render_markdown("| h1 | h2 |\n| :-- | --: |\n| a | b |");
        let plain = rendered.plain();
        let row = plain
            .lines()
            .find(|line| line.starts_with('a'))
            .expect("数据行应被渲染");
        assert!(row.starts_with('a'), "左对齐列内容贴左：{row:?}");
        assert!(row.ends_with('b'), "右对齐列内容贴右：{row:?}");
    }

    #[test]
    fn bullet_lines_report_a_continuation_prefix() {
        // 折行后的续行要停在同一缩进列，圆点换成等宽空白、引用竖线照抄。
        assert_eq!(continuation_prefix("• 正文"), "  ");
        assert_eq!(continuation_prefix("12. 正文"), "    ");
        assert_eq!(continuation_prefix("│ • 正文"), "│   ");
        assert_eq!(continuation_prefix("普通正文"), "");
    }

    #[test]
    fn code_lines_are_not_mistaken_for_list_items() {
        // 代码内容常常以 `• ` 或 `1. ` 开头，它们是内容而不是列表标记。
        let listed = render_markdown("- 真列表项");
        let line = listed.split_lines()[0].clone();
        assert!(!is_code_line(&line), "列表项不是代码行");
        let code = render_markdown("```\n• bullet\n1. numbered\n```");
        let code_lines: Vec<StyledText> = code
            .split_lines()
            .into_iter()
            .filter(|line| !line.plain().trim().is_empty())
            .collect();
        assert!(
            code_lines.iter().all(is_code_line),
            "代码块每行都应识别为代码行：{code_lines:?}"
        );
        // 代码行的 `• `/`1. ` 是内容：调用方靠 is_code_line 跳过悬挂缩进，两者必须配合。
        let prefixes: Vec<String> = code_lines
            .iter()
            .filter(|line| !is_code_line(line))
            .map(|line| continuation_prefix(&line.plain()))
            .collect();
        assert!(
            prefixes.is_empty(),
            "代码行已被 is_code_line 排除，不应再算结构前缀：{prefixes:?}"
        );
        // 行内代码与代码块共用同一块灰底，但它是正文，不能算代码行（否则列表项会丢悬挂缩进）。
        let inline = render_markdown("`kb_search` 后面还有正文");
        let inline_line = inline.split_lines()[0].clone();
        assert!(!is_code_line(&inline_line), "行内代码不是代码块行");
    }

    #[test]
    fn code_block_inside_a_list_item_keeps_its_bullet() {
        // 列表项里直接放代码块时没有正文事件消费行首前缀，圆点不能被吞掉。
        let rendered = render_markdown("- 说明：\n\n  ```rust\n  let a = 1;\n  ```");
        let plain = rendered.plain();
        assert!(plain.contains("• 说明"), "{plain:?}");
        assert!(plain.contains("let a = 1;"), "{plain:?}");
    }

    #[test]
    fn uniform_gray_overrides_foreground_but_keeps_structure() {
        let rendered = uniform_gray("# 标题\n\n正文");
        assert!(rendered.plain().contains("标题"));
        assert!(rendered.plain().contains("正文"));
        assert!(rendered
            .spans()
            .iter()
            .all(|span| span.style.contains("bright_black")));
        // 结构样式（粗体）保留，只是叠加灰阶前景。
        assert!(rendered
            .spans()
            .iter()
            .any(|span| span.style.contains("bold")));
    }

    #[test]
    fn empty_markdown_renders_nothing() {
        assert_eq!(render_markdown("").plain(), "");
        assert_eq!(uniform_gray("").plain(), "");
    }
}
