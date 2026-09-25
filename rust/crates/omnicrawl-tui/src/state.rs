//! 界面状态机：消息流记录、输入框、遥测与待决工具批次的接线。
//!
//! 所有状态变化都发生在主线程，事件来源只有两处：内核帧（[`AppState::apply`]）与
//! 用户输入（输入框与面板）。渲染只读，不修改状态。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use omnicrawl_commands::CommandOption;
use omnicrawl_core::AgentLoopObservation;
use omnicrawl_ipc::{HostEvent, Id};

use crate::args::ApprovalMode;
use crate::host::{self, BatchContext, TodoItem, Waiting};
use crate::ui::fullscreen::input::menu::{CommandMenu, MenuAction, MenuKey};
use crate::ui::fullscreen::input::sessions_menu::{SessionMenuItem, SessionsMenu};
use crate::ui::fullscreen::random::Rng;
use crate::ui::fullscreen::rendering::logo_anim::LogoAnimation;
use crate::ui::fullscreen::rendering::widgets::{SubAgentConversation, SubAgentProgressTree};
use crate::ui::fullscreen::status::hud::load_carousel_message_lines;
use crate::ui::fullscreen::status::indicators as queue;
use crate::ui::fullscreen::status::indicators::{
    Carousel, CarouselPage, CarouselSource, CarouselTick, CAROUSEL_ANIMATION_FRAME_SECONDS,
};
use crate::ui::fullscreen::text::StyledText;

/// 相邻增量间隔超过这个时长视为待机（工具执行、模型停顿），不计入输出时长。
const IDLE_GAP: Duration = Duration::from_secs(2);

/// 输入框可见行数上限：超过后在编辑器内滚动。
pub const COMPOSER_MAX_LINES: usize = 5;

/// 轮播随机源种子：固定值让留言页与乱码帧在测试里可复现。
const CAROUSEL_SEED: u64 = 0x0C1C_2025;

/// 输入框上方那行瞬时提示（拖选复制等）的存活时长。
pub const NOTICE_LINE_LINGER: Duration = Duration::from_secs(3);

/// 生成期间 FIFO 排队预览的可见条数上限与摘要长度上限。
///
/// 直接复用对映层常量，不另立一份：预览行数与 HUD 的 `QUEUE` 段读的是同一组值。
pub use crate::ui::fullscreen::status::indicators::{
    QUEUE_PREVIEW_MAX_ROWS, QUEUE_PREVIEW_SUMMARY_LIMIT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCard {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    /// 原始参数（模型给的那份）：标题行走对映层的 `tool_disclosure_title`，
    /// 它要按参数拼出路径 / 命令 / 行数统计，因此不能只留 `summary` 文本。
    pub arguments: serde_json::Value,
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    started: Instant,
    pub body: Vec<String>,
}

impl ToolCard {
    /// 当前耗时：终态用落定值，运行中按起始时刻实时算。
    ///
    /// 对映 Python `ToolDisclosure` 标题行的活动计时器：运行中的卡片也要显示已耗时，
    /// 收口后不再变化（渲染保持只读，只是读了一个随时间变化的量）。
    pub fn live_elapsed_seconds(&self) -> f64 {
        match self.elapsed {
            Some(duration) => duration.as_secs_f64(),
            None => self.started.elapsed().as_secs_f64(),
        }
    }
}

/// 一次工具调用在「参数还在流里」阶段的侧信道状态（按 call_id 索引）。
///
/// 不塞进 `ToolCard` 的理由：卡片由批次执行（`tool.started`）创建，而这里的条目从模型
/// 刚吐出 `tool.started`（`turn.tool_call_started`）就存在，两者生命周期不同；压缩提示
/// 也要在批次执行之后继续挂在卡片上方，所以整轮都留着，回合结束再清。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamingTool {
    /// 累积的 arguments 原文（可能还是半截 JSON）。
    pub text: String,
    /// 容错解析出来的参数：流式阶段标题行与文件预览都从这里取。
    pub arguments: serde_json::Value,
    /// 压缩阶段提示（`正在压缩…` / `已压缩 a → b 字符`）。
    pub compression: Option<String>,
    /// 是否仍在写参数（批次执行开始后置 false，卡片正文改由真实载荷驱动）。
    pub streaming: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    User(String),
    Reasoning(String),
    Assistant(String),
    Tool(ToolCard),
    Notice(String),
    /// 一个 SubAgent 批次的进度树：同批次的任务事件在第 `i` 条记录上原地更新
    /// （对映 Python 把 `SubAgentProgressTree` 挂进消息区、后续事件复用同一组件）。
    SubagentTree(SubAgentProgressTree),
    /// 一个 SubAgent 批次的流式对话面板（`/review` 这类派生评审流程用；
    /// 对映 Python 的 `_handle_subagent_conversation_event` 把 `SubAgentConversation`
    /// 按 `batch_id` 挂进消息区）。
    SubagentConversation(SubAgentConversation),
}

/// Monitor 任务状态 → 卡片状态：字符串取值与 `MonitorManager` 的快照同源。
///
/// `stopped`（被显式终止）归「成功」是因为它同样是终态；用词不精确但原始状态字符串
/// 仍在卡片正文里（`Monitor · id · stopped`），不必为了措辞新增一个渲染分支。
fn monitor_status(status: &str) -> ToolStatus {
    match status {
        "running" => ToolStatus::Running,
        "failed" => ToolStatus::Failed,
        _ => ToolStatus::Ok,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnState {
    Idle,
    Running { turn_id: String },
}

impl TurnState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Running { turn_id } => Some(turn_id),
            Self::Idle => None,
        }
    }
}

/// 输出速度估计：累计估算 token 数与真正的连续输出时长，间隔超过 [`IDLE_GAP`] 不计时。
#[derive(Debug, Clone, Default)]
pub struct RateEstimator {
    tokens: f64,
    active: Duration,
    last: Option<Instant>,
    value: Option<f64>,
}

impl RateEstimator {
    pub fn record(&mut self, text: &str, now: Instant) {
        self.tokens += estimated_tokens(text) as f64;
        if let Some(last) = self.last {
            let gap = now.saturating_duration_since(last);
            if gap <= IDLE_GAP {
                self.active += gap;
            }
        }
        self.last = Some(now);
        if self.active > Duration::from_millis(200) {
            self.value = Some(self.tokens / self.active.as_secs_f64());
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    /// 模型流中断回滚：撤销本回合累计的量，重新计时。
    pub fn rollback(&mut self) {
        self.tokens = 0.0;
        self.active = Duration::ZERO;
        self.last = None;
        self.value = None;
    }
}

#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub context_window: Option<u64>,
    pub rate: RateEstimator,
}

/// 单行起步、按显示宽度软折行的输入框；最多显示 [`COMPOSER_MAX_LINES`] 行。
///
/// 输入框自己带着斜杠命令菜单与候选表：菜单跟着文本变化刷新（对映 Textual 的
/// `on_text_area_changed` → `_refresh_command_menu`），因此不必在每个改动点手动
/// 同步——漏掉一处就会让菜单与实际输入脱节。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composer {
    text: String,
    cursor: usize,
    commands: Vec<CommandOption>,
    menu: CommandMenu,
    /// 粘贴折叠：占位符 → 原始文本（对映 Python `_compact_pastes`）。
    /// 提交时按占位符还原，删除时整块删掉。
    pastes: Vec<(String, String)>,
    paste_sequence: usize,
}

/// 粘贴折叠的阈值：超过这么多行就折成一个占位符（对映 Python `_PASTE_COMPACT_LINE_THRESHOLD`）。
pub const PASTE_COMPACT_LINE_THRESHOLD: usize = 5;

/// 终端粘贴常带 CRLF/CR，先统一成 LF（对映 Python `_normalize_pasted_text`）。
fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 按编辑器语义数行数：末尾空行也算一行（对映 Python `_count_paste_lines`）。
fn paste_line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    text.matches('\n').count() + 1
}

/// 占位符文本（对映 Python `[粘贴 #{n} +{lines} 行]`）。
fn paste_placeholder(sequence: usize, lines: usize) -> String {
    format!("[粘贴 #{sequence} +{lines} 行]")
}

impl Composer {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// 当前全文（集成测试与命令分派核对用）。
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.refresh_menu();
    }

    /// 用给定文本替换全文，并把光标放到末尾（撤回排队消息、菜单补全时用）。
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.prune_pastes();
        self.refresh_menu();
    }

    /// 取走内容并清空；提交时用。
    ///
    /// 返回的是**展开后**的全文：粘贴折叠块在这里还原成真实内容（对映 Python
    /// 提交前调 `_expand_compact_paste_placeholders` 再清空映射）。
    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.refresh_menu();
        text
    }

    /// 粘贴入口：多行（> [`PASTE_COMPACT_LINE_THRESHOLD`] 行）折成一个 `[粘贴 #n +N 行]`
    /// 占位符，原文按序号存起来、提交时还原（对映 Python `_compact_paste_if_needed`）。
    ///
    /// 短粘贴（含单行）与原来一样直接插进去：折起来反而更难改。
    pub fn insert_paste(&mut self, text: &str) {
        let normalized = normalize_paste(text);
        let lines = paste_line_count(&normalized);
        if lines <= PASTE_COMPACT_LINE_THRESHOLD {
            self.insert(&normalized);
            return;
        }
        self.paste_sequence += 1;
        let placeholder = paste_placeholder(self.paste_sequence, lines);
        self.pastes.push((placeholder.clone(), normalized));
        self.insert(&placeholder);
    }

    /// 展开全部占位符；`take()` 与命令分派核对都读它。
    pub fn expanded_text(&self) -> String {
        let mut expanded = self.text.clone();
        for (placeholder, original) in &self.pastes {
            expanded = expanded.replace(placeholder.as_str(), original.as_str());
        }
        expanded
    }

    /// 字符下标 `index` 落在哪个占位符里（左闭右开），没有就是 `None`。
    fn placeholder_span(&self, index: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = self.text.chars().collect();
        for (placeholder, _) in &self.pastes {
            let needle: Vec<char> = placeholder.chars().collect();
            if needle.is_empty() || needle.len() > chars.len() {
                continue;
            }
            let found = (0..=chars.len() - needle.len())
                .find(|start| chars[*start..*start + needle.len()] == needle[..]);
            if let Some(start) = found {
                if index >= start && index < start + needle.len() {
                    return Some((start, start + needle.len()));
                }
            }
        }
        None
    }

    /// 光标落在占位符里就整块删掉：粘贴块不逐字符删（用户要求，也是与 Python 的差异点）。
    fn remove_placeholder(&mut self, index: usize) -> bool {
        let Some((start, end)) = self.placeholder_span(index) else {
            return false;
        };
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.drain(start..end);
        self.text = chars.into_iter().collect();
        self.cursor = start;
        self.prune_pastes();
        self.refresh_menu();
        true
    }

    /// 丢掉已经不在文本里的占位符，避免原文一直挂着占内存
    /// （对映 Python `_prune_compact_paste_placeholders`）。
    fn prune_pastes(&mut self) {
        if self.pastes.is_empty() {
            return;
        }
        let text = self.text.clone();
        self.pastes
            .retain(|(placeholder, _)| text.contains(placeholder.as_str()));
    }

    /// 装载命令菜单候选表（统一命令源）；运行期 Skill 上下线时由宿主重装。
    pub fn set_commands(&mut self, commands: Vec<CommandOption>) {
        self.commands = commands;
        self.refresh_menu();
    }

    pub fn commands(&self) -> &[CommandOption] {
        &self.commands
    }

    /// 斜杠命令菜单状态（渲染与高度预算读它）。
    pub fn menu(&self) -> &CommandMenu {
        &self.menu
    }

    /// 把菜单选择键交给菜单；完整命令时返回 [`MenuAction::Passthrough`]（放行提交）。
    pub fn menu_handle_key(&mut self, key: MenuKey) -> MenuAction {
        self.menu.handle_key(key, &self.text)
    }

    /// 按当前文本重新筛选候选（对映 `on_text_area_changed` 的刷新时机）。
    fn refresh_menu(&mut self) {
        self.menu.refresh(&self.text, &self.commands);
    }

    pub fn insert(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        let inserted: Vec<char> = normalized.chars().collect();
        let count = inserted.len();
        chars.splice(at..at, inserted);
        self.text = chars.into_iter().collect();
        self.cursor = at + count;
        self.refresh_menu();
    }

    pub fn newline(&mut self) {
        self.insert("\n");
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // 光标左侧落在粘贴折叠块里：整块删掉，而不是逐字符删。
        if self.remove_placeholder(self.cursor - 1) {
            return;
        }
        let mut chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        chars.remove(at - 1);
        self.text = chars.into_iter().collect();
        self.cursor = at - 1;
        self.prune_pastes();
        self.refresh_menu();
    }

    pub fn delete(&mut self) {
        let mut chars: Vec<char> = self.text.chars().collect();
        if self.cursor >= chars.len() {
            return;
        }
        // 光标右侧落在粘贴折叠块里：整块删掉（含块首与块内）。
        if self.remove_placeholder(self.cursor) {
            return;
        }
        chars.remove(self.cursor);
        self.text = chars.into_iter().collect();
        self.prune_pastes();
        self.refresh_menu();
    }

    /// 左右移动把占位符当一格：光标停在块首/块尾，不会停到块中间。
    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        match self.placeholder_span(self.cursor - 1) {
            // 已在块首则跨出块外一格，否则跳到块首。
            Some((start, _)) => self.cursor = if self.cursor > start { start } else { start - 1 },
            None => self.cursor -= 1,
        }
    }

    pub fn move_right(&mut self) {
        let length = self.text.chars().count();
        if self.cursor >= length {
            return;
        }
        match self.placeholder_span(self.cursor) {
            Some((_, end)) => self.cursor = end,
            None => self.cursor += 1,
        }
    }

    pub fn move_home(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[..at]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[at..]
            .iter()
            .position(|ch| *ch == '\n')
            .map(|index| at + index)
            .unwrap_or(chars.len());
    }

    /// 按显示宽度软折行；阶段一按列断行，不做英文单词级避断。
    pub fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let width = width.max(1) as usize;
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut used = 0usize;
        for ch in self.text.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
                used = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > width && used > 0 {
                lines.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(ch);
            used += cell;
        }
        lines.push(current);
        lines
    }

    /// 光标所在的行号与列（列按显示宽度，行按 `width` 软折行后的行）。
    pub fn cursor_position(&self, width: u16) -> (usize, usize) {
        let width = width.max(1) as usize;
        let mut row = 0usize;
        let mut column = 0usize;
        for (index, ch) in self.text.chars().enumerate() {
            if index >= self.cursor {
                break;
            }
            if ch == '\n' {
                row += 1;
                column = 0;
                continue;
            }
            let cell = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if column + cell > width && column > 0 {
                row += 1;
                column = 0;
            }
            column += cell;
        }
        (row, column)
    }

    /// 需要展示的行与光标在其中的行号：超过行数上限时随光标滚动。
    pub fn visible_lines(&self, width: u16) -> (Vec<String>, usize) {
        let (lines, cursor, _, _) = self.visible_window(width);
        (lines, cursor)
    }

    /// 与 [`Self::visible_lines`] 同一个窗口，另外给出窗口起点与总行数。
    ///
    /// 输入卡右侧的细线滚动条要画滑块，需要知道「当前可见的是哪一段」。
    pub fn visible_window(&self, width: u16) -> (Vec<String>, usize, usize, usize) {
        let lines = self.wrapped_lines(width);
        let (row, _) = self.cursor_position(width);
        if lines.len() <= COMPOSER_MAX_LINES {
            return (
                lines.clone(),
                row.min(COMPOSER_MAX_LINES - 1),
                0,
                lines.len(),
            );
        }
        let start = row
            .saturating_sub(COMPOSER_MAX_LINES - 1)
            .min(lines.len() - COMPOSER_MAX_LINES);
        (
            lines[start..start + COMPOSER_MAX_LINES].to_vec(),
            row - start,
            start,
            lines.len(),
        )
    }
}

