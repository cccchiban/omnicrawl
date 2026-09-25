//! 消息流：把记录渲染成显示行，并按滚动位置取出可见窗口。
//!
//! 版式对映 Python `#conversation` 与 `.message*` 系列：区内左右各 1 格内边距、
//! 右缘 1 格细滚动条；用户消息是灰色斜体 `user：` 标签行 + 显式白色正文 + 左侧青色
//! 竖条，思考段是暗底灰字斜体，工具卡无边框（`●` 状态点 + 缩进正文），运行状态行
//! 作为会话流里最后一条临时消息渲染（不再单占底部条带）。
//!
//! 记录先按真实列宽软折行，再进入窗口计算，因此终端自己的换行不会打乱滚动位置。
//!
//! 每行可带一个 [`LineHit`]：鼠标点击落在该行时由 [`hit_test`] 解析成工具卡展开 /
//! 收起动作（对映 Python `ToolDisclosure` 的提示行点击展开、展开态点击卡片收起）。
//! 点击目标的解析不能只看「第几条记录」——同一条记录会折成多行，因此命中信息跟着
//! 显示行一起产出。

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use super::{display_width, fit, truncate_styled, wrap_display};
use crate::state::{AppState, Record, ToolCard, ToolStatus};
use crate::ui::fullscreen::rendering::{latex, markdown, tool_diff};
use crate::ui::fullscreen::rendering::widgets::{self, SubAgentConversation, SubAgentProgressTree};
use crate::ui::fullscreen::terminal::theme;
use crate::ui::fullscreen::text::StyledText;
use crate::ui::panels;

/// 工具卡正文在缩略态与展开态的行数上限（对映 Python `MAX_EXPANDED_BODY_LINES`）。
const TOOL_BODY_LIMIT: usize = widgets::MAX_EXPANDED_BODY_LINES;
/// 助手正文的显示前缀（对映 Python 工作台给 `AssistantMessage` 加的 `◇ `）。
const ASSISTANT_PREFIX: &str = "◇ ";
/// 缩略态正文保留的首部 / 尾部有效行数（对映 `HEAD_BODY_LINES` / `TAIL_BODY_LINES`）。
const TOOL_HEAD_LINES: usize = widgets::HEAD_BODY_LINES;
const TOOL_TAIL_LINES: usize = widgets::TAIL_BODY_LINES;
/// 正文相对标题的缩进（对映 `BODY_INDENT`，叠加在消息内边距之上）。
const TOOL_BODY_INDENT: &str = widgets::BODY_INDENT;
/// 省略区提示行文案（对映 `EXPAND_HINT`）。
const TOOL_EXPAND_HINT: &str = widgets::EXPAND_HINT;
/// 思考段默认折叠时展示的最新行数（对映 `ReasoningDisclosure.COLLAPSED_HEIGHT`）。
const REASONING_TAIL: usize = 5;
/// 会话区内边距（对映 CSS `#conversation { padding: 0 1 }`）。
pub const CONVERSATION_PAD: u16 = 1;
/// 右缘滚动条宽度（对映 Textual 的 1 格细滚动条）。
pub const SCROLLBAR_WIDTH: u16 = 1;
/// 单条消息的内边距（对映 CSS `.message { padding: 0 1 }`）。
const MESSAGE_PAD: &str = " ";
/// 用户消息左侧竖条：一格填青色背景（对映 CSS `border-left: solid $terminal-cyan`）。
const USER_STRIPE: &str = " ";
/// 欢迎 Logo 的垂直下移行数（对映 CSS `#welcome-logo { offset: 0 4 }`）。
const LOGO_OFFSET_ROWS: usize = 4;

/// 一行显示行上的可点击目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineHit {
    /// 工具卡省略区的提示行：点击展开被省略的正文。
    ToolHint { call_id: String },
    /// 工具卡其余部分：处于展开态时点击收起。
    ToolCard { call_id: String },
    /// 思考段：点击在折叠与展开之间切换（`index` 是该记录在消息流里的下标）。
    Reasoning { index: usize },
    /// 运行状态行尾部的 `[ ESC ]`：点击等价于按键盘 Esc（取消当前回合 / 空闲时聚焦输入框）。
    RuntimeEsc,
}

/// 显示行 + 它承载的点击目标。
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayLine {
    pub line: Line<'static>,
    pub hit: Option<LineHit>,
}

impl DisplayLine {
    fn plain(line: Line<'static>) -> Self {
        Self { line, hit: None }
    }

