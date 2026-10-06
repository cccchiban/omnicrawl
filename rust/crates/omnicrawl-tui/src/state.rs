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
use crate::ui::conversation::ConversationCache;
use crate::ui::image_preview::ImagePreviews;
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_ipc::bridge::TurnImageAttachment;
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
use crate::ui::RefreshVersions;

/// 相邻增量间隔超过这个时长视为待机（工具执行、模型停顿），不计入输出时长。
const IDLE_GAP: Duration = Duration::from_secs(2);

/// 输入框可见行数上限：超过后在编辑器内滚动。
pub const COMPOSER_MAX_LINES: usize = 5;

/// 轮播随机源种子：固定值让留言页与乱码帧在测试里可复现。
const CAROUSEL_SEED: u64 = 0x0C1C_2025;

/// 输入框上方那行瞬时提示（拖选复制等）的存活时长。
pub const NOTICE_LINE_LINGER: Duration = Duration::from_secs(3);

/// 思考段平滑显现：每帧至少推进的字符数。
///
/// 模型分片是突发到达的（一帧内可能应用多片，网络停顿后尤其明显），逐片重绘会让好几行
/// 一次蹦出来。慢流时积压本来就小于这个值，内容因此仍按到达节奏即时显示。
const REASONING_REVEAL_MIN_CHARS: usize = 2;

/// 思考段平滑显现：每帧按积压量的几分之一追赶。
///
/// 取 3 意味着一次突发被摊成四五帧连续铺开（≈100ms），既不落后于模型、也不再整段蹦出。
const REASONING_REVEAL_CATCHUP_DIVISOR: usize = 3;

/// 思考段平滑显现：单帧推进的字符数上限。
///
/// 防止「整段思考一次性到达」时出现肉眼可见的整屏跳变，把最大的突发也摊平成连续铺开。
const REASONING_REVEAL_MAX_CHARS: usize = 96;

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
    /// 压缩阶段提示（`正在压缩…` / `已压缩 a → b 字符`）或失败文案（压缩超时/压缩失败）。
    pub compression: Option<String>,
    /// 提示是不是失败文案：渲染时用红色，好和正常的计量提示区分。
    pub compression_failed: bool,
    /// 是否仍在写参数（批次执行开始后置 false，卡片正文改由真实载荷驱动）。
    pub streaming: bool,
}

/// 一条用户消息：正文 + 随它发出的图片。
///
/// 图片是**用户消息的一部分**（`Ctrl+V` 贴进来的剪贴板位图），因此与文本绑在同一条记录上：
/// 会话区渲染成缩略图、重放时从事件载荷重建、提交时随 `turn.submit` 一起送给内核。
#[derive(Debug, Clone, PartialEq)]
pub struct UserRecord {
    pub text: String,
    /// 随这条消息发出的图片，按粘贴顺序（与 `turn.submit` 的 `TurnImageAttachment` 同形）。
    pub images: Vec<TurnImageAttachment>,
    /// 图片预览在 [`ImagePreviews`] 里的锚点（与工具卡用 `call_id` 同一思路）。
    ///
    /// 每条用户消息一个：同一条消息的多张图属于同一块，滚动/重算都按这个键找回。
    pub image_key: String,
}

impl UserRecord {
    /// 纯文本消息（没有粘贴图片，或旧重放路径没带图片）。
    pub fn text_only(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
            image_key: String::new(),
        }
    }

    /// 这条消息会不会出图。
    pub fn has_images(&self) -> bool {
        !self.images.is_empty() && !self.image_key.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    User(UserRecord),
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
/// 距固定帧长的下一帧还有多久：帧号按 `started` 起算，返回值至少 1ms（避免忙等）。

fn frame_delay(now: Instant, started: Instant, frame_seconds: f64) -> Duration {
    let elapsed = now.saturating_duration_since(started).as_secs_f64();
    let frame = (elapsed / frame_seconds).floor() * frame_seconds;
    let remaining = (frame + frame_seconds - elapsed).max(0.001);
    Duration::from_secs_f64(remaining)
}

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

/// 输入框可见窗口里的一行：文本 + 它在全文里的**字符区间** `[start, end)`。
///
/// 区间用的是字符下标（与 [`Composer::cursor`] 同一套），渲染鼠标选区高亮时按它把
/// 选区换算到行内位置。行与行之间的换行符不属于任何一行，所以上一行的 `end` 不等于
/// 下一行的 `start`——换算时必须按区间来，不能用行长度累加。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerRow {
    pub text: String,
    pub start: usize,
    pub end: usize,
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
    /// 鼠标在输入框里拖出来的选区：两个**字符下标**（锚点、拖动端），不保证大小顺序。
    ///
    /// 只在鼠标按下到松手之间活着：松手即复制并清掉（同消息区拖选），文本一改也作废——
    /// 旧下标在改动后会指向别的字符，留着高亮就会出现「选中的和复制出去的不一致」。
    selection: Option<(usize, usize)>,
    commands: Vec<CommandOption>,
    menu: CommandMenu,
    /// 粘贴折叠：占位符 → 原始文本（对映 Python `_compact_pastes`）。
    /// 提交时按占位符还原，删除时整块删掉。
    pastes: Vec<(String, String)>,
    paste_sequence: usize,
    /// 粘贴的图片：占位符 → 图片附件（Ctrl+V 贴进来的剪贴板位图）。
    ///
    /// 与 `pastes` 同一套占位符机制：`[ #1 Image ]` 在输入框里是一整块，
    /// 删除/左右移动都按块处理，提交时按顺序抽出图片随 prompt 一起送出。
    images: Vec<(String, PendingImage)>,
    image_sequence: usize,
    /// 已发送消息的历史，供上下键回看（对映 Python `Composer._history`）。
    history: Vec<String>,
    /// -1 表示输入框里是用户自己的草稿；>= 0 表示正在浏览第 N 条历史。
    history_index: isize,
    /// 进入历史浏览前输入框里的内容，翻到最新一条之后再按一次下键即恢复。
    history_draft: String,
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

/// 粘贴进输入框、尚未随 prompt 发出的图片（`Ctrl+V` 贴进来的剪贴板位图，或粘贴进来的图片路径）。
///
/// `placeholder` 是它在输入框文本里的锚点（`[ #1 Image ]`）：撤回排队消息时据此把附件
/// 挂回同一块，不靠重新编号或位置猜测。只留原始字节：Base64 编码在提交时一次性做
/// （输入框按住不动时不必先膨胀 33%）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingImage {
    pub placeholder: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// 一条「已从输入框交出、尚未发往内核」的提问：文本 + 随行的图片。
///
/// 生成期间进来的提问先进 FIFO 队列，回合结束后按顺序提交；图片必须与文本同行，
/// 否则排队等一轮就会把图片丢掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSubmission {
    pub text: String,
    pub images: Vec<PendingImage>,
}

