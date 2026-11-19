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
pub use crate::document_path::PendingDocument;
use crate::host::{self, BatchContext, TodoItem, Waiting};
use crate::ui::conversation::ConversationCache;
use crate::ui::fullscreen::input::menu::{CommandMenu, MenuAction, MenuKey};
use crate::ui::fullscreen::input::sessions_menu::{SessionMenuItem, SessionsMenu};
use crate::ui::fullscreen::rendering::logo_anim::LogoAnimation;
use crate::ui::fullscreen::rendering::welcome_logo::BADGE_SEGMENT_COUNT;
use crate::ui::fullscreen::status::indicators as queue;
use crate::ui::fullscreen::status::indicators::{hud_line_text, HudSource};
use crate::ui::fullscreen::text::StyledText;
use crate::ui::image_preview::ImagePreviews;
use crate::ui::RefreshVersions;
use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_controllers::types::ToolImageAttachment;
use omnicrawl_ipc::bridge::TurnImageAttachment;

mod activity;
mod batch_flow;
mod composer;
mod composer_flow;
mod failure;
mod records;
mod replay;
mod selection;
mod subagent_flow;
mod telemetry;

// 这些类型原先直接定义在本文件里，因此对 crate 内可见；拆分后必须原样再导出，
// 否则 `crate::state::Record` 这类既有路径会失效。
pub use composer::{Composer, ComposerRow, PendingImage, PendingSubmission};
pub use records::{
    is_monitor_tool, subagent_status_is_terminal, Record, StreamingTool, SubagentWindowNotice,
    ToolCard, ToolStatus, TurnOutcome, TurnState, UserRecord,
};
pub use selection::{NetApprovalPanel, ReasoningReveal, TextSelection};
pub use telemetry::{estimated_tokens, RateEstimator, Telemetry};

// 拆分前这些是同一文件内的私有辅助，兄弟子模块直接调得到；现在需要显式引入。
pub(super) use records::{default_subagent_stream_root, monitor_status};
pub(super) use replay::partial_arguments;

/// 相邻增量间隔超过这个时长视为待机（工具执行、模型停顿），不计入输出时长。
const IDLE_GAP: Duration = Duration::from_secs(2);

/// 输入框可见行数上限：超过后在编辑器内滚动。
pub const COMPOSER_MAX_LINES: usize = 5;

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

/// 生成期间 FIFO 排队预览的可见条数上限。
///
/// 直接复用对映层常量，不另立一份：预览行数与 HUD 的 `QUEUE` 段读的是同一组值。
pub use crate::ui::fullscreen::status::indicators::QUEUE_PREVIEW_MAX_ROWS;





















/// 粘贴折叠的阈值：超过这么多行就折成一个占位符（对映 Python `_PASTE_COMPACT_LINE_THRESHOLD`）。
pub const PASTE_COMPACT_LINE_THRESHOLD: usize = 5;

/// 输入框历史最多保留的条数。
///
/// 上下键回看只用到最近若干条，历史本身没有保留更早内容的理由；无上限时任何一处
/// 反复提交（死循环、重试抖动）都会把它推到内存上限。
pub const COMPOSER_HISTORY_LIMIT: usize = 500;