/// 会话流的文本选区（鼠标拖选）。
///
/// 行下标是全部显示行（`ui::conversation::display_lines`）的下标，列是显示列宽，
/// 因此滚动窗口、软折行与宽字符（CJK）都不会错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    /// 按下左键时的位置。
    pub anchor: (usize, usize),
    /// 当前拖到的位置（与 `anchor` 构成选区两端）。
    pub head: (usize, usize),
}

impl TextSelection {
    /// 归一化后的（起点, 终点）：阅读顺序，起点永远不在终点之后。
    pub fn normalized(self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// 只按了一下没拖动（空选区）时为真。
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    /// 选区跨的显示行数（单行选区为 1）。
    pub fn rows(self) -> usize {
        let ((start, _), (end, _)) = self.normalized();
        end.saturating_sub(start) + 1
    }
}

pub struct AppState {
    pub project: String,
    pub model: String,
    pub approval: ApprovalMode,
    pub version: String,
    pub records: Vec<Record>,
    pub composer: Composer,
    pub scroll_from_bottom: usize,
    pub status: Option<String>,
    /// 输入框上方那行瞬时提示（拖选复制等）：不进会话流，超过 [`NOTICE_LINE_LINGER`] 自散。
    pub notice_line: Option<(String, Instant)>,
    /// 本轮「参数还在流里」的工具调用（渲染流式卡片与压缩提示用）。
    streaming_tools: HashMap<String, StreamingTool>,
    pub todos: Vec<TodoItem>,
    pub paused: bool,
    pub turn: TurnState,
    pub telemetry: Telemetry,
    /// 已启用的 MCP Server 数量（HUD 的 MCP 段）。
    pub mcp_servers: u64,
    /// 本回合开始时刻；状态行的 spinner 按它推进。
    pub turn_started: Option<Instant>,
    /// 是否在消息流里显示思考段（`ui.show_thinking`，由设置面板的「思考显示」页更新）。
    pub show_thinking: bool,
    /// 空会话首屏的欢迎 Logo：未收到任何记录时展示（对映 Python 的 `#welcome-logo`
    /// 静态块，以及只在首次挂载播放一次的入场动画）。
    pub logo: LogoAnimation,
    /// 生成期间按 Enter 排队的消息（FIFO）。回合结束后按顺序自动提交，
    /// 避免同一进程里出现重叠的回合 worker（对映 Python 的 `_pending_inputs`）。
    pub pending_inputs: VecDeque<String>,
    /// 排队预览是否处于展开态（超出可见上限时才可切换）。
    pub pending_queue_expanded: bool,
    /// 被点击展开正文的工具卡（按 `call_id`），对映 Python `ToolDisclosure._expanded`。
    pub expanded_tools: HashSet<String>,
    /// 被点击展开的思考段（按记录下标），对映 Python `ReasoningDisclosure._expanded`。
    /// 下标随记录清空/回滚而变，展开态在那种情况下自然失效（服务端不会回填）。
    pub expanded_reasoning: HashSet<usize>,
    /// 子代理对话流是否处于开启态（`/review` 运行期间）：事件进流式对话面板
    /// 而不是进度树，对映 Python 的 `_conversation_stream_active`。
    pub subagent_stream: bool,
    batch: Option<host::PendingBatch>,
    /// 底部单行轮播状态机（对映 Python `_carousel_*`）：遥测 → 工作区路径 → 留言
    /// 三页各 10s 循环，换页时以解密扫描特效过渡。
    carousel: Carousel,
    /// 当前要显示的轮播整行（渲染只读；由 [`AppState::refresh_carousel`] 按帧推进）。
    pub carousel_text: StyledText,
    /// 本页进入时刻（停留计时起点）。
    carousel_since: Instant,
    /// 上一帧动画时刻：按 `CAROUSEL_ANIMATION_FRAME_SECONDS` 限速推进解密扫描。
    carousel_anim_at: Option<Instant>,
    /// 首帧是否已经装配过轮播文本。
    carousel_ready: bool,
    /// 轮播随机源（留言页抽取与乱码字符；固定种子便于测试复现）。
    carousel_rand: Rng,
    /// 留言页候选（编译期内嵌的 `carousel_messages.txt`，启动期读一次）。
    carousel_lines: Vec<String>,
    /// 鼠标拖选的文本区间（`None` = 没有选区）：渲染层按它加反显高亮，
    /// 松手时由宿主抽出文本写入剪切板。坐标是「`display_lines` 行下标 + 行内显示列」。
    selection: Option<TextSelection>,
    /// 会话流末尾 `[ ESC ]` 提示是否被鼠标悬停（悬停时染成淡黄色）。
    pub runtime_esc_hover: bool,
    /// `/sessions` 在输入框上方打开的可选列表。
    pub sessions_menu: SessionsMenu,
}

impl AppState {
    pub fn new(project: String, model: String, approval: ApprovalMode) -> Self {
        let mut state = Self {
            project,
            model,
            approval,
            version: format!("v{}", env!("CARGO_PKG_VERSION")),
            records: Vec::new(),
            composer: Composer::default(),
            scroll_from_bottom: 0,
            status: None,
            notice_line: None,
            streaming_tools: HashMap::new(),
            todos: Vec::new(),
            paused: false,
            turn: TurnState::Idle,
            telemetry: Telemetry::default(),
            mcp_servers: 0,
            turn_started: None,
            // 缺省显示思考（与 Python 的 `ui.show_thinking` 缺省一致），启动期由宿主按配置覆盖。
            show_thinking: true,
            logo: LogoAnimation::new(),
            pending_inputs: VecDeque::new(),
            pending_queue_expanded: false,
            expanded_tools: HashSet::new(),
            expanded_reasoning: HashSet::new(),
            subagent_stream: false,
            batch: None,
            carousel: Carousel::new(),
            carousel_text: StyledText::new(),
            carousel_since: Instant::now(),
            carousel_anim_at: None,
            carousel_ready: false,
            carousel_rand: Rng::new(CAROUSEL_SEED),
            carousel_lines: load_carousel_message_lines(),
            selection: None,
            runtime_esc_hover: false,
            sessions_menu: SessionsMenu::new(),
        };
        // 首帧先把轮播文本装好，渲染路径保持只读：即使宿主一次都没 tick 过，
        // 底部 HUD 也有内容可画（测试直接构造 AppState 时会走这条路径）。
        state.refresh_carousel(Instant::now(), "");
        state
    }

    /// 轮播数据源快照。<`reasoning_effort`> 不在界面状态里（它属于模型配置），
    /// 由宿主在 tick 时传入。
    fn carousel_source(&self, reasoning_effort: &str) -> CarouselSource {
        CarouselSource {
            workspace: self.project.clone(),
            input_tokens: self.telemetry.input_tokens as i64,
            output_tokens: self.telemetry.output_tokens as i64,
            cached_input_tokens: self.telemetry.cached_input_tokens as i64,
            // 总量未知时传 0：对映层与 Python 一样按 max(1) 兜底，不编造分母。
            context_limit: self.telemetry.context_window.unwrap_or(0) as i64,
            tokens_per_second: self.telemetry.rate.value().unwrap_or(0.0),
            model: self.model.clone(),
            reasoning_effort: reasoning_effort.to_string(),
            approval_mode: self.approval.label().to_string(),
            mcp_enabled_count: self.mcp_servers as i64,
            pending_count: self.pending_inputs.len() as i64,
        }
    }