/// 粘贴图片的占位符（对映用户要求的 `[ #1 Image ]`）。
///
/// `#` 后是图片序号（粘贴多张时递增），`Image` 是类型名，整个方括号是一块：
/// 删除、左右移动都按块处理，不会停在块中间。
fn image_placeholder(sequence: usize) -> String {
    format!("[ #{sequence} Image ]")
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
        self.images.clear();
        self.exit_browse_mode();
        self.refresh_menu();
    }

    /// 用给定文本替换全文，并把光标放到末尾（撤回排队消息、菜单补全时用）。
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.prune_pastes();
        self.exit_browse_mode();
        self.refresh_menu();
    }

    /// 记录一条已发送消息供上下键回看；与最近一条重复时不重复入列。
    ///
    /// 提交即退出浏览态：无论是否新入列，下一条上键都从最新一条开始。
    pub fn history_record(&mut self, text: &str) {
        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.exit_browse_mode();
    }

    /// 是否正在浏览已发送消息。
    pub fn is_browsing_history(&self) -> bool {
        self.history_index >= 0
    }

    /// 退出历史浏览态（用户手动编辑输入框、或输入框被程序化重写时调用）。
    pub fn exit_browse_mode(&mut self) {
        self.history_index = -1;
        self.history_draft.clear();
    }

    /// 按上下键浏览已发送消息；返回 `true` 表示按键已被历史浏览消费。
    ///
    /// `direction` 为 -1（上键，向更早翻）或 1（下键，向更新翻）。进入浏览前先把
    /// 输入框当前内容存成草稿；下键翻过最新一条之后再按一次即恢复草稿并退出浏览，
    /// 与 bash readline 一致。没有历史、或下键时本就不在浏览态时返回 `false`，
    /// 让调用方回退到会话滚动。
    pub fn navigate_history(&mut self, direction: isize) -> bool {
        if self.history.is_empty() {
            return false;
        }
        if direction < 0 {
            if self.history_index < 0 {
                self.history_draft = self.text.clone();
                self.history_index = self.history.len() as isize - 1;
            } else if self.history_index > 0 {
                self.history_index -= 1;
            } else {
                // 已在最早一条：消费按键但不改变内容。
                return true;
            }
        } else {
            if self.history_index < 0 {
                return false;
            }
            if self.history_index >= self.history.len() as isize - 1 {
                let draft = std::mem::take(&mut self.history_draft);
                self.history_index = -1;
                self.replace_history_text(&draft);
                return true;
            }
            self.history_index += 1;
        }
        let text = self.history[self.history_index as usize].clone();
        self.replace_history_text(&text);
        true
    }

    /// 浏览历史时替换全文并把光标移到末尾；此时**不刷新命令菜单**。
    ///
    /// 否则以 `/` 开头的历史条目会弹出候选菜单，接下来的上下键被菜单抢走、
    /// 连续浏览被打断（对映 Python `_replace_history_text` + `_hide_command_menu`）。
    fn replace_history_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.chars().count();
        self.prune_pastes();
        self.menu.hide();
    }

    /// 取走内容并清空；提交时用。
    ///
    /// 返回的是**展开后**的全文：粘贴折叠块在这里还原成真实内容（对映 Python
    /// 提交前调 `_expand_compact_paste_placeholders` 再清空映射）。
    ///
    /// 图片块不展开成文本（`[ #1 Image ]` 照旧留在文本里），附件由 [`Self::take_images`]
    /// 单独取走；这里一并清掉图片状态，避免取消提交后残留。
    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.images.clear();
        self.exit_browse_mode();
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

    /// 图片粘贴入口：在光标处插入 `[ #n Image ]` 占位符，附件按序号存起来等提交。
    ///
    /// 与文本粘贴折叠同一套占位符语义：整块删除、左右移动不停在块中间；占位符被删掉后
    /// 附件随之丢弃（[`Self::prune_pastes`]），不会被静默发出去。
    pub fn insert_image(&mut self, media_type: &str, bytes: Vec<u8>) {
        self.image_sequence += 1;
        let placeholder = image_placeholder(self.image_sequence);
        self.images.push((
            placeholder.clone(),
            PendingImage {
                placeholder: placeholder.clone(),
                media_type: media_type.to_string(),
                bytes,
            },
        ));
        self.insert(&placeholder);
    }

    /// 取走待发图片（按粘贴顺序），并清掉图片块状态。
    ///
    /// 提交路径专用：`take()` 只交出文本，图片得单独取——两者一起清掉才不会漏发。
    pub fn take_images(&mut self) -> Vec<PendingImage> {
        self.images.drain(..).map(|(_, image)| image).collect()
    }

    /// 把这些图片按各自的占位符挂回输入框（撤回排队消息时用）。
    ///
    /// 占位符已在文本里，因此只重建映射、不重新编号：撤回的那条消息回到输入框后，
    /// 图片块与提交前完全一致。
    pub fn restore_images(&mut self, images: Vec<PendingImage>) {
        for image in images {
            let placeholder = image.placeholder.clone();
            self.images.push((placeholder, image));
        }
    }

    /// 输入框里当前有几张待发图片（提交时判断要不要走图片路径）。
    pub fn image_count(&self) -> usize {
        self.images.len()
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
    ///
    /// 粘贴文本块与图片块共用这套判定：两者在输入框里都是一整块。
    fn placeholder_span(&self, index: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = self.text.chars().collect();
        let placeholders = self
            .pastes
            .iter()
            .map(|(placeholder, _)| placeholder.as_str())
            .chain(self.images.iter().map(|(placeholder, _)| placeholder.as_str()));
        for placeholder in placeholders {
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
    ///
    /// 图片块同样在这里回收：占位符被删掉后附件就不该再随提交发出去。
    fn prune_pastes(&mut self) {
        if self.pastes.is_empty() && self.images.is_empty() {
            return;
        }
        let text = self.text.clone();
        self.pastes
            .retain(|(placeholder, _)| text.contains(placeholder.as_str()));
        self.images
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
    ///
    /// 文本改动都会走到这里（插入、删字、清空、替换全文），所以鼠标选区也在这里作废：
    /// 选区按字符下标记，改动之后旧下标指向别处，留着高亮就会出现「选中的和复制出去的
    /// 不一致」。只动光标的 [`Self::move_left`] 等不刷新菜单，那边单独清。
    fn refresh_menu(&mut self) {
        self.clear_mouse_selection();
        self.menu.refresh(&self.text, &self.commands);
    }

    /// 作废鼠标选区（见 [`Self::refresh_menu`] 的说明）。
    fn clear_mouse_selection(&mut self) {
        self.selection = None;
    }

    pub fn insert(&mut self, text: &str) {
        self.exit_browse_mode();
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
        self.exit_browse_mode();
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
        self.exit_browse_mode();
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
        self.clear_mouse_selection();
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
        self.clear_mouse_selection();
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
        self.clear_mouse_selection();
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[..at]
            .iter()
            .rposition(|ch| *ch == '\n')
            .map(|index| index + 1)
            .unwrap_or(0);
    }

    pub fn move_end(&mut self) {
        self.clear_mouse_selection();
        let chars: Vec<char> = self.text.chars().collect();
        let at = self.cursor.min(chars.len());
        self.cursor = chars[at..]
            .iter()
            .position(|ch| *ch == '\n')
            .map(|index| at + index)
            .unwrap_or(chars.len());
    }

    /// 鼠标选区（**归一化**后的字符区间）；空选区（刚按下还没拖）返回 `None`。
    pub fn selection(&self) -> Option<(usize, usize)> {
        let (anchor, head) = self.selection?;
        let (start, end) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        (start < end).then_some((start, end))
    }

    /// 在输入框里按下左键：把插入点挪过去，同时起一个新选区的锚点。
    ///
    /// 与消息区同一套语义：只按下不拖（空选区）就是「点一下定位插入点」，拖出去才是选取。
    pub fn begin_selection(&mut self, index: usize) {
        let index = self.clamped_index(index);
        self.selection = Some((index, index));
        self.cursor = index;
    }

    /// 拖动中：把选区的另一端（也是插入点，供粗光标与窗口跟随）挪到新位置。
    pub fn extend_selection(&mut self, index: usize) {
        let index = self.clamped_index(index);
        if let Some((anchor, _)) = self.selection {
            self.selection = Some((anchor, index));
            self.cursor = index;
        }
    }

    /// 清掉鼠标选区（复制完成 / 点到别处 / 文本改了）。
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// 选区里的文本（空选区返回 `None`）；写剪切板用。
    ///
    /// 取的是输入框里**显示出来的**文本：粘贴折叠块不展开（「所选即所见」，折叠原文
    /// 只在提交时还原）。全是空白的选区照样返回，要不要写剪切板由调用方判定。
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text.chars().skip(start).take(end - start).collect())
    }

    /// 屏幕坐标反查：输入框可见窗口里的（行, 显示列）→ 字符下标。
    ///
    /// 折行、宽字符、超出行数上限后的窗口滚动都用 [`Self::visible_rows`] 同一套结果，
    /// 否则鼠标点到的字符会和落下去的光标对不上。点在某行末尾之后就算该行末尾（下一行的
    /// 行首在那个换行符之后），点在整个文本之后就是末尾；落在占位符块中间时按块取近端
    /// （块在输入框里是一整块，光标不停在块里，与左右键同一套语义）。
    pub fn char_index_at(&self, width: u16, row: usize, column: usize) -> usize {
        let (_, _, start_row, _) = self.visible_rows(width);
        let width = width.max(1) as usize;
        let target = start_row + row;
        let chars: Vec<char> = self.text.chars().collect();
        let mut row_now = 0usize;
        let mut cell = 0usize;
        let mut found = chars.len();
        for (index, ch) in chars.iter().enumerate() {
            if *ch == '\n' {
                // 行尾：点在这一行最后一个字符之后，插入点落在这个换行符之前（渲染出来
                // 就在本行末尾；落到 `index + 1` 会画到下一行行首）。
                if row_now == target && column >= cell {
                    found = index;
                    break;
                }
                row_now += 1;
                cell = 0;
                continue;
            }
            let char_width = unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            if cell + char_width > width && cell > 0 {
                row_now += 1;
                cell = 0;
            }
            if row_now > target || (row_now == target && cell >= column) {
                found = index;
                break;
            }
            cell += char_width;
        }
        self.snap_to_placeholder(found)
    }

    /// 把下标夹进文本长度以内（鼠标算出的行尾列可能落在文本之后）。
    fn clamped_index(&self, index: usize) -> usize {
        index.min(self.text.chars().count())
    }

    /// 落在占位符块中间时取近端的一半：块内不停光标（与左右键、整块删除同一套语义）。
    fn snap_to_placeholder(&self, index: usize) -> usize {
        match self.placeholder_span(index) {
            Some((start, end)) => {
                if index - start <= end - index {
                    start
                } else {
                    end
                }
            }
            None => index,
        }
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

    /// 可见窗口：行（带全文字符区间）+ 光标在窗口里的行号 + 窗口起点行号 + 总行数。
    ///
    /// 窗口算法只在这一处：折行、跟着光标滚动、上限裁剪都在这里定，
    /// [`Self::visible_lines`] 与 [`Self::visible_window`] 都从它派生——高度计算、
    /// 渲染与鼠标点击换算因此共用同一套换行结果，不会各算一份而对不上。
    pub fn visible_rows(&self, width: u16) -> (Vec<ComposerRow>, usize, usize, usize) {
        let lines = self.wrapped_lines(width);
        let total = lines.len();
        let (row, _) = self.cursor_position(width);
        let (start, count, cursor) = if total <= COMPOSER_MAX_LINES {
            (0, total, row.min(COMPOSER_MAX_LINES - 1))
        } else {
            let start = row
                .saturating_sub(COMPOSER_MAX_LINES - 1)
                .min(total - COMPOSER_MAX_LINES);
            (start, COMPOSER_MAX_LINES, row - start)
        };
        // 每行在全文里的字符区间：折行行内不含换行符，行与行之间把它跳过去。
        let chars: Vec<char> = self.text.chars().collect();
        let mut index = 0usize;
        let mut rows: Vec<ComposerRow> = Vec::with_capacity(count);
        for (position, text) in lines.into_iter().enumerate() {
            while chars.get(index) == Some(&'\n') {
                index += 1;
            }
            let row_start = index;
            index += text.chars().count();
            if position >= start && rows.len() < count {
                rows.push(ComposerRow {
                    text,
                    start: row_start,
                    end: index,
                });
            }
        }
        (rows, cursor, start, total)
    }

    /// 与 [`Self::visible_rows`] 同一个窗口，另外给出窗口起点与总行数。
    ///
    /// 输入卡右侧的细线滚动条要画滑块，需要知道「当前可见的是哪一段」。
    pub fn visible_window(&self, width: u16) -> (Vec<String>, usize, usize, usize) {
        let (rows, cursor, start, total) = self.visible_rows(width);
        (
            rows.into_iter().map(|row| row.text).collect(),
            cursor,
            start,
            total,
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

/// 思考段的逐帧显现游标。
///
/// 模型分片是**突发到达**的：网络成批投递、一次 `drain_frames` 里可能应用好几片，
/// 若每片都立刻整段重绘，画面就会「一次蹦出好几行」。这里只记「已显现到第几个字符」，
/// 渲染时按它截断，每帧推进一点，内容于是连续铺开。
///
/// 截断只发生在渲染层：记录本身仍是完整思考，复制、会话投影与收口后的全文都不受影响。
/// `index` 为 `None`（缺省）表示没有段在显现——历史回放、恢复等路径直接按全文显示。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReasoningReveal {
    /// 正在显现的思考记录下标。
    index: Option<usize>,
    /// 已显现的字符数。
    revealed: usize,
    /// 当前已累积到的字符数。
    target: usize,
}

impl ReasoningReveal {
    /// 某条思考记录开始流式显现（首片到达）：从 0 起铺开。
    fn start(&mut self, index: usize) {
        self.index = Some(index);
        self.revealed = 0;
        self.target = 0;
    }

    /// 该记录又长了：更新目标长度（收窄时跟着回退，例如流回滚）。
    fn grow(&mut self, index: usize, target: usize) {
        if self.index != Some(index) {
            return;
        }
        self.target = target;
        self.revealed = self.revealed.min(target);
    }

    /// 退出显现态：之后所有思考段都按全文显示（收口、展开/收起、回放、回滚）。
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 推进一帧；返回是否还有没铺完的内容（事件循环据此决定要不要继续按帧重画）。
    fn advance(&mut self) -> bool {
        if self.index.is_none() || self.revealed >= self.target {
            return false;
        }
        let backlog = self.target - self.revealed;
        let step = (backlog / REASONING_REVEAL_CATCHUP_DIVISOR)
            .clamp(REASONING_REVEAL_MIN_CHARS, REASONING_REVEAL_MAX_CHARS);
        self.revealed = (self.revealed + step).min(self.target);
        self.revealed < self.target
    }

    /// 该记录本次渲染可见的字符数。
    fn visible_chars(&self, index: usize, total: usize) -> usize {
        match self.index {
            Some(active) if active == index => self.revealed.min(total),
            _ => total,
        }
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
    pub pending_inputs: VecDeque<PendingSubmission>,
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
    /// 输入区方框上边框里 `[ ESC ]` 的屏幕矩形（渲染时登记，鼠标据此悬停/点击）。
    ///
    /// 状态行搬到方框边框后就不再走会话流的命中测试，只能由渲染路径把位置记下来：
    /// 渲染函数拿的是 `&AppState`，所以用 `Cell`（与设置面板的 `hits` 同一思路）。
    runtime_esc_area: std::cell::Cell<Option<ratatui::layout::Rect>>,
    /// `/sessions` 在输入框上方打开的可选列表。
    pub sessions_menu: SessionsMenu,
    /// 三块刷新的内容版本号（会话 / 输入框 / 底部）。
    ///
    /// 事件循环用它决定要不要重画：三块都没变就整帧跳过（见 `App::needs_redraw`）。
    refresh: RefreshVersions,
    /// 会话区显示行的分块增量缓存。
    ///
    /// 渲染与命中测试只拿得到 `&AppState`，所以用 `RefCell` 装
    /// （与 `runtime_esc_area` 用 `Cell` 同一思路）。
    conversation_cache: std::cell::RefCell<ConversationCache>,
    /// 工具卡的图片预览（`read_image` 等工具回的视觉附件）。
    ///
    /// 与显示行缓存同样只被渲染路径读取，因此也装在 `RefCell` 里；解码在后台线程，
    /// 宿主每帧调 [`Self::tick_image_previews`] 收结果。
    image_previews: std::cell::RefCell<ImagePreviews>,
    /// 思考段的逐帧显现游标（见 [`ReasoningReveal`]）。
    reasoning_reveal: std::cell::RefCell<ReasoningReveal>,
    /// 用户消息图片块的锚点序号（进程内自增，见 [`Record::User`] 的 `image_key`）。
    user_image_sequence: u64,
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
            runtime_esc_area: std::cell::Cell::new(None),
            sessions_menu: SessionsMenu::new(),
            refresh: RefreshVersions::default(),
            conversation_cache: std::cell::RefCell::new(ConversationCache::default()),
            image_previews: std::cell::RefCell::new(ImagePreviews::default()),
            reasoning_reveal: std::cell::RefCell::new(ReasoningReveal::default()),
            user_image_sequence: 0,
        };
        // 首帧先把轮播文本装好，渲染路径保持只读：即使宿主一次都没 tick 过，
        // 底部 HUD 也有内容可画（测试直接构造 AppState 时会走这条路径）。
        state.refresh_carousel(Instant::now(), "");
        // 首帧把轮播置脏，保证第一帧一定会画出来。
        state.refresh.bump_bottom();
        state
    }

    // ---- 三块刷新：版本号与缓存失效 -----------------------------------------------

    /// 当前的三块刷新版本号。
    pub fn refresh_versions(&self) -> RefreshVersions {
        self.refresh
    }

    /// 输入块置脏（键盘鼠标输入、清单/队列/面板/状态行变化）。
    pub fn touch_composer(&mut self) {
        self.refresh.bump_composer();
    }

    /// 写运行状态行（画在输入区方框的上边框上）。
    ///
    /// 状态行属于输入块，写入时必须置脏；`status` 字段直接赋值仍可用，但会漏掉重画。
    pub fn set_status(&mut self, status: Option<String>) {
        self.status = status;
        self.touch_composer();
    }

    /// 内核的「进行中」提示让位（`Status` / `RetryStatus` / `ProtocolWait` 写入的那条）。
    ///
    /// 这类提示说的是「此刻在等什么」（正在重试第 N 次、等待协议、网关降级…），回合一旦
    /// 重新往前走就不再成立，状态行该回到运行态的默认文案。不清理它会一直停在那句话上，
    /// 直到回合结束（对映 Python 每片增量都重设「正在思考 / 正在回复」）。
    ///
    /// 只在回合进行中让位：慢命令（`/review` 等）在空闲时把状态行写成「正在评审」，
    /// 不能被子代理的流式增量顶掉。
    fn settle_transient_status(&mut self) {
        if self.turn.is_running() && self.status.is_some() {
            self.set_status(None);
        }
    }

    /// 底部块置脏（轮播换页与解密扫描）。
    pub fn touch_bottom(&mut self) {
        self.refresh.bump_bottom();
    }

    /// 会话块整块失效：记录被清空/重放、展开态或思考显示变化时用。
    ///
    /// 下次渲染会从第一条记录起重建显示行（缓存的水位归零）。
    pub fn touch_conversation(&mut self) {
        self.refresh.bump_conversation();
        self.conversation_cache.get_mut().mark_all_dirty();
    }

    /// 会话区需要重画，但**显示行缓存仍然有效**（滚动、拖选高亮这类只影响「怎么画」）。
    pub fn touch_conversation_view(&mut self) {
        self.refresh.bump_conversation();
    }

    /// 清空会话视图（`Ctrl+L` 一类命令）：记录与图片预览一起丢掉。
    ///
    /// 预览必须同步清掉：记录没了以后没有任｛Desensitized:684｝引用那些锚点，留着只占内存。
    pub fn clear_conversation(&mut self) {
        self.records.clear();
        self.image_previews.get_mut().clear();
        self.touch_conversation();
    }

    /// 会话区尾部追加了一条记录：显示行只需补算新记录，但界面要重画。
    fn touch_appended(&mut self) {
        self.refresh.bump_conversation();
    }

    /// 第 `index` 条记录的内容变了：从它开始重算显示行（增量），并标记会话块要重画。
    fn touch_record(&mut self, index: usize) {
        self.refresh.bump_conversation();
        self.conversation_cache.get_mut().mark_record_dirty(index);
    }

    /// 会话区显示行缓存（`ui::conversation` 内部使用）。
    pub(crate) fn conversation_cache(&self) -> &std::cell::RefCell<ConversationCache> {
        &self.conversation_cache
    }

    /// 工具卡的图片预览存储（`ui::conversation` 内部使用）。
    pub(crate) fn image_previews(&self) -> &std::cell::RefCell<ImagePreviews> {
        &self.image_previews
    }

    // ---- 工具卡图片预览 -----------------------------------------------------------

    /// 收下后台解码完的图片；返回是否出现了新的预览（会话区据此重画）。
    ///
    /// 新图意味着卡片正文要多铺几行占位块，必须让那一条记录的显示行重算——
    /// 否则图片到了屏幕上也不会出现（缓存里还是旧的行数）。
    pub fn tick_image_previews(&mut self) -> bool {
        let ready = self.image_previews.get_mut().drain();
        if ready.is_empty() {
            return false;
        }
        // 只重算那几张卡的显示行：图片从「正在准备」换成整块，或者（解码失败时）换回载荷正文。
        // 整块失效在这里太贵——一张 2000 行的会话每来一张图就要从头渲染一遍。
        for key in &ready {
            if let Some(slot) = self.image_block_slot(key) {
                self.touch_record(slot);
            }
        }
        true
    }

    /// 是否还有图片在后台解码（事件循环据此按活动帧率醒来取结果）。
    pub fn has_pending_image_previews(&self) -> bool {
        self.image_previews.borrow().is_pending()
    }

    /// 把一次工具调用回的视觉附件交给图片预览（宿主回填结果时调用）。
    ///
    /// 附件一到就登记，卡片随即按**最终行数**占出图片区；解码在后台完成，
    /// 图片到达前先显示一行「正在准备图片」，不会出现「先排一次短的再把下面挤开」的跳变。
    pub fn register_tool_images(&mut self, call_id: &str, images: &[ToolImageAttachment]) {
        if call_id.is_empty() || images.is_empty() {
            return;
        }
        self.image_previews.borrow_mut().register(call_id, images);
        // 正文要从载荷换成图片区：那一条记录得重算显示行。
        if let Some(slot) = self.tool_card_slot(call_id) {
            self.touch_record(slot);
        }
    }

    // ---- 思考段平滑显现 ---------------------------------------------------------

    /// 推进一帧思考段显现；返回 `true` 表示还没铺完（事件循环据此继续按帧重画）。
    pub fn tick_reasoning_reveal(&mut self) -> bool {
        // 记录被清空/回滚后游标可能指向已经不存在的记录：那种情况下直接退出显现态，
        // 否则「还没铺完」会永远为真，事件循环就按活动帧率空转。
        let stale = self
            .reasoning_reveal
            .borrow()
            .index
            .is_some_and(|index| index >= self.records.len());
        if stale {
            self.reasoning_reveal.get_mut().reset();
            return false;
        }
        let before = {
            let reveal = self.reasoning_reveal.borrow();
            (reveal.index, reveal.revealed)
        };
        let more = self.reasoning_reveal.get_mut().advance();
        let after = {
            let reveal = self.reasoning_reveal.borrow();
            (reveal.index, reveal.revealed)
        };
        // 显现量变了就是这一条记录的内容变了：必须让它重算显示行，只重画是不够的。
        if before != after {
            if let Some(index) = after.0 {
                self.touch_record(index);
            }
        }
        more
    }

    /// 略过逐帧显现、立刻铺完全部思考（收口、展开/收起、回放与回合结束时用）。
    ///
    /// 只清掉显现游标：记录本身就是完整思考，此后所有思考段都按全文渲染。
    pub fn settle_reasoning_reveal(&mut self) {
        self.reasoning_reveal.get_mut().reset();
    }

    /// 该记录本次渲染应显示的字符数（只有正在显现的那条会被截断）。
    pub fn reasoning_visible_chars(&self, index: usize, total: usize) -> usize {
        self.reasoning_reveal.borrow().visible_chars(index, total)
    }

    /// 是否有思考段正在逐帧铺开。
    pub fn is_reasoning_revealing(&self) -> bool {
        let reveal = self.reasoning_reveal.borrow();
        reveal.index.is_some() && reveal.revealed < reveal.target
    }

    /// 按 `call_id` 找工具卡在消息流里的下标。
    ///
    /// 匹配规则与 [`Self::tool_card_mut`] 完全一致（包括「空 id 退化为最近的运行中卡片」），
    /// 因此可以用它定位就地修改过的卡片、做增量失效；没有命中时返回 `None`。
    fn tool_card_slot(&self, call_id: &str) -> Option<usize> {
        self.records
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, record)| {
                let Record::Tool(card) = record else {
                    return None;
                };
                let hit = if call_id.is_empty() {
                    card.status == ToolStatus::Running
                } else {
                    card.call_id == call_id
                };
                hit.then_some(index)
            })
    }

    /// 图片锚点落在哪条记录上：工具卡（`call_id`）或用户消息（`image_key`）。
    ///
    /// 两类记录共用同一份 [`ImagePreviews`]，解码完成后都要重算自己那一条的显示行，
    /// 因此这里按同一把键同时找两处，调用方不必知道键是哪一类。
    fn image_block_slot(&self, key: &str) -> Option<usize> {
        if key.is_empty() {
            return self.tool_card_slot(key);
        }
        self.records
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, record)| match record {
                Record::Tool(card) if card.call_id == key => Some(index),
                Record::User(message) if message.image_key == key => Some(index),
                _ => None,
            })
            .or_else(|| self.tool_card_slot(key))
    }

    /// 让仍在运行的工具卡保持「活」的耗时显示。
    ///
    /// 卡片标题里的耗时（`<1s` 时是毫秒）逐帧在变，而显示行是缓存的：
    /// 有回合在跑时逐帧把这些卡片重新置脏，帧率与以前一致，代价只落在这一两张卡上。
    fn touch_running_tools(&mut self) {
        let dirty: Vec<usize> = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                matches!(record, Record::Tool(card) if card.status == ToolStatus::Running)
            })
            .map(|(index, _)| index)
            .collect();
        for index in dirty {
            self.touch_record(index);
        }
    }

    /// 每帧的活动计时：回合在跑时状态行 spinner（每 80ms 换帧）与工具卡耗时在变。
    ///
    /// 空闲时什么都不做，三块版本号不变，事件循环因此可以整帧跳过绘制。
    /// 子代理进度树的耗时由 [`Self::refresh_subagent_trees`] 自己按记录置脏，不在这里重复处理。
    pub fn tick_activity(&mut self) {
        if !self.turn.is_running() {
            return;
        }
        self.touch_composer();
        self.touch_running_tools();
    }

    /// 是否存在需要按帧推进的活动（运行中的工具卡 / 子代理树 / 回合状态行）。
    ///
    /// 事件循环用它决定等待上限：没有活动就不必按活动帧率唤醒。
    pub fn has_running_activity(&self) -> bool {
        self.turn.is_running()
            || self
                .records
                .iter()
                .any(|record| match record {
                    Record::Tool(card) => card.status == ToolStatus::Running,
                    Record::SubagentTree(tree) => tree.is_active(),
                    _ => false,
                })
    }

    /// 下一个动画帧的到期时刻距现在还有多久；没有动画在播时返回 `None`。
    ///
    /// 欢迎 Logo 与轮播解密扫描都按固定帧长换帧：事件循环据此精确睡到下一帧，
    /// 既不会早醒空转，也不会晚到掉帧。
    pub fn next_animation_frame(&self, now: Instant) -> Option<Duration> {
        let mut due: Option<Duration> = None;
        if let Some(delay) = self.logo.next_frame_delay(now) {
            due = Some(delay);
        }
        // 只在动画真的还在播时才算帧：轮播落定后 `carousel_anim_at` 仍留着上一次的时间戳，
        // 不看 `is_animating()` 会把每个空闲帧都当成「下一帧马上到」而忙等。
        if self.carousel.is_animating() {
            if let Some(last) = self.carousel_anim_at {
                let remaining = frame_delay(now, last, CAROUSEL_ANIMATION_FRAME_SECONDS);
                due = Some(due.map_or(remaining, |known| known.min(remaining)));
            }
        }
        due
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
    ///
    /// 文本没变时不写回、不标脏：空闲时底部块因此保持干净，事件循环可以整帧跳过绘制。
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
                CarouselTick::Frame(text) => self.set_carousel_text(text),
                CarouselTick::Settled(text) => {
                    self.set_carousel_text(text);
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
        let next = self
            .carousel
            .display_text(&source, &self.carousel_lines, &mut self.carousel_rand);
        self.set_carousel_text(next);
    }

    /// 写回轮播文本：内容真的变了才置脏底部块。
    fn set_carousel_text(&mut self, text: StyledText) {
        if self.carousel_text == text {
            return;
        }
        self.carousel_text = text;
        self.touch_bottom();
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
    pub fn begin_turn(
        &mut self,
        turn_id: String,
        text: String,
        images: Vec<TurnImageAttachment>,
    ) {
        self.push_user_message(text, images);
        self.turn = TurnState::Running { turn_id };
        self.turn_started = Some(Instant::now());
        self.status = None;
        self.paused = false;
        self.scroll_from_bottom = 0;
        // 状态行（方框上边框）与排队预览都跟着回合状态变。
        self.touch_composer();
    }

    /// 把一条用户消息推进记录流，并给它挂上图片预览。
    ///
    /// 图片在这里就交给后台解码（与工具附件同一入口）：记录按**最终行数**占出图片区，
    /// 解码期间先显示一行提示，不会出现「先排一次短的再被图片撑开」的跳变。
    pub fn push_user_message(&mut self, text: String, images: Vec<TurnImageAttachment>) {
        let image_key = if images.is_empty() {
            String::new()
        } else {
            self.next_user_image_key()
        };
        if !image_key.is_empty() {
            let attachments: Vec<ToolImageAttachment> = images
                .iter()
                .map(|image| ToolImageAttachment {
                    media_type: image.media_type.clone(),
                    data_base64: image.data_base64.clone(),
                    filename: String::new(),
                    detail: image.detail.clone(),
                })
                .collect();
            self.image_previews
                .borrow_mut()
                .register(&image_key, &attachments);
        }
        self.records.push(Record::User(UserRecord {
            text,
            images,
            image_key,
        }));
        self.touch_appended();
    }

    /// 用户消息图片块的锚点：进程内自增，不与工具调用的 `call_id` 撞车。
    fn next_user_image_key(&mut self) -> String {
        self.user_image_sequence += 1;
        format!("user-image-{}", self.user_image_sequence)
    }

    /// 提交输入框内容；空输入返回 `None`。
    ///
    /// 文本与图片一起取走：图片占位符留在文本里（`[ #1 Image ]`），附件单独返回，
    /// 两者必须同时清空，否则会漏发或残留。
    pub fn submit(&mut self) -> Option<PendingSubmission> {
        if self.composer.is_empty() {
            return None;
        }
        let images = self.composer.take_images();
        let text = self.composer.take();
        self.touch_composer();
        Some(PendingSubmission { text, images })
    }

    /// 生成期间按 Enter：把输入排进 FIFO 队列（对映 Python `_pending_inputs.append`）。
    ///
    /// 图片跟着一起排队：生成期间粘贴的图片若只留在输入框状态里，回合结束时那条消息
    /// 会悄悄丢掉图片。
    pub fn queue_pending(&mut self, text: String, images: Vec<PendingImage>) {
        self.pending_inputs.push_back(PendingSubmission { text, images });
        self.sync_queue_expanded();
        self.touch_composer();
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
        let Some(submission) = self.pending_inputs.remove(index) else {
            return false;
        };
        self.composer.set_text(&submission.text);
        self.composer.restore_images(submission.images);
        self.sync_queue_expanded();
        self.touch_composer();
        true
    }

    /// 展开/收起排队预览中被折叠的条目；不超过可见上限时不动作。
    pub fn toggle_queue_expanded(&mut self) {
        if !queue::can_toggle_queue_expanded(self.pending_inputs.len(), QUEUE_PREVIEW_MAX_ROWS) {
            return;
        }
        self.pending_queue_expanded = !self.pending_queue_expanded;
        self.touch_composer();
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
    pub fn take_next_pending(&mut self) -> Option<PendingSubmission> {
        if self.turn.is_running() {
            return None;
        }
        let submission = self.pending_inputs.pop_front()?;
        self.sync_queue_expanded();
        self.touch_composer();
        Some(submission)
    }

    /// 丢开全部排队消息（退出前收尾，对映 Python `_pending_inputs.clear()`）。
    pub fn clear_pending(&mut self) {
        self.pending_inputs.clear();
        self.pending_queue_expanded = false;
        self.touch_composer();
    }

    /// 展开一张工具卡的完整正文；提示行点击触发。
    pub fn expand_tool(&mut self, call_id: &str) {
        if call_id.is_empty() {
            return;
        }
        self.expanded_tools.insert(call_id.to_string());
        // 展开态只影响这一张卡片：增量重算它本身，不用整块重排。
        if let Some(index) = self.tool_card_slot(call_id) {
            self.touch_record(index);
        }
    }

    /// 点开着的工具卡收起；返回是否真的从展开态收了回去。
    ///
    /// 对映 Python `ToolDisclosure.on_click`：缩略态点卡片不做任何事（展开只能
    /// 通过提示行），所以这里没有 toggle，只有单向收起。
    pub fn collapse_tool(&mut self, call_id: &str) -> bool {
        if call_id.is_empty() || !self.expanded_tools.remove(call_id) {
            return false;
        }
        if let Some(index) = self.tool_card_slot(call_id) {
            self.touch_record(index);
        }
        true
    }

    pub fn is_tool_expanded(&self, call_id: &str) -> bool {
        self.expanded_tools.contains(call_id)
    }

    /// 点击思考段：在折叠与展开之间切换；返回切换后的展开态。
    pub fn toggle_reasoning_expanded(&mut self, index: usize) -> bool {
        let expanded = if self.expanded_reasoning.remove(&index) {
            false
        } else {
            self.expanded_reasoning.insert(index);
            true
        };
        // 用户主动查看：不再逐帧铺开，剩下的内容当场补全（展开态尤其要看到全文）。
        self.settle_reasoning_reveal();
        // 展开态变了：只需要重算这一段思考。
        self.touch_record(index);
        expanded
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
                // 模型又开始出内容：上一轮的「正在重试」提示到此为止。
                self.settle_transient_status();
            }
            HostEvent::ReasoningDelta(payload) => {
                self.telemetry.rate.record(&payload.text, now);
                self.append_streamed(&payload.text, true);
                self.settle_transient_status();
            }
            HostEvent::Status(payload) | HostEvent::RetryStatus(payload) => {
                self.set_status(Some(payload.message.clone()));
            }
            // 会话区提示（脱敏占位符还原告警等）：写进对话流，**不动**输入框上的状态行——
            // 这类提示说的是已经落到对话里的内容，用它顶掉「正在调用」会让回合看起来停了。
            HostEvent::Notice(payload) => self.notice(payload.message.clone()),
            HostEvent::ProtocolWait => self.set_status(Some("等待协议…".to_string())),
            HostEvent::StreamRollback => {
                if matches!(self.records.last(), Some(Record::Assistant(_))) {
                    self.records.pop();
                    // 回滚会把最后一条正文整段抽掉：缓存多出来的块要一并丢掉（罕见路径，整块重算）。
                    self.touch_conversation();
                }
                // 思考段也被截断（目标长度回退）：显现游标按新长度收敛，免得停在旧位置。
                self.settle_reasoning_reveal();
                self.telemetry.rate.rollback();
            }
            HostEvent::TokenUsage(payload) => {
                // 与 Python HUD 同口径（`max(0, int(...))`）：展示层不显示负值，
                // 但协议与归一化层仍保留上游给的原值。
                self.telemetry.input_tokens = payload.input_tokens.max(0) as u64;
                self.telemetry.output_tokens = payload.output_tokens.max(0) as u64;
                self.telemetry.cached_input_tokens = payload.cached_input_tokens.max(0) as u64;
            }
            HostEvent::ContextCompaction(payload) => {
                // 压缩刚落地：把遥测换成压缩后的真实上下文大小（见 `refresh_after_compaction`）。
                if let Some(tokens) = payload.post_compaction_context_tokens {
                    self.refresh_after_compaction(tokens);
                }
            }
            HostEvent::ToolCallStarted(payload) => {
                // 模型开始写工具参数：上一次「正在重试 / 等待协议」到此结束。
                self.settle_transient_status();
                // 模型刚开始写这个调用的参数：卡片先立起来，参数随后逐段补
                // （Python 只在批次执行时才画卡片，这里是 Rust 侧刻意的增量渲染）。
                self.streaming_tools.insert(
                    payload.call_id.clone(),
                    StreamingTool {
                        text: String::new(),
                        arguments: serde_json::Value::Object(serde_json::Map::new()),
                        compression: None,
                        compression_failed: false,
                        streaming: true,
                    },
                );
                match self.tool_card_slot(&payload.call_id) {
                    // 已存在的卡片现在改用「流式参数」渲染标题：重算这一条。
                    Some(index) => self.touch_record(index),
                    None => {
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
                        self.touch_appended();
                    }
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
                if let Some(index) = self.tool_card_slot(&payload.call_id) {
                    if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                        card.summary = parsed
                            .as_object()
                            .map(host::summarize_arguments)
                            .unwrap_or_default();
                    }
                    // 参数逐段到达：只重算这一张卡片（流式渲染的关键路径）。
                    self.touch_record(index);
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
                        compression_failed: false,
                        streaming: false,
                    });
                // 失败/超时：内核把备好的文案放在 `error` 里（用户要求「压缩超时要显示压缩超时」）。
                let failed = payload.phase == "failed";
                let note = if failed {
                    payload.error.clone()
                } else if finished {
                    format!(
                        "已压缩 {} → {} 字符",
                        thousands(payload.before_chars),
                        thousands(payload.after_chars)
                    )
                } else {
                    "正在压缩…".to_string()
                };
                if let Some(entry) = self.streaming_tools.get_mut(&payload.call_id) {
                    entry.compression = Some(note);
                    entry.compression_failed = failed;
                }
                // 压缩提示与正文替换都画在这张卡片上：重算这一条。
                if let Some(index) = self.tool_card_slot(&payload.call_id) {
                    self.touch_record(index);
                }
            }
            HostEvent::ToolStarted(payload) => {
                if let Some(entry) = self.streaming_tools.get_mut(&payload.call.id) {
                    entry.streaming = false;
                }
                // 流式阶段已经建过卡片的（同 call_id）：就地更新，保持卡片出现的先后顺序，
                // 用户已经看到的行也不会闪一下再重排。
                if let Some(index) = self.tool_card_slot(&payload.call.id) {
                    if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                        card.name = payload.call.name.clone();
                        card.arguments =
                            serde_json::Value::Object(payload.call.arguments.clone());
                        card.summary = host::summarize_arguments(&payload.call.arguments);
                    }
                    self.touch_record(index);
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
                self.touch_appended();
            }
            HostEvent::ToolFinished(payload) => {
                self.update_tool(&payload.call, &payload.result, now);
                // 批次执行结束：清掉「仍在写参数」的标记，但**保留**侧信道条目，
                // 压缩阶段的通知随后还要往它上面写提示（并替换卡片正文）。
                if let Some(entry) = self.streaming_tools.get_mut(&payload.call.id) {
                    entry.streaming = false;
                }
                if let Some(index) = self.tool_card_slot(&payload.call.id) {
                    self.touch_record(index);
                }
            }
            HostEvent::ToolOutputUpdate(payload) => {
                if let Some(index) = self.tool_card_slot(&payload.call.id) {
                    if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                        card.body = body_lines(&payload.result.output);
                    }
                    self.touch_record(index);
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
                    // 任务清单画在输入块里。
                    self.touch_composer();
                }
            }
            HostEvent::TurnFinished(payload) => {
                // 工具调用压缩发生在回合收尾、`turn.finished` 之前：压缩后大小随本通知带来，
                // 这里刷新遥测（上下文压缩那条路走 `turn.context_compaction`）。
                if let Some(tokens) = payload.post_compaction_context_tokens {
                    self.refresh_after_compaction(tokens);
                }
                // 清掉侧信道会让卡片上的压缩提示消失：会话块整块重算（每回合一次）。
                self.streaming_tools.clear();
                // 回合已结束：思考段不再逐帧铺开，剩下的内容当场补全。
                self.settle_reasoning_reveal();
                self.touch_conversation();
                // 回合结束时仍停在「调用中」的卡片一律收口：这类卡片拿不到结果了
                // （流被截断、批次被丢弃、工具超时后结果被丢弃），留着就会永远转圈。
                self.close_running_cards("工具调用在回合结束前未收到结果。");
                self.turn = TurnState::Idle;
                self.turn_started = None;
                self.status = None;
                // 状态行随回合结束消失：悬停态一并复位，避免下一回合复用旧高亮。
                self.runtime_esc_hover = false;
                // 状态行（方框上边框）也在输入块里。
                self.touch_composer();
                if payload.paused {
                    self.records.push(Record::Notice(
                        "已被模型暂停：本回合不再自动继续。".to_string(),
                    ));
                    self.touch_appended();
                }
            }
            // 压缩计量与模型 Hook 触发点只供宿主分发插件 Hook，不改动对话视图；
            // 压缩的可见边界由内核另发的 `turn.notice`（「---已压缩 xxk~xxk ---」）落到会话流。
            HostEvent::ModelResponseAfter(_) | HostEvent::ModelRequestError(_) => {}
        }
    }

    /// 追加一条系统消息；不改动回合状态。
    pub fn notice(&mut self, message: String) {
        self.records.push(Record::Notice(message));
        self.touch_appended();
    }

    /// 压缩后刷新遥测：上下文占用与 ↑ 换成压缩后的真实大小，↓/† 清零。
    ///
    /// 压缩发生在回合收尾之后，此后不会再发模型请求，`turn.token_usage` 会一直停在压缩前那次
    /// 的用量上；不在这里改写，底部遥测就会显示一个已经被替换掉的旧上下文大小。
    ///
    /// ↓（本次输出）与 †（缓存命中输入）属于**那一次已经过去的请求**，压缩后不再成立，因此清零
    /// 而不是保留；缓存率由 ↑/† 现算，清零后自然回到 `CH0%`。速率是会话累计均值，与上下文大小无关，不动。
    ///
    /// 底部文本由 `refresh_carousel` 每帧按最新遥测重建，这里只改数值即可在下一帧显示。
    pub fn refresh_after_compaction(&mut self, context_tokens: i64) {
        self.telemetry.input_tokens = context_tokens.max(0) as u64;
        self.telemetry.output_tokens = 0;
        self.telemetry.cached_input_tokens = 0;
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
        self.touch_conversation_view();
    }

    /// 拖动中：把选区的另一端移到新位置。
    pub fn extend_selection(&mut self, line: usize, column: usize) {
        if let Some(selection) = self.selection.as_mut() {
            selection.head = (line, column);
        }
        self.touch_conversation_view();
    }

    /// 清掉选区（Esc / 点击空白 / 复制完成后）。
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.touch_conversation_view();
    }

    /// 打开 `/sessions` 的会话选择菜单（输入框上方）。
    pub fn open_sessions_menu(&mut self, items: Vec<SessionMenuItem>) {
        self.sessions_menu.open(items);
        self.touch_composer();
    }

    /// 归并一条后台任务日志（工具卡形状）；不改动回合状态。
    ///
    /// 与 Python 的差异（Rust 侧刻意）：Python 把每个 Monitor 批次当 `tool` 消息追加进
    /// 对话区，同一个任务每 0.5 秒就多一条卡片；这里按 `monitor_id` 归并到同一张卡片——
    /// 状态行原地替换为最新快照，事件行追加到正文尾部。因此一个后台任务在会话流里
    /// 始终只占一张卡。`call_id` 用 `monitor:<id>` 前缀既做归并键，也避免与真实工具
    /// 调用的 id 相撞（工具卡靠它做展开/收起）。
    ///
    /// `text` 是 `format_monitor_display_batch` 的输出：首行是状态行，其余是本次事件行。
    pub fn push_monitor_batch(&mut self, monitor_id: &str, status: &str, text: String) {
        let call_id = format!("monitor:{monitor_id}");
        let mut lines = text.lines();
        // 首行是任务状态行，其余是本次增量的事件行。
        let header = lines.next().unwrap_or_default().to_string();
        let events: Vec<String> = lines.map(str::to_string).collect();
        let status = monitor_status(status);
        if let Some(index) = self.tool_card_slot(&call_id) {
            if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                card.status = status;
                // 状态行原地替换为最新快照，事件行接在尾部。
                if card.body.is_empty() {
                    card.body.push(header);
                } else {
                    card.body[0] = header;
                }
                card.body.extend(events);
                // 终态后不再计时：卡片会一直留在会话流里，不能让耗时继续涨。
                if status != ToolStatus::Running {
                    card.elapsed = Some(Instant::now().saturating_duration_since(card.started));
                }
            }
            self.touch_record(index);
            return;
        }
        self.records.push(Record::Tool(ToolCard {
            call_id,
            name: "monitor".to_string(),
            summary: monitor_id.to_string(),
            // Monitor 批次不是模型发起的工具调用，没有参数可拼标题。
            arguments: serde_json::Value::Null,
            status,
            elapsed: None,
            started: Instant::now(),
            body: std::iter::once(header).chain(events).collect(),
        }));
        self.touch_appended();
    }

    /// 用内核回给的会话历史重建对话视图（`/resume` 与 `/undo` 后的重放）。
    ///
    /// 只投影 user/assistant 的**文本**消息：工具卡、推理段、通知与子任务进度树属于
    /// 「本进程这次运行」的观感，转录里没有它们的等价物，因此重放后它们自然消失——
    /// 这正是 `/undo` 要的效果（被撤回的消息与工具卡不能留着）。内容是多模态数组或
    /// 非字符串的消息跳过，不把 JSON 塞进消息流。
    pub fn replay_history(&mut self, history: &[serde_json::Value]) {
        self.records.clear();
        // 回放重建的是「已发生的事实」的投影，工具卡本身不会回来，图片预览也跟着作废。
        self.image_previews.get_mut().clear();
        // 回放是「已发生的事实」，直接显示全文，不从半句开始铺。
        self.settle_reasoning_reveal();
        // 整块视图被重建：显示行缓存整体失效（后续都是追加，只算一次）。
        self.touch_conversation();
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
                "user" => self.push_user_message(content.to_string(), Vec::new()),
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
        // 同上：事件流重放出来的卡片没有附件原文，旧预览若留着会挂到不存在（或别的）卡片上。
        self.image_previews.get_mut().clear();
        // 回放是「已发生的事实」，直接显示全文，不从半句开始铺。
        self.settle_reasoning_reveal();
        // 整块视图被重建：显示行缓存整体失效（后续都是追加，只算一次）。
        self.touch_conversation();
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
                        // 随用户消息发出的图片也在事件里：重放照样出缩略图
                        // （旧事件没有该字段，退化成纯文本）。
                        let images = replay_user_images(&payload);
                        self.push_user_message(content, images);
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
                    } else if other == "tool_call_summary" {
                        // 回合末的整轮工具调用压缩：被概括的逐条工具事件已被投影剔除，
                        // 这里补一行与实时通知同款的计量边界（用户要求「和上下文压缩一样」）。
                        if let Some(note) = replay_compaction_notice(&payload) {
                            self.records.push(Record::Notice(note));
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
        // 先定位（必要时新建）这条批次对应的进度树：改完按下标做增量失效。
        let index = match self.records.iter().rposition(|record| {
            matches!(record, Record::SubagentTree(tree) if tree.batch_id == batch_id)
        }) {
            Some(index) => index,
            None => {
                self.records
                    .push(Record::SubagentTree(SubAgentProgressTree::new(&batch_id)));
                self.records.len() - 1
            }
        };
        if let Some(Record::SubagentTree(tree)) = self.records.get_mut(index) {
            tree.update_task(
                task_id,
                agent_type,
                description.unwrap_or(task_id),
                status,
                None,
            );
        }
        self.touch_record(index);
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
        let agent_type = payload_text(payload, "agent_type").unwrap_or("subagent");
        // 定位（必要时新建）这条批次对应的对话面板：改完按下标做增量失效。
        let index = match self.records.iter().rposition(|record| {
            matches!(record, Record::SubagentConversation(panel) if panel.batch_id == batch_id)
        }) {
            Some(index) => index,
            None => {
                self.records
                    .push(Record::SubagentConversation(SubAgentConversation::new(
                        &batch_id, agent_type,
                    )));
                self.records.len() - 1
            }
        };
        let Some(Record::SubagentConversation(panel)) = self.records.get_mut(index) else {
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
        self.touch_record(index);
    }

    /// 推进仍活跃的进度树的运行耗时（对映 Python 的耗时 tick）：终态树不再重绘。
    ///
    /// 返回是否还有活跃树需要继续 tick；耗时跨过整秒前可见文本不变，而渲染结果
    /// 相同的刷新由 [`SubAgentProgressTree::refresh_elapsed`] 内部自行跳过重绘。
    pub fn refresh_subagent_trees(&mut self) -> bool {
        let mut active = false;
        let mut changed: Vec<usize> = Vec::new();
        for (index, record) in self.records.iter_mut().enumerate() {
            if let Record::SubagentTree(tree) = record {
                if tree.is_active() {
                    active = true;
                    // 耗时跨过整秒前可见文本不变：只有真变了才置脏（缓存因此不被空转刷新打破）。
                    if tree.refresh_elapsed(None) {
                        changed.push(index);
                    }
                }
            }
        }
        for index in changed {
            self.touch_record(index);
        }
        active
    }

    /// 回合失败或取消：状态复位并把原因写进消息流。
    pub fn fail_turn(&mut self, message: String) {
        self.turn = TurnState::Idle;
        self.turn_started = None;
        self.status = None;
        // 回合失败/取消时不能留下一直转圈的卡片。
        self.close_running_cards("工具调用在回合结束前未收到结果。");
        // 中途停下的思考不再逐帧铺开：当场补全到已到达的长度，免得停在半句。
        self.settle_reasoning_reveal();
        self.records.push(Record::Notice(message));
        self.touch_appended();
        self.touch_composer();
    }

    /// 是否在消息流里显示思考段（`ui.show_thinking`）。
    ///
    /// 这是个纯界面开关，但会改变会话区的显示行（思考段整段出现/消失），
    /// 因此必须走 setter 让缓存整块失效。
    pub fn set_show_thinking(&mut self, enabled: bool) {
        if self.show_thinking == enabled {
            return;
        }
        self.show_thinking = enabled;
        // 开关变化后整块重排：思考段不再逐帧铺开，避免显示到一半的半句停在屏幕上。
        self.settle_reasoning_reveal();
        self.touch_conversation();
    }

    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(delta.unsigned_abs());
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(delta as usize);
        }
        // 滚动只换窗口、不动显示行：缓存不用重算，但画面要重画。
        self.touch_conversation_view();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_from_bottom = 0;
        self.touch_conversation_view();
    }

    /// 在输入框上方那一行显示一条瞬时提示（拖选复制等）。
    pub fn show_notice_line(&mut self, text: impl Into<String>, now: Instant) {
        self.notice_line = Some((text.into(), now));
        self.touch_composer();
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
            // 提示行淡出后输入区组高会少一行：布局要重算。
            self.touch_composer();
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
        // 待决面板与任务清单都在输入块里。
        self.touch_composer();
        step
    }

    pub fn select_question(&mut self, delta: isize) {
        if let Some(batch) = self.batch.as_mut() {
            batch.select_question(delta);
        }
        self.touch_composer();
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
        // 待决面板与任务清单都在输入块里。
        self.touch_composer();
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
        self.touch_composer();
        Some(step)
    }

    /// 插件改写调用参数（`tool.call.before` 的 transform 结局）；无批次时返回 `false`。
    pub fn rewrite_call_arguments(
        &mut self,
        index: usize,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> bool {
        let changed = match self.batch.as_mut() {
            Some(batch) => batch.rewrite_arguments(index, arguments),
            None => false,
        };
        self.touch_composer();
        changed
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
        // 插件改写参数或拒绝都会重绘待决面板：包一层统一置脏。
        let step = self.guard_pending_approval_inner(guard);
        self.touch_composer();
        step
    }

    fn guard_pending_approval_inner<F>(&mut self, guard: F) -> Option<host::BatchStep>
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
        // 图片预览与「模型是否在看图」无关：附件到了就先交给后台解码，随后出示缩略图。
        if let Some(vision) = vision.as_ref() {
            let call_id = self
                .batch
                .as_ref()
                .and_then(|batch| batch.calls().get(index))
                .map(|call| call.id.clone())
                .unwrap_or_default();
            self.register_tool_images(&call_id, &vision.images);
        }
        let ready = match self.batch.as_mut() {
            Some(batch) => batch.record_result(index, result, vision),
            None => false,
        };
        // 待决面板的进度（还差几个工具）画在输入块里。
        self.touch_composer();
        ready
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
        self.touch_composer();
    }

    /// 执行阶段开始：先落一张「运行中」工具卡。
    /// 把仍在「调用中」的卡片收口为失败并补一句原因（回合结束、回合失败时用）。
    fn close_running_cards(&mut self, message: &str) {
        let now = Instant::now();
        let pending: Vec<usize> = self
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match record {
                Record::Tool(card) if card.status == ToolStatus::Running => Some(index),
                _ => None,
            })
            .collect();
        for index in pending {
            if let Some(Record::Tool(card)) = self.records.get_mut(index) {
                card.status = ToolStatus::Failed;
                card.elapsed = Some(now.saturating_duration_since(card.started));
                if card.body.is_empty() {
                    card.body = vec![message.to_string()];
                }
            }
            self.touch_record(index);
        }
    }

    /// 批次里**已经有结果**、但卡片还停在「调用中」的调用收口；返回收口的数量。
    ///
    /// `update_todos` / `pause_work` / `ask_user` 由批次推进逻辑在宿主内直接写结果
    /// （`PendingBatch::advance` / `answer`），从不经过执行层，也就永远不会走到
    /// `finish_tool_run`——卡片于是停在「调用中」、计时一直涨。用户报的
    /// 「ask_user 提问结束后不显示结束、还在计时」就是这个成因。
    pub fn settle_internal_batch_calls(&mut self) -> usize {
        let Some(batch) = self.batch.as_ref() else {
            return 0;
        };
        let pairs: Vec<(omnicrawl_core::ToolCall, omnicrawl_core::ToolResult)> = batch
            .calls()
            .iter()
            .zip(batch.results())
            .filter_map(|(call, result)| result.clone().map(|result| (call.clone(), result)))
            .collect();
        let now = Instant::now();
        let mut settled = 0;
        for (call, result) in pairs {
            // 空 id 不能参与匹配（`tool_card_mut` 对空 id 的语义是「最近一张运行中的卡」）。
            if call.id.is_empty() {
                continue;
            }
            let pending = match self.tool_card(&call.id) {
                Some(card) => card.status == ToolStatus::Running,
                // 卡片不存在也补一张：这些调用在 Python 侧本来就有工具卡。
                None => true,
            };
            if !pending {
                continue;
            }
            self.begin_tool_run(&call, now);
            self.finish_tool_run(&call, &result, now);
            settled += 1;
        }
        settled
    }

    pub fn begin_tool_run(&mut self, call: &omnicrawl_core::ToolCall, now: Instant) {
        // 流式阶段（`turn.tool_call_started`）可能已经为这次调用立过卡片：
        // 同一 `call_id` 只保留**一张**卡片并就地置为执行中。否则会留下两张卡，
        // 而流式那张永远停在「调用中」——看起来就像工具调用一直不结束，
        // 也像「上一个工具还没完就开始跑下一轮」。
        if let Some(entry) = self.streaming_tools.get_mut(&call.id) {
            // 批次真正开始执行：参数以卡片上的真实载荷为准，不再用半截 JSON 预览。
            entry.streaming = false;
        }
        let arguments = serde_json::Value::Object(call.arguments.clone());
        let summary = host::summarize_arguments(&call.arguments);
        if !call.id.is_empty() {
            if let Some(card) = self.tool_card_mut(&call.id) {
                card.name = call.name.clone();
                card.summary = summary;
                card.arguments = arguments;
                card.status = ToolStatus::Running;
                card.elapsed = None;
                return;
            }
        }
        self.records.push(Record::Tool(ToolCard {
            call_id: call.id.clone(),
            name: call.name.clone(),
            summary,
            arguments,
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
        // 记录尾部可能压着一条会话区提示（`turn.notice`）：它插在正文中间只是中间多一行说明，
        // 但正文合并必须跳过它，否则一段回复会被拆成两条记录、中间硬生生断开。
        let target_index = self
            .records
            .iter()
            .rposition(|record| !matches!(record, Record::Notice(_)));
        if matches!(target_index.and_then(|index| self.records.get(index)), Some(record) if target(record))
        {
            let index = target_index.expect("目标记录存在");
            let length = if let Some(record) = self.records.get_mut(index) {
                match record {
                    Record::Reasoning(body) | Record::Assistant(body) => {
                        body.push_str(text);
                        body.chars().count()
                    }
                    _ => unreachable!("target 只匹配思考与正文记录"),
                }
            } else {
                0
            };
            // 思考段逐帧显现：新段从 0 起铺开，已有段只更新目标长度（渲染按已显现量截断）。
            if reasoning {
                let reveal = self.reasoning_reveal.get_mut();
                if reveal.index != Some(index) {
                    reveal.start(index);
                }
                reveal.grow(index, length);
            }
            // 正文逐片追加：只重算这一条记录（分块增量缓存的关键路径）。
            self.touch_record(index);
        } else {
            let body = text.to_string();
            self.records.push(if reasoning {
                Record::Reasoning(body)
            } else {
                Record::Assistant(body)
            });
            if reasoning {
                let index = self.records.len() - 1;
                let length = text.chars().count();
                let reveal = self.reasoning_reveal.get_mut();
                reveal.start(index);
                reveal.grow(index, length);
            }
            self.touch_appended();
        }
    }

    /// 按 `call_id` 找最近一张工具卡；调用没有 id 时退化为最近一张仍在运行的工具卡。
    /// 输入区边框上 `[ ESC ]` 的屏幕矩形（上一帧渲染登记的，没有则为 `None`）。
    pub fn runtime_esc_area(&self) -> Option<ratatui::layout::Rect> {
        self.runtime_esc_area.get()
    }

    /// 渲染路径登记 `[ ESC ]` 的位置（渲染只拿 `&AppState`，因此用 `Cell` 写入）。
    pub fn set_runtime_esc_area(&self, area: Option<ratatui::layout::Rect>) {
        self.runtime_esc_area.set(area);
    }

    /// 只读取工具卡（按 `call_id`；空 id 不匹配，避免误命中别的调用）。
    pub fn tool_card(&self, call_id: &str) -> Option<&ToolCard> {
        if call_id.is_empty() {
            return None;
        }
        self.records.iter().rev().find_map(|record| match record {
            Record::Tool(card) if card.call_id == call_id => Some(card),
            _ => None,
        })
    }

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
        let Some(index) = self.tool_card_slot(&call.id) else {
            return;
        };
        if let Some(Record::Tool(card)) = self.records.get_mut(index) {
            card.status = match (result.ok, result.error_code.as_deref()) {
                (true, _) => ToolStatus::Ok,
                (false, Some(host::DENIED)) => ToolStatus::Denied,
                (false, _) => ToolStatus::Failed,
            };
            card.elapsed = Some(now.saturating_duration_since(card.started));
            card.body = body_lines(&result.output);
        }
        self.touch_record(index);
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

/// `tool_call_summary` 事件 → 会话区计量行；载荷缺计量字段时返回 `None`。
///
/// 计量取事件自己记下的原始与概括字符数：回放时重算不出来（正文已含落盘说明），
/// 而实时通知与回放必须显示同一行，否则恢复会话后边界会变样。
fn replay_compaction_notice(payload: &serde_json::Value) -> Option<String> {
    let raw = payload.get("raw_chars").and_then(serde_json::Value::as_u64)?;
    let summary = payload
        .get("summary_chars")
        .and_then(serde_json::Value::as_u64)?;
    Some(omnicrawl_controllers::compression::compaction_notice(
        raw as usize,
        summary as usize,
    ))
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

/// 回放 `user_message` 载荷里的图片附件；没有（或形状不对）时返回空表。
///
/// 与 `turn.submit` 的 `TurnImageAttachment` 同形状：旧事件没有该字段，退回纯文本。
fn replay_user_images(payload: &serde_json::Value) -> Vec<TurnImageAttachment> {
    let Some(items) = payload.get("images").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            Some(TurnImageAttachment {
                media_type: item.get("media_type")?.as_str()?.to_string(),
                data_base64: item.get("data_base64")?.as_str()?.to_string(),
                detail: item
                    .get("detail")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect()
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
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
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
        assert_eq!(
            state.records[0],
            Record::User(UserRecord::text_only("问一句"))
        );
        assert_eq!(state.records[1], Record::Reasoning("想一下".to_string()));
        assert_eq!(state.records[2], Record::Assistant("回答结束".to_string()));
    }

    /// 思考段逐帧铺开：大突发不会在同一帧里整段蹦出，但几帧内一定能铺完。
    #[test]
    fn reasoning_reveal_spreads_a_burst_across_frames() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        let burst: String = "想".repeat(300);
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: burst.clone() }),
            now,
        );

        let total = burst.chars().count();
        assert_eq!(
            state.reasoning_visible_chars(1, total),
            0,
            "首帧一个字符都不该先显示出来"
        );
        assert!(state.is_reasoning_revealing());

        // 逐帧推进：每次只铺开一部分，绝不会一帧到齐。
        let mut frames = 0;
        while state.tick_reasoning_reveal() {
            frames += 1;
            let visible = state.reasoning_visible_chars(1, total);
            assert!(visible < total, "还有剩就该「没铺完」，第 {frames} 帧");
            assert!(frames < 200, "铺开过程不该无限长");
        }
        assert_eq!(
            state.reasoning_visible_chars(1, total),
            total,
            "最终必须铺完"
        );
        assert!(!state.is_reasoning_revealing());

        // 记录本身始终是完整思考：截断只发生在渲染层。
        assert_eq!(state.records[1], Record::Reasoning(burst));
    }

    /// 慢流（积压小于每帧最小步长）立刻显示，不额外滞后于模型。
    #[test]
    fn reasoning_reveal_keeps_up_with_a_slow_stream() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload { text: "想".into() }),
            now,
        );
        assert_eq!(state.reasoning_visible_chars(1, 1), 0, "首片先不显示");
        state.tick_reasoning_reveal();
        assert_eq!(
            state.reasoning_visible_chars(1, 1),
            1,
            "一帧之内就该跟上单字符增量"
        );
    }

    /// 收口路径（回合结束/失败/回放）当场补全，不留半句在屏幕上。
    #[test]
    fn reasoning_reveal_settles_on_turn_end_and_replay() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "想".repeat(200),
            }),
            now,
        );
        state.settle_reasoning_reveal();
        assert_eq!(state.reasoning_visible_chars(1, 200), 200);
        assert!(!state.is_reasoning_revealing());

        // 又一段流式思考后回合结束：同样立刻铺完。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "再".repeat(200),
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "答复".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 0,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert!(
            !state.is_reasoning_revealing(),
            "回合结束后不该还有内容在慢慢铺"
        );
        assert_eq!(state.reasoning_visible_chars(1, 400), 400);

        // 回放历史属于「已发生的事实」，直接全文显示。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "又".repeat(100),
            }),
            now,
        );
        state.replay_history(&[]);
        assert!(!state.is_reasoning_revealing());
    }

    /// 正文（非思考）不受显现机制影响：它一直是即时逐片追加。
    #[test]
    fn reasoning_reveal_never_truncates_assistant_text() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问一句".to_string(), Vec::new());
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "正".repeat(500),
            }),
            now,
        );
        assert_eq!(state.records[1], Record::Assistant("正".repeat(500)));
        assert!(
            !state.is_reasoning_revealing(),
            "正文不参与显现，不该出现「还在铺」的状态"
        );
    }

    #[test]
    fn tool_cards_track_status_body_and_elapsed() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "跑一下".to_string(), Vec::new());
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

    /// 同一个后台任务的每批增量都落在同一张卡上：状态行换新、事件行累积。
    #[test]
    fn monitor_batches_merge_into_one_card_per_task() {
        let mut state = state();
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[system] 已启动，shell=bash。".to_string(),
        );
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[stdout] ready in 300ms".to_string(),
        );
        state.push_monitor_batch(
            "m1",
            "completed",
            "Monitor · m1 · completed\n[stdout] done".to_string(),
        );

        let cards: Vec<&ToolCard> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "一个后台任务只占一张卡：{cards:?}");
        assert_eq!(cards[0].call_id, "monitor:m1");
        assert_eq!(cards[0].status, ToolStatus::Ok, "终态跟随最新快照");
        assert_eq!(
            cards[0].body,
            vec![
                "Monitor · m1 · completed".to_string(),
                "[system] 已启动，shell=bash。".to_string(),
                "[stdout] ready in 300ms".to_string(),
                "[stdout] done".to_string(),
            ],
            "状态行原地替换，事件行按批追加"
        );
        assert!(cards[0].elapsed.is_some(), "终态卡片的耗时要落定");
    }

    /// 不同后台任务各自一张卡，互不覆盖。
    #[test]
    fn monitor_batches_of_different_tasks_keep_separate_cards() {
        let mut state = state();
        state.push_monitor_batch("m1", "running", "Monitor · m1 · running".to_string());
        state.push_monitor_batch("m2", "running", "Monitor · m2 · running".to_string());
        state.push_monitor_batch(
            "m1",
            "running",
            "Monitor · m1 · running\n[stdout] 一行".to_string(),
        );

        let ids: Vec<&str> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) => Some(card.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["monitor:m1", "monitor:m2"]);
        let m1 = match &state.records[0] {
            Record::Tool(card) => card,
            other => panic!("第一张应是 m1 的卡：{other:?}"),
        };
        assert_eq!(m1.body.len(), 2, "m1 的第二批追加到自己的卡上");
    }

    #[test]
    fn stream_rollback_drops_half_streamed_record() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
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

    /// 状态行写在输入块里：`Status` / `RetryStatus` / `ProtocolWait` 都必须把输入块置脏。
    ///
    /// 这三个事件以前直接给 `state.status` 赋值（绕过 `set_status`），三块刷新判定「没变」
    /// 就让事件循环整帧跳过绘制——状态文字停在旧值上不刷新。用户报的「模型重试之后状态
    /// 显示不会自动刷新」就是这条。
    #[test]
    fn kernel_status_events_mark_the_composer_dirty() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        let base = state.refresh_versions();

        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在重试(第1次)".to_string(),
            }),
            now,
        );
        let after_retry = state.refresh_versions();
        assert_eq!(state.status.as_deref(), Some("正在重试(第1次)"));
        assert_eq!(
            after_retry.composer,
            base.composer + 1,
            "重试提示写在输入框上边框上，必须置脏输入块"
        );

        state.apply(
            &HostEvent::ProtocolWait,
            now,
        );
        let after_wait = state.refresh_versions();
        assert_eq!(state.status.as_deref(), Some("等待协议…"));
        assert_eq!(after_wait.composer, after_retry.composer + 1);

        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "网关降级，正在重试".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.refresh_versions().composer,
            after_wait.composer + 1,
            "普通状态提示同样要置脏"
        );
    }

    /// 重试提示只是「此刻在等什么」，模型一旦重新出内容就该让位回运行态。
    ///
    /// 不留旧提示的话，状态行会一直写着「正在重试(第N次)」，看起来像卡在重试上。
    #[test]
    fn streaming_clears_the_retry_prompt() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "请求失败，正在自动重试（第1次）".to_string(),
            }),
            now,
        );
        assert!(state.status.is_some());

        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "答复".into(),
            }),
            now,
        );
        assert_eq!(state.status, None, "正文到达后重试提示应当让位");
        let after = state.refresh_versions();
        state.apply(
            &HostEvent::RetryStatus(omnicrawl_ipc::bridge::MessagePayload {
                message: "请求失败，正在自动重试（第2次）".to_string(),
            }),
            now,
        );
        assert_eq!(state.status.as_deref(), Some("请求失败，正在自动重试（第2次）"));

        // 思考增量同理：模型重新动起来就不再是「等待」。
        state.apply(
            &HostEvent::ReasoningDelta(TextPayload {
                text: "再想想".into(),
            }),
            now,
        );
        assert_eq!(state.status, None, "思考增量到达后提示也应当让位");
        assert!(state.refresh_versions().composer >= after.composer);
    }

    /// 空闲时的状态行（慢命令的「正在评审」）不被子代理的流式增量顶掉。
    #[test]
    fn idle_status_survives_subagent_streaming() {
        let mut state = state();
        let now = Instant::now();
        state.set_status(Some("正在评审".to_string()));
        state.apply(
            &HostEvent::Delta(TextPayload {
                text: "评审正文".into(),
            }),
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("正在评审"),
            "回合没在跑时不能清掉慢命令的状态行"
        );
    }

    #[test]
    fn token_usage_and_finish_reset_status() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
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
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert!(!state.turn.is_running());
        assert_eq!(state.status, None);
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    /// 压缩后遥测要换成压缩后的真实上下文大小，↓/† 清零（它们属于那一次已经过去的请求）。
    #[test]
    fn context_compaction_refreshes_telemetry_to_the_post_compaction_size() {
        let mut state = state();
        let now = Instant::now();
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5
            }),
            now,
        );
        state.apply(
            &HostEvent::ContextCompaction(omnicrawl_ipc::bridge::ContextCompactionPayload {
                post_turn_context_tokens: 150_000,
                trigger_context_tokens: 100_000,
                turn_id: "t1".to_string(),
                post_compaction_context_tokens: Some(12_000),
            }),
            now,
        );
        assert_eq!(state.telemetry.input_tokens, 12_000, "CTX 与 ↑ 换成压缩后大小");
        assert_eq!(state.telemetry.output_tokens, 0, "↓ 属于上一次请求，压缩后清零");
        assert_eq!(state.telemetry.cached_input_tokens, 0, "† 同上");
    }

    /// 回合末工具调用压缩走 `turn.finished` 带同一字段。
    #[test]
    fn turn_finished_carries_the_tool_compaction_size() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5
            }),
            now,
        );
        state.apply(
            &HostEvent::TurnFinished(TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: "完成".to_string(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 1,
                paused: false,
                post_compaction_context_tokens: Some(8_000),
            }),
            now,
        );
        assert_eq!(state.telemetry.input_tokens, 8_000);
        assert_eq!(state.telemetry.output_tokens, 0);
        assert_eq!(state.telemetry.cached_input_tokens, 0);
    }

    /// 没有压缩的回合不能动遥测：字段缺省时维持模型请求回报的用量。
    #[test]
    fn turn_finished_without_compaction_keeps_the_usage() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::TokenUsage(TokenUsagePayload {
                input_tokens: 10,
                output_tokens: 20,
                cached_input_tokens: 5
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
                post_compaction_context_tokens: None,
            }),
            now,
        );
        assert_eq!(state.telemetry.input_tokens, 10);
        assert_eq!(state.telemetry.output_tokens, 20);
        assert_eq!(state.telemetry.cached_input_tokens, 5);
    }

    /// 会话区提示不能走状态行：状态行要留着显示「正在调用」等运行状态。
    #[test]
    fn notice_events_go_to_the_conversation_and_keep_the_status_line() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(
            &HostEvent::Status(omnicrawl_ipc::bridge::MessagePayload {
                message: "正在调用".to_string(),
            }),
            now,
        );
        state.apply(
            &HostEvent::Notice(omnicrawl_ipc::bridge::MessagePayload {
                message: "检测到 1 处疑似畸形脱敏占位符，已按原样保留。".to_string(),
            }),
            now,
        );
        assert_eq!(
            state.status.as_deref(),
            Some("正在调用"),
            "会话区提示不得覆盖运行状态行"
        );
        assert!(
            state.records.iter().any(|record| matches!(
                record,
                Record::Notice(text) if text.contains("疑似畸形")
            )),
            "会话区应当出现这条提示：{:?}",
            state.records
        );
    }

    /// 提示插在流式正文中间时不能把一段回复拆成两条记录。
    #[test]
    fn notice_mid_stream_does_not_split_the_reply() {
        let mut state = state();
        let now = Instant::now();
        state.begin_turn("t1".to_string(), "问".to_string(), Vec::new());
        state.apply(&HostEvent::Delta(TextPayload { text: "前半".into() }), now);
        state.apply(
            &HostEvent::Notice(omnicrawl_ipc::bridge::MessagePayload {
                message: "检测到 1 处疑似畸形脱敏占位符，已按原样保留。".to_string(),
            }),
            now,
        );
        state.apply(&HostEvent::Delta(TextPayload { text: "后半".into() }), now);

        let assistants: Vec<String> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Assistant(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            assistants,
            vec!["前半后半".to_string()],
            "提示不得把一段回复拆开：{:?}",
            state.records
        );
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
    /// 鼠标坐标 → 字符下标：宽字符按显示列算，行尾之后算行尾，整个文本之后算末尾。
    #[test]
    fn click_maps_to_the_character_under_the_cursor() {
        let mut composer = Composer::default();
        composer.set_text("中文abc");
        // 「中」占列 0-1、「文」占 2-3、`a` 在列 4：点某一格就落在该格所属的字符之前。
        assert_eq!(composer.char_index_at(40, 0, 0), 0);
        assert_eq!(composer.char_index_at(40, 0, 2), 1);
        assert_eq!(composer.char_index_at(40, 0, 4), 2);
        assert_eq!(composer.char_index_at(40, 0, 5), 3);
        // 行尾之后：插入点落在末尾（这一行没有换行符）。
        assert_eq!(composer.char_index_at(40, 0, 99), 5);
    }

    /// 软折行与窗口滚动下也要对得上：行号是**可见窗口内**的行号，不是全文行号。
    #[test]
    fn click_maps_across_wrapped_and_scrolled_rows() {
        let mut composer = Composer::default();
        composer.set_text("abcd");
        // 宽 2：两行 `ab` / `cd`；点第二行第 1 格 → 第 3 个字符。
        assert_eq!(composer.char_index_at(2, 0, 1), 1);
        assert_eq!(composer.char_index_at(2, 1, 0), 2);
        assert_eq!(composer.char_index_at(2, 1, 9), 4);

        // 超过 5 行时窗口跟着光标滚：`visible_rows` 给的是窗口里的行，
        // 点窗口第一行应当落到被滚上去的那一行，而不是全文第一行。
        let mut composer = Composer::default();
        composer.set_text("1\n2\n3\n4\n5\n6\n7");
        let (rows, cursor, start, total) = composer.visible_rows(40);
        assert_eq!((rows.len(), cursor, start, total), (5, 4, 2, 7));
        assert_eq!(rows[0].text, "3");
        // 窗口第一行第 0 格 → 全文里的「3」（下标 4：`1\n2\n` 各占一格）。
        assert_eq!(composer.char_index_at(40, 0, 0), 4);
        // 每行的字符区间要跳过行与行之间的换行符。
        assert_eq!((rows[0].start, rows[0].end), (4, 5));
        assert_eq!((rows[1].start, rows[1].end), (6, 7));
    }

    /// 拖选：两个端点的字符下标取自全文，反向拖也照样归一化；空选区不算选区。
    #[test]
    fn mouse_selection_reads_back_the_selected_characters() {
        let mut composer = Composer::default();
        composer.set_text("中文 abc");
        composer.begin_selection(2);
        assert_eq!(composer.selection(), None, "只按下还没拖：不是选区");
        assert_eq!(composer.selected_text(), None);

        composer.extend_selection(6);
        assert_eq!(composer.selection(), Some((2, 6)));
        assert_eq!(composer.selected_text().as_deref(), Some(" abc"));
        // 锚点在心里是 2，反向拖到 0：归一化后是 (0, 2)。
        composer.extend_selection(0);
        assert_eq!(composer.selection(), Some((0, 2)));
        assert_eq!(composer.selected_text().as_deref(), Some("中文"));

        composer.clear_selection();
        assert_eq!(composer.selection(), None);
        assert_eq!(composer.selected_text(), None);
    }

    /// 文本一改、光标一动，旧选区就作废：下标会指向别的字符，高亮与复制就会不一致。
    #[test]
    fn editing_or_moving_the_caret_drops_the_mouse_selection() {
        let mut composer = Composer::default();
        composer.set_text("abcd");
        composer.begin_selection(1);
        composer.extend_selection(3);
        composer.insert("x");
        assert_eq!(composer.selection(), None, "插入后选区作废");

        composer.begin_selection(1);
        composer.extend_selection(3);
        composer.move_left();
        assert_eq!(composer.selection(), None, "左右键后选区作废");
    }

    /// 鼠标点在占位符块中间时取近端：块在输入框里是一整块，光标不停在块里。
    #[test]
    fn click_inside_a_placeholder_snaps_to_the_nearer_edge() {
        let mut composer = Composer::default();
        composer.insert_paste(&"a\n".repeat(7));
        let placeholder = "[粘贴 #1 +8 行]";
        assert_eq!(composer.text(), placeholder, "8 行会被折成占位符");
        let length = placeholder.chars().count();
        // 块内取近端：左半边回块首，右半边到块尾。
        assert_eq!(composer.snap_to_placeholder(1), 0);
        assert_eq!(composer.snap_to_placeholder(length - 1), length);
        // 块外照旧，一点不动。
        assert_eq!(composer.snap_to_placeholder(0), 0);
        assert_eq!(composer.snap_to_placeholder(length), length);
        // 点在块首那一格：光标就落在块首，不会跑进块里。
        assert_eq!(composer.char_index_at(40, 0, 0), 0);
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

    /// 用户消息可以带粘贴的图片：记录里留着附件，界面才有东西可画。
    #[test]
    fn begin_turn_keeps_the_pasted_images_on_the_user_message() {
        let mut state = state();
        let image = TurnImageAttachment {
            media_type: "image/png".to_string(),
            data_base64: "AAA".to_string(),
            detail: "auto".to_string(),
        };
        state.begin_turn("t1".to_string(), "看图".to_string(), vec![image.clone()]);

        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:718｝消息：{:?}", state.records);
        };
        assert_eq!(message.text, "看图");
        assert_eq!(message.images, vec![image]);
        assert!(message.has_images(), "有附件就该出图片块");
        assert!(!message.image_key.is_empty(), "图片块要有锚点");
    }

    /// 纯文本消息不带锚点：不然界面上会多铺一块空的图片区。
    #[test]
    fn a_text_only_message_has_no_image_block() {
        let mut state = state();
        state.begin_turn("t1".to_string(), "只有字".to_string(), Vec::new());
        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:718｝消息");
        };
        assert!(!message.has_images());
        assert!(message.image_key.is_empty());
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

        assert!(
            matches!(state.records.first(), Some(Record::User(message)) if message.text == "读一下 a.py")
        );
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

    /// 回合末的工具调用压缩在会话区留一行计量（用户要求「和上下文压缩一样」）：
    /// 与 `compact_summary` 的回放同口径——原事件照旧画出来，边界提示追加在流里。
    #[test]
    fn replay_events_shows_the_tool_compaction_notice() {
        let mut state = state();
        state.replay_events(&[
            event("user_message", json!({"content": "看一下日志"})),
            event(
                "tool_call_requested",
                json!({"tool": "bash", "tool_call_id": "c1", "arguments": {}}),
            ),
            event(
                "tool_result",
                json!({"tool": "bash", "tool_call_id": "c1", "ok": true, "output": "很长"}),
            ),
            event(
                "tool_call_summary",
                json!({
                    "content": "本轮工具调用概括：\n看了日志。",
                    "covered_event_ids": ["e1"],
                    "raw_chars": 12_345,
                    "summary_chars": 1_234,
                }),
            ),
        ]);

        assert!(
            state.records.iter().any(|record| matches!(record, Record::Notice(text)
                if text == "已压缩 12,345 → 1,234 字符")),
            "会话区应当出现压缩计量：{:?}",
            state.records
        );
        assert!(
            matches!(state.records.last(), Some(Record::Notice(text))
                if text == "已压缩 12,345 → 1,234 字符"),
            "计量画在工具调用之后（压缩本来就发生在整轮调用之后）：{:?}",
            state.records
        );
    }

    /// 缺计量字段的旧事件不假装有计量（宁可不显示，也不显示 0 → 0）。
    #[test]
    fn replay_events_skips_the_notice_without_char_counts() {
        let mut state = state();
        state.replay_events(&[event("tool_call_summary", json!({"content": "概括"}))]);
        assert!(
            state.records.iter().all(|record| !matches!(record, Record::Notice(_))),
            "没有 char 计量就不显示提示：{:?}",
            state.records
        );
    }

    /// 重放用户消息时把随消息发出的图片也装回来：否则 `/resume` 之后图片就没了。
    #[test]
    fn replay_events_restores_the_user_message_images() {
        let mut state = state();
        state.replay_events(&[event(
            "user_message",
            json!({
                "content": "看一下这张图",
                "images": [
                    {"media_type": "image/png", "data_base64": "AAA", "detail": "auto"},
                ],
            }),
        )]);

        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:719｝消息：{:?}", state.records);
        };
        assert_eq!(message.text, "看一下这张图");
        assert_eq!(message.images.len(), 1);
        assert_eq!(message.images[0].data_base64, "AAA");
        assert!(message.has_images(), "重放的图片也要出图片块");
    }

    /// 旧事件没有 `images` 字段：照旧是纯文本消息，不能凭空多出一块图片区。
    #[test]
    fn replay_events_without_images_stays_text_only() {
        let mut state = state();
        state.replay_events(&[event("user_message", json!({"content": "纯文本"}))]);
        let Some(Record::User(message)) = state.records.first() else {
            panic!("第一条记录应当是用｛Desensitized:719｝消息");
        };
        assert!(!message.has_images());
        assert!(message.image_key.is_empty());
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

/// 粘贴图片：`Ctrl+V` 贴进来的位图在｛Desensitized:712｝以 `[ #n Image ]` 占位，
/// 与文本粘贴折叠同一套占位符语义（对映用户要求）。
#[cfg(test)]
mod image_paste_tests {
    use super::*;

    fn png() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 1, 2, 3]
    }

    #[test]
    fn pasted_images_get_numbered_placeholders() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert("看一下");
        composer.insert_image("image/png", png());
        assert_eq!(
            composer.text(),
            "[ #1 Image ]看一下[ #2 Image ]",
            "序号按粘贴顺序递增，占位符插在光标处"
        );
        assert_eq!(composer.image_count(), 2);
    }

    #[test]
    fn backspace_removes_the_whole_image_block() {
        let mut composer = Composer::default();
        composer.insert("前");
        composer.insert_image("image/png", png());
        composer.backspace();
        assert_eq!(composer.text(), "前", "退格要整块删掉占位符");
        assert_eq!(composer.image_count(), 0, "占位符没了附件也要跟着丢");
    }

    #[test]
    fn arrows_treat_the_image_block_as_a_single_cell() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert("尾巴");
        composer.move_home();
        composer.move_right();
        // 右移一次跨过整个块，不会停在块中间。
        composer.insert("这里");
        assert_eq!(composer.text(), "[ #1 Image ]这里尾巴");
    }

    #[test]
    fn take_images_hands_out_attachments_in_order() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.insert_image("image/png", png());
        let images = composer.take_images();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].placeholder, "[ #1 Image ]", "顺序与序号一致");
        assert_eq!(images[1].placeholder, "[ #2 Image ]");
        assert_eq!(composer.image_count(), 0, "取走后不残留");
    }

    #[test]
    fn take_clears_images_without_dropping_the_placeholder_text() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        // 文本仍带占位符（提交时随 prompt 一起送），附件由 `take_images` 单独取。
        assert_eq!(composer.take(), "[ #1 Image ]");
        assert_eq!(composer.image_count(), 0, "take 也要清掉图片状态，避免残留");
    }

    #[test]
    fn clearing_the_composer_drops_pending_images() {
        let mut composer = Composer::default();
        composer.insert_image("image/png", png());
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.image_count(), 0);
    }
}