    fn with_hit(line: Line<'static>, hit: LineHit) -> Self {
        Self {
            line,
            hit: Some(hit),
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let text = text_area(area);
    let lines = display_lines(state, text.width);
    let window = window(&lines, text.height as usize, state.scroll_from_bottom);
    frame.render_widget(Paragraph::new(window), text);
    render_scrollbar(
        frame,
        area,
        lines.len(),
        text.height as usize,
        state.scroll_from_bottom,
    );
}

/// 会话文本区：扣掉左右内边距与右缘滚动条格（对映 CSS `#conversation { padding: 0 1 }`
/// 与 Textual 的 1 格滚动条）。滚动条格始终预留，内容长短变化时不会导致折行跳动。
pub fn text_area(area: Rect) -> Rect {
    let pad = CONVERSATION_PAD.min(area.width);
    let reserved = pad.saturating_mul(2).saturating_add(SCROLLBAR_WIDTH);
    let width = area
        .width
        .saturating_sub(reserved)
        .min(area.width.saturating_sub(pad))
        .max(1);
    Rect {
        x: area.x + pad,
        width,
        ..area
    }
}

/// 右缘 1 格细滚动条（对映 `#conversation` 的 scrollbar 设置）：内容不溢出时不画。
fn render_scrollbar(
    frame: &mut Frame,
    area: Rect,
    total: usize,
    height: usize,
    scroll_from_bottom: usize,
) {
    if height == 0 || total <= height || area.width == 0 || area.height == 0 {
        return;
    }
    let column = Rect {
        x: area.x + area.width.saturating_sub(SCROLLBAR_WIDTH),
        width: SCROLLBAR_WIDTH.min(area.width),
        ..area
    };
    if column.width == 0 {
        return;
    }
    let max_offset = total - height;
    let offset = max_offset.saturating_sub(scroll_from_bottom);
    let mut scrollbar = ScrollbarState::new(max_offset).position(offset);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        // 轨道保持透明（对映 `scrollbar-background: $terminal-background`）。
        .track_symbol(None)
        .thumb_symbol("█")
        .style(theme::rich_style(theme::SCROLLBAR));
    frame.render_stateful_widget(bar, column, &mut scrollbar);
}

/// 全部记录的显示行（已按宽度折行）。
///
/// 没有任何记录时（空会话首屏）只展示欢迎 Logo，与 Python 侧 `#welcome-logo`
/// 在首条消息出现前可见、清空会话后重新出现的语义一致。
///
/// 末尾追加运行状态行（对映 `.message.runtime-status-message`）：它是会话流里的
/// 最后一条临时消息，因此会随消息一起滚动。
pub fn display_lines(state: &AppState, width: u16) -> Vec<DisplayLine> {
    if state.records.is_empty() {
        return welcome_logo_lines(state, width);
    }
    let width = width.max(1) as usize;
    let mut lines: Vec<DisplayLine> = Vec::new();
    for (index, record) in state.records.iter().enumerate() {
        // 「思考显示」关闭时思考段整段不出现（连它上面那行空行也不占位）。
        if matches!(record, Record::Reasoning(_)) && !state.show_thinking {
            continue;
        }
        lines.push(DisplayLine::plain(Line::raw("")));
        match record {
            Record::User(text) => push_user(&mut lines, text, width),
            Record::Assistant(text) => push_assistant(&mut lines, text, width),
            Record::Reasoning(text) => push_reasoning(
                &mut lines,
                text,
                width,
                index,
                state.is_reasoning_expanded(index),
            ),
            Record::Notice(text) => push_prefixed(&mut lines, "· ", text, width, Color::DarkGray),
            Record::Tool(card) => push_tool(&mut lines, card, width, state),
            Record::SubagentTree(tree) => push_subagent_tree(&mut lines, tree),
            Record::SubagentConversation(panel) => {
                push_subagent_conversation(&mut lines, panel, width)
            }
        }
    }
    lines.push(DisplayLine::plain(Line::raw("")));
    if let Some(status) = runtime_status_line(state, width) {
        lines.push(status);
    }
    lines
}

/// 运行状态行：spinner + 状态文本 + `[ ESC ]` 提示（对映 Python `RuntimeStatus`）。
///
/// 配色对映 `.message.runtime-status-message { color: $terminal-text-muted;
/// text-style: bold }`，提示段对映 `#runtime-status-esc-hint` 的 `bold dim`。
/// 状态文本按可用宽度收尾（Textual 会折行；这里保持单行不撑高消息区）。
fn runtime_status_line(state: &AppState, width: usize) -> Option<DisplayLine> {
    let text = match (&state.status, state.turn.is_running(), state.paused) {
        (Some(message), _, _) => message.clone(),
        (None, true, _) => "正在调用（跟随最新记录）".to_string(),
        (None, false, true) => "已暂停：输入新消息即可继续".to_string(),
        (None, false, false) => return None,
    };
    let style = theme::rich_style(theme::TEXT_MUTED).add_modifier(Modifier::BOLD);
    let running = state.turn.is_running();
    let hint = " [ ESC ]";
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    if running {
        let frame_index = state
            .turn_started
            .map(|started| (started.elapsed().as_millis() / 80) as usize)
            .unwrap_or(0);
        let spinner = format!("{} ", panels::spinner_frame(frame_index));
        used += display_width(&spinner);
        spans.push(Span::styled(spinner, style));
    }
    let reserve = if running { display_width(hint) } else { 0 };
    let room = width.saturating_sub(used + reserve).max(1);
    spans.push(Span::styled(fit(&text, room), style));
    if running {
        // 鼠标悬停 `[ ESC ]` 时染成淡黄色加粗（对映 Python `:hover` 的提亮效果），
        // 离开恢复灰色 dim。
        let hint_style = if state.runtime_esc_hover {
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD)
        } else {
            style.add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(hint, hint_style));
    }
    let line = Line::from(spans);
    Some(if running {
        // 只有运行中的状态行才带可点的 `[ ESC ]`。
        DisplayLine::with_hit(line, LineHit::RuntimeEsc)
    } else {
        DisplayLine::plain(line)
    })
}

/// 欢迎 Logo 的显示行：绝对定位覆盖层（对映 CSS `#welcome-logo { position: absolute;
/// offset: 0 4; padding: 0 2; content-align: center top; text-wrap: nowrap }`）。
///
/// 垂直下移 [`LOGO_OFFSET_ROWS`] 行、水平居中；块字比多数窗口宽，这里不折行：
/// 超出部分按 ratatui 的默认行为裁掉，避免把块字拆成两段错位。
fn welcome_logo_lines(state: &AppState, width: u16) -> Vec<DisplayLine> {
    let width = width as usize;
    let mut lines: Vec<DisplayLine> = (0..LOGO_OFFSET_ROWS)
        .map(|_| DisplayLine::plain(Line::raw("")))
        .collect();
    for line in state.logo.text().split_lines() {
        lines.push(DisplayLine::plain(centered_line(&line, width)));
    }
    lines
}

/// 一行富文本水平居中（左侧补空格，不动原有样式）。
fn centered_line(text: &StyledText, width: usize) -> Line<'static> {
    let used = text.display_width();
    let left = width.saturating_sub(used) / 2;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if left > 0 {
        spans.push(Span::raw(" ".repeat(left)));
    }
    spans.extend(text.to_spans());
    Line::from(spans)
}