    /// 推进底部单行轮播：停留到点换页，动画中按帧距推进解密扫描。
    ///
    /// 定时器由装配层持有（对映 Python 把停留/帧定时器交给 Textual 的做法），
    /// 这里只吃「现在几点」并更新 [`AppState::carousel_text`]。
    pub fn refresh_carousel(&mut self, now: Instant, reasoning_effort: &str) {
        let source = self.carousel_source(reasoning_effort);
        if self.carousel.is_animating() {
            let due = self
                .carousel_anim_at
                .map(|last| {
                    now.duration_since(last).as_secs_f64() >= CAROUSEL_ANIMATION_FRAME_SECONDS
                })
                .unwrap_or(true);
            if !due {
                return;
            }
            self.carousel_anim_at = Some(now);
            match self.carousel.animation_tick(
                &source,
                &self.carousel_lines,
                &mut self.carousel_rand,
            ) {
                CarouselTick::Frame(text) => self.carousel_text = text,
                CarouselTick::Settled(text) => {
                    self.carousel_text = text;
                    // 动画收口后重新开始本页的停留计时。
                    self.carousel_since = now;
                }
            }
            return;
        }
        if !self.carousel_ready {
            self.carousel_ready = true;
            self.carousel_since = now;
        } else if now.duration_since(self.carousel_since).as_secs_f64()
            >= self.carousel.page_duration()
        {
            let next = self.carousel.next_page();
            self.carousel.switch_to(
                next,
                true,
                &source,
                &self.carousel_lines,
                &mut self.carousel_rand,
            );
            self.carousel_anim_at = None;
            self.carousel_since = now;
            // 动画帧由下一帧推进，本帧仍是旧页文本（与 Python 切换当帧的可见状态一致）。
            return;
        }
        // 稳态：每帧按最新遥测重建当前页（对映 Python 的 `_carousel_refresh`，
        // 否则 token 计数要在下一次换页才会追上）。
        self.carousel_text =
            self.carousel
                .display_text(&source, &self.carousel_lines, &mut self.carousel_rand);
    }

    /// 当前轮播页（测试与调试用）。
    pub fn carousel_page(&self) -> CarouselPage {
        self.carousel.page()
    }

    pub fn waiting(&self) -> Option<&Waiting> {
        self.batch.as_ref().and_then(|batch| batch.waiting())
    }

    /// 待决审批对应的调用（工具名 + 当前参数）；没有待决审批时 `None`。
    ///
    /// 拒绝时的 MCP 审计要用它，而且要用批次里的**当前**参数：插件可能在
    /// `tool.call.before` 里改写过参数，审计应与用户看到的、执行层拿到的版本一致。
    pub fn pending_approval_call(&self) -> Option<omnicrawl_core::ToolCall> {
        let batch = self.batch.as_ref()?;
        if !matches!(batch.waiting(), Some(host::Waiting::Approval(_))) {
            return None;
        }
        batch.current().map(|(_, call)| call.clone())
    }

    /// 待决批次的请求 id：界面把决定回给内核时要用它配对。
    pub fn batch_request_id(&self) -> Option<Id> {
        self.batch.as_ref().map(|batch| batch.request_id().clone())
    }

    /// 开始一个回合：用户消息落进消息流，输入框清空，滚动回到底部。
    pub fn begin_turn(&mut self, turn_id: String, text: String) {
        self.records.push(Record::User(text));
        self.turn = TurnState::Running { turn_id };
        self.turn_started = Some(Instant::now());
        self.status = None;
        self.paused = false;
        self.scroll_from_bottom = 0;
    }

    /// 提交输入框内容；空输入返回 `None`。
    pub fn submit(&mut self) -> Option<String> {
        if self.composer.is_empty() {
            return None;
        }
        Some(self.composer.take())
    }

    /// 生成期间按 Enter：把输入排进 FIFO 队列（对映 Python `_pending_inputs.append`）。
    pub fn queue_pending(&mut self, text: String) {
        self.pending_inputs.push_back(text);
        self.sync_queue_expanded();
    }

    /// 撤回第 `index` 条排队消息并回填输入框；返回是否真的撤回。
    ///
    /// 守卫与 Python `_withdraw_pending_input` 一致：只在回合进行中、索引有效、
    /// 且当前没有待回答的提问（question / 确认）时执行——否则回填的内容会被
    /// 提问模式吞掉。撤回只回填、不自动发送，用户改完再按 Enter。
    pub fn withdraw_pending(&mut self, index: usize) -> bool {
        if !self.turn.is_running() || index >= self.pending_inputs.len() {
            return false;
        }
        if self.waiting().is_some() {
            return false;
        }
        let Some(text) = self.pending_inputs.remove(index) else {
            return false;
        };
        self.composer.set_text(&text);
        self.sync_queue_expanded();
        true
    }

    /// 展开/收起排队预览中被折叠的条目；不超过可见上限时不动作。
    pub fn toggle_queue_expanded(&mut self) {
        if !queue::can_toggle_queue_expanded(self.pending_inputs.len(), QUEUE_PREVIEW_MAX_ROWS) {
            return;
        }
        self.pending_queue_expanded = !self.pending_queue_expanded;
    }

    /// 队列缩回可见上限内时退出展开态，避免残留无效的展开/收起行。
    fn sync_queue_expanded(&mut self) {
        if self.pending_inputs.len() <= QUEUE_PREVIEW_MAX_ROWS {
            self.pending_queue_expanded = false;
        }
    }

    /// 取队首消息准备提交；回合进行中时不取（对映 Python 的排空循环守卫）。
    ///
    /// 模态页（设置面板）是否放行由调用方判断，与 Python 的
    /// `len(self.screen_stack) == 1` 守卫同义。
    pub fn take_next_pending(&mut self) -> Option<String> {
        if self.turn.is_running() {
            return None;
        }
        let text = self.pending_inputs.pop_front()?;
        self.sync_queue_expanded();
        Some(text)
    }

    /// 丢开全部排队消息（退出前收尾，对映 Python `_pending_inputs.clear()`）。
    pub fn clear_pending(&mut self) {
        self.pending_inputs.clear();
        self.pending_queue_expanded = false;
    }

    /// 展开一张工具卡的完整正文；提示行点击触发。
    pub fn expand_tool(&mut self, call_id: &str) {
        if call_id.is_empty() {
            return;
        }
        self.expanded_tools.insert(call_id.to_string());
    }

    /// 点开着的工具卡收起；返回是否真的从展开态收了回去。
    ///
    /// 对映 Python `ToolDisclosure.on_click`：缩略态点卡片不做任何事（展开只能
    /// 通过提示行），所以这里没有 toggle，只有单向收起。
    pub fn collapse_tool(&mut self, call_id: &str) -> bool {
        if call_id.is_empty() {
            return false;
        }
        self.expanded_tools.remove(call_id)
    }

    pub fn is_tool_expanded(&self, call_id: &str) -> bool {
        self.expanded_tools.contains(call_id)
    }

    /// 点击思考段：在折叠与展开之间切换；返回切换后的展开态。
    pub fn toggle_reasoning_expanded(&mut self, index: usize) -> bool {
        if self.expanded_reasoning.remove(&index) {
            return false;
        }
        self.expanded_reasoning.insert(index);
        true
    }

    pub fn is_reasoning_expanded(&self, index: usize) -> bool {
        self.expanded_reasoning.contains(&index)
    }

    /// 内核事件 → 消息流。
    pub fn apply(&mut self, event: &HostEvent, now: Instant) {
        match event {
            HostEvent::Delta(payload) => {
                self.telemetry.rate.record(&payload.text, now);
                self.append_streamed(&payload.text, false);
            }
            HostEvent::ReasoningDelta(payload) => {
                self.telemetry.rate.record(&payload.text, now);
                self.append_streamed(&payload.text, true);
            }
            HostEvent::Status(payload) | HostEvent::RetryStatus(payload) => {
                self.status = Some(payload.message.clone());
            }
            HostEvent::ProtocolWait => self.status = Some("等待协议…".to_string()),
            HostEvent::StreamRollback => {
                if matches!(self.records.last(), Some(Record::Assistant(_))) {
                    self.records.pop();
                }
                self.telemetry.rate.rollback();
            }
            HostEvent::TokenUsage(payload) => {
                // 与 Python HUD 同口径（`max(0, int(...))`）：展示层不显示负值，
                // 但协议与归一化层仍保留上游给的原值。
                self.telemetry.input_tokens = payload.input_tokens.max(0) as u64;
                self.telemetry.output_tokens = payload.output_tokens.max(0) as u64;
                self.telemetry.cached_input_tokens = payload.cached_input_tokens.max(0) as u64;
            }
            HostEvent::ToolCallStarted(payload) => {
                // 模型刚开始写这个调用的参数：卡片先立起来，参数随后逐段补
                // （Python 只在批次执行时才画卡片，这里是 Rust 侧刻意的增量渲染）。
                self.streaming_tools.insert(
                    payload.call_id.clone(),
                    StreamingTool {
                        text: String::new(),
                        arguments: serde_json::Value::Object(serde_json::Map::new()),
                        compression: None,
                        streaming: true,
                    },
                );
                if self.tool_card_mut(&payload.call_id).is_none() {
                    self.records.push(Record::Tool(ToolCard {
                        call_id: payload.call_id.clone(),
                        name: payload.tool.clone(),
                        summary: String::new(),
                        arguments: serde_json::Value::Object(serde_json::Map::new()),
                        status: ToolStatus::Running,
                        elapsed: None,
                        started: now,
                        body: Vec::new(),
                    }));
                }
            }
            HostEvent::ToolCallArguments(payload) => {
                let parsed = {
                    let Some(entry) = self.streaming_tools.get_mut(&payload.call_id) else {
                        return;
                    };
                    entry.text.push_str(&payload.delta);
                    entry.arguments = partial_arguments(&entry.text);
                    entry.arguments.clone()
                };
                if let Some(card) = self.tool_card_mut(&payload.call_id) {
                    card.summary = parsed
                        .as_object()
                        .map(host::summarize_arguments)
                        .unwrap_or_default();
                }
            }
            HostEvent::ToolOutputCompression(payload) => {
                let finished = payload.phase == "finished";
                if finished {
                    // 压缩完成后用**压缩后的正文替换**卡片里的原始输出（用户要求）：
                    // 卡片正文、折叠统计与「已压缩 a → b 字符」的提示都基于压缩文本。
                    if let Some(card) = self.tool_card_mut(&payload.call_id) {
                        card.body = body_lines(&payload.output);
                    }
                }
                // 压缩发生在工具调用之后，所以这里可以放心新建侧信道条目：
                // 卡片此时已经是终态，提示行按「完成后才显示」的规则渲染。
                self.streaming_tools
                    .entry(payload.call_id.clone())
                    .or_insert_with(|| StreamingTool {
                        text: String::new(),
                        arguments: serde_json::Value::Object(serde_json::Map::new()),
                        compression: None,
                        streaming: false,
                    })
                    .compression = Some(if finished {
                    format!(
                        "已压缩 {} → {} 字符",
                        thousands(payload.before_chars),
                        thousands(payload.after_chars)
                    )
                } else {
                    "正在压缩…".to_string()
                });
            }
            HostEvent::ToolStarted(payload) => {
                // 流式阶段已经建过卡片的（同 call_id）：就地更新，保持卡片出现的先后顺序，
                // 用户已经看到的行也不会闪一下再重排。
                if self.streaming_tools.contains_key(&payload.call.id) {
                    if let Some(entry) = self.streaming_tools.get_mut(&payload.call.id) {
                        entry.streaming = false;
                    }
                }
                if let Some(card) = self.tool_card_mut(&payload.call.id) {
                    card.name = payload.call.name.clone();
                    card.arguments = serde_json::Value::Object(payload.call.arguments.clone());
                    card.summary = host::summarize_arguments(&payload.call.arguments);
                    return;
                }
                self.records.push(Record::Tool(ToolCard {
                    call_id: payload.call.id.clone(),
                    name: payload.call.name.clone(),
                    summary: host::summarize_arguments(&payload.call.arguments),
                    arguments: serde_json::Value::Object(payload.call.arguments.clone()),
                    status: ToolStatus::Running,
                    elapsed: None,
                    started: now,
                    body: Vec::new(),
                }));
            }
            HostEvent::ToolFinished(payload) => {
                self.update_tool(&payload.call, &payload.result, now);
                // 批次执行结束：清掉「仍在写参数」的标记，但**保留**侧信道条目，
                // 压缩阶段的通知随后还要往它上面写提示（并替换卡片正文）。
                if let Some(entry) = self.streaming_tools.get_mut(&payload.call.id) {
                    entry.streaming = false;
                }
            }
            HostEvent::ToolOutputUpdate(payload) => {
                if let Some(card) = self.tool_card_mut(&payload.call.id) {
                    card.body = body_lines(&payload.result.output);
                }
            }
            HostEvent::SubagentEvent(payload) => {
                self.apply_subagent_event(&payload.name, &payload.payload);
            }
            HostEvent::TodoUpdate(payload) => {
                if let Some(items) = payload.todos.as_array() {
                    let mut map = serde_json::Map::new();
                    map.insert("todos".to_string(), serde_json::Value::Array(items.clone()));
                    self.todos = host::parse_todos(&map);
                }
            }
            HostEvent::TurnFinished(payload) => {
                self.streaming_tools.clear();
                self.turn = TurnState::Idle;
                self.turn_started = None;
                self.status = None;
                // 状态行随回合结束消失：悬停态一并复位，避免下一回合复用旧高亮。
                self.runtime_esc_hover = false;
                if payload.paused {
                    self.records.push(Record::Notice(
                        "已被模型暂停：本回合不再自动继续。".to_string(),
                    ));
                }
            }
            // 压缩计量与模型 Hook 触发点只供宿主分发插件 Hook，不改动对话视图；
            // 压缩的可见边界仍由 `turn.status` 的提示呈现。
            HostEvent::ContextCompaction(_)
            | HostEvent::ModelResponseAfter(_)
            | HostEvent::ModelRequestError(_) => {}
        }
    }