/// 已发送消息的上下键回看（对映 Python `input/composer.py` 的历史浏览部分）。
#[cfg(test)]
mod history_tests {
    use super::*;

    fn composer_with(entries: &[&str]) -> Composer {
        let mut composer = Composer::default();
        for entry in entries {
            composer.history_record(entry);
        }
        composer
    }

    #[test]
    fn without_history_up_key_is_not_consumed() {
        let mut composer = Composer::default();
        assert!(!composer.navigate_history(-1), "没有历史要让位给会话滚动");
        assert!(!composer.navigate_history(1));
    }

    #[test]
    fn down_key_without_browsing_is_not_consumed() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        assert!(
            !composer.navigate_history(1),
            "未在浏览时下键不该被消费（否则会话滚不下去）"
        );
    }

    #[test]
    fn repeated_submissions_are_recorded_once() {
        let mut composer = Composer::default();
        composer.history_record("同一句");
        composer.history_record("同一句");
        composer.history_record("");
        assert_eq!(composer.history.len(), 1, "重复与空提交不入列");
    }

    #[test]
    fn up_key_walks_backwards_and_down_key_restores_the_draft() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.insert("还没发出去的草稿");

        // 上键：从草稿进入最新一条。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第二条");
        // 全角按显示宽度计列：三个全角字占 6 列。
        assert_eq!(composer.cursor_position(40), (0, 6), "光标要落在末尾");

        // 再上：向更早翻；到头后停在最早一条。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第一条");
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第一条", "到头不循环");

        // 下键逐条回来，翻过最新一条后恢复草稿并退出浏览态。
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "第二条");
        assert!(composer.is_browsing_history());
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "还没发出去的草稿", "草稿要能切回来");
        assert!(!composer.is_browsing_history());
    }

    #[test]
    fn submit_exits_browse_so_next_up_starts_from_the_latest() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.navigate_history(-1);
        composer.navigate_history(-1);
        assert_eq!(composer.text(), "第一条");

        composer.history_record("第三条");
        assert!(!composer.is_browsing_history(), "提交即退出浏览态");
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第三条", "下一条上键从最新一条开始");
    }

    #[test]
    fn manual_edit_ends_browsing_and_keeps_the_edited_text() {
        let mut composer = composer_with(&["第一条", "第二条"]);
        composer.navigate_history(-1);
        composer.insert("补充");

        assert!(!composer.is_browsing_history(), "手动编辑结束浏览态");
        assert_eq!(composer.text(), "第二条补充");
        // 再上键从最新一条重新开始（草稿是编辑后的内容）。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "第二条");
        assert!(composer.navigate_history(1));
        assert_eq!(composer.text(), "第二条补充");
    }

    #[test]
    fn browsing_does_not_leave_the_command_menu_open() {
        let mut composer = Composer::default();
        composer.set_commands(crate::commands::command_options());
        composer.history_record("/settings");
        composer.insert("/ne");
        assert!(composer.menu().is_open(), "正常输入时菜单照常弹出");

        // 回看以 `/` 开头的历史时菜单必须收起，否则接下来的上下键会被菜单抢走。
        assert!(composer.navigate_history(-1));
        assert_eq!(composer.text(), "/settings");
        assert!(!composer.menu().is_open(), "浏览历史不该弹出候选菜单");

        // 手动编辑才让菜单回来：退回到 `/se` 前缀，候选立即重新出现。
        while composer.text() != "/se" {
            composer.backspace();
        }
        assert!(composer.menu().is_open(), "手动编辑后菜单要恢复刷新");
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
                error: if phase == "failed" {
                    "压缩超时（120 秒），保留原始输出：请求超时。".to_string()
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

    #[test]
    fn internal_calls_are_settled_after_the_question_is_answered() {
        // 回归（用户报）：ask_user 由批次推进在宿主内直接定调结果，从不经过执行层，
        // 卡片因此一直停在「调用中」并继续计时。update_todos 同理。
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let call = |name: &str, arguments: serde_json::Value| omnicrawl_core::ToolCall {
            name: name.to_string(),
            arguments: arguments.as_object().cloned().unwrap_or_default(),
            id: format!("call-{name}"),
            function_name: name.to_string(),
        };
        let todos = call(
            "update_todos",
            json!({"todos": [{"step": "写骨架", "completed": false}]}),
        );
        let ask = call(
            "ask_user",
            json!({"question": "要不要继续？", "options": ["继续", "停下"]}),
        );
        let _ = state.start_batch(
            omnicrawl_ipc::Id::Number(1),
            vec![todos.clone(), ask.clone()],
        );
        // 推进到提问点：任务清单已在宿主内定调，提问还在等答案。
        state.settle_internal_batch_calls();
        assert_eq!(
            state.tool_card(&todos.id).map(|card| card.status),
            Some(ToolStatus::Ok),
            "update_todos 是内部定调的，应当立刻收口"
        );
        assert!(
            state.tool_card(&ask.id).is_none(),
            "提问在回答前不该有终态卡片"
        );

        state.answer_question("继续".to_string()).expect("应当在等提问");
        assert_eq!(
            state.settle_internal_batch_calls(),
            1,
            "回答之后提问卡片要收口"
        );
        let card = state.tool_card(&ask.id).expect("提问应当有卡片");
        assert_eq!(
            card.status,
            ToolStatus::Ok,
            "提问结束后不能再停在「调用中」"
        );
        assert!(
            card.body.iter().any(|line| line.contains("继续")),
            "卡片正文带回答：{:?}",
            card.body
        );
    }

    #[test]
    fn compression_failure_note_is_shown_on_the_card() {
        // 用户要求：压缩超时显示「压缩超时」，其他错误显示错误（以前只 eprintln，界面看不到）。
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolOutputCompression(ToolOutputCompressionPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
                phase: "failed".to_string(),
                before_chars: 100_000,
                after_chars: 0,
                output: String::new(),
                error: "压缩超时（120 秒），保留原始输出：请求超时。".to_string(),
            }),
            now,
        );
        let entry = state.streaming_tool("call-1").expect("应当有侧信道条目");
        assert!(
            entry
                .compression
                .as_deref()
                .unwrap_or_default()
                .contains("压缩超时"),
            "卡片上要显示压缩超时：{:?}",
            entry.compression
        );
        assert!(entry.compression_failed, "失败提示要走错误样式");
    }

    #[test]
    fn a_streamed_call_keeps_one_card_and_gets_finished() {
        // 回归：流式阶段立过卡片后，批次执行**不能**再推一张卡片，
        // 否则流式那张永远停在「调用中」（用户报「工具调用一直不结束」）。
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: json!({"command": "ls"}).as_object().cloned().unwrap_or_default(),
            id: "call-1".to_string(),
            function_name: "bash".to_string(),
        };
        state.begin_tool_run(&call, now);
        state.begin_tool_run(&call, now);
        let cards: Vec<&ToolCard> = state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Tool(card) if card.call_id == "call-1" => Some(card),
                _ => None,
            })
            .collect();
        assert_eq!(cards.len(), 1, "同一 call_id 只应有一张卡片");
        assert!(!state.streaming_tool("call-1").unwrap().streaming, "批次执行后不再是流式态");

        // 结果回填后卡片进入终态（不会一直是「调用中」）。
        state.finish_tool_run(
            &call,
            &omnicrawl_core::ToolResult {
                ok: true,
                output: "文件内容：42".to_string(),
                full_output: String::new(),
                error_code: None,
                retryable: false,
            },
            now,
        );
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card,
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.status, ToolStatus::Ok, "完成后状态必须是终态");
    }

    #[test]
    fn turn_end_closes_cards_still_marked_running() {
        // 回合结束仍停在「调用中」的卡片必须收口（用户报「工具调用一直不结束」）。
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let now = Instant::now();
        state.apply(
            &omnicrawl_ipc::HostEvent::ToolCallStarted(ToolCallStartedPayload {
                call_id: "call-1".to_string(),
                tool: "bash".to_string(),
            }),
            now,
        );
        state.apply(
            &omnicrawl_ipc::HostEvent::TurnFinished(omnicrawl_ipc::bridge::TurnFinishedPayload {
                turn_id: "t1".to_string(),
                final_text: String::new(),
                reasoning: String::new(),
                model_turns: 1,
                tool_calls: 1,
                paused: false,
                post_compaction_context_tokens: None,
            }),
            now,
        );
        let card = match state
            .records
            .iter()
            .find(|record| matches!(record, Record::Tool(_)))
        {
            Some(Record::Tool(card)) => card,
            other => panic!("应当有工具卡：{other:?}"),
        };
        assert_eq!(card.status, ToolStatus::Failed, "回合结束不能再停在调用中");
        assert_eq!(card.body, vec!["工具调用在回合结束前未收到结果。".to_string()]);
    }

    #[test]
    fn batch_waits_for_every_call_before_observations() {
        // 并发工具调用：整批**全部**回填后才打包成观察交给下一轮模型请求。
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        let calls: Vec<omnicrawl_core::ToolCall> = ["call-1", "call-2"]
            .iter()
            .map(|id| omnicrawl_core::ToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "a.py"}).as_object().cloned().unwrap_or_default(),
                id: (*id).to_string(),
                function_name: "read_file".to_string(),
            })
            .collect();
        let _ = state.start_batch(omnicrawl_ipc::Id::Number(1), calls.clone());
        let result = omnicrawl_core::ToolResult {
            ok: true,
            output: "文件内容：42".to_string(),
            full_output: String::new(),
            error_code: None,
            retryable: false,
        };
        assert!(!state.record_tool_result(0, result.clone(), None), "只回填一条时整批未就绪");
        assert!(state.take_observations(false).is_none(), "未整批回填时不能打包观察");
        assert!(state.record_tool_result(1, result, None), "两条都回填后整批就绪");
        let observations = state.take_observations(false).expect("整批就绪后应当能取观察");
        assert_eq!(observations.len(), 2, "观察按模型调用顺序整批产出");
        let _ = &calls;
    }

    /// 半截 JSON：字符串与对象都没闭合（模型流中断在这一刻的样子）。
    const HALF_JSON: &str = r#"{"path": "a.py", "content": "第一\n第二"#;

    // ---- 三块刷新（会话 / 输入框 / 底部） ------------------------------------------

    fn three_block_state() -> AppState {
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        state.begin_turn("t1".to_string(), "问题".to_string(), Vec::new());
        state
    }

    /// 只读操作（取行、取窗口、算滚动条）不能把任何一块置脏。
    ///
    /// 事件循环就是靠「三块都没变」来跳过整帧绘制；空转刷新一旦置脏，
    /// 2000 行的会话又会回到每帧重画。
    #[test]
    fn idle_reads_never_mark_any_block_dirty() {
        let state = three_block_state();
        let before = state.refresh_versions();
        for _ in 0..10 {
            let _ = crate::ui::conversation::display_lines(&state, 80);
            let _ = crate::ui::conversation::visible_window(&state, 80, 20, 0);
            let _ = state.carousel_page();
        }
        assert_eq!(state.refresh_versions(), before, "只读跟踪不得置脏");
    }

    /// 三块各管各的：改会话不会带脏底部，换轮播不会把会话整块失效。
    #[test]
    fn the_three_blocks_are_versioned_independently() {
        let mut state = three_block_state();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        let base = state.refresh_versions();
        let rebuilds = state.conversation_cache().borrow().rebuilds;

        // 底部轮播换页：只有底部版本动，会话的显示行缓存连一行都不重算。
        state.touch_bottom();
        let after_bottom = state.refresh_versions();
        assert_eq!(after_bottom.bottom, base.bottom + 1);
        assert_eq!(after_bottom.conversation, base.conversation);
        assert_eq!(after_bottom.composer, base.composer);

        // 会话区滚动：只换窗口，不是内容变化。
        state.scroll_by(-4);
        let after_scroll = state.refresh_versions();
        assert_eq!(after_scroll.conversation, after_bottom.conversation + 1);
        assert_eq!(after_scroll.bottom, after_bottom.bottom);
        let _ = crate::ui::conversation::display_lines(&state, 80);
        assert_eq!(
            state.conversation_cache().borrow().rebuilds,
            rebuilds,
            "滚动不得触发显示行重算"
        );

        // 拖选同理：只加高亮。
        state.begin_selection(1, 0);
        state.extend_selection(2, 3);
        state.clear_selection();
        let after_selection = state.refresh_versions();
        assert_eq!(after_selection.conversation, after_scroll.conversation + 3);
        assert_eq!(after_selection.composer, after_scroll.composer);

        // 新记录：会话与输入框都变（输入框上是排队/状态信息），底部不变。
        state.notice("提示".to_string());
        let after_notice = state.refresh_versions();
        assert_eq!(after_notice.conversation, after_selection.conversation + 1);
        assert_eq!(after_notice.bottom, after_selection.bottom);
    }

    /// 空闲时 `tick_activity` 什么也不做；回合在跑时它维持 spinner 与活动耗时。
    #[test]
    fn activity_tick_only_runs_while_a_turn_is_in_flight() {
        let mut state = AppState::new("proj".to_string(), "model".to_string(), ApprovalMode::Manual);
        // 空闲：一条版本都不动。
        let idle = state.refresh_versions();
        state.tick_activity();
        assert_eq!(state.refresh_versions(), idle, "空闲时不该有活动刷新");

        // 回合在跑：状态行 spinner 与运行中卡片耗时都要重画。
        state.begin_turn("t1".to_string(), "问题".to_string(), Vec::new());
        let running = state.refresh_versions();
        state.tick_activity();
        let ticked = state.refresh_versions();
        assert_eq!(ticked.composer, running.composer + 1, "spinner 在输入框上边框");
        assert_eq!(ticked.conversation, running.conversation, "没有运行中的卡片就不动会话");

        // 有一张运行中的卡片：它的耗时按帧重算。
        let mut state = three_block_state();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        let rebuilds = state.conversation_cache().borrow().rebuilds;
        let call = omnicrawl_core::ToolCall {
            name: "bash".to_string(),
            arguments: serde_json::json!({"command": "pytest -q"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
            id: "c1".to_string(),
            function_name: "bash".to_string(),
        };
        state.begin_tool_run(&call, Instant::now());
        state.tick_activity();
        let _ = crate::ui::conversation::display_lines(&state, 80);
        assert!(
            state.conversation_cache().borrow().rebuilds > rebuilds,
            "运行中卡片的耗时应当被重新算过"
        );
    }
}