/// 从底部起取可见窗口：`scroll_from_bottom` 为向上滚过的行数，滚到顶就停在最早的行。
pub fn window(
    lines: &[DisplayLine],
    height: usize,
    scroll_from_bottom: usize,
) -> Vec<Line<'static>> {
    let (start, end) = window_range(lines.len(), height, scroll_from_bottom);
    lines[start..end]
        .iter()
        .map(|rendered| rendered.line.clone())
        .collect()
}

/// 可见窗口在全部显示行里的下标区间；`height` 为 0 时是空区间。
pub fn window_range(total: usize, height: usize, scroll_from_bottom: usize) -> (usize, usize) {
    if height == 0 {
        return (0, 0);
    }
    let offset = scroll_from_bottom.min(total.saturating_sub(height));
    let end = total - offset;
    (end.saturating_sub(height), end)
}

/// 点击落在消息区第 `row` 行（区内相对行号）时的目标。
///
/// 行号先换算成全部显示行里的绝对下标（与 [`render`] 同一套文本区宽度与滚动计算），
/// 再取该行的命中信息；越界或该行没有目标时返回 `None`。
pub fn hit_test(
    state: &AppState,
    area: Rect,
    scroll_from_bottom: usize,
    row: usize,
) -> Option<LineHit> {
    let text = text_area(area);
    let lines = display_lines(state, text.width);
    let (start, end) = window_range(lines.len(), text.height as usize, scroll_from_bottom);
    let index = start.checked_add(row)?;
    if index >= end {
        return None;
    }
    lines.get(index).and_then(|line| line.hit.clone())
}

/// 正文采样：超出上限时保留首尾各两行有效行，中间以可点击提示行代替。
///
/// 对映 Python `ToolDisclosure._body_parts`：先按非空行判断是否真的需要缩略
/// （有效行不超过上限时原样显示），再把首尾有效行与省略的有效行数一并返回。
///
/// `write_file` / `Edit_file` 豁免折叠（对映 `UNLIMITED_TOOL_NAMES`）：文件变更
/// 预览本身就是「首尾两行看不出改了什么」，所以保持完整正文。判定用归一化后的
/// operation（Python 那边直接比对原工具名，命名空间前缀下 Rust 这里更宽松一点）。
pub fn collapsed_body(
    tool_name: &str,
    body: &StyledText,
) -> (Vec<StyledText>, usize, Vec<StyledText>) {
    let parts = body.split_lines();
    if tool_diff::is_file_change_tool(tool_name) || parts.len() <= TOOL_BODY_LIMIT {
        return (parts, 0, Vec::new());
    }
    let effective: Vec<&StyledText> = parts
        .iter()
        .filter(|line| !line.plain().trim().is_empty())
        .collect();
    if effective.len() <= TOOL_BODY_LIMIT {
        return (parts, 0, Vec::new());
    }
    let head: Vec<StyledText> = effective[..TOOL_HEAD_LINES]
        .iter()
        .map(|line| (*line).clone())
        .collect();
    let tail: Vec<StyledText> = effective[effective.len() - TOOL_TAIL_LINES..]
        .iter()
        .map(|line| (*line).clone())
        .collect();
    let hidden = effective.len() - TOOL_HEAD_LINES - TOOL_TAIL_LINES;
    (head, hidden, tail)
}

/// 用户消息：顶部灰色斜体 `user：` 标签行 + 显式白色正文，左侧一格青色竖条。
///
/// 对映 CSS `.user-message { color: $terminal-white; border-left: solid $terminal-cyan }`
/// 与 `pipeline.py` 的 `_append_message`（去掉旧版的 `$ ` 前缀）。
fn push_user(lines: &mut Vec<DisplayLine>, text: &str, width: usize) {
    let stripe = Style::new().bg(Color::Cyan);
    let label_style = theme::rich_style(&format!("{} italic", theme::TOOL_TEXT));
    let body_style = theme::rich_style(theme::ACCENT_WHITE);
    let body_width = width.saturating_sub(display_width(MESSAGE_PAD)).max(1);
    lines.push(DisplayLine::plain(striped_line(
        &format!("{MESSAGE_PAD}user："),
        label_style,
        stripe,
    )));
    for chunk in wrap_display(&text.replace('\r', ""), body_width) {
        lines.push(DisplayLine::plain(striped_line(
            &format!("{MESSAGE_PAD}{chunk}"),
            body_style,
            stripe,
        )));
    }
}

/// 用户消息的一行：左侧一格填色竖条 + 内容。
fn striped_line(text: &str, style: Style, stripe: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(USER_STRIPE, stripe),
        Span::styled(text.to_string(), style),
    ])
}

/// 助手正文：先 LaTeX 归一到 Unicode，再走对映层 Markdown 渲染。
///
/// 与 Python `AssistantMessage` 同一句——`render_markdown(format!("{display_prefix}{}",
/// latex_to_text(body)))`，行首的 `◇ ` 也算在 Markdown 文本里。渲染出来的富文本按真实列宽
/// 软折行（行首保留 CSS `.message { padding: 0 1 }` 的 1 格内边距），以便滚动窗口的行数
/// 计算保持准确。
fn push_assistant(lines: &mut Vec<DisplayLine>, text: &str, width: usize) {
    // 正文先 LaTeX 归一（行首数学围栏要求 `$`/`$$` 从行首开始），再整段走 Markdown 渲染。
    let body = text.strip_prefix("◇ ").unwrap_or(text);
    let block = markdown::render_markdown(&latex::latex_to_text(body));
    push_markdown(lines, &block, width, ASSISTANT_PREFIX);
}