    /// 追加一条系统消息；不改动回合状态。
    pub fn notice(&mut self, message: String) {
        self.records.push(Record::Notice(message));
    }

    /// 当前文本选区（鼠标拖选）；没有选区时返回 `None`。
    pub fn selection(&self) -> Option<TextSelection> {
        self.selection
    }

    /// 按下左键：开一个新选区的锚点（`line` 为显示行下标，`column` 为行内显示列）。
    pub fn begin_selection(&mut self, line: usize, column: usize) {
        self.selection = Some(TextSelection {
            anchor: (line, column),
            head: (line, column),
        });
    }

    /// 拖动中：把选区的另一端移到新位置。
    pub fn extend_selection(&mut self, line: usize, column: usize) {
        if let Some(selection) = self.selection.as_mut() {
            selection.head = (line, column);
        }
    }

    /// 清掉选区（Esc / 点击空白 / 复制完成后）。
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 打开 `/sessions` 的会话选择菜单（输入框上方）。
    pub fn open_sessions_menu(&mut self, items: Vec<SessionMenuItem>) {
        self.sessions_menu.open(items);
    }

    /// 追加一条后台任务日志（工具卡形状）；不改动回合状态。
    ///
    /// 对映 Python 把 Monitor 增量批次当 `tool` 消息追加进对话区：`call_id` 用
    /// `monitor:<id>` 前缀，避免与真实工具调用的 id 相撞（工具卡靠它做展开/收起）。
    pub fn push_monitor_batch(&mut self, monitor_id: &str, status: &str, text: String) {
        self.records.push(Record::Tool(ToolCard {
            call_id: format!("monitor:{monitor_id}"),
            name: "monitor".to_string(),
            summary: monitor_id.to_string(),
            // Monitor 批次不是模型发起的工具调用，没有参数可拼标题。
            arguments: serde_json::Value::Null,
            status: monitor_status(status),
            elapsed: None,
            started: Instant::now(),
            body: text.lines().map(str::to_string).collect(),
        }));
    }

    /// 用内核回给的会话历史重建对话视图（`/resume` 与 `/undo` 后的重放）。
    ///
    /// 只投影 user/assistant 的**文本**消息：工具卡、推理段、通知与子任务进度树属于
    /// 「本进程这次运行」的观感，转录里没有它们的等价物，因此重放后它们自然消失——
    /// 这正是 `/undo` 要的效果（被撤回的消息与工具卡不能留着）。内容是多模态数组或
    /// 非字符串的消息跳过，不把 JSON 塞进消息流。
    pub fn replay_history(&mut self, history: &[serde_json::Value]) {
        self.records.clear();
        for message in history {
            let role = message
                .get("role")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let Some(content) = message.get("content").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            match role {
                "user" => self.records.push(Record::User(content.to_string())),
                "assistant" => self.records.push(Record::Assistant(content.to_string())),
                _ => {}
            }
        }
        self.scroll_to_bottom();
    }

