//! 界面状态机：消息流记录、输入框、遥测与待决工具批次的接线。
//!
//! 所有状态变化都发生在主线程，事件来源只有两处：内核帧（[`AppState::apply`]）与
//! 用户输入（输入框与面板）。渲染只读，不修改状态。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use omnicrawl_commands::CommandOption;
use omnicrawl_core::AgentLoopObservation;
use omnicrawl_ipc::{HostEvent, Id};

use crate::args::ApprovalMode;
use crate::host::{self, BatchContext, TodoItem, Waiting};
use crate::ui::conversation::ConversationCache;
use crate::ui::fullscreen::input::menu::{CommandMenu, MenuAction, MenuKey};
use crate::ui::fullscreen::input::sessions_menu::{SessionMenuItem, SessionsMenu};
use crate::ui::fullscreen::random::Rng;
use crate::ui::fullscreen::rendering::logo_anim::LogoAnimation;
use crate::ui::fullscreen::status::hud::load_carousel_message_lines;
use crate::ui::fullscreen::status::indicators as queue;
use crate::ui::fullscreen::status::indicators::{
    Carousel, CarouselPage, CarouselSource, CarouselTick, CAROUSEL_ANIMATION_FRAME_SECONDS,
};
use crate::ui::fullscreen::text::StyledText;
use crate::ui::image_preview::ImagePreviews;
use crate::ui::RefreshVersions;
use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_ipc::bridge::TurnImageAttachment;

mod activity;
mod batch_flow;
mod composer_flow;
mod failure;
mod records;
mod replay;
mod subagent_flow;

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

    /// 这张卡上的「立即结束」按钮该结束哪个后台任务；不是运行中的 monitor 卡时 `None`。
    ///
    /// 两类卡片都要认：模型调 `monitor` 工具形成的真实工具卡（`call_id` 是内核 id，
    /// 任务 id 在参数里），以及本地归并出来的 `monitor:<id>` 卡（任务 id 在 `call_id` 里）。
    /// 参数还在流里时卡片自己的参数是空的，任务 id 只存在于侧信道的半截 JSON 里。
    pub fn monitor_task_id(&self, streaming: Option<&StreamingTool>) -> Option<String> {
        if let Some(monitor_id) = self
            .call_id
            .strip_prefix("monitor:")
            .filter(|id| !id.is_empty())
        {
            return Some(monitor_id.to_string());
        }
        if self.status != ToolStatus::Running || !is_monitor_tool(&self.name) {
            return None;
        }
        [
            Some(&self.arguments),
            streaming.map(|entry| &entry.arguments),
        ]
        .into_iter()
        .flatten()
        .find_map(|arguments| {
            arguments
                .get("monitor_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|monitor_id| !monitor_id.is_empty())
                .map(str::to_string)
        })
    }
}

/// 这张卡是不是模型调 `monitor` 工具形成的卡（工具名可带命名空间前缀）。
///
/// 与 `ui::conversation::is_monitor_card` 同一判据：`monitor` 工具卡与本地归并出来的
/// `monitor:<id>` 卡都算后台任务卡。
pub fn is_monitor_tool(name: &str) -> bool {
    name.rsplit('.').next().unwrap_or(name) == "monitor"
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
    /// 回合失败的提示：正文 + 是否提供「重试」按钮。
    ///
    /// 只有**可重发**的失败才带按钮（网络中断、上游 5xx、重试用尽）：点击后用原样的
    /// 用户消息再提交一次，等价于用户手动重发，从而重新进入自动重试循环。
    /// 取消、上下文超限、工具状态被拒这类需要先改输入或先压缩的失败不带按钮。
    Failure {
        message: String,
        retryable: bool,
    },
    /// 一个 SubAgent 批次在宿主侧的独立窗口受理提示。
    ///
    /// 子代理的过程文本与工具调用不再进消息流（它们由 `subagent-window` 子进程渲染），
    /// 消息流里只留这一行，避免把主会话撑成进度树。
    SubagentWindow(SubagentWindowNotice),
}

/// 窗口受理提示：批次号 + 流水文件（用户可据此自己重开窗口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentWindowNotice {
    pub batch_id: String,
    pub stream: String,
}

impl SubagentWindowNotice {
    /// 消息流里那一行（渲染成 `· ` 提示行）。
    pub fn line(&self) -> String {
        format!(
            "子代理在独立窗口运行（批次 {}，结束自动关闭）；流水：{}",
            self.batch_id, self.stream
        )
    }
}