/// 把一段对映层富文本铺成显示行：每行按列宽软折行，续行同样带 1 格内边距。
///
/// `prefix` 会捕到**第一行的行首**。这一点与 Python 略有不同：Python 把 `◇ `
/// 直接拼在 Markdown 源码前面（`RichMarkdown("◇ " + …)`），于是首行的 `#` 不在行首，
/// 「首行就是标题」这类语法解析不出来。这里改成渲染后再补前缀（见 README 已知差异）。
fn push_markdown(lines: &mut Vec<DisplayLine>, block: &StyledText, width: usize, prefix: &str) {
    let pad = display_width(MESSAGE_PAD);
    let body = width.saturating_sub(pad).max(1);
    for (index, line) in block.split_lines().iter().enumerate() {
        for (row_index, row) in wrap_spans(&line.to_spans(), body, body).iter().enumerate() {
            let mut cells: Vec<Span<'static>> = vec![Span::raw(MESSAGE_PAD.to_string())];
            if index == 0 && row_index == 0 && !prefix.is_empty() {
                cells.push(Span::raw(prefix.to_string()));
            }
            cells.extend(row.spans.iter().cloned());
            lines.push(DisplayLine::plain(Line::from(cells)));
        }
    }
}

fn push_prefixed(
    lines: &mut Vec<DisplayLine>,
    prefix: &str,
    text: &str,
    width: usize,
    color: Color,
) {
    // 行首的 MESSAGE_PAD 对映 CSS `.message { padding: 0 1 }`（所有消息共用）。
    let head_width = display_width(MESSAGE_PAD) + display_width(prefix);
    let body_width = width.saturating_sub(head_width).max(1);
    let style = Style::new().fg(color);
    for (index, chunk) in wrap_display(&text.replace('\r', ""), body_width)
        .iter()
        .enumerate()
    {
        let head = if index == 0 {
            format!("{MESSAGE_PAD}{prefix}")
        } else {
            " ".repeat(head_width)
        };
        lines.push(DisplayLine::plain(Line::from(vec![
            Span::styled(head, style),
            Span::styled(chunk.clone(), style),
        ])));
    }
}

/// 消息内边距 + 一行富文本（子任务树与子代理会话面板用）。
fn padded_styled(text: &StyledText) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(MESSAGE_PAD.to_string())];
    spans.extend(text.to_spans());
    Line::from(spans)
}

/// 子任务进度树：直接铺开对映层构造好的富文本（每行自带图标与状态的取色）。
///
/// Python 侧 `SubAgentProgressTree` 是 `Static`，高度随行数自适应；这里同样不折行，
/// 超宽部分按 ratatui 默认行为裁断。耗时按最近一次刷新的时刻落定（由每一帧的
/// [`AppState::refresh_subagent_trees`] 推进），因此渲染本身保持只读。
fn push_subagent_tree(lines: &mut Vec<DisplayLine>, tree: &SubAgentProgressTree) {
    for line in tree.render_text(None).split_lines() {
        lines.push(DisplayLine::plain(padded_styled(&line)));
    }
}

/// 子代理流式对话面板：`│` 包裹的会话面板按可用宽度换行。
fn push_subagent_conversation(
    lines: &mut Vec<DisplayLine>,
    panel: &SubAgentConversation,
    width: usize,
) {
    let body = width.saturating_sub(display_width(MESSAGE_PAD)).max(1);
    for line in panel.render_text(body).split_lines() {
        lines.push(DisplayLine::plain(padded_styled(&line)));
    }
}

/// 思考段：暗背景 + 灰前景 + 斜体（对映 CSS `.reasoning-message`），折叠态只显示最新
/// [`REASONING_TAIL`] 行（对映 `ReasoningDisclosure.COLLAPSED_HEIGHT`），点击切换展开。
fn push_reasoning(
    lines: &mut Vec<DisplayLine>,
    text: &str,
    width: usize,
    index: usize,
    expanded: bool,
) {
    // 思考块的暗底灰字：必须写成 `on <背景色>`，否则会被当成前景色（暗底暗字）。
    let style = theme::rich_style(&format!(
        "{} on {} italic",
        theme::REASONING_TEXT,
        theme::REASONING_BACKGROUND
    ));
    let body_width = width.saturating_sub(2 * display_width(MESSAGE_PAD)).max(1);
    // 思考正文同样走对映层 Markdown 渲染，只是颜色统一成灰（Python `uniform_gray`）：
    // 取渲染后的纯文本再套思考块的暗底灰字斜体样式，正文里的 `**`/`#` 等标记就会被去掉。
    let body = markdown::uniform_gray(&latex::latex_to_text(text)).plain();
    // Markdown 收尾常见一条空行（段落分隔）——留着会在思考块末尾多出一整行空白。
    let wrapped = wrap_display(body.trim_end_matches('\n'), body_width);
    let folded = if expanded {
        0
    } else {
        wrapped.len().saturating_sub(REASONING_TAIL)
    };
    for chunk in &wrapped[folded..] {
        lines.push(DisplayLine::with_hit(
            background_line(chunk, style, width),
            LineHit::Reasoning { index },
        ));
    }
    let hint = match (expanded, folded) {
        (true, _) => format!("⋯ 思考（已展开，共 {} 行）", wrapped.len()),
        (false, 0) => "⋯ 思考".to_string(),
        (false, _) => format!(
            "⋯ 思考已折叠，仅显示最新 {REASONING_TAIL} 行（共 {} 行）",
            wrapped.len()
        ),
    };
    lines.push(DisplayLine::with_hit(
        background_line(&hint, style, width),
        LineHit::Reasoning { index },
    ));
}

/// 一条铺满整行背景的消息行（思考块用）：左内边距 + 正文 + 右侧补白。
///
/// 背景要铺满整行（对映 CSS `background: $terminal-reasoning-background`），
/// 因此不足整宽的部分用同一样式的空格补齐。
fn background_line(text: &str, style: Style, width: usize) -> Line<'static> {
    let used = display_width(MESSAGE_PAD) + display_width(text);
    let fill = width.saturating_sub(used);
    Line::from(vec![
        Span::styled(MESSAGE_PAD.to_string(), style),
        Span::styled(text.to_string(), style),
        Span::styled(" ".repeat(fill), style),
    ])
}