    /// 按持久化事件流重建对话视图（对映 Python `_replay_session_events`）。
    ///
    /// 与 [`Self::replay_history`] 的分工：消息投影把工具请求/结果写成了 assistant 文本，
    /// 只够恢复文字；历史页面必须消费事件流，才能还原工具卡、结果状态、计划清单与
    /// SubAgent 进度树。事件流为空是权威结果（例如 `/undo` 撤掉了唯一一轮），因此必须
    /// 清空视图，而不是保留旧消息。
    pub fn replay_events(&mut self, events: &[serde_json::Value]) {
        self.records.clear();
        // 未收口的工具卡：`(call_id, tool, 记录下标, 请求时间)`。没有 call id 的旧事件按
        // 工具名延后匹配（与 Python 的 `pending_by_id` / `pending_by_tool` 同口径）。
        let mut pending: Vec<(String, String, usize, Option<DateTime<Utc>>)> = Vec::new();
        // 被拒绝的调用：`call_id` 或 `tool:<name>` → 原因；收口时优先用它当正文。
        let mut denied: Vec<(String, String)> = Vec::new();
        for event in events {
            let event_type = event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let payload = match event.get("payload") {
                Some(serde_json::Value::Object(_)) => event["payload"].clone(),
                _ => serde_json::json!({}),
            };
            match event_type {
                "user_message" => {
                    if let Some(content) = replay_text(&payload, "content") {
                        self.records.push(Record::User(content));
                    }
                }
                "assistant_message" => {
                    // 压缩后的会话正文放在 `session_content`，回放优先读它。
                    let content = replay_text(&payload, "session_content")
                        .or_else(|| replay_text(&payload, "content"));
                    if let Some(content) = content {
                        self.records.push(Record::Assistant(content));
                    }
                }
                "tool_call_requested" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    if tool.is_empty() {
                        continue;
                    }
                    if tool == host::TODO_TOOL {
                        // 计划清单是展示层状态，不在会话区生成工具卡（对映 Python
                        // `_handle_todo_update` 分支）。
                        let mut map = serde_json::Map::new();
                        let todos = payload
                            .get("arguments")
                            .and_then(|arguments| arguments.get("todos"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!([]));
                        map.insert("todos".to_string(), todos);
                        self.todos = host::parse_todos(&map);
                        continue;
                    }
                    if tool == host::ASK_USER_TOOL {
                        continue;
                    }
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    let empty = serde_json::Map::new();
                    let arguments = payload
                        .get("arguments")
                        .and_then(serde_json::Value::as_object)
                        .unwrap_or(&empty);
                    let summary = host::summarize_arguments(arguments);
                    let index = self.records.len();
                    self.records.push(Record::Tool(ToolCard {
                        call_id: call_id.clone(),
                        name: tool.clone(),
                        summary,
                        arguments: payload
                            .get("arguments")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        status: ToolStatus::Running,
                        elapsed: None,
                        started: Instant::now(),
                        body: Vec::new(),
                    }));
                    pending.push((call_id, tool, index, replay_time(event)));
                }
                "tool_call_denied" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    let reason = replay_text(&payload, "reason")
                        .unwrap_or_else(|| "工具调用未获批准。".to_string());
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    if !call_id.is_empty() {
                        denied.push((call_id, reason));
                    } else if !tool.is_empty() {
                        denied.push((format!("tool:{tool}"), reason));
                    }
                }
                "tool_result" => {
                    let tool = replay_text(&payload, "tool").unwrap_or_default();
                    if tool == host::TODO_TOOL || tool == host::ASK_USER_TOOL {
                        continue;
                    }
                    let call_id = replay_text(&payload, "tool_call_id").unwrap_or_default();
                    // 先按 call id 配对，再按工具名兜底；两者都没有就补一张空参数卡。
                    let matched = pending
                        .iter()
                        .position(|(id, _, _, _)| !call_id.is_empty() && *id == call_id)
                        .or_else(|| pending.iter().position(|(_, name, _, _)| *name == tool));
                    let (index, started) = match matched {
                        Some(at) => {
                            let entry = pending.remove(at);
                            (entry.2, entry.3)
                        }
                        None => {
                            let index = self.records.len();
                            self.records.push(Record::Tool(ToolCard {
                                call_id: call_id.clone(),
                                name: if tool.is_empty() {
                                    "未知工具".to_string()
                                } else {
                                    tool.clone()
                                },
                                summary: String::new(),
                                // 没有配对的 tool_call_requested：没有参数可拼标题。
                                arguments: serde_json::Value::Null,
                                status: ToolStatus::Running,
                                elapsed: None,
                                started: Instant::now(),
                                body: Vec::new(),
                            }));
                            (index, replay_time(event))
                        }
                    };
                    let ok = payload
                        .get("ok")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let finished = replay_time(event);
                    if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                        card.status = if ok {
                            ToolStatus::Ok
                        } else {
                            ToolStatus::Failed
                        };
                        card.elapsed = replay_elapsed(started, finished);
                        card.body = body_lines(&replay_output(&payload));
                    }
                }
                other => {
                    if let Some(name) = replay_subagent_event_name(other) {
                        self.apply_subagent_event(name, &payload);
                    } else if other == "turn_cancelled" {
                        self.records.push(Record::Assistant(
                            replay_text(&payload, "summary").unwrap_or_else(|| {
                                "（上一回合被取消，未生成最终回复）".to_string()
                            }),
                        ));
                    } else if other == "session_interrupted" {
                        self.records
                            .push(Record::Notice("上一回合在会话恢复前中断。".to_string()));
                    } else if other == "compact_summary" {
                        if let Some(content) = replay_text(&payload, "content") {
                            self.records
                                .push(Record::Assistant(format!("会话压缩摘要：\n{content}")));
                        }
                    }
                }
            }
        }
        // 尾部：仍挂着的卡片按「被拒绝」或「会话结束前没有结果」收口（对映 Python
        // `_replay_session_events` 末尾遍历未收口组件的分支）。
        for (call_id, tool, index, _) in pending {
            let denial = denied
                .iter()
                .find(|(key, _)| !call_id.is_empty() && *key == call_id)
                .or_else(|| {
                    denied
                        .iter()
                        .find(|(key, _)| *key == format!("tool:{tool}"))
                });
            let (status, reason) = match denial {
                Some((_, reason)) => (ToolStatus::Denied, reason.clone()),
                None => (
                    ToolStatus::Failed,
                    "工具调用在会话结束前未收到结果。".to_string(),
                ),
            };
            if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                card.status = status;
                card.body = vec![reason];
            }
        }
        self.scroll_to_bottom();
    }

    /// 子任务事件 → 进度树：按 `batch_id` 找树，没有就新开一棵并挂到消息流末尾。
    ///
    /// 对映 Python `_handle_subagent_event`：只处理任务状态类事件（子代理对话、工具与
    /// 文本事件不进树），状态无法识别时同样忽略。载荷字段缺失时按 Python 的 `or` 口径
    /// 回落（`task_id` → `task`，`batch_id` → `batch-<task_id>`，`description` → `task_id`）。
    fn apply_subagent_event(&mut self, name: &str, payload: &serde_json::Value) {
        // `/review` 等派生评审流程开启流式态时，整批事件都进会话面板，不再画进度树
        // （对映 Python `_handle_subagent_event` 顶部的 `_conversation_stream_active` 早退）。
        if self.subagent_stream {
            self.apply_subagent_conversation(name, payload);
            return;
        }
        let Some(status) = subagent_status_for_event(name) else {
            return;
        };
        let task_id = payload_text(payload, "task_id").unwrap_or("task");
        let batch_id = payload_text(payload, "batch_id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("batch-{task_id}"));
        let agent_type = payload_text(payload, "agent_type").unwrap_or("subagent");
        let description = payload_text(payload, "description");
        let tree = match self
            .records
            .iter_mut()
            .rev()
            .find_map(|record| match record {
                Record::SubagentTree(tree) if tree.batch_id == batch_id => Some(tree),
                _ => None,
            }) {
            Some(tree) => tree,
            None => {
                self.records
                    .push(Record::SubagentTree(SubAgentProgressTree::new(&batch_id)));
                match self.records.last_mut() {
                    Some(Record::SubagentTree(tree)) => tree,
                    // 刚压入的记录类型不会变；这里只作为不可能路径的兜底。
                    _ => return,
                }
            }
        };
        tree.update_task(
            task_id,
            agent_type,
            description.unwrap_or(task_id),
            status,
            None,
        );
    }

    /// 子代理对话流事件 → 按批次挂一块流式对话面板。
    ///
    /// 对映 Python `_handle_subagent_conversation_event`：首次事件建面板，
    /// 之后 `turn.text` / `tool.started` / `tool.completed` 逐行追加，
    /// 任务终态收口面板。只有流式态（`subagent_stream`）下才走这条路径。
    fn apply_subagent_conversation(&mut self, name: &str, payload: &serde_json::Value) {
        let task_id = payload_text(payload, "task_id").unwrap_or("task");
        let batch_id = payload_text(payload, "batch_id")
            .map(str::to_string)
            .unwrap_or_else(|| format!("batch-{task_id}"));
        let has_panel = self.records.iter().any(|record| {
            matches!(record, Record::SubagentConversation(panel) if panel.batch_id == batch_id)
        });
        if !has_panel {
            let agent_type = payload_text(payload, "agent_type").unwrap_or("subagent");
            self.records
                .push(Record::SubagentConversation(SubAgentConversation::new(
                    &batch_id, agent_type,
                )));
        }
        let Some(panel) = self
            .records
            .iter_mut()
            .rev()
            .find_map(|record| match record {
                Record::SubagentConversation(panel) if panel.batch_id == batch_id => Some(panel),
                _ => None,
            })
        else {
            return;
        };
        match name {
            "subagent.turn.text" => {
                let text = payload
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        panel.append(line, "");
                    }
                }
            }
            "subagent.tool.started" => {
                let tool = payload_text(payload, "tool").unwrap_or_default();
                panel.append(
                    &format!("⌁ {}", subagent_tool_brief(tool, payload.get("arguments"))),
                    "",
                );
            }
            "subagent.tool.completed" => {
                let ok = payload
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let suffix = payload
                    .get("duration_seconds")
                    .and_then(serde_json::Value::as_f64)
                    .map(|seconds| format!(" · {seconds:.2}s"))
                    .unwrap_or_default();
                panel.append(
                    &format!("● {}{suffix}", if ok { "成功" } else { "失败" }),
                    if ok { "green" } else { "red" },
                );
                let output = payload
                    .get("output")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                for line in sample_output_lines(output) {
                    panel.append(&line, "dim");
                }
            }
            "subagent.task.completed" => panel.finish("✓ 子代理评审完成"),
            "subagent.task.cancelled" | "subagent.task.approval_cancelled" => {
                panel.finish("– 子代理评审已取消")
            }
            "subagent.task.failed" => panel.finish(&subagent_failure_line(payload)),
            _ => {}
        }
    }

    /// 推进仍活跃的进度树的运行耗时（对映 Python 的耗时 tick）：终态树不再重绘。
    ///
    /// 返回是否还有活跃树需要继续 tick；耗时跨过整秒前可见文本不变，而渲染结果
    /// 相同的刷新由 [`SubAgentProgressTree::refresh_elapsed`] 内部自行跳过重绘。
    pub fn refresh_subagent_trees(&mut self) -> bool {
        let mut active = false;
        for record in self.records.iter_mut() {
            if let Record::SubagentTree(tree) = record {
                if tree.is_active() {
                    active = true;
                    tree.refresh_elapsed(None);
                }
            }
        }
        active
    }

    /// 回合失败或取消：状态复位并把原因写进消息流。
    pub fn fail_turn(&mut self, message: String) {
        self.turn = TurnState::Idle;
        self.turn_started = None;
        self.status = None;
        self.records.push(Record::Notice(message));
    }

    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(delta.unsigned_abs());
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(delta as usize);
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_from_bottom = 0;
    }

    /// 在输入框上方那一行显示一条瞬时提示（拖选复制等）。
    pub fn show_notice_line(&mut self, text: impl Into<String>, now: Instant) {
        self.notice_line = Some((text.into(), now));
    }

    /// 当前该显示的提示行文本；超过存活时间返回 `None`。
    pub fn notice_line_text(&self, now: Instant) -> Option<&str> {
        let (text, at) = self.notice_line.as_ref()?;
        if now.saturating_duration_since(*at) > NOTICE_LINE_LINGER {
            return None;
        }
        Some(text.as_str())
    }

    /// 宿主每帧调一次：把过期的提示行真的清掉（渲染只读，不负责回收）。
    /// 流式 / 压缩阶段的工具调用侧信道状态（渲染只读）。
    pub fn streaming_tool(&self, call_id: &str) -> Option<&StreamingTool> {
        self.streaming_tools.get(call_id)
    }

    pub fn tick_notice_line(&mut self, now: Instant) {
        if self.notice_line.is_some() && self.notice_line_text(now).is_none() {
            self.notice_line = None;
        }
    }

    /// 开始处理一个工具批次，推进到等待点、执行点或整批结束。
    pub fn start_batch(
        &mut self,
        request_id: Id,
        calls: Vec<omnicrawl_core::ToolCall>,
    ) -> host::BatchStep {
        let mut batch = host::PendingBatch::new(request_id, calls);
        let step = {
            let mut ctx = self.context();
            batch.advance(&mut ctx)
        };
        self.batch = Some(batch);
        step
    }

    pub fn select_question(&mut self, delta: isize) {
        if let Some(batch) = self.batch.as_mut() {
            batch.select_question(delta);
        }
    }

    /// 回答待决提问；返回下一步（派发执行或整批结束）。
    pub fn answer_question(&mut self, answer: String) -> Option<host::BatchStep> {
        let mut batch = self.batch.take()?;
        let step = {
            let mut ctx = self.context();
            batch.answer(answer, &mut ctx)
        };
        let step = step?;
        self.batch = Some(batch);
        Some(step)
    }

    /// 审批待决工具调用；返回下一步（派发执行或整批结束）。
    pub fn decide_approval(&mut self, approved: bool) -> Option<host::BatchStep> {
        let mut batch = self.batch.take()?;
        let step = {
            let mut ctx = self.context();
            batch.decide(approved, &mut ctx)
        };
        let step = step?;
        self.batch = Some(batch);
        Some(step)
    }

    /// 插件改写调用参数（`tool.call.before` 的 transform 结局）；无批次时返回 `false`。
    pub fn rewrite_call_arguments(
        &mut self,
        index: usize,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> bool {
        match self.batch.as_mut() {
            Some(batch) => batch.rewrite_arguments(index, arguments),
            None => false,
        }
    }

    /// 插件守卫当前待决审批。
    ///
    /// `guard` 返回 `Ok(Some(arguments))` 表示插件改写了参数（回写完继续弹审批面板）；
    /// `Ok(None)` 表示放行；`Err(reason)` 表示拒绝（写入插件给出的文案并推进批次，
    /// 与 `decide_approval(false)` 的差别只在拒绝文案来源）。返回值 `Some(step)` 表示
    /// 批次已被推进，调用方要接着按新步骤处理。
    pub fn guard_pending_approval<F>(&mut self, guard: F) -> Option<host::BatchStep>
    where
        F: FnOnce(
            &omnicrawl_core::ToolCall,
        ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String>,
    {
        let mut batch = self.batch.take()?;
        let current = match batch.current() {
            Some((index, call)) if matches!(batch.waiting(), Some(host::Waiting::Approval(_))) => {
                Some((index, call.clone()))
            }
            _ => None,
        };
        let Some((index, call)) = current else {
            self.batch = Some(batch);
            return None;
        };
        match guard(&call) {
            Ok(Some(arguments)) => {
                batch.rewrite_arguments(index, arguments);
                self.batch = Some(batch);
                None
            }
            Ok(None) => {
                self.batch = Some(batch);
                None
            }
            Err(reason) => {
                batch.record_result(index, host::denied_with_reason(&reason), None);
                let step = {
                    let mut ctx = self.context();
                    batch.decide(true, &mut ctx)
                };
                self.batch = Some(batch);
                step
            }
        }
    }

    /// 执行层回填一个调用的结果；返回是否整批就绪。
    pub fn record_tool_result(
        &mut self,
        index: usize,
        result: omnicrawl_core::ToolResult,
        vision: Option<host::VisionPayload>,
    ) -> bool {
        match self.batch.as_mut() {
            Some(batch) => batch.record_result(index, result, vision),
            None => false,
        }
    }

    /// 整批就绪时取走观察并卸下批次；`attach_images` 决定是否把图片注入下一步请求。
    pub fn take_observations(&mut self, attach_images: bool) -> Option<Vec<AgentLoopObservation>> {
        if !self.batch.as_ref().is_some_and(|batch| batch.is_ready()) {
            return None;
        }
        let batch = self.batch.take()?;
        Some(batch.observations(attach_images))
    }

    /// 执行超时：把未回填的调用写成超时结果、把仍在运行的工具卡收口，并留一条提示；
    /// 返回是否整批就绪。
    pub fn fill_tool_timeout(&mut self, timeout_seconds: i64) -> bool {
        let Some(batch) = self.batch.as_mut() else {
            return false;
        };
        let timed_out = batch.fill_timeout(timeout_seconds);
        let ready = batch.is_ready();
        let now = Instant::now();
        for (_, call) in &timed_out {
            // 与 Python `_tool_timeout_result` 一致：只有 ok=false 与文案，没有错误码。
            let result = omnicrawl_core::ToolResult {
                ok: false,
                output: format!("工具执行超时（超过 {timeout_seconds} 秒未完成），已中止等待。"),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            };
            self.update_tool(call, &result, now);
        }
        if !timed_out.is_empty() {
            self.records.push(Record::Notice(format!(
                "工具执行超过 {timeout_seconds} 秒未完成，已按超时继续（后台结果会被丢弃）。"
            )));
        }
        ready
    }

    /// 取消当前批次（`Esc`）：批次卸下，已在执行的工具由取消令牌回收。
    pub fn cancel_batch(&mut self) {
        self.batch = None;
    }

    /// 执行阶段开始：先落一张「运行中」工具卡。
    pub fn begin_tool_run(&mut self, call: &omnicrawl_core::ToolCall, now: Instant) {
        self.records.push(Record::Tool(ToolCard {
            call_id: call.id.clone(),
            name: call.name.clone(),
            summary: host::summarize_arguments(&call.arguments),
            arguments: serde_json::Value::Object(call.arguments.clone()),
            status: ToolStatus::Running,
            elapsed: None,
            started: now,
            body: Vec::new(),
        }));
    }

    /// 执行阶段结束：更新对应工具卡的状态、耗时与正文。
    pub fn finish_tool_run(
        &mut self,
        call: &omnicrawl_core::ToolCall,
        result: &omnicrawl_core::ToolResult,
        now: Instant,
    ) {
        self.update_tool(call, result, now);
    }

    fn context(&mut self) -> BatchContext<'_> {
        BatchContext {
            approval: self.approval,
            todos: &mut self.todos,
            paused: &mut self.paused,
            // TUI 侧没有宿主工具事实表可给：删除意图识别按无事实处理。
            tools: None,
        }
    }

    fn append_streamed(&mut self, text: &str, reasoning: bool) {
        let target = |record: &Record| match record {
            Record::Reasoning(_) => reasoning,
            Record::Assistant(_) => !reasoning,
            _ => false,
        };
        match self.records.last_mut() {
            Some(record) if target(record) => match record {
                Record::Reasoning(body) | Record::Assistant(body) => body.push_str(text),
                _ => unreachable!("target 只匹配思考与正文记录"),
            },
            _ => {
                let body = text.to_string();
                self.records.push(if reasoning {
                    Record::Reasoning(body)
                } else {
                    Record::Assistant(body)
                });
            }
        }
    }

    /// 按 `call_id` 找最近一张工具卡；调用没有 id 时退化为最近一张仍在运行的工具卡。
    fn tool_card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.records.iter_mut().rev().find_map(|record| {
            let Record::Tool(card) = record else {
                return None;
            };
            let hit = if call_id.is_empty() {
                card.status == ToolStatus::Running
            } else {
                card.call_id == call_id
            };
            hit.then_some(card)
        })
    }

    fn update_tool(
        &mut self,
        call: &omnicrawl_core::ToolCall,
        result: &omnicrawl_core::ToolResult,
        now: Instant,
    ) {
        let Some(card) = self.tool_card_mut(&call.id) else {
            return;
        };
        card.status = match (result.ok, result.error_code.as_deref()) {
            (true, _) => ToolStatus::Ok,
            (false, Some(host::DENIED)) => ToolStatus::Denied,
            (false, _) => ToolStatus::Failed,
        };
        card.elapsed = Some(now.saturating_duration_since(card.started));
        card.body = body_lines(&result.output);
    }
}