pub struct AppState {
    pub project: String,
    pub model: String,
    pub approval: ApprovalMode,
    /// 底栏显示的产品版本（`v0.4.6`，取自 `omnicrawl_version::VERSION`）。
    pub version: String,
    /// registry 上是否有更新版本；为真时底栏版本号旁标 ↑。
    pub upgrade_available: bool,
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
    /// 上一个回合的结局（终端标签页标题的 ✓ / ⩍ / ✕）；`None` 表示还没跑过回合。
    pub turn_outcome: Option<TurnOutcome>,
    pub telemetry: Telemetry,
    /// 已启用的 MCP Server 数量（HUD 的 MCP 段）。
    pub mcp_servers: u64,
    /// 本回合开始时刻；状态行的 spinner 按它推进。
    pub turn_started: Option<Instant>,
    /// 是否在消息流里显示思考段（`ui.show_thinking`，启动期按配置文件定初值）。
    pub show_thinking: bool,
    /// 思考段是否默认折叠（`ui.thinking_collapsed`，由 `/thinking_collapsed` 更新）。
    pub thinking_collapsed: bool,
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
    /// 底部单行 HUD 的整行文本（渲染只读；由 [`AppState::refresh_hud`] 按最新遥测重建）。
    pub hud_text: StyledText,
    /// 底栏工作区名是否被鼠标悬停（悬停时那段染成淡黄色，点击打开仓库文件夹）。
    hud_workspace_hover: bool,
    /// 底栏工作区名的屏幕矩形（渲染时登记，鼠标据此悬停/点击）。
    hud_workspace_area: std::cell::Cell<Option<ratatui::layout::Rect>>,
    /// 沙盒网络审批：当前在等用户回答的请求（`ask` 档的连接）。
    ///
    /// 放在 `AppState` 而不是 `App` 上，因为渲染层只拿得到 `&AppState`；
    /// 与工具审批面板（`Waiting::Approval`）是两回事——那个随工具批次出现，
    /// 这个由沙盒内某条命令的一次连接产生，可能在批次之外单独发生。
    pub net_approval: Option<NetApprovalPanel>,
    /// 鼠标拖选的文本区间（`None` = 没有选区）：渲染层按它加反显高亮，
    /// 松手时由宿主抽出文本写入剪切板。坐标是「`display_lines` 行下标 + 行内显示列」。
    selection: Option<TextSelection>,
    /// 会话流末尾 `[ ESC ]` 提示是否被鼠标悬停（悬停时染成淡黄色）。
    pub runtime_esc_hover: bool,
    /// 欢迎页徽标各可点段的屏幕矩形（渲染时登记，鼠标据此悬停/点击）。
    ///
    /// 徽标位置随窗口宽度与居中偏移变化，只能由渲染路径记下来（`Cell` 与
    /// [`Self::runtime_esc_area`] 同一思路）。
    welcome_badge_areas: std::cell::Cell<[Option<ratatui::layout::Rect>; BADGE_SEGMENT_COUNT]>,
    /// 正在被悬停的徽标段下标（悬停段染成淡黄色，点击跳到对应设置页）。
    welcome_badge_hover: Option<usize>,
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
    /// 输入框插入点本帧所在的屏幕坐标（渲染时登记，绘制完把终端光标挪过去）。
    ///
    /// 光标块是自绘的，终端光标一直隐藏；但输入法的候选窗与前摄文本跟的是**终端光标**的
    /// 位置，不让位就会停在最后一处写屏位置（通常是会话区末尾），打字时前摄文字飘在别处。
    /// 与 [`Self::runtime_esc_area`] 同样由渲染路径写入。
    composer_caret: std::cell::Cell<Option<ratatui::layout::Position>>,
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
            version: format!("v{}", omnicrawl_version::VERSION),
            upgrade_available: false,
            records: Vec::new(),
            composer: Composer::default(),
            scroll_from_bottom: 0,
            status: None,
            notice_line: None,
            streaming_tools: HashMap::new(),
            todos: Vec::new(),
            paused: false,
            turn: TurnState::Idle,
            turn_outcome: None,
            telemetry: Telemetry::default(),
            mcp_servers: 0,
            turn_started: None,
            net_approval: None,
            // 缺省显示思考（与 Python 的 `ui.show_thinking` 缺省一致），启动期由宿主按配置覆盖。
            show_thinking: true,
            // 缺省折叠（与 `ui.thinking_collapsed` 的缺省一致），启动期由宿主按配置覆盖。
            thinking_collapsed: true,
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
            hud_text: StyledText::new(),
            hud_workspace_hover: false,
            hud_workspace_area: std::cell::Cell::new(None),
            selection: None,
            runtime_esc_hover: false,
            welcome_badge_areas: std::cell::Cell::new([None; BADGE_SEGMENT_COUNT]),
            welcome_badge_hover: None,
            stop_button_hover: None,
            retry_button_hover: None,
            subagent_view_hover: None,
            runtime_esc_area: std::cell::Cell::new(None),
            composer_caret: std::cell::Cell::new(None),
            sessions_menu: SessionsMenu::new(),
            refresh: RefreshVersions::default(),
            conversation_cache: std::cell::RefCell::new(ConversationCache::default()),
            image_previews: std::cell::RefCell::new(ImagePreviews::default()),
            reasoning_reveal: std::cell::RefCell::new(ReasoningReveal::default()),
            user_image_sequence: 0,
        };
        // 首帧先把 HUD 文本装好，渲染路径保持只读：即使宿主一次都没 tick 过，
        // 底部 HUD 也有内容可画（测试直接构造 AppState 时会走这条路径）。
        state.refresh_hud();
        // 首帧把底部置脏，保证第一帧一定会画出来。
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
                            body: String::new(),
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
                    // 卡片正文与折叠统计都基于压缩文本。
                    if let Some(card) = self.tool_card_mut(&payload.call_id) {
                        card.body = crate::ui::fullscreen::rendering::tool_diff::stored_result_text(
                            &card.name,
                            &payload.output,
                        );
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
                // `finished` 不再往卡片上加计量行：净省按回合累加，由回合末的合并提示统一回答。
                let failed = payload.phase == "failed";
                let note = if failed {
                    Some(payload.error.clone())
                } else if finished {
                    None
                } else {
                    Some("正在压缩…".to_string())
                };
                if let Some(entry) = self.streaming_tools.get_mut(&payload.call_id) {
                    entry.compression = note;
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
                    body: String::new(),
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
                        card.body = crate::ui::fullscreen::rendering::tool_diff::stored_result_text(
                            &card.name,
                            &payload.result.output,
                        );
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
                self.turn_outcome = Some(if payload.paused {
                    TurnOutcome::Warning
                } else {
                    TurnOutcome::Done
                });
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
            // 压缩计量与模型观测点只用于宿主侧计量，不改动对话视图；
            // 压缩的可见边界由内核另发的 `turn.notice`（「已压缩：N Token」）落到会话流。
            HostEvent::ModelResponseAfter(_) | HostEvent::ModelRequestError(_) => {}
        }
    }
}

















#[cfg(test)]
mod tests;