/// 工具卡（方案 6：无边框，状态只看标题行首的 `●` 色点，正文缩进）。
///
/// 标题行整体交给对映层的 [`tool_diff::tool_disclosure_title`]（工具中文名、参数上下文、
/// 文件增删统计、`· 状态 · 耗时` 尾段），这里只负责消息内边距与宽度截断；
/// 缩略态正文是「首 2 行 + 省略提示行 + 尾 2 行」，提示行是卡片上唯一保留点击交互的正文元素。
fn push_tool(lines: &mut Vec<DisplayLine>, card: &ToolCard, width: usize, state: &AppState) {
    let expanded = state.is_tool_expanded(&card.call_id);
    let result_text = card.body.join("\n");
    let title = tool_diff::tool_disclosure_title(
        &card.name,
        &card.arguments,
        status_label(card.status),
        card.live_elapsed_seconds(),
        expanded,
        &result_text,
    );
    for line in title.split_lines() {
        lines.push(DisplayLine::with_hit(
            message_line(&line, width),
            LineHit::ToolCard {
                call_id: card.call_id.clone(),
            },
        ));
    }

    // 正文统一由对映层生成（Python `tool_disclosure_body`）：文件变更工具从参数
    // 画出 diff 预览（运行中也能看到）、read 与记忆/知识库工具正文为空、
    // fetcher 只留 URL/状态/标题、其余工具原样输出。
    let body = tool_diff::tool_disclosure_body(&card.name, &card.arguments, &result_text);
    let (head, hidden, tail) = if expanded {
        (body.split_lines(), 0, Vec::new())
    } else {
        collapsed_body(&card.name, &body)
    };
    for line in head {
        for row in tool_body_rows(&line, width, false) {
            lines.push(DisplayLine::with_hit(
                row,
                LineHit::ToolCard {
                    call_id: card.call_id.clone(),
                },
            ));
        }
    }
    if hidden > 0 {
        let hint = TOOL_EXPAND_HINT.replace("{lines}", &hidden.to_string());
        for row in tool_body_rows(&StyledText::styled(&hint, ""), width, true) {
            lines.push(DisplayLine::with_hit(
                row,
                LineHit::ToolHint {
                    call_id: card.call_id.clone(),
                },
            ));
        }
    }
    for line in tail {
        for row in tool_body_rows(&line, width, false) {
            lines.push(DisplayLine::with_hit(
                row,
                LineHit::ToolCard {
                    call_id: card.call_id.clone(),
                },
            ));
        }
    }
}

/// 消息内边距 + 一行富文本（工具卡标题用；样式由对映层的拼装结果给出）。
fn message_line(text: &StyledText, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw(MESSAGE_PAD.to_string())];
    spans.extend(truncate_styled(
        text,
        width.saturating_sub(display_width(MESSAGE_PAD)),
    ));
    Line::from(spans)
}

/// 工具卡正文的显示行：消息内边距 + `BODY_INDENT` 缩进（空行不加缩进），超宽时
/// 按显示宽度软折行。
///
/// 折行对映 Textual `Static` 的默认 `text-wrap`：正文只做 160 字预截断
/// （`_clip_line`），剩下的由组件按终端宽度折，而不是在这里加省略号。续行不缩进，
/// 与「缩进是加在整段文本首部的一格 span」一致。
///
/// 提示行按 CSS `ToolDisclosure > .tool-disclosure-hint { color: $terminal-text-gray;
/// text-style: underline italic }` 加下划线斜体，缩进本身不画下划线（与 Python 一致）。
fn tool_body_rows(line: &StyledText, width: usize, hint: bool) -> Vec<Line<'static>> {
    let indent = format!("{MESSAGE_PAD}{TOOL_BODY_INDENT}");
    let indent_width = display_width(&indent);
    let base = theme::rich_style(theme::TOOL_TEXT);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !line.plain().trim().is_empty() {
        // 对映 `_indent_body_lines`：只给非空行加缩进。
        let style = if hint {
            base.add_modifier(Modifier::ITALIC)
        } else {
            base
        };
        spans.push(Span::styled(indent, style));
    }
    if hint {
        let style = base.add_modifier(Modifier::ITALIC | Modifier::UNDERLINED);
        spans.push(Span::styled(line.plain(), style));
    } else {
        // 正文样式由对映层给出（文件变更预览的 +/− 着色就在里面），不再覆盖。
        spans.extend(line.to_spans());
    }
    wrap_spans(&spans, width.saturating_sub(indent_width).max(1), width.max(1))
}

/// 按显示宽度把一行富文本拆成多行：首行预算 `first`（扣掉缩进），续行预算 `rest`。
fn wrap_spans(spans: &[Span<'static>], first: usize, rest: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let budget = if rows.len() == 1 { first } else { rest };
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > budget && used > 0 {
                if !chunk.is_empty() {
                    if let Some(row) = rows.last_mut() {
                        row.push(Span::styled(std::mem::take(&mut chunk), span.style));
                    }
                }
                rows.push(Vec::new());
                used = 0;
            }
            chunk.push(ch);
            used += cell;
        }
        if !chunk.is_empty() {
            if let Some(row) = rows.last_mut() {
                row.push(Span::styled(chunk, span.style));
            }
        }
    }
    rows.into_iter().map(Line::from).collect()
}