/// 从「可能还是半截」的 arguments 原文里尽力取出已经到达的字段。
///
/// 模型流里的 arguments 是分片到达的，中途必然是半截 JSON
/// （`{"path": "a.py", "content": "第一`）。但标题行与 `write_file` / `Edit_file` 的内容预览
/// 要跟着流一起长出来，所以这里做一次容错解析：先用 `serde_json` 正常解析，失败就按
/// 「补上收尾」的候选逐个再试（补未闭合的字符串 / 补未闭合的对象），最后退化成
/// 「截到最后一个完整字段」。
fn partial_arguments(text: &str) -> serde_json::Value {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        if value.is_object() {
            return value;
        }
    }
    let trimmed = text.trim_end();
    let quote = '"';
    let mut candidates = vec![
        format!("{trimmed}{quote}}}"),
        format!("{trimmed}}}"),
    ];
    if let Some(cut) = last_complete_field(trimmed) {
        // 截到最后一个完整字段：末尾那个逗号要去掉，否则补出来的对象带尾逗号仍然不合法。
        candidates.push(format!(
            "{}}}",
            trimmed[..cut].trim_end().trim_end_matches(',')
        ));
    }
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if value.is_object() {
                return value;
            }
        }
    }
    serde_json::Value::Object(serde_json::Map::new())
}

/// 半截 JSON 里最后一个「完整顶层字段」的结束位置（逗号之后），没有就返回 None。
///
/// 扫描时跟踪字符串状态：字段值内部的逗号不能当分隔符。
fn last_complete_field(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut last = None;
    for (index, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth <= 1 => last = Some(index + 1),
            _ => {}
        }
    }
    last
}

/// 千分位（`12345` → `12,345`）：压缩前后的字符数动辄五六位，分隔开才好读。
fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn body_lines(output: &str) -> Vec<String> {
    output.lines().map(|line| line.to_string()).collect()
}

/// 回放取文本字段：非空字符串才算命中（对映 Python 的 `isinstance(value, str) and value`）。
fn replay_text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 事件时间戳（`created_at`，RFC3339）；缺失或不可解析时为空。
fn replay_time(event: &serde_json::Value) -> Option<DateTime<Utc>> {
    event
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|value| value.with_timezone(&Utc))
}

/// 回放工具卡耗时：两端时间戳齐备且顺序正确时给出差值，否则不显示耗时。
fn replay_elapsed(
    started: Option<DateTime<Utc>>,
    finished: Option<DateTime<Utc>>,
) -> Option<Duration> {
    let delta = finished?.signed_duration_since(started?);
    let millis = delta.num_milliseconds();
    if millis < 0 {
        None
    } else {
        Some(Duration::from_millis(millis as u64))
    }
}

/// 回放取工具正文：优先 UI 正文，其次模型可见输出（与 Python `_replay_tool_output` 同口径）。
fn replay_output(payload: &serde_json::Value) -> String {
    for key in ["full_output", "output", "model_output", "output_preview"] {
        if let Some(value) = replay_text(payload, key) {
            return value;
        }
    }
    "无输出".to_string()
}

/// 转录里的 SubAgent 事件类型 → 实时通知名（对映 Python `subagent_event_names`）。
fn replay_subagent_event_name(event_type: &str) -> Option<&'static str> {
    match event_type {
        "subagent_task_queued" => Some("subagent.task.queued"),
        "subagent_task_started" => Some("subagent.task.started"),
        "subagent_task_running" => Some("subagent.task.running"),
        "subagent_task_waiting_approval" => Some("subagent.task.waiting_approval"),
        "subagent_task_completed" => Some("subagent.task.completed"),
        "subagent_task_failed" => Some("subagent.task.failed"),
        "subagent_task_cancelled" => Some("subagent.task.cancelled"),
        "subagent_task_approval_cancelled" => Some("subagent.task.approval_cancelled"),
        _ => None,
    }
}

/// 子任务事件名 → 进度树状态；对映 Python `_handle_subagent_event` 的 `status_by_event`。
///
/// 表外事件（`subagent.tool.*`、`subagent.turn.text` 等子代理对话流）不进树。
fn subagent_status_for_event(name: &str) -> Option<&'static str> {
    match name {
        "subagent.task.queued" => Some("queued"),
        "subagent.task.started" | "subagent.task.running" => Some("running"),
        "subagent.task.waiting_approval" => Some("waiting_approval"),
        "subagent.task.completed" => Some("completed"),
        "subagent.task.failed" => Some("failed"),
        "subagent.task.cancelled" | "subagent.task.approval_cancelled" => Some("cancelled"),
        _ => None,
    }
}