/// 子代理窗口流水的默认落盘目录：与诊断日志同根（`~/.omnicrawl/logs/subagents`）。
///
/// 放在日志目录下是因为它和 `tui.log` 一样是「现场」，任务结束后仍值得回看。
fn default_subagent_stream_root() -> PathBuf {
    user_config_dir(&ConfigEnvironment::from_process())
        .join("logs")
        .join("subagents")
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

/// 输入框历史最多保留的条数。
///
/// 上下键回看只用到最近若干条，历史本身没有保留更早内容的理由；无上限时任何一处
/// 反复提交（死循环、重试抖动）都会把它推到内存上限。
pub const COMPOSER_HISTORY_LIMIT: usize = 500;

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
    /// 发送前要不要把字节复制一份到工作区临时目录。
    ///
    /// 只有 `Ctrl+V` 的剪贴板位图需要（磁盘上原本没有这份图，模型事后想再读就没有了）；
    /// 粘贴进来的图片路径本来就指向磁盘上的原文件，不必再复制。
    pub archive: bool,
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
        // 先退出浏览态再裁历史：浏览中的 `history_index` 指向旧下标，裁剪会让它错位。
        self.exit_browse_mode();
        if !text.is_empty() && self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            // 只留最近 [`COMPOSER_HISTORY_LIMIT`] 条：更早的既翻不到，也没有保留价值。
            if self.history.len() > COMPOSER_HISTORY_LIMIT {
                let overflow = self.history.len() - COMPOSER_HISTORY_LIMIT;
                self.history.drain(..overflow);
            }
        }
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
    ///
    /// 粘贴进来的图片路径走这里：原文件本来就在磁盘上，不必再复制一份。
    pub fn insert_image(&mut self, media_type: &str, bytes: Vec<u8>) {
        self.push_image(media_type, bytes, false);
    }

    /// 剪贴板位图的粘贴入口：与 [`Self::insert_image`] 同形，但发送前会复制一份到工作区
    /// 临时目录（`Ctrl+V` 的图只在内存与内核请求里存在，事后想再读就得有这份副本）。
    pub fn insert_clipboard_image(&mut self, media_type: &str, bytes: Vec<u8>) {
        self.push_image(media_type, bytes, true);
    }

    fn push_image(&mut self, media_type: &str, bytes: Vec<u8>, archive: bool) {
        self.image_sequence += 1;
        let placeholder = image_placeholder(self.image_sequence);
        self.images.push((
            placeholder.clone(),
            PendingImage {
                placeholder: placeholder.clone(),
                media_type: media_type.to_string(),
                bytes,
                archive,
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
            .chain(
                self.images
                    .iter()
                    .map(|(placeholder, _)| placeholder.as_str()),
            );
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
            Some((start, _)) => {
                self.cursor = if self.cursor > start {
                    start
                } else {
                    start - 1
                }
            }
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
    /// 是否仍逐帧揭示。
    ///
    /// 用户主动展开/折叠、回合收口、回滚之后置为 `false`：这条记录**以后一律全文显示**。
    /// `index` 必须保留——后续增量到达时要靠它认出「还是同一条记录」；若连 index 一起清掉，
    /// 下一片会走「首片到达」分支把 `revealed` 归零，整段思考就会从头重新铺一遍。
    tracking: bool,
}

impl ReasoningReveal {
    /// 某条思考记录开始流式显现（首片到达）：从 0 起铺开。
    fn start(&mut self, index: usize) {
        self.index = Some(index);
        self.revealed = 0;
        self.target = 0;
        self.tracking = true;
    }

    /// 该记录又长了：按本次增量推进目标长度，不对整段重数。
    fn extend(&mut self, index: usize, appended: usize) {
        if self.index != Some(index) {
            return;
        }
        self.target += appended;
    }

    /// 退出逐帧揭示：这条记录此后一律全文显示。
    ///
    /// **不清 `index`**：它标记「哪一条在揭示」。清掉之后同一段思考的下一次增量会被当成
    /// 首片，从 0 重新铺一遍（展开/折叠时表现为整段重放）。没有段在揭示时仍是彻底复位。
    fn settle(&mut self) {
        match self.index {
            Some(_) => {
                self.tracking = false;
                self.revealed = self.target;
            }
            None => *self = Self::default(),
        }
    }

    /// 彻底复位（记录被清空、回滚到不存在的下标等）。
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 推进一帧；返回是否还有没铺完的内容（事件循环据此决定要不要继续按帧重画）。
    fn advance(&mut self) -> bool {
        if !self.tracking || self.index.is_none() || self.revealed >= self.target {
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
            Some(active) if active == index && self.tracking => self.revealed.min(total),
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
    /// 一个 SubAgent 批次在宿主侧开的独立窗口（`/review` 与 `subagent` 工具共用）。
    ///
    /// 过程文本与工具调用不进消息流：它们由 `subagent-window` 子进程渲染，这里只记一行
    /// 受理提示（`Record::Notice`），因此主会话不再有进度树。
    pub subagent_window: crate::subagent_window::SubagentWindows,
    /// 子代理窗口流水的落盘目录（`~/.omnicrawl/logs/subagents`）。
    pub subagent_stream_root: PathBuf,
    /// 用来拉起窗口的可执行文件；`None` 时只落流水、不开窗（测试与无自身路径的场景）。
    pub subagent_window_exe: Option<PathBuf>,
    batch: Option<host::PendingBatch>,
    /// 当前批次是否属于子代理回合：为真时不在主会话挂工具卡（只回填结果）。
    batch_quiet: bool,
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
    /// 正在被悬停的后台任务卡「立即结束」按钮所属的 `call_id`（悬停时染成淡黄色）。
    ///
    /// 同一时刻只可能悬停一个按钮，用「命中的卡片」而不是布尔值，卡片重排后也不会串色。
    stop_button_hover: Option<String>,
    /// 正在被悬停的「重试」按钮所属的失败记录下标（悬停时染成淡黄色）。
    retry_button_hover: Option<usize>,
    /// 正在被悬停的子代理「查看会话」按钮所属的工具卡 `call_id`（悬停时染成淡黄色）。
    subagent_view_hover: Option<String>,
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
            subagent_window: crate::subagent_window::SubagentWindows::new(),
            subagent_stream_root: default_subagent_stream_root(),
            // 窗口要拉起的可执行文件由宿主在构造后补上：`AppState` 自己猜会拿到测试
            // 载荷（集成测试里 `current_exe()` 是测试二进制，不是工作台）。
            subagent_window_exe: None,
            batch: None,
            batch_quiet: false,
            carousel: Carousel::new(),
            carousel_text: StyledText::new(),
            carousel_since: Instant::now(),
            carousel_anim_at: None,
            carousel_ready: false,
            carousel_rand: Rng::new(CAROUSEL_SEED),
            carousel_lines: load_carousel_message_lines(),
            selection: None,
            runtime_esc_hover: false,
            stop_button_hover: None,
            retry_button_hover: None,
            subagent_view_hover: None,
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

    /// 会话区显示行缓存（`ui::conversation` 内部使用）。
    pub(crate) fn conversation_cache(&self) -> &std::cell::RefCell<ConversationCache> {
        &self.conversation_cache
    }

    /// 工具卡的图片预览存储（`ui::conversation` 内部使用）。
    pub(crate) fn image_previews(&self) -> &std::cell::RefCell<ImagePreviews> {
        &self.image_previews
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
                        card.arguments = serde_json::Value::Object(payload.call.arguments.clone());
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
            // 压缩的可见边界由内核另发的 `turn.notice`（「已压缩：省 N Token」）落到会话流。
            HostEvent::ModelResponseAfter(_) | HostEvent::ModelRequestError(_) => {}
        }
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
    let mut candidates = vec![format!("{trimmed}{quote}}}"), format!("{trimmed}}}")];
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
    let raw = payload
        .get("raw_chars")
        .and_then(serde_json::Value::as_u64)?;
    let summary = payload
        .get("summary_chars")
        .and_then(serde_json::Value::as_u64)?;
    // 没有净省就不显示：实时那条也不显示，回放必须与它逐字一致。
    omnicrawl_controllers::compression::saved_tokens_notice(raw as usize, summary as usize)
}

/// `tool_call_eviction` 事件 → 会话区计量行；载荷缺计量字段时返回 `None`。
///
/// 淘汰的净省量是剔除前后实测出来的（不是按字符数折算），事件里直接记着最终值，
/// 回放照原样显示即可——旧事件没有该字段时宁可不显示，也不显示一个假的 0。
fn replay_eviction_notice(payload: &serde_json::Value) -> Option<String> {
    let saved = payload
        .get("saved_tokens")
        .and_then(serde_json::Value::as_i64)?;
    Some(omnicrawl_controllers::turn::compaction::format_compaction_savings_notice(saved))
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

/// 取非空字符串字段；缺失、类型不符或空串都按「未提供」处理（对映 Python 的 `x or 默认值`）。
fn payload_text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 子任务状态是否已是终态（独立窗口用它挡住迟到的活动事件）。
pub fn subagent_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "approval_cancelled"
    )
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
mod tests;