/// 工具卡状态 → Python 的状态文案（对映 `format_tool_status` 的值域）。
fn status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "调用中",
        ToolStatus::Ok => "成功",
        ToolStatus::Failed => "失败",
        ToolStatus::Denied => "已取消",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ApprovalMode;
    use crate::state::AppState;
    use serde_json::json;
    use std::time::Instant;

    fn text_of(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    fn plain(display: &[DisplayLine]) -> Vec<String> {
        display.iter().map(|line| text_of(&line.line)).collect()
    }

    /// 把一段带 Markdown/LaTeX 的助手正文喂进状态机，再取实际显示行。
    fn assistant_lines(markdown: &str) -> (AppState, Vec<DisplayLine>) {
        let mut state = AppState::new("prj".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn("t1".to_string(), "问题".to_string());
        state.apply(
            &omnicrawl_ipc::HostEvent::Delta(omnicrawl_ipc::bridge::TextPayload {
                text: markdown.to_string(),
            }),
            Instant::now(),
        );
        let lines = display_lines(&state, 60);
        (state, lines)
    }

    #[test]
    fn assistant_body_renders_markdown_and_latex() {
        let (_state, display) = assistant_lines("# 标题\n\n普通 **加粗** 与 `code`，还有 $E=mc^2$。\n");
        let rendered = plain(&display);
        let joined = rendered.join("\n");
        assert!(joined.contains("◇ 标题"), "首行标题要带前缀且去掉 `#`：{rendered:?}");
        assert!(!joined.contains("**"), "加粗标记应被渲染掉：{rendered:?}");
        assert!(joined.contains("E=mc²"), "LaTeX 应归一成 Unicode：{rendered:?}");
        assert!(joined.contains("code"), "行内代码正文保留：{rendered:?}");
        // 真的套上了样式（不是只把标记删掉）。
        assert!(
            display.iter().any(|line| line
                .line
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))),
            "标题/加粗应带 BOLD：{rendered:?}"
        );
        assert!(
            display.iter().any(|line| line
                .line
                .spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Cyan))),
            "行内代码应是青字：{rendered:?}"
        );
    }

    #[test]
    fn streaming_deltas_accumulate_into_one_assistant_record() {
        let mut state = AppState::new("prj".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn("t1".to_string(), "问题".to_string());
        let now = Instant::now();
        for chunk in ["第一段", "第二段", "第三段"] {
            state.apply(
                &omnicrawl_ipc::HostEvent::Delta(omnicrawl_ipc::bridge::TextPayload {
                    text: chunk.to_string(),
                }),
                now,
            );
            // 每收到一片就能看到已到达的内容（逐片渲染，不是攒到回合结束）。
            let rendered = plain(&display_lines(&state, 60));
            assert!(
                rendered.iter().any(|line| line.contains(chunk)),
                "分片 {chunk} 应在到达当帧就上屏：{rendered:?}"
            );
        }
        let bodies: Vec<&String> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Assistant(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(bodies.len(), 1, "同一回合的正文应累加到一条记录");
        assert_eq!(bodies[0], "第一段第二段第三段");
    }

    /// 起一个带正文的工具卡（`read` 之外的工具才有正文）。
    fn state_with_tool(ok: bool, body_lines: usize) -> AppState {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn("t1".to_string(), "跑工具".to_string());
        let now = Instant::now();
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: serde_json::json!({"command": "pytest -q"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: "c1".to_string(),
            function_name: "bash".to_string(),
        };
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolStarted(
                omnicrawl_ipc::bridge::ToolStartedPayload {
                    step: 1,
                    call: call.clone(),
                },
            ),
            now,
        );
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolFinished(
                omnicrawl_ipc::bridge::ToolEventPayload {
                    call,
                    result: omnicrawl_core::ToolResult {
                        ok,
                        output: (1..=body_lines)
                            .map(|index| format!("行{index}\n"))
                            .collect(),
                        full_output: String::new(),
                        error_code: None,
                        retryable: false,
                    },
                },
            ),
            now + std::time::Duration::from_millis(1500),
        );
        state
    }

    #[test]
    fn empty_conversation_shows_welcome_logo_only() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        let rendered = plain(&display_lines(&state, 40));
        assert_eq!(
            rendered.len(),
            LOGO_OFFSET_ROWS + 8,
            "空会话首屏是下移四行的 8 行块字：{rendered:?}"
        );
        assert!(
            rendered[..LOGO_OFFSET_ROWS].iter().all(String::is_empty),
            "Logo 上方是空白行（对映 offset: 0 4）：{rendered:?}"
        );
        // 水平居中：块字行首有居中留白（对映 `content-align: center`）。
        assert!(
            rendered[LOGO_OFFSET_ROWS + 2]
                .trim_start()
                .starts_with("██████"),
            "{:?}",
            rendered[LOGO_OFFSET_ROWS + 2]
        );
        assert!(
            rendered[LOGO_OFFSET_ROWS + 2].starts_with(' '),
            "Logo 应居中而非贴左边：{:?}",
            rendered[LOGO_OFFSET_ROWS + 2]
        );

        // 首条记录进来后 Logo 让位；清空会话后重新出现（动画已落定为静态字形）。
        state.notice("第一条提示".to_string());
        let with_record = plain(&display_lines(&state, 40));
        assert!(with_record
            .iter()
            .any(|line| line.trim_start().starts_with("· 第一条提示")));
        assert!(!with_record.iter().any(|line| line.contains("██████")));
        state.records.clear();
        assert_eq!(
            plain(&display_lines(&state, 40)).len(),
            LOGO_OFFSET_ROWS + 8
        );
    }

    #[test]
    fn records_render_prefixes_and_wrap_to_width() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.begin_turn(
            "t1".to_string(),
            "这是一个很长的用户问题需要折行".to_string(),
        );
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::Delta(omnicrawl_ipc::bridge::TextPayload {
                text: "回答".to_string(),
            }),
            Instant::now(),
        );
        let lines = display_lines(&state, 20);
        let rendered = plain(&lines);
        // 用户消息：`user：` 标签行（灰斜）+ 白色正文，不再有 `$ ` 前缀。
        assert!(
            rendered.iter().any(|line| line.contains("user：")),
            "{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.trim_start().starts_with("这是一个很长")),
            "用户正文应另起一行：{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("$ 这是")),
            "旧版的 `$ ` 前缀应已移除：{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.trim_start().starts_with("◇ 回答")),
            "{rendered:?}"
        );
        for line in &rendered {
            assert!(display_width(line) <= 20, "行超宽：{line:?}");
        }
    }

    #[test]
    fn subagent_tree_record_renders_one_line_per_task() {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::SubagentEvent(
                omnicrawl_ipc::bridge::SubagentEventPayload {
                    name: "subagent.task.running".to_string(),
                    payload: serde_json::json!({
                        "task_id": "t1",
                        "batch_id": "batch-1",
                        "agent_type": "reviewer",
                        "description": "审查改动",
                    }),
                },
            ),
            Instant::now(),
        );
        let rendered = plain(&display_lines(&state, 40));
        let header = rendered
            .iter()
            .find(|line| line.contains("子任务进度"))
            .expect("应有进度树标题");
        assert!(header.contains("0/1 完成"), "{header}");
        assert!(
            rendered
                .iter()
                .any(|line| line.trim_start().starts_with("└─ ● ")),
            "运行中任务一行一项：{rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("审查改动") && line.contains("· 运行中")),
            "{rendered:?}"
        );
        // 进度树不参与点击命中（Python 侧无交互动画）。
        assert!(display_lines(&state, 40)
            .iter()
            .all(|line| line.hit.is_none()));
    }

    #[test]
    fn reasoning_collapses_to_latest_lines_with_hint() {
        // 用空行分段：Markdown 会把同一段里的换行当软换行合并成一行（与 Python `uniform_gray` 同义）。
        let text: String = (1..=8).map(|index| format!("想法{index}\n\n")).collect();
        let mut lines = Vec::new();
        push_reasoning(&mut lines, text.trim_end(), 20, 3, false);
        let rendered = plain(&lines);
        assert_eq!(rendered.len(), REASONING_TAIL + 1, "五行正文 + 一行提示");
        assert!(
            rendered[0].contains("想法4"),
            "应显示最新五行：{rendered:?}"
        );
        assert!(
            rendered[rendered.len() - 1].contains("思考已折叠"),
            "{rendered:?}"
        );
        assert_eq!(
            lines[0].hit,
            Some(LineHit::Reasoning { index: 3 }),
            "思考段可点击，命中带上记录下标"
        );

        // 展开态显示全部行，提示语随之变化。
        let mut expanded = Vec::new();
        push_reasoning(&mut expanded, text.trim_end(), 20, 3, true);
        let rendered = plain(&expanded);
        assert_eq!(rendered.len(), 9, "八行正文 + 一行提示");
        assert!(rendered[0].contains("想法1"), "{rendered:?}");
        assert!(rendered[8].contains("已展开，共 8 行"), "{rendered:?}");
    }

    #[test]
    fn tool_card_is_borderless_with_status_dot_and_sampled_body() {
        let state = state_with_tool(true, 9);
        let display = display_lines(&state, 40);
        let rendered = plain(&display);
        let title = rendered
            .iter()
            .find(|line| line.contains("bash"))
            .expect("应有工具卡标题");
        assert!(title.starts_with(" ● "), "标题行首是状态色点：{title:?}");
        assert!(
            title.contains(" · ✓ 成功 · "),
            "标题带状态与耗时尾段：{title:?}"
        );
        assert!(title.contains("1.5s"), "{title:?}");
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains('┌') || line.contains('│') || line.contains('└')),
            "方案 6：工具卡不再画边框：{rendered:?}"
        );
        // 正文行 = 消息内边距 + BODY_INDENT 缩进；缩略态是首 2 行 + 提示行 + 尾 2 行。
        let body: Vec<&String> = rendered
            .iter()
            .filter(|line| line.trim_start().starts_with("行"))
            .collect();
        assert_eq!(
            body.len(),
            TOOL_HEAD_LINES + TOOL_TAIL_LINES,
            "缩略态只留首尾各两行：{body:?}"
        );
        assert!(
            body.iter()
                .all(|line| line.starts_with(&format!("{MESSAGE_PAD}{TOOL_BODY_INDENT}"))),
            "正文应缩进到消息内边距 + BODY_INDENT：{body:?}"
        );
        let hint_line = rendered
            .iter()
            .find(|line| line.contains("点击展开 5 行"))
            .expect("应有省略提示行");

        // 提示行带 ToolHint 命中，其余卡片行是 ToolCard。
        let hint = display
            .iter()
            .find(|line| matches!(line.hit, Some(LineHit::ToolHint { .. })))
            .expect("应有提示行命中");
        assert_eq!(text_of(&hint.line), *hint_line);
    }

    #[test]
    fn expanding_a_tool_card_shows_full_body_and_drops_the_hint() {
        let mut state = state_with_tool(true, 9);
        state.expand_tool("c1");
        let rendered = plain(&display_lines(&state, 40));
        let body: Vec<&String> = rendered
            .iter()
            .filter(|line| line.trim_start().starts_with("行"))
            .collect();
        assert_eq!(body.len(), 9, "展开态显示完整正文：{body:?}");
        assert!(
            !rendered.iter().any(|line| line.contains("点击展开")),
            "{rendered:?}"
        );

        // 收起后回到缩略态（首尾各两行 + 提示行）。
        assert!(state.collapse_tool("c1"));
        assert_eq!(
            plain(&display_lines(&state, 40))
                .iter()
                .filter(|line| line.trim_start().starts_with("行"))
                .count(),
            TOOL_HEAD_LINES + TOOL_TAIL_LINES
        );
    }

    /// 把若干行拼成一份富文本正文（对映 `tool_disclosure_body` 的返回类型）。
    fn styled_lines<I, S>(lines: I) -> StyledText
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let joined: Vec<String> = lines.into_iter().map(Into::into).collect();
        StyledText::styled(&joined.join("\n"), "")
    }

    fn plain_text(lines: &[StyledText]) -> Vec<String> {
        lines.iter().map(|line| line.plain()).collect()
    }

    /// 起一个指定工具名的卡片（`state_with_tool` 的泛化版）。
    fn state_with_named_tool(name: &str, arguments: serde_json::Value, output: &str) -> AppState {
        let mut state = AppState::new("demo".to_string(), "m".to_string(), ApprovalMode::Manual);
        state.telemetry.context_window = Some(1_000_000);
        state.begin_turn("t1".to_string(), "跑工具".to_string());
        let now = Instant::now();
        let call = omnicrawl_core::ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: "c1".to_string(),
            function_name: name.to_string(),
        };
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolStarted(
                omnicrawl_ipc::bridge::ToolStartedPayload { step: 1, call: call.clone() },
            ),
            now,
        );
        state.apply(
            &omnicrawl_ipc::bridge::HostEvent::ToolFinished(
                omnicrawl_ipc::bridge::ToolEventPayload {
                    call,
                    result: omnicrawl_core::ToolResult {
                        ok: true,
                        output: output.to_string(),
                        full_output: String::new(),
                        error_code: None,
                        retryable: false,
                    },
                },
            ),
            now + std::time::Duration::from_millis(1500),
        );
        state
    }

    #[test]
    fn collapsed_body_keeps_head_and_tail() {
        let body = styled_lines((1..=9).map(|index| format!("行{index}")));
        let (head, hidden, tail) = collapsed_body("bash", &body);
        assert_eq!(plain_text(&head), vec!["行1", "行2"]);
        assert_eq!(hidden, 5);
        assert_eq!(plain_text(&tail), vec!["行8", "行9"]);

        // 有效行不超过上限时原样返回（含空行的正文按非空行计数）。
        let short = styled_lines(["行1", "", "行2"]);
        let (head, hidden, tail) = collapsed_body("bash", &short);
        assert_eq!(head.len(), 3);
        assert_eq!(hidden, 0);
        assert!(tail.is_empty());
    }

    #[test]
    fn write_file_body_is_not_folded() {
        // 文件变更工具豁免五行限制（对映 `UNLIMITED_TOOL_NAMES`）。
        let body = styled_lines((1..=9).map(|index| format!("行{index}")));
        let (head, hidden, tail) = collapsed_body("write_file", &body);
        assert_eq!(head.len(), 9);
        assert_eq!(hidden, 0);
        assert!(tail.is_empty());
    }

    #[test]
    fn read_body_is_hidden_and_file_change_body_is_not() {
        // read 的正文由对映层直接隐藏（正文为空，不保留提示行）。
        let read = state_with_named_tool("read", json!({"path": "a.py"}), "文件正文不应出现在对话里");
        let rendered = plain(&display_lines(&read, 80));
        assert!(
            !rendered.iter().any(|line| line.contains("文件正文")),
            "read 的正文应被隐藏：{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("点击展开")),
            "正文为空时不应出现提示行：{rendered:?}"
        );

        // write_file 的正文来自参数（diff 预览），而不是工具输出。
        let write = state_with_named_tool(
            "write_file",
            json!({"path": "a.py", "content": "行1\n行2\n行3\n行4\n行5\n行6\n行7"}),
            "已写入 a.py",
        );
        let rendered = plain(&display_lines(&write, 80));
        assert!(
            rendered.iter().any(|line| line.contains("行1")),
            "write_file 应显示 diff 正文：{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("点击展开")),
            "文件变更工具豁免折叠：{rendered:?}"
        );
    }

    #[test]
    fn long_body_lines_wrap_instead_of_truncating() {
        let long = "甲".repeat(60);
        let state = state_with_named_tool("bash", json!({"command": "echo"}), &long);
        let rendered = plain(&display_lines(&state, 40));
        let body: Vec<&String> = rendered
            .iter()
            .filter(|line| line.contains('甲'))
            .collect();
        assert!(body.len() > 1, "超宽正文应折行：{body:?}");
        let joined: String = body.iter().map(|line| line.trim().to_string()).collect();
        assert_eq!(joined, long, "折行不应丢字：{body:?}");
        assert!(
            !body.iter().any(|line| line.contains('…')),
            "折行而不是截断：{body:?}"
        );
    }

    #[test]
    fn hit_test_maps_window_rows_to_tool_targets() {
        let mut state = state_with_tool(true, 9);
        let area = Rect::new(0, 0, 40, 30);
        let width = text_area(area).width;
        let total = display_lines(&state, width).len();
        let height = total;
        let hint_row = (0..height)
            .find(|row| {
                matches!(
                    hit_test(&state, area, 0, *row),
                    Some(LineHit::ToolHint { .. })
                )
            })
            .expect("提示行应可命中");
        assert_eq!(
            hit_test(&state, area, 0, hint_row),
            Some(LineHit::ToolHint {
                call_id: "c1".to_string()
            })
        );
        // 标题行在提示行之上第三行（标题 + 两行首部正文）。
        let title_row = hint_row - 3;
        assert_eq!(
            hit_test(&state, area, 0, title_row),
            Some(LineHit::ToolCard {
                call_id: "c1".to_string()
            })
        );
        assert_eq!(hit_test(&state, area, 0, height + 5), None, "越界无命中");

        // 展开态下卡片点击应触发收起；缩略态点击无效（与 Python `ToolDisclosure.on_click` 一致）。
        assert!(!state.collapse_tool("c1"), "缩略态没有可收起的卡片");
        state.expand_tool("c1");
        let hit = hit_test(&state, area, 0, title_row).expect("标题行命中");
        assert_eq!(
            hit,
            LineHit::ToolCard {
                call_id: "c1".to_string()
            }
        );
        assert!(state.collapse_tool("c1"), "展开态点击卡片应收起");
    }

    #[test]
    fn window_follows_bottom_until_scrolled_up() {
        let lines: Vec<DisplayLine> = (0..10)
            .map(|index| DisplayLine::plain(Line::raw(format!("行{index}"))))
            .collect();
        let bottom = texts(&window(&lines, 3, 0));
        assert_eq!(bottom, vec!["行7", "行8", "行9"]);
        let scrolled = texts(&window(&lines, 3, 2));
        assert_eq!(scrolled, vec!["行5", "行6", "行7"]);
        let over = texts(&window(&lines, 4, 99));
        assert_eq!(
            over,
            vec!["行0", "行1", "行2", "行3"],
            "滚到顶就停在最早的行"
        );
        assert!(texts(&window(&lines, 4, 6)) == vec!["行0", "行1", "行2", "行3"]);
        assert_eq!(window_range(10, 0, 0), (0, 0));
    }
}