/// 取非空字符串字段；缺失、类型不符或空串都按「未提供」处理（对映 Python 的 `x or 默认值`）。
fn payload_text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 把子代理工具调用压成一行摘要（对映 Python `_subagent_tool_brief`）。
fn subagent_tool_brief(tool_name: &str, arguments: Option<&serde_json::Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(serde_json::Value::Object(map)) = arguments {
        if tool_name == "git" {
            if let Some(action) = map.get("action").and_then(serde_json::Value::as_str) {
                if !action.is_empty() {
                    parts.push(action.to_string());
                }
            }
            if let Some(serde_json::Value::Array(args)) = map.get("args") {
                parts.extend(args.iter().map(scalar_text));
            }
        } else {
            for key in ["path", "paths", "pattern", "query", "text", "scope"] {
                match map.get(key) {
                    Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
                        parts.push(text.trim().to_string());
                        break;
                    }
                    Some(serde_json::Value::Array(items)) if !items.is_empty() => {
                        parts.push(scalar_text(&items[0]));
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    let mut brief = parts.join(" ").trim().to_string();
    if brief.chars().count() > 60 {
        brief = brief.chars().take(57).collect::<String>();
        brief.push_str("...");
    }
    if brief.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} · {brief}")
    }
}

/// 对映 Python `str(value)` 的标量取值；容器退化为 JSON 文本。
fn scalar_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// 工具输出采样：最多五行，超出时保留首尾各两行（对映 Python `_sample_output_lines`）。
fn sample_output_lines(output: &str) -> Vec<String> {
    let lines: Vec<String> = output
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() <= 5 {
        return lines;
    }
    let mut sampled: Vec<String> = lines[..2].to_vec();
    sampled.push("…".to_string());
    sampled.extend(lines[lines.len() - 2..].iter().cloned());
    sampled
}

/// 失败面板的终态行（对映 Python 失败分支的「原因（分类）」拼法）。
fn subagent_failure_line(payload: &serde_json::Value) -> String {
    let mut line = "× 子代理评审失败".to_string();
    let Some(error) = payload.get("error") else {
        return line;
    };
    if let Some(reason) = error.get("message").and_then(serde_json::Value::as_str) {
        if !reason.trim().is_empty() {
            line.push_str(&format!("：{reason}"));
        }
    }
    let code = error
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let category = error
        .get("diagnostic")
        .and_then(|diagnostic| diagnostic.get("category"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let labels: Vec<&str> = [code, category]
        .into_iter()
        .filter(|label| !label.is_empty())
        .collect();
    if !labels.is_empty() {
        line.push_str(&format!("（{}）", labels.join("，")));
    }
    line
}

/// Token 估算：CJK 字符按 1 token，其他字符按 4 字符 1 token。
pub fn estimated_tokens(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_core::{ToolCall, ToolResult};
    use omnicrawl_ipc::bridge::{
        TextPayload, TodoUpdatePayload, TokenUsagePayload, ToolEventPayload, ToolStartedPayload,
        TurnFinishedPayload,
    };
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: json!({"path": "a.py"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: id.to_string(),
            function_name: name.to_string(),
        }
    }

    fn state() -> AppState {
        AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Manual,
        )
    }

    #[test]
    fn deltas_merge_into_one_record_per_segment() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: "想".into() }),
            now,
        );
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "一下".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "回答".into(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "结束".into(),
            }),
            now,
        );

        assert_eq!(
            state.records.len(),
            3,
            "用户 + 思考 + 正文：{:?}",
            state.records
        );
        assert_eq!(state.records[0], Record::User("问一句".to_string()));
        assert_eq!(state.records[1], Record::Reasoning("想一下".to_string()));
        assert_eq!(state.records[2], Record::Assistant("回答结束".to_string()));
    }

    #[test]
    fn tool_cards_track_status_body_and_elapsed() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "跑一下".to_string());
        let tool_call = call("c1", "bash");
        state.apply(
            &HostEvent::ToolStarted(ToolStartedPayload {
                step: 1,
                call: tool_call.clone(),
            }),
            now,
        );
        state.apply(
            &HostEvent::ToolFinished(ToolEventPayload {
                call: tool_call.clone(),
                result: ToolResult {
                    ok: false,
                    output: "第一行\n第二行".to_string(),
                    full_output: String::new(),
                    error_code: Some("denied".to_string()),
                    retryable: false,
                },
            }),
            now + Duration::from_millis(120),
        );

        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Denied);
                assert_eq!(card.body, vec!["第一行".to_string(), "第二行".to_string()]);
                assert_eq!(card.elapsed, Some(Duration::from_millis(120)));
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
    }

    #[test]
    fn stream_rollback_drops_half_streamed_record() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "半截".into(),
            }),
            now,
        );
        state.apply(&HostEvent::StreamRollback, now);
        assert_eq!(state.records.len(), 1, "正文记录应被撤销");
        assert!(state.telemetry.rate.value().is_none(), "回滚后速度归零");
    }

    #[test]
    fn token_usage_and_finish_reset_status() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string());
        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在分析".to_string(),
            }),
            now,
        );
        assert_eq!(state.status.as_deref(), Some("正在分析"));
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5,
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
            }),
            now,
        );
        assert!(!state.turn.is_running());
        assert_eq!(state.status, None);
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    #[test]
    fn todos_arrive_from_both_tool_and_notification() {
        let mut state = state();
        let now = Instant::now();
        let step = state.start_batch(
            Id::Number(1),
            vec![ToolCall {
                name: host::TODO_TOOL.to_string(),
                arguments: json!({"todos": [{"id": "1", "step": "写骨架", "completed": false}]})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                id: "c1".to_string(),
                function_name: host::TODO_TOOL.to_string(),
            }],
        );
        assert_eq!(step, host::BatchStep::Complete, "自持工具不需要用户介入");
        assert_eq!(state.todos.len(), 1);
        assert_eq!(state.todos[0].step, "写骨架");

        state.apply(
            &HostEvent::TodoUpdate(TodoUpdatePayload {
                todos: json!([{"id": "2", "step": "接审批", "completed": true}]),
            }),
            now,
        );
        assert_eq!(state.todos.len(), 1);
        assert!(state.todos[0].completed);
    }

    #[test]
    fn batch_waiting_for_approval_can_be_decided_through_state() {
        let mut state = state();
        let step = state.start_batch(Id::Number(9), vec![call("c1", "bash")]);
        assert_eq!(step, host::BatchStep::Awaiting, "manual 模式应停在审批");
        assert!(matches!(state.waiting(), Some(Waiting::Approval(_))));

        let jobs = match state.decide_approval(true).expect("批准后应有下一步") {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("批准后应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);

        // 执行层回填结果：整批就绪后取走观察。
        let call = jobs[0].1.clone();
        assert!(state.record_tool_result(
            jobs[0].0,
            omnicrawl_core::ToolResult {
                ok: true,
                output: "文件内容".to_string(),
                full_output: "文件内容".to_string(),
                error_code: None,
                retryable: false,
            },
            None
        ));
        let observations = state.take_observations(false).expect("整批就绪应能取观察");
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].tool_call.id, call.id);
        assert_eq!(observations[0].result.output, "文件内容");
        assert!(state.batch_request_id().is_none(), "整批结束后不再挂起");
    }

    #[test]
    fn cancelling_a_batch_drops_it_without_observations() {
        let mut state = state();
        state.start_batch(Id::Number(11), vec![call("c1", "bash")]);
        assert!(state.waiting().is_some());
        state.cancel_batch();
        assert!(state.waiting().is_none());
        assert!(state.take_observations(false).is_none());
    }

    #[test]
    fn tool_timeout_reaps_running_cards_and_notes_the_user() {
        let mut state = AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Auto,
        );
        let tool_call = call("c1", "bash");
        let step = state.start_batch(Id::Number(21), vec![tool_call.clone()]);
        let jobs = match step {
            host::BatchStep::Execute(jobs) => jobs,
            other => panic!("auto 模式应派发执行，实际：{other:?}"),
        };
        assert_eq!(jobs.len(), 1);
        state.begin_tool_run(&jobs[0].1, Instant::now());

        assert!(state.fill_tool_timeout(600), "超时回填后整批就绪");

        match state
            .records
            .iter()
            .rev()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed, "运行中的卡片要被收口");
                assert!(
                    card.body.iter().any(|line| line.contains("工具执行超时")),
                    "{:?}",
                    card.body
                );
            }
            other => panic!("应当有工具卡，实际：{other:?}"),
        }
        assert!(
            state.records.iter().any(
                |record| matches!(record, Record::Notice(text) if text.contains("已按超时继续"))
            ),
            "应当给用户一条超时提示：{:?}",
            state.records
        );
        let observations = state.take_observations(false).expect("整批就绪");
        assert!(observations[0].result.output.contains("工具执行超时"));
        assert_eq!(
            observations[0].result.error_code, None,
            "与 Python 一致：超时结果只有文案，不带错误码"
        );
    }

    #[test]
    fn tool_runs_are_rendered_as_cards() {
        let mut state = state();
        let now = Instant::now();
        let tool_call = call("c1", "read");
        state.begin_tool_run(&tool_call, now);
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Running);
                assert_eq!(card.name, "read");
                assert!(card.elapsed.is_none());
            }
            other => panic!("应落一张运行中的工具卡，实际：{other:?}"),
        }
        state.finish_tool_run(
            &tool_call,
            &omnicrawl_core::ToolResult {
                ok: false,
                output: "出错了".to_string(),
                full_output: "出错了".to_string(),
                error_code: Some("FS_EDIT_NOT_FOUND".to_string()),
                retryable: false,
            },
            now + Duration::from_millis(30),
        );
        match state.records.last() {
            Some(Record::Tool(card)) => {
                assert_eq!(card.status, ToolStatus::Failed);
                assert_eq!(card.body, vec!["出错了".to_string()]);
                assert_eq!(card.elapsed, Some(Duration::from_millis(30)));
            }
            other => panic!("应更新同一张工具卡，实际：{other:?}"),
        }
    }

    #[test]
    fn subagent_events_fill_one_tree_per_batch() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &subagent_event("subagent.task.queued", "t1", "batch-1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.started", "t1", "batch-1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.completed", "t1", "batch-1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.running", "t2", "batch-2"),
            now,
        );

        let trees: Vec<&SubAgentProgressTree> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::SubagentTree(tree) => Some(tree),
                _ => None,
            })
            .collect();
        assert_eq!(trees.len(), 2, "每个 batch_id 一棵树：{:?}", state.records);
        assert_eq!(trees[0].batch_id, "batch-1");
        assert!(!trees[0].is_active(), "批次 1 的任务已收口");
        assert!(trees[1].is_active(), "批次 2 仍是运行中");
        let rendered = trees[0].render_text(None).plain();
        assert!(rendered.contains("└─ ✓ "), "终态图标要落在树上：{rendered}");
        assert!(rendered.contains("· 完成"), "{rendered}");
        assert!(rendered.contains("1/1 完成"), "{rendered}");
    }

    #[test]
    fn subagent_event_falls_back_to_defaults_and_ignores_other_events() {
        let mut state = state();
        let now = Instant::now();
        // 子代理对话/工具事件不进树（对映 Python 的 status_by_event 表外分支）。
        state.apply(
            &subagent_event("subagent.tool.started", "t1", "batch-1"),
            now,
        );
        assert!(!state
            .records
            .iter()
            .any(|record| matches!(record, Record::SubagentTree(_))));

        state.apply(
            &HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
                name: "subagent.task.queued".to_string(),
                payload: json!({}),
            }),
            now,
        );
        match state.records.last() {
            Some(Record::SubagentTree(tree)) => {
                assert_eq!(tree.batch_id, "batch-task", "缺字段时回落 batch-<task_id>");
                let plain = tree.render_text(None).plain();
                assert!(plain.contains("└─ ○ task  subagent · 等待中"), "{plain}");
            }
            other => panic!("应当有一棵进度树，实际：{other:?}"),
        }
    }

    #[test]
    fn terminal_subagent_task_is_not_reopened_by_late_events() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &subagent_event("subagent.task.completed", "t1", "batch-1"),
            now,
        );
        state.apply(
            &subagent_event("subagent.task.running", "t1", "batch-1"),
            now,
        );
        let tree = match state.records.last() {
            Some(Record::SubagentTree(tree)) => tree,
            other => panic!("应当有一棵进度树，实际：{other:?}"),
        };
        assert!(
            tree.render_text(None).plain().contains("└─ ✓ "),
            "迟到的活动事件不得把已完成节点改回运行中"
        );
    }

    #[test]
    fn active_subagent_trees_refresh_their_elapsed_time() {
        let mut state = state();
        state.apply(
            &subagent_event("subagent.task.running", "t1", "batch-1"),
            Instant::now(),
        );
        assert!(
            state.refresh_subagent_trees(),
            "运行中的树每次 tick 都要重算耗时"
        );
        state.apply(
            &subagent_event("subagent.task.completed", "t1", "batch-1"),
            Instant::now(),
        );
        assert!(!state.refresh_subagent_trees(), "终态树不再需要耗时 tick");
    }

    #[test]
    fn streaming_subagent_events_fill_conversation_panel() {
        let mut state = state();
        state.subagent_stream = true;
        let now = Instant::now();
        // 任务开始：建对话面板，不建进度树。
        state.apply(
            &subagent_event("subagent.task.started", "t1", "batch-1"),
            now,
        );
        assert!(
            !state
                .records
                .iter()
                .any(|record| matches!(record, Record::SubagentTree(_))),
            "流式态不画进度树"
        );
        assert!(panel_text(&state).contains("子代理对话"));

        state.apply(
            &conversation_event(
                "subagent.turn.text",
                "t1",
                "batch-1",
                json!({"text": "正在看代码"}),
            ),
            now,
        );
        state.apply(
            &conversation_event(
                "subagent.tool.started",
                "t1",
                "batch-1",
                json!({"tool": "read", "arguments": {"path": "a.py"}}),
            ),
            now,
        );
        state.apply(
            &conversation_event(
                "subagent.tool.completed",
                "t1",
                "batch-1",
                json!({"tool": "read", "ok": true, "output": "文件内容：42", "duration_seconds": 0.5}),
            ),
            now,
        );
        let text = panel_text(&state);
        assert!(text.contains("正在看代码"));
        assert!(text.contains("⌁ read · a.py"), "工具摘要：{text}");
        assert!(text.contains("● 成功 · 0.50s"), "工具结果：{text}");
        assert!(text.contains("文件内容：42"));

        state.apply(
            &subagent_event("subagent.task.completed", "t1", "batch-1"),
            now,
        );
        assert!(panel_text(&state).contains("✓ 子代理评审完成"));
        // 收口后迟到的事件被忽略（面板 finish 后 append 无效）。
        state.apply(
            &conversation_event(
                "subagent.turn.text",
                "t1",
                "batch-1",
                json!({"text": "迟到"}),
            ),
            now,
        );
        assert!(!panel_text(&state).contains("迟到"));
    }

    fn panel_text(state: &AppState) -> String {
        state
            .records
            .iter()
            .find_map(|record| match record {
                Record::SubagentConversation(panel) => Some(panel.logical_text()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn conversation_event(
        name: &str,
        task_id: &str,
        batch_id: &str,
        extra: serde_json::Value,
    ) -> HostEvent {
        let mut payload = json!({
            "task_id": task_id,
            "batch_id": batch_id,
            "agent_type": "reviewer",
        });
        if let (Some(target), Some(source)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
            name: name.to_string(),
            payload,
        })
    }

    fn subagent_event(name: &str, task_id: &str, batch_id: &str) -> HostEvent {
        HostEvent::SubagentEvent(omnicrawl_ipc::bridge::SubagentEventPayload {
            name: name.to_string(),
            payload: json!({
                "task_id": task_id,
                "batch_id": batch_id,
                "agent_type": "reviewer",
                "description": format!("审查 {task_id}"),
            }),
        })
    }

    #[test]
    fn composer_edits_by_character_not_byte() {
        let mut composer = Composer::default();
        composer.insert("你好ab");
        composer.move_left();
        composer.insert("世界");
        assert_eq!(composer.text, "你好a世界b");
        assert_eq!(composer.cursor, 5);
        // 光标停在 'b' 之前，退格删掉它左边的 '界'。
        composer.backspace();
        assert_eq!(composer.text, "你好a世b");
        composer.move_home();
        composer.delete();
        assert_eq!(composer.text, "好a世b");
        composer.move_end();
        composer.insert("\n第二行");
        assert_eq!(composer.text, "好a世b\n第二行");
    }

    #[test]
    fn composer_menu_follows_text_and_answers_selection_keys() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        assert!(!composer.menu().is_open(), "空输入不弹菜单");

        composer.insert("/set");
        let names: Vec<&str> = composer
            .menu()
            .matches()
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert_eq!(names, vec!["/settings"]);

        // Tab 补全：菜单把插入文本交回调用方，自己不写输入框。
        assert_eq!(
            composer.menu_handle_key(MenuKey::Tab),
            MenuAction::Complete {
                insert: "/settings".to_string()
            }
        );
        composer.set_text("/settings");
        // 完整命令名：候选后面附上参数提示，Enter 放行提交。
        let names: Vec<&str> = composer
            .menu()
            .matches()
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert_eq!(names, vec!["/settings", "--chat"]);
        assert_eq!(
            composer.menu_handle_key(MenuKey::Enter),
            MenuAction::Passthrough
        );

        // 输入不再是命令前缀（普通提问）：菜单立即收起。
        composer.set_text("帮我看看这个报错");
        assert!(!composer.menu().is_open());
        assert_eq!(
            composer.menu_handle_key(MenuKey::Down),
            MenuAction::Passthrough,
            "菜单收起时不能吃掉选择键"
        );
    }

    #[test]
    fn composer_menu_switches_to_parameter_candidates_after_a_space() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        composer.insert("/settings --");
        let names: Vec<&str> = composer
            .menu()
            .matches()
            .iter()
            .map(|option| option.command.as_str())
            .collect();
        assert_eq!(names, vec!["--chat"]);
        assert_eq!(composer.menu().matches()[0].insert, "/settings --chat");
    }

    #[test]
    fn composer_wraps_by_display_width() {
        let mut composer = Composer::default();
        // 全角字符各占 2 列：宽度 4 时每行两个。
        composer.insert("中文测试");
        assert_eq!(composer.wrapped_lines(4), vec!["中文", "测试"]);
        composer.move_home();
        assert_eq!(composer.cursor_position(4), (0, 0));
        assert_eq!(composer.wrapped_lines(1), vec!["中", "文", "测", "试"]);
    }

    #[test]
    fn composer_scrolls_to_cursor_after_five_lines() {
        let mut composer = Composer::default();
        for index in 1..=7 {
            if index > 1 {
                composer.newline();
            }
            composer.insert(&format!("第{index}行"));
        }
        let (lines, cursor_row) = composer.visible_lines(40);
        assert_eq!(lines.len(), COMPOSER_MAX_LINES);
        assert_eq!(lines[cursor_row], "第7行", "光标所在行必须在窗口内");
    }

    #[test]
    fn rate_estimator_ignores_long_idle_gaps() {
        let mut rate = RateEstimator::default();
        let start = Instant::now();
        rate.record("一二三四", start);
        assert!(rate.value().is_none(), "只有一次增量时没有可用的时长");
        rate.record("五六七八", start + Duration::from_millis(500));
        let first = rate.value().expect("累计时长超过 0.2 秒后应有速度");
        assert!((first - 8.0 / 0.5).abs() < 0.01, "实际：{first}");
        // 间隔超过 2 秒视为待机：token 计入，时长不计入。
        rate.record("九十", start + Duration::from_secs(3));
        let value = rate.value().expect("仍应保留平均值");
        assert!((value - 10.0 / 0.5).abs() < 0.01, "实际：{value}");
    }

    #[test]
    fn token_estimate_counts_cjk_as_one_token() {
        assert_eq!(estimated_tokens("你好世界"), 4);
        assert_eq!(estimated_tokens("abcdefgh"), 2);
        assert_eq!(estimated_tokens("你好abcd"), 3);
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use serde_json::{json, Value};

    fn state() -> AppState {
        AppState::new(
            "demo".to_string(),
            "test-model".to_string(),
            ApprovalMode::Manual,
        )
    }

    fn event(event_type: &str, payload: Value) -> Value {
        json!({
            "version": 1,
            "session_id": "20260101-000000-abcdef",
            "event_id": "e1",
            "created_at": "2026-01-01T00:00:00Z",
            "type": event_type,
            "payload": payload,
        })
    }

    fn tool_cards(state: &AppState) -> Vec<&ToolCard> {
        state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn replay_events_rebuilds_messages_and_closes_unfinished_cards() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "读一下 a.py"})),
            event("assistant_message", json!({"content": "我看看"})),
            event(
                "tool_call_requested",
                json!({"tool": "read", "tool_call_id": "c1", "arguments": {"path": "a.py"}}),
            ),
            event(
                "tool_result",
                json!({"tool": "read", "tool_call_id": "c1", "ok": true, "output": "print(1)"}),
            ),
            // 没有结果的事件：尾部要按「未收到结果」收口，而不是留一张永远在跑的工具卡。
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c2", "arguments": {"command": "ls"}}),
            ),
        ]);

        assert!(matches!(state.records.first(), Some(Record::User(text)) if text == "读一下 a.py"));
        assert!(matches!(state.records.get(1), Some(Record::Assistant(text)) if text == "我看看"));
        let cards = tool_cards(&state);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "read");
        assert_eq!(cards[0].status, ToolStatus::Ok);
        assert_eq!(cards[0].body, vec!["print(1)".to_string()]);
        assert_eq!(cards[1].name, "bash");
        assert_eq!(cards[1].status, ToolStatus::Failed);
        assert_eq!(
            cards[1].body,
            vec!["工具调用在会话结束前未收到结果。".to_string()]
        );
    }

    #[test]
    fn replay_events_marks_denied_cards_and_skips_control_tools() {
        let mut state = state();
        state.replay_events(&[
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c1", "arguments": {"command": "rm -rf /"}}),
            ),
            event(
                "tool_call_denied",
                json!({"tool": "bash", "reason": "用户拒绝执行。"}),
            ),
            // 计划清单与提问是展示层/入口状态，不生成工具卡。
            event(
                "tool_call_requested",
                json!({"tool": host::TODO_TOOL, "arguments": {"todos": [{"step": "改 bug"}]}}),
            ),
            event("tool_call_requested", json!({"tool": host::ASK_USER_TOOL})),
        ]);

        let cards = tool_cards(&state);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].status, ToolStatus::Denied);
        assert_eq!(cards[0].body, vec!["用户拒绝执行。".to_string()]);
        assert_eq!(state.todos.len(), 1);
    }

    #[test]
    fn replay_events_restores_subagent_trees_and_compaction_boundary() {
        let mut state = state();
        state.replay_events(&[
            event(
                "subagent_task_started",
                json!({"task_id": "t1", "batch_id": "b1", "agent_type": "review"}),
            ),
            event(
                "subagent_task_completed",
                json!({"task_id": "t1", "batch_id": "b1"}),
            ),
            event("compact_summary", json!({"content": "前情提要"})),
            event("session_interrupted", json!({})),
            event("turn_cancelled", json!({})),
        ]);

        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::SubagentTree(_))));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text)
                if text == "会话压缩摘要：\n前情提要")));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Notice(text)
                if text == "上一回合在会话恢复前中断。")));
        assert!(state
            .records
            .iter()
            .any(|record| matches!(record, Record::Assistant(text)
                if text == "（上一回合被取消，未生成最终回复）")));
    }

    #[test]
    fn replay_events_clears_the_view_on_an_empty_stream() {
        let mut state = state();
        state.notice("旧消息".to_string());
        // 空事件流是权威结果（`/undo` 撤掉唯一一轮）：视图必须清空，不能保留旧记录。
        state.replay_events(&[]);
        assert!(state.records.is_empty());
    }
}

#[cfg(test)]
mod notice_line_tests {
    use super::{AppState, NOTICE_LINE_LINGER};
    use crate::args::ApprovalMode;
    use std::time::{Duration, Instant};

    #[test]
    fn notice_line_shows_then_expires() {
        let mut state = AppState::new("项目".to_string(), "模型".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        assert!(state.notice_line_text(now).is_none(), "默认没有提示行");

        state.show_notice_line("已复制 2 行到剪切板。", now);
        assert_eq!(state.notice_line_text(now), Some("已复制 2 行到剪切板。"));

        let later = now + NOTICE_LINE_LINGER + Duration::from_millis(10);
        assert!(state.notice_line_text(later).is_none(), "超时后不再显示");
        state.tick_notice_line(later);
        assert!(state.notice_line.is_none(), "tick 要把过期提示真的回收掉");
    }
}

/// 粘贴折叠：多行粘贴折成 `[粘贴 #n +N 行]`，提交时还原，删除时整块删
/// （对映 Python `editing.py` 的 `_compact_paste_if_needed` / `_expand_compact_paste_placeholders`）。
#[cfg(test)]
mod paste_tests {
    use super::*;

    fn pasted(lines: usize) -> String {
        (1..=lines)
            .map(|index| format!("第 {index} 行"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn multiline_paste_is_folded_and_expands_on_take() {
        let original = pasted(8);
        let mut composer = Composer::default();
        composer.insert_paste(&original);

        assert_eq!(composer.text(), "[粘贴 #1 +8 行]", "多行粘贴要折成占位符");
        assert_eq!(
            composer.expanded_text(),
            original,
            "展开后要拿回原文（提交路径读的就是它）"
        );
        assert_eq!(composer.take(), original, "take 必须还原原文");
        assert!(composer.is_empty(), "take 之后输入框要清空");
    }

    #[test]
    fn short_paste_stays_verbatim() {
        let mut composer = Composer::default();
        composer.insert_paste("第一行\n第二行\n第三行");
        assert_eq!(
            composer.text(),
            "第一行\n第二行\n第三行",
            "5 行及以内不折叠（阈值对映 Python）"
        );
    }

    #[test]
    fn crlf_paste_is_normalized_and_counted_like_python() {
        let mut composer = Composer::default();
        composer.insert_paste("a\r\nb\r\nc\r\nd\r\ne\r\nf");
        // 6 行 → 折叠；CRLF 已归一化，因此展开后是 LF。
        assert_eq!(composer.text(), "[粘贴 #1 +6 行]");
        assert_eq!(composer.take(), "a\nb\nc\nd\ne\nf");
    }

    #[test]
    fn backspace_and_delete_remove_the_whole_block() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(9));
        composer.backspace();
        assert!(composer.is_empty(), "退格要整块删，而不是逐字符删");

        composer.insert_paste(&pasted(9));
        composer.move_home();
        composer.delete();
        assert!(composer.is_empty(), "块首的 Delete 也要整块删");
    }

    #[test]
    fn arrows_treat_the_block_as_a_single_cell() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(9));
        composer.move_left();
        // 已在块首：再退格不会删到块里的字符（否则会变成残缺的占位符文本）。
        composer.backspace();
        assert_eq!(composer.text(), "[粘贴 #1 +9 行]");
        composer.move_right();
        composer.insert("尾巴");
        assert_eq!(composer.take(), format!("{}尾巴", pasted(9)));
    }

    #[test]
    fn typing_then_submitting_keeps_the_order() {
        let mut composer = Composer::default();
        composer.insert_paste(&pasted(7));
        composer.insert("（请按这个格式）");
        assert_eq!(composer.take(), format!("{}（请按这个格式）", pasted(7)));
    }
}


#[cfg(test)]
mod streaming_tests {
    use super::*;
    use serde_json::json;
    use omnicrawl_ipc::bridge::{
        ToolCallArgumentsPayload, ToolCallStartedPayload, ToolOutputCompressionPayload,
    };

    #[test]
    fn partial_arguments_reads_what_has_arrived() {
        // 完整 JSON 直接解析。
        let full = partial_arguments(r#"{"path": "a.py", "line": 3}"#);
        assert_eq!(full["path"], json!("a.py"));
        assert_eq!(full["line"], json!(3));
        // 半截：字符串还没闭合、对象也没闭合——已到达的那一段要能取出来。
        let half = partial_arguments(HALF_JSON);
        assert_eq!(half["path"], json!("a.py"));
        assert_eq!(half["content"], json!("第一
第二"));
        // 连值都没开始的半截：只拿到前面的字段。
        let broken = partial_arguments(r#"{"path": "a.py", "cont"#);
        assert_eq!(broken["path"], json!("a.py"));
        // 完全不是 JSON：给空对象（渲染退回卡片自己的参数）。
        assert_eq!(
            partial_arguments("not json"),
            serde_json::Value::Object(serde_json::Map::new())
        );
    }

    #[test]
    fn thousands_separates_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234), "1,234");
        assert_eq!(thousands(12_345_678), "12,345,678");
    }

    #[test]
    fn streaming_card_grows_with_argument_deltas() {
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        for delta in [r#"{"command": "#, r#""ls -la""#, "}"] {
            state.apply(
                &omnicrawl_ipc::HostEvent::ToolCallArguments(ToolCallArgumentsPayload {
                    call_id: "call-1".to_string(),
                    delta: delta.to_string(),
                }),
                now,
            );
        }
        let entry = state.streaming_tool("call-1").expect("应当有侧信道状态");
        assert!(entry.streaming, "批次执行前仍处于流式阶段");
        assert_eq!(entry.arguments["command"], json!("ls -la"));
        assert_eq!(entry.text, r#"{"command": "ls -la"}"#);
        // 卡片在流式阶段就已经立起来了（标题走的是容错解析出来的参数）。
        let lines = crate::ui::conversation::display_lines(&state, 80);
        let title = lines
            .iter()
            .map(|line| {
                line.line
                    .spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .find(|text| text.contains("bash"))
            .expect("流式阶段应当已经有工具卡");
        assert!(title.contains("ls -la"), "标题应带上已到达的命令：{title}");
    }

    #[test]
    fn compression_note_tracks_the_two_phases() {
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        let phase = |phase: &str, before: usize, after: usize| {
            omnicrawl_ipc::HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
                phase: phase.to_string(),
                before_chars: before,
                after_chars: after,
                output: if phase == "finished" {
                    "压缩后的正文".to_string()
                } else {
                    String::new()
                },
            })
        };
        state.apply(&phase("started", 12_345, 0), now);
        assert_eq!(
            state.streaming_tool("call-1").unwrap().compression.as_deref(),
            Some("正在压缩…")
        );
        state.apply(&phase("finished", 12_345, 1_234), now);
        assert_eq!(
            state.streaming_tool("call-1").unwrap().compression.as_deref(),
            Some("已压缩 12,345 → 1,234 字符")
        );
        // 压缩完成后用**压缩后的正文替换**卡片里的原始输出（用户要求）。
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card.clone(),
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.body, vec!["压缩后的正文".to_string()]);
    }

    /// 半截 JSON：字符串与对象都没闭合（模型流中断在这一刻的样子）。
    const HALF_JSON: &str = r#"{"path": "a.py", "content": "第一\n第二"#;
}
