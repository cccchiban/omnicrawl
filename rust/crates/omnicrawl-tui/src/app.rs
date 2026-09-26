//! 接线层：内核帧、用户按键、工具执行层与状态机之间的翻译。
//!
//! 工具执行放在独立线程上（一个调用一个线程，同批并发），主线程只跑事件循环：
//! 执行结果经通道回到主线程，再回填批次并按模型顺序回内核。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::{json, Value};

use omnicrawl_commands::framework::{Channel, CommandResult, ParsedCommand};
use omnicrawl_commands::slash::{
    build_review_task_prompt, check_review_preconditions, format_review_report,
    registry as command_registry, workspace_switch_success_message, REVIEW_TASK_DESCRIPTION,
    WORKSPACE_SWITCH_PENDING_MESSAGE,
};
use omnicrawl_commands::SessionSummary;
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::core::workspace::save_workspace_root;
use omnicrawl_config::core::settings::{
    load_feature_enabled, load_show_thinking, save_context_compaction_trigger_percent,
    save_context_window_tokens, save_feature_enabled, save_mcp_config, save_show_thinking,
    save_subagent_setting, McpConfigData, McpPolicyData, McpServerData,
};
use omnicrawl_config::features::advisor::{
    load_advisor_config, save_advisor_config, AdvisorConfig,
};
use omnicrawl_config::features::agent_workspace::{
    load_agent_workspace_config, save_agent_workspace_config, AgentWorkspaceConfig,
};
use omnicrawl_config::features::context_compaction::load_context_compaction_config;
use omnicrawl_config::features::desensitization::{
    load_desensitization_config, save_desensitization_config, DesensitizationConfig,
};
use omnicrawl_config::features::image_gen::{
    load_image_gen_configuration, save_image_gen_configuration, ImageGenConfiguration,
};
use omnicrawl_config::features::run_guard::{
    load_run_guard_config, save_run_guard_config, RunGuardConfig,
};
use omnicrawl_config::features::subagents::{
    load_subagent_config, validate_subagent_advanced_setting, SubAgentConfig,
};
use omnicrawl_config::features::tool_output_compression::{
    load_tool_output_compression_config, save_tool_output_compression_config,
    ToolOutputCompressionConfig,
};
use omnicrawl_config::features::tools::{
    load_disabled_tools, load_tool_switches, save_tool_switch, TOOL_SWITCH_KEYS, TOOL_SWITCH_LABELS,
};
use omnicrawl_config::features::tts::{
    load_tts_configuration, save_tts_configuration, TtsConfiguration,
};
use omnicrawl_config::features::tts_api::{
    load_tts_api_configuration, save_tts_api_configuration, TtsApiConfiguration,
};
use omnicrawl_config::models::channels::{
    default_channel, load_channel_configuration, provider_options, save_channel_configuration,
    ChannelConfig, ChannelConfiguration,
};
use omnicrawl_config::models::llm::{
    load_llm_config, normalize_reasoning_effort, save_active_model_ref, save_reasoning_effort,
    ActiveModelRef, LlmConfig,
};
use omnicrawl_config::models::llm_multi::apply_model_selection;
use omnicrawl_config::models::model_store::load_model_store;
use omnicrawl_config::models::vision::{
    load_vision_configuration, resolve_native_vision, save_native_vision,
    save_vision_configuration, VisionConfiguration,
};
use omnicrawl_controllers::settings::context_compaction_trigger_tokens;
use omnicrawl_controllers::subagents::definitions::AgentDefinitionRegistry;
use omnicrawl_controllers::subagents::tasks::is_terminal;
use omnicrawl_controllers::vision_proxy::vision_proxy_configured;
use omnicrawl_controllers::workspace::{
    pending_worktrees_error, resolve_switch_target, subagent_drain_error, WorktreeRef,
};
use omnicrawl_controllers::AgentError;
use omnicrawl_core::{ToolCall, ToolResult};
use omnicrawl_host::plugins::PluginHost;
use omnicrawl_host::prompt::{PromptOptions, PromptRuntime};
use omnicrawl_host::prompt_cache::build_prompt_cache_identity;
use omnicrawl_ipc::{
    bridge::{
        Command, HostEvent, InitializeParams, KernelCompactionConfig, KernelModelConfig,
        KernelSessionConfig, ModelHookRequest, ModelHookResult, SessionAppendParams,
        SessionHistoryParams, SessionListParams, SessionModelSettings, SessionRenameParams,
        SessionResumeParams, SessionSettingsParams, SubagentRunParams, ToolBatch,
    },
    error_code, Frame, Id, PROTOCOL_VERSION,
};
use omnicrawl_mcp::client::McpClientManager;
use omnicrawl_mcp::config::load_mcp_config;
use omnicrawl_session::{PromptHistoryEntry, SessionIndexEntry};
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, finalize_subagent_worktrees, IsolationSession,
};

use crate::args::{ApprovalMode, Options};
use crate::commands::{self, TuiHostAgent};
use crate::host::{self, BatchStep, Waiting};
use crate::kernel::{frame_api_key_env, KernelClient};
use crate::state::{AppState, Record};
use crate::tools::{
    AdvisorOptions, ImageGenOptions, MemoryOptions, RegistryOptions, ToolRegistry, TtsOptions,
};
use crate::ui;
use crate::ui::config_chat::{ConfigChatEvent, ConfigChatState};
use crate::ui::conversation::{self, LineHit};
use crate::ui::file_picker::{FilePickerEvent, FilePickerState};
use crate::ui::fullscreen::input::menu::{MenuAction, MenuKey};
use crate::ui::fullscreen::input::sessions_menu::SessionMenuItem;
use crate::ui::queue::{self, QueueHit};
use crate::ui::settings::{
    nearest_compaction_percent, reasoning_label, ChannelRow, FieldValue, FormKind, McpChange,
    McpServerDraft, McpServerRow, McpSettingsValues, SettingsChange, SettingsEvent, SettingsState,
    SettingsValues, SubagentChange, SubagentRow, ToolSwitchRow, TtsChange, TtsDraft, TtsValues,
    VisionChange, VisionModelRef, SUBAGENT_ADVANCED_SPECS,
};
use crate::ui::splash::{report_startup_log, LogLevel};
use omnicrawl_tts::api::ApiTtsConfig;
use omnicrawl_tts::config::TtsConfig;
use omnicrawl_tts::download::{download_models_into, models_ready};
use omnicrawl_tts::engine::TtsEngine;
use omnicrawl_tts::voices::{
    all_voice_names, default_root, delete_custom_voice, list_custom_voice_names,
};

/// 握手响应最多等这么久；内核启动即刻回帧，卡住说明进程有问题。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 方向键滚动步长与翻页步长（行）。
const SCROLL_STEP: isize = 1;
const PAGE_STEP: isize = 10;
/// 鼠标滚轮一格滚动的行数（Textual 的滚轮一步也是按行推进，这里取同一观感）。
const WHEEL_STEP: isize = 3;

/// 退出前等一次在途慢命令结果的时长（拿得到的候选子系统要 close 干净）。
const SLOW_TASK_SETTLE_MS: u64 = 200;

/// 装配工作区切换的候选子系统（工具表 + MCP 连接 + 提示词运行时）。
///
/// 放在模块级自由函数里是因为它同时被同步入口与工作线程调用：只读快照 + 新根，
/// 不碰 `App` 与旧工作区的任何东西（因此可以跨线程）。失败时主动 close 已经建好的
/// 候选资源（MCP 与后台任务），不留孤儿进程。
fn prepare_workspace(
    environment: &ConfigEnvironment,
    options: &Options,
    root: &Path,
    registry_options: RegistryOptions,
    command_timeout_seconds: i64,
) -> Result<PreparedWorkspace, String> {
    let registry = match ToolRegistry::new(root, &registry_options, command_timeout_seconds) {
        Ok(registry) => registry,
        Err(error) => {
            if let Some(mcp) = registry_options.mcp.as_ref() {
                mcp.close();
            }
            return Err(format!("工作区切换失败：{}", error.message));
        }
    };
    match load_prompt_runtime(environment, options, root) {
        Ok(prompt) => Ok(PreparedWorkspace { registry, prompt }),
        Err(error) => {
            registry.close_monitors();
            registry.close_mcp();
            Err(error)
        }
    }
}

/// 工具调用前的插件守卫：`tool.call.before` → `tool.approval.before` → `tool.execute.before`。
///
/// 与 `omnicrawl-host::turn` 的同名逻辑同序同文案：返回 `Err(reason)` 表示被挡下
/// （`reason` 已是展示给用户的文案），`Ok(Some(arguments))` 表示插件改写了调用参数。
fn plugin_tool_guards(
    plugins: &PluginHost,
    call: &ToolCall,
    requires_confirmation: bool,
    mode: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
    let mut arguments = call.arguments.clone();
    if let Err(error) = plugins.tool_call_before(&call.name, &mut arguments) {
        eprintln!(
            "[tui] {0} 被插件挡下（tool.call.before）：{error}",
            call.name
        );
        return Err(format!("插件拒绝工具调用：{}。", call.name));
    }
    if let Err(error) =
        plugins.tool_approval_before(&call.name, &arguments, requires_confirmation, mode)
    {
        eprintln!(
            "[tui] {0} 被插件挡下（tool.approval.before）：{error}",
            call.name
        );
        return Err(format!("插件在审批前拒绝：{}。", call.name));
    }
    if let Err(error) = plugins.tool_execute_before(&call.name, &arguments) {
        eprintln!(
            "[tui] {0} 被插件挡下（tool.execute.before）：{error}",
            call.name
        );
        return Err(format!("插件在执行前拒绝：{}。", call.name));
    }
    if arguments == call.arguments {
        Ok(None)
    } else {
        Ok(Some(arguments))
    }
}

/// 一次工具执行的完成通知。
struct ToolCompletion {
    index: usize,
    call: ToolCall,
    result: ToolResult,
    vision: Option<host::VisionPayload>,
}

/// 主通道是否走命令行给定的外部渠道（`--base-url` 非空）。
///
/// 这种情形下 provider / protocol / 生成选项 / 凭据变量名全由命令行决定，不再叠加
/// `config.toml` 的渠道设置。
pub fn uses_external_channel(options: &Options) -> bool {
    !options.base_url.trim().is_empty()
}

/// 握手交给内核的凭据环境变量名。
///
/// 内核的 `read_api_key` 只看这个名字对应的环境变量，所以起内核时的凭据注入
/// （`KernelClient::spawn_with_env`）必须用同一个名字，否则注入了也不会被读到。
///
/// 口径：命令行给了 `--base-url` 时用命令行那个名字；否则优先 `config.toml` 渠道里的名字，
/// 渠道没写名字就用 Provider 默认名。
pub fn effective_api_key_env(
    external_channel: bool,
    provider: &str,
    llm_env: &str,
    cli_env: &str,
) -> String {
    if external_channel {
        return frame_api_key_env(provider, cli_env);
    }
    if llm_env.trim().is_empty() {
        frame_api_key_env(provider, "")
    } else {
        llm_env.to_string()
    }
}

/// 按工作区装配提示词运行时：模板 → system prompt、AGENTS.md → 项目规范、
/// Skill 目录 → 索引。
///
/// 启动（`App::new`）与运行中切换工作区（`/workspace`）共用同一份规则：两条路径
/// 的系统提示词/项目规范口径必须一致，否则切完工作区后上下文会「换了一份规范」。
/// 装配失败只降级为「只有命令行 system prompt」的旧行为，不阻断界面；两次都失败
/// 才算致命。
fn load_prompt_runtime(
    environment: &ConfigEnvironment,
    options: &Options,
    workspace: &Path,
) -> Result<PromptRuntime, String> {
    let mut prompt_options = PromptOptions::new(workspace.to_path_buf());
    prompt_options.agent_temp_dir = omnicrawl_host::prompt::DEFAULT_AGENT_TEMP_DIR.to_string();
    prompt_options.workspace_detection_summary =
        omnicrawl_config::core::context::detect_project_context(environment, None)
            .detection_summary();
    prompt_options.system_prompt_override = Some(options.system_prompt.clone());
    prompt_options.advisor_active = options.advisor.enabled;
    prompt_options.advisor_blacklisted = options
        .advisor
        .disabled_for_models
        .iter()
        .any(|name| name == &options.model);
    match PromptRuntime::load(environment, prompt_options) {
        Ok(prompt) => Ok(prompt),
        Err(error) => {
            eprintln!("[tui] 系统提示词装配失败，改用命令行给的文本：{error}");
            let mut fallback = PromptOptions::new(workspace.to_path_buf());
            fallback.system_prompt_override = Some(options.system_prompt.clone());
            PromptRuntime::load(environment, fallback)
                .map_err(|error| format!("系统提示词装配失败：{error}"))
        }
    }
}

/// 两个路径是否指向同一目录（先规范化再比较，避免 `./x` 与绝对路径判不等）。
fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// 审查闸的判定结果：直接放行、批准、或带原因拒绝。
///
/// 与 `omnicrawl-host::turn` 的同名逻辑同义，但 TUI 的执行线程不能同步跑审查模型
/// （审查是一次网络请求，会阻塞界面），因此这里把「需要审查」与「调用审查模型」
/// 拆成两步：批次线程先判定哪些调用需要审查，再在后台线程完成模型调用，结果经
/// [`ToolCompletion`] 通道回填。
///
/// 构建审查运行期（含脱敏工厂）：模型取 `approval.review_model`，为空时回落主模型；
/// 基地址与凭据沿用主渠道。
fn build_review_options(
    llm: &LlmConfig,
    environment: &ConfigEnvironment,
) -> Option<omnicrawl_host::review::ReviewOptions> {
    let review_model =
        omnicrawl_config::features::approval::load_approval_review_model(environment, None)
            .unwrap_or_default();
    let model = if review_model.trim().is_empty() {
        llm.model.clone()
    } else {
        review_model
    };
    if model.trim().is_empty() {
        return None;
    }
    Some(omnicrawl_host::review::ReviewOptions {
        model,
        base_url: llm.base_url.clone(),
        api_key: llm.api_key.clone(),
        api_key_env: llm.api_key_env.clone(),
        request_timeout_seconds: llm.request_timeout_seconds,
        masking: omnicrawl_host::review::masking_from_config(environment).map(Arc::new),
    })
}

/// 内核 stderr 行 → 会话区提示文案；不是报错行时返回 `None`。
///
/// 内核把 `[kernel] 回合失败：…` 一类诊断写到 stderr；只有这些行值得占用会话流，
/// 常规信息（如「会话已就绪」）直接丢弃。
fn kernel_error_notice(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let is_error = ["失败", "错误", "异常", "无法"]
        .iter()
        .any(|keyword| trimmed.contains(keyword));
    is_error.then(|| trimmed.to_string())
}

pub struct App {
    pub state: AppState,
    pub options: Options,
    pub kernel: KernelClient,
    pub quit: bool,
    /// 宿主工作区根：`initialize` 交给内核，供回合快照（`/undo`）使用。
    pub workspace: PathBuf,
    /// 提示词运行时：system prompt、AGENTS.md 合并结果与 Skill 索引（`initialize` 与
    /// `/plan`、`/skills` 都读它；模式切换后连 system prompt 一起下发给内核）。
    prompt: PromptRuntime,
    registry: Arc<ToolRegistry>,
    /// 构造共享工具表用的选项：子任务的隔离批次要用它另建一个「根在别处」的表。
    registry_options: RegistryOptions,
    /// 本批工具的执行环境：子任务带隔离根时是临时工具表，否则用共享的那张。
    batch_registry: Option<Arc<ToolRegistry>>,
    completions: Receiver<ToolCompletion>,
    completion_sender: Sender<ToolCompletion>,
    /// 当前批次的执行截止时间：与 Python 一致，按批次绝对时刻算，
    /// 单个慢工具不会因为「每个工具各给一次超时」而把等待累加。
    tool_deadline: Option<Instant>,
    /// 待响应的宿主命令（`turn.undo`、`session.compact`）：响应到达时据此回填结果。
    pending_commands: HashMap<Id, KernelCommand>,
    /// 同步内核往返期间收到的其他帧：按原顺序交给下一次 `drain_frames`，不丢通知。
    deferred_frames: VecDeque<Frame>,
    next_turn: u64,
    /// 顾问可见的工作分支（工具批次开始前由对话记录刷新）。
    advisor_context: Arc<Mutex<Vec<serde_json::Value>>>,
    /// 设置面板：`Some` 表示它正铺满整屏，键盘都归它。
    pub settings: Option<SettingsState>,
    /// 待回填的 `session.settings` 请求：内核拒绝时要在状态文本里如实说明。
    settings_request: Option<(Id, SettingsChange)>,
    /// 待回填的「提示词更新」请求（`/plan` 启用模式后下发 system prompt 与上下文消息）。
    prompt_request: Option<Id>,
    /// 启动期插件加载诊断：由 `main.rs` 写进启动页日志框（不进对话流，避免顶掉首屏 Logo）。
    pub startup_plugin_lines: Vec<String>,
    /// 启动期解析出来的模型/渠道视图（config.toml + models.toml + 环境变量）。
    ///
    /// 握手与设置面板都读它：`initialize.model` 的 Provider/协议/生成选项/上下文窗口
    /// 与真实配置同源，不再是空壳；切换渠道后这里会被同步更新。
    llm: LlmConfig,
    /// 最近一帧的终端区域：鼠标命中判定与渲染共用同一套布局计算（[`ui::layout`]）。
    viewport: Rect,
    /// 插件运行期：与 API 同源（配置 → `PluginRuntime`），Hook 在回合与工具边界上分发。
    plugins: Arc<PluginHost>,
    /// 审查运行期（`approval.mode = review` 时用）：与 Python `_review_tool_call` 同源。
    ///
    /// 审查模型不可用（未配模型或凭据）时为 `None`，此时需审查的调用按 fail-closed 拒绝。
    review: Option<omnicrawl_host::review::ReviewOptions>,
    /// 审查载荷里的两条会话事实（最近一条用户消息 + 最近一次 ask_user 问答）。
    review_context: omnicrawl_host::review::ReviewContext,
    /// 文件选择弹层（TTS 参考音频）：`Some` 时铺在设置面板之上，键盘都归它。
    pub file_picker: Option<FilePickerState>,
    /// 配置对话弹层（`/settings --chat`）：`Some` 时铺满整屏，键盘都归它。
    pub config_chat: Option<ConfigChatState>,
    /// 后台 TTS 任务（模型下载 / 音色克隆）的结果通道。
    tts_task: Option<Receiver<TtsTaskResult>>,
    /// 本界面独有的 Monitor 日志消费游标（对映 Python `MonitorStateAdapter`）。
    monitor_state: crate::monitor::MonitorStateAdapter,
    /// 上次轮询 Monitor 事件的时刻：主循环每帧调 `tick_monitor_events`，按
    /// `MONITOR_POLL_INTERVAL` 节流。
    monitor_polled_at: Instant,
    /// 主 Agent 隔离区会话：退出收尾时 apply 变更 + 按策略清理（对映 Python `attach_isolation_session`）。
    isolation: Option<IsolationSession>,
    /// 内核当前持有的会话 id：握手与各会话命令的回执里同步过来。
    ///
    /// `/new` 的提示文案、`/sessions` 的当前项标记与「`/resume` 是否真换了会话」都看它，
    /// 因此它是宿主侧唯一需要跟着内核走的会话状态。
    session_id: String,
    /// 退出时 `session.close.before` 是否已发：`request_shutdown` 可能被调多次
    /// （命令退出一次、`main` 的收尾一次），但关闭钩子只能各发一次。
    session_close_before_sent: bool,
    /// 慢命令的宿主侧后台任务（`/workspace`、`/mcp`），每帧轮询取回结果。
    slow_task: Option<SlowTask>,
    /// 会话区右缘滚动条正在被按住拖动（松手才结束）。
    scrollbar_drag: bool,
    /// 内核 stderr 的逐行通道：运行期报错转成会话区提示，不再直接写终端盖住输入框。
    kernel_logs: Option<Receiver<String>>,
    /// 渠道页「模型 ID」的自动检测后台任务。
    channel_models_task: Option<Receiver<ChannelModelsResult>>,
}

/// 渠道模型列表自动检测的产出：候选模型 ID 与失败原因。
struct ChannelModelsResult {
    models: Vec<String>,
    message: String,
}

/// 后台 TTS 任务的产出（下载 / 克隆），由 UI 线程的轮询取回。
enum TtsTaskResult {
    /// 模型下载：成功或失败文案。
    Download(Result<(), String>),
    /// 音色克隆：成功或失败文案。
    Clone(Result<(), String>),
}

/// 慢命令的宿主侧后台任务（对映 Python `CommandOutcome.execution == "slow"`）。
///
/// Python 把延迟执行体交给工作线程（线程安全的 Agent）；宿主的 `App` 与工具表不是
/// 跨线程可共享的，因此这两条命令由宿主接管，只把真正慢的那一段放进线程：
/// `/mcp` 的状态文本（会触发 MCP 发现/连接）与 `/workspace` 的候选子系统装配
/// （工具表 + MCP + 提示词运行时）；提交与内核下发仍在主线程。
struct SlowTask {
    kind: SlowTaskKind,
    outcome: Receiver<SlowOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlowTaskKind {
    /// `/workspace <路径>`：预备阶段在工作线程，提交在主线程。
    WorkspaceSwitch,
    /// `/mcp`：状态文本在工作线程读。
    McpStatus,
}

impl SlowTaskKind {
    /// 后台任务进行中的状态行文案。
    fn working_status(self) -> &'static str {
        match self {
            Self::WorkspaceSwitch => "正在准备新工作区…",
            Self::McpStatus => "正在读取 MCP 状态",
        }
    }
}

/// 慢命令线程的产出。
enum SlowOutcome {
    WorkspacePrepared {
        /// 解析后的新工作区根。
        root: PathBuf,
        /// 用户输入的原文（写回 config.toml 用它，与命令层同口径）。
        requested: String,
        /// 候选子系统；失败时是已经带前缀的中文原因。
        prepared: Result<PreparedWorkspace, String>,
    },
    McpStatus(String),
}

/// 工作线程装配好的候选子系统。
///
/// 只装不提交：工具表与提示词运行时的所有权直接交给主线程，主线程负责插件门禁、
/// 收尾旧资源与状态切换（避免跨线程改 `App` 状态）。
struct PreparedWorkspace {
    registry: ToolRegistry,
    prompt: PromptRuntime,
}

impl App {
    pub fn new(options: Options, kernel: KernelClient, workspace: &Path) -> Result<Self, String> {
        let advisor_context: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let advisor_tools: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let advisor_messages = {
            let slot = Arc::clone(&advisor_context);
            Arc::new(move || slot.lock().map(|guard| guard.clone()).unwrap_or_default())
        };
        let advisor_inventory = {
            let slot = Arc::clone(&advisor_tools);
            Arc::new(move || slot.lock().map(|guard| guard.clone()).unwrap_or_default())
        };
        // 记忆开关与 Python 侧同源：`entry.py` 读 `load_feature_enabled("memory", default=True)`，
        // 即缺省开启。此前这里直接走 `RegistryOptions::default()`（`memory_enabled: false`），
        // 导致记忆整组工具永远进不了表，而 Python 侧缺省是进的。
        let memory_enabled = load_feature_enabled(
            &ConfigEnvironment::from_process(),
            "memory",
            true,
            None,
            None,
        )
        .unwrap_or(true);
        let registry_options = RegistryOptions {
            session_held_by_kernel: options.session_root.is_some(),
            // 记忆整组工具进表，作用域与 Python 的 `_create_memory_stores` 对齐：项目级与
            // 用户级常开（目录由 `MemoryOptions::store` 按 `DEFAULT_MEMORY_DIRECTORY` 与
            // `$HOME` 解析）。会话级记忆不开——内核自持会话时宿主拿不到 session id，
            // 强行开启只会让模型调用 `scope="session"` 时拿到「未启用」。
            memory_enabled,
            memory: MemoryOptions {
                project_enabled: true,
                user_enabled: true,
                session_enabled: false,
                ..MemoryOptions::default()
            },
            subagent_types: subagent_role_names(),
            mcp: mcp_manager(workspace),
            image_gen: ImageGenOptions {
                enabled: options.image_gen.enabled,
                base_url: options.image_gen.base_url.clone(),
                model: options.image_gen.model.clone(),
                api_key_env: options.image_gen.api_key_env.clone(),
                ..ImageGenOptions::default()
            },
            advisor: AdvisorOptions {
                enabled: options.advisor.enabled,
                model: options.advisor.model.clone(),
                base_url: if options.advisor.base_url.trim().is_empty() {
                    options.base_url.clone()
                } else {
                    options.advisor.base_url.clone()
                },
                api_key_env: if options.advisor.api_key_env.trim().is_empty() {
                    options.api_key_env.clone()
                } else {
                    options.advisor.api_key_env.clone()
                },
                effort: options.advisor.effort.clone(),
                executor_model: options.model.clone(),
                disabled_for_models: options.advisor.disabled_for_models.clone(),
                timeout_seconds: options.tool_timeout_seconds,
                messages: advisor_messages,
                tools: advisor_inventory,
                ..AdvisorOptions::default()
            },
            tts: tts_options(workspace),
            ..RegistryOptions::default()
        };
        let registry = Arc::new(
            ToolRegistry::new(
                workspace,
                &registry_options,
                options.command_timeout_seconds,
            )
            .map_err(|error| format!("工具表构建失败：{}", error.message))?,
        );
        if let Ok(mut slot) = advisor_tools.lock() {
            *slot = registry.tool_inventory();
        }
        let project = workspace
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "omnicrawl".to_string());
        let mut state = AppState::new(project, options.model.clone(), options.approval);
        state.telemetry.context_window = options.context_window_tokens;
        // 思考显示是纯界面开关：启动期按 `ui.show_thinking` 定初值，设置面板里可即时改。
        state.show_thinking =
            load_show_thinking(&ConfigEnvironment::from_process(), None).unwrap_or(true);
        state.mcp_servers = registry
            .mcp()
            .map(|manager| manager.config().enabled_servers().len() as u64)
            .unwrap_or(0);
        let prompt = load_prompt_runtime(&ConfigEnvironment::from_process(), &options, workspace)?;
        let (completion_sender, completions) = mpsc::channel();
        // 模型/渠道视图：配置读不出来时退回环境变量默认值，界面照常可用（缺什么会由内核回错）。
        let llm = match load_llm_config(&ConfigEnvironment::from_process()) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("[tui] 模型配置读取失败，按环境变量默认值启动：{error}");
                LlmConfig::with_environment(&ConfigEnvironment::from_process())
            }
        };
        // 上下文占用段用「CLI 显式值 → 配置」的同一套口径（与发给内核的 handshake 一致）；
        // 只认 CLI 值会出现总量未知的 `0/1 0%`，而且与内核的实际窗口不一致。
        let context_window = effective_context_window(options.context_window_tokens, &llm);
        state.telemetry.context_window = u64::try_from(context_window).ok().filter(|value| *value > 0);
        // 插件运行期：启动失败只影响插件本身，工作台照常可用。
        // 诊断不再写进对话流（那样会把首屏的欢迎 Logo 顶掉），而是交给启动页日志框：
        // `main.rs` 的 `prepare_startup` 读 [`Self::startup_plugin_lines`] 再写进 splash。
        let plugins = Arc::new(PluginHost::from_environment(
            &ConfigEnvironment::from_process(),
            workspace,
        ));
        let startup_plugin_lines = plugins.start();
        if plugins.active() {
            plugins.notify_app_started();
        }
        // 审查运行期：与 Python `_review_tool_call` 同源；未配模型时为 `None`，
        // 此时 `review` 模式下需审查的调用按 fail-closed 拒绝。
        let review = build_review_options(&llm, &ConfigEnvironment::from_process());
        // 命令菜单的候选表来自统一命令源（注册表声明），启动时装载一次。
        state.composer.set_commands(commands::command_options());
        Ok(Self {
            state,
            options,
            kernel,
            quit: false,
            workspace: workspace.to_path_buf(),
            prompt,
            registry,
            registry_options,
            batch_registry: None,
            completions,
            completion_sender,
            tool_deadline: None,
            pending_commands: HashMap::new(),
            deferred_frames: VecDeque::new(),
            next_turn: 1,
            advisor_context,
            settings: None,
            settings_request: None,
            prompt_request: None,
            llm,
            viewport: Rect::default(),
            plugins,
            review,
            review_context: omnicrawl_host::review::ReviewContext::default(),
            file_picker: None,
            config_chat: None,
            tts_task: None,
            monitor_state: crate::monitor::MonitorStateAdapter::default(),
            monitor_polled_at: Instant::now(),
            isolation: None,
            session_id: String::new(),
            session_close_before_sent: false,
            slow_task: None,
            scrollbar_drag: false,
            startup_plugin_lines,
            kernel_logs: None,
            channel_models_task: None,
        })
    }

    /// 接管内核 stderr 通道：由 [`Self::drain_kernel_logs`] 把报错转成会话区提示。
    pub fn attach_kernel_logs(&mut self, logs: Receiver<String>) {
        self.kernel_logs = Some(logs);
    }

    /// 挂载主 Agent 隔离区会话：退出收尾时自动 apply + 清理。
    pub fn attach_isolation_session(&mut self, session: IsolationSession) {
        self.isolation = Some(session);
    }

    /// 退出收尾：按 `[agent_workspace]` 的 `apply_on_exit` / `cleanup_on_exit` 处理隔离区，
    /// 再按 auto 策略收尾 SubAgent worktree（只清理无变更的，成果绝不自动 apply）。
    ///
    /// 对映 Python `LocalToolAgent._finalize_attached_isolation`：TUI / API / 连接器
    /// 各入口共用同一收尾路径，摘要打印到 stderr。
    fn finalize_isolation(&mut self) {
        let mut summaries: Vec<String> = Vec::new();
        if let Some(session) = self.isolation.take() {
            let config = load_agent_workspace_config(&ConfigEnvironment::from_process(), None)
                .unwrap_or_default();
            let summary = finalize_isolation_session(
                &session,
                config.apply_on_exit,
                &config.cleanup_on_exit,
                None,
                None,
            );
            if !summary.is_empty() {
                summaries.push(summary);
            }
        }
        let sub_summary = finalize_subagent_worktrees(None, "origin");
        if !sub_summary.is_empty() {
            summaries.push(sub_summary);
        }
        if !summaries.is_empty() {
            eprintln!("[isolation] {}", summaries.join("；"));
        }
    }

    /// 记录当前终端区域（每帧由事件循环在渲染前更新）。
    pub fn set_viewport(&mut self, area: Rect) {
        self.viewport = area;
    }

    /// 把当前对话记录投影成顾问可见的工作分支（user/assistant 文本，保持顺序）。
    fn refresh_advisor_context(&self) {
        let messages: Vec<serde_json::Value> = self
            .state
            .records
            .iter()
            .filter_map(|record| match record {
                Record::User(text) => Some(json!({"role": "user", "content": text})),
                Record::Assistant(text) => Some(json!({"role": "assistant", "content": text})),
                _ => None,
            })
            .collect();
        if let Ok(mut slot) = self.advisor_context.lock() {
            *slot = messages;
        }
    }

    /// 本批实际使用的工具表：隔离批次用临时表，其余用共享表。
    fn batch(&self) -> &Arc<ToolRegistry> {
        self.batch_registry.as_ref().unwrap_or(&self.registry)
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// 握手：`initialize` 带上模型配置（内核自己发请求）、工具声明与可选的会话块。
    ///
    /// 取值规则两条：
    /// - **没显式给基地址**（`--base-url` / `OPENAI_BASE_URL` 为空）时按配置走：Provider、协议、
    ///   基地址、凭据变量名、生成选项（推理强度/温度/最大输出/超时/重试/provider_options）与
    ///   上下文窗口都来自 config.toml + models.toml 的解析结果——独立运行时不依赖命令行参数。
    /// - **显式给了基地址**时视为「外部渠道」：Provider/协议/生成选项/超时/重试/凭据变量名
    ///   一律用命令行给的值（内核按运行时默认语义发请求），不叠加配置里那条渠道的设置，
    ///   免得把别的协议塞给这个端点。
    ///
    /// `--model` 始终优先。提示词缓存身份在这里按稳定前缀算出来交给内核（它据此
    /// 派生 `prompt_cache_key`），与 Python `build_prompt_cache_identity` 逐字节对齐。
    pub fn handshake(&mut self) -> Result<(), String> {
        let external_channel = uses_external_channel(&self.options);
        // 稳定前缀的三样来源：system prompt、工具声明、项目规范。
        // 先算好再进 `KernelModelConfig` 字面量，避免同一份内容算两遍（模板可能很大）。
        let system_prompt = self.prompt.system_prompt();
        let tool_declarations = self.registry.declarations();
        // 活动 Skill 取决于用户本轮输入，握手时还没有，因此传空列表；
        // 此时 `active_skill_context_hash` 就是空数组的哈希，与 Python 无命中时同值。
        let prompt_cache_identity = build_prompt_cache_identity(
            &system_prompt,
            self.workspace.as_path(),
            &self.prompt.project_instructions().unwrap_or_default(),
            &self.prompt.skill_metas(),
            &[],
            &tool_declarations,
        );
        let model = KernelModelConfig {
            model: self.options.model.clone(),
            provider: if external_channel {
                String::new()
            } else {
                self.llm.provider.clone()
            },
            protocol: if external_channel {
                String::new()
            } else {
                self.llm.protocol.clone()
            },
            base_url: if external_channel {
                self.options.base_url.clone()
            } else {
                self.llm.base_url.clone()
            },
            api_key_env: effective_api_key_env(
                external_channel,
                &self.llm.provider,
                &self.llm.api_key_env,
                &self.options.api_key_env,
            ),
            user_agent: format!("omnicrawl-tui/{}", env!("CARGO_PKG_VERSION")),
            system_prompt,
            context_messages: match self.prompt.context_messages_with_plugins(
                true,
                Some(self.plugins.as_ref()),
                None,
                None,
            ) {
                Ok(messages) => messages,
                Err(error) => {
                    // `unwrap_or_default` 会让模型静默地收不到项目规范 / Skill / 运行环境，
                    // 看起来就像「系统提示词没生效」；装不出来就明说。
                    self.state
                        .notice(format!("上下文装配失败：{error}（本轮只发系统提示词与历史）"));
                    Vec::new()
                }
            },
            tools: tool_declarations,
            options: if external_channel {
                json!({})
            } else {
                generation_options(&self.llm)
            },
            request_timeout_seconds: if external_channel {
                None
            } else if self.llm.request_timeout_seconds > 0 {
                Some(self.llm.request_timeout_seconds as f64)
            } else {
                None
            },
            // 上下文窗口与渠道无关：命令行显式值优先，其次配置。
            context_window_tokens: effective_context_window(
                self.options.context_window_tokens,
                &self.llm,
            ),
            // Provider 能力声明来自自定义模型条目的 `capabilities.prompt_cache`；
            // 未声明时内核 `should_send_prompt_cache_key` 对 GPT 系列有回退分支，
            // 与 Python 在 `capabilities.prompt_cache` 未声明时的行为一致。
            prompt_cache_capable: self.llm.prompt_cache.unwrap_or(false),
            prompt_cache_identity: prompt_cache_identity.to_identity_map(),
            // 交给内核做优先级判定：为真时带图观察直送主模型，不再走 `[vision]` 代理。
            native_vision: self.options.native_vision,
            request_retry_count: if external_channel {
                1
            } else {
                self.llm.request_retry_count.max(1) as u32
            },
        };
        let session = self.options.session_root.as_ref().map(|root| {
            Box::new(KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                session_id: String::new(),
                memory_root: None,
                workspace_root: Some(self.workspace.to_string_lossy().to_string()),
                compaction: None,
            })
        });
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_string(),
            client: json!({
                "name": "omnicrawl-tui",
                "version": env!("CARGO_PKG_VERSION"),
            }),
            model: Some(Box::new(model)),
            session,
            // TUI 总是持有插件运行期：声明能力，内核才会在模型请求前发 `model.hook`。
            plugin_model_hooks: true,
        };
        let id = self.kernel.next_id();
        let frame = Command::Initialize(params).to_frame(id.clone());
        self.kernel
            .send_frame(&frame)
            .map_err(|error| format!("发送握手帧失败：{error}"))?;

        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500));
            let Some(frame) = self.kernel.recv_timeout(wait) else {
                continue;
            };
            if frame.is_response() && frame.id() == Some(&id) {
                if let Some(error) = frame.error.as_ref() {
                    return Err(format!("内核拒绝握手：{}", error.message));
                }
                // 会话 id 只在内核侧产生（`initialize.session` 没给 id 时内核新建一条），
                // 握手回包是宿主知道「现在在哪个会话」的唯一途径。
                if let Some(session_id) = frame
                    .result
                    .as_ref()
                    .and_then(|result| result.get("session_id"))
                    .and_then(Value::as_str)
                {
                    self.session_id = session_id.to_string();
                }
                let declared = self.registry.declarations().len();
                report_startup_log(LogLevel::Info, &format!("已向内核声明 {declared} 个工具。"));
                return Ok(());
            }
            self.handle_frame(frame);
        }
        Err("等待内核握手响应超时。".to_string())
    }

    /// 首屏挂载后启动欢迎 Logo 的入场动画（对映 Python 的
    /// `_start_welcome_logo_animation`：只在空会话首屏播放一次，重复调用无效）。
    pub fn start_welcome_logo_animation(&mut self, now: Instant) {
        self.state.logo.start(now);
    }

    /// 推进一帧欢迎 Logo 动画（对映 Textual 的 0.05s 定时器）。
    ///
    /// 事件循环的节拍跟随按键与内核帧，不能按计次推进，因此把当前时刻交给动画
    /// 游标按经过时间换算帧号；动画落定后这里不再做任何事。
    pub fn tick_welcome_logo_animation(&mut self, now: Instant) {
        if self.state.logo.is_playing() {
            self.state.logo.tick(now);
        }
    }

    /// 推进底部单行轮播（对映 Python 把轮播停留/帧定时器交给 Textual 的做法）。
    ///
    /// 页面切换与解密扫描都由 [`AppState::refresh_carousel`] 驱动；推理强度不在界面
    /// 状态里（它属于模型配置），因此从宿主配置透传。
    pub fn tick_carousel(&mut self, now: Instant) {
        let reasoning_effort = self.llm.reasoning_effort.clone();
        self.state.refresh_carousel(now, &reasoning_effort);
    }

    /// 推进活跃子任务进度树的运行耗时（对映 Python 的 80ms 耗时定时器）。
    ///
    /// 只刷新仍有非终态任务的树：批次收口后树的耗时不再变化，不必每帧重算。
    pub fn tick_subagent_trees(&mut self) {
        self.state.refresh_subagent_trees();
    }

    /// 收干内核帧与工具执行结果；内核退出时收尾退出。
    pub fn drain_frames(&mut self) {
        // 先消化同步往返期间让路的帧，再读内核通道：两边都按到达顺序处理。
        while let Some(frame) = self.deferred_frames.pop_front() {
            self.handle_frame(frame);
        }
        while let Some(frame) = self.kernel.try_recv() {
            self.handle_frame(frame);
        }
        self.drain_completions();
        self.drain_kernel_logs();
        self.drain_channel_models();
        self.enforce_tool_deadline();
        if self.kernel.is_closed() && !self.quit {
            self.state.fail_turn("内核进程已退出。".to_string());
            self.plugins
                .turn_error("内核进程已退出。", None, self.current_turn_id().as_deref());
            self.quit = true;
        }
    }

    /// 把内核 stderr 里的运行期报错转成会话区提示。
    ///
    /// 内核的 stderr 以前直接继承到终端，报错会写在光标处、盖住底部输入框；
    /// 现在逐行收进通道，只有错误行（失败 / 错误 / 异常 / 无法）作为提示进会话流，
    /// 常规日志丢弃，避免刷屏。
    fn drain_kernel_logs(&mut self) {
        let Some(logs) = self.kernel_logs.take() else {
            return;
        };
        let mut lines: Vec<String> = Vec::new();
        let mut disconnected = false;
        loop {
            match logs.try_recv() {
                Ok(line) => lines.push(line),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        for line in lines {
            if let Some(message) = kernel_error_notice(&line) {
                self.state.notice(message);
            }
        }
        // 内核退出时通道断开：保留 `None`，不再每帧空转。
        if !disconnected {
            self.kernel_logs = Some(logs);
        }
    }

    /// 收集已完成的工具执行；整批就绪时回内核。
    fn drain_completions(&mut self) {
        let mut finished: Vec<ToolCompletion> = Vec::new();
        while let Ok(completion) = self.completions.try_recv() {
            finished.push(completion);
        }
        for completion in finished {
            let now = Instant::now();
            self.state
                .finish_tool_run(&completion.call, &completion.result, now);
            self.state
                .record_tool_result(completion.index, completion.result, completion.vision);
        }
        self.flush_ready_batch();
    }

    /// 整批就绪就回观察；批次 id 必须在取观察前记下（取完即卸下批次）。
    fn flush_ready_batch(&mut self) {
        let Some(request_id) = self.state.batch_request_id() else {
            return;
        };
        let attach_images = self.attach_vision_images();
        if let Some(observations) = self.state.take_observations(attach_images) {
            self.respond_batch(&request_id, observations);
        }
    }

    /// 是否把图片交给内核：原生视觉或 `[vision]` 代理任一可用即可（与 Python
    /// `route_image_result` 同义）。两者都没有时图片不会进请求——主模型看不懂图，
    /// 交出去只会白跑一趟 base64 过管道。
    fn attach_vision_images(&self) -> bool {
        self.options.native_vision || vision_proxy_configured(&ConfigEnvironment::from_process())
    }

    fn handle_frame(&mut self, frame: Frame) {
        if frame.is_notification() {
            match HostEvent::from_frame(&frame) {
                Ok(event) => {
                    // 回合落地后按 FIFO 排空排队消息（对映 Python `_finish` 里的排空调用）。
                    let finished = matches!(event, HostEvent::TurnFinished(_));
                    // 回合 id 要在 `apply` 之前取：落地处理会把当前回合卸下。
                    let turn_id = self.current_turn_id();
                    // 内核发来的插件 Hook 触发点：宿主在此分发，对映 Python 在 agent runtime
                    // 内的同名派发（压缩计量、模型请求前后）。
                    let session_id = self.current_session_id();
                    let session_id = (!session_id.is_empty()).then_some(session_id);
                    match &event {
                        HostEvent::ContextCompaction(payload) => {
                            self.plugins.compaction_after_turn(
                                payload.post_turn_context_tokens,
                                payload.trigger_context_tokens,
                                session_id.as_deref(),
                                (!payload.turn_id.is_empty()).then_some(payload.turn_id.as_str()),
                            )
                        }
                        HostEvent::ModelResponseAfter(payload) => {
                            self.plugins.model_response_after(
                                &payload.model,
                                &payload.content,
                                payload.tool_call_count,
                                session_id.as_deref(),
                            )
                        }
                        HostEvent::ModelRequestError(payload) => self.plugins.model_request_error(
                            &payload.error,
                            &payload.model,
                            session_id.as_deref(),
                        ),
                        _ => {}
                    }
                    self.state.apply(&event, Instant::now());
                    if finished {
                        self.plugins.turn_end(None, turn_id.as_deref());
                        self.drain_pending_inputs();
                    }
                }
                Err(error) => eprintln!("[tui] 未识别的内核通知：{error}"),
            }
            return;
        }
        let Some(id) = frame.id().cloned() else {
            eprintln!("[tui] 内核发来没有 id 的帧，已忽略。");
            return;
        };
        match frame.method() {
            Some(method) if method == omnicrawl_ipc::method::TOOL_BATCH => {
                self.handle_tool_batch(id, &frame);
            }
            Some(method) if method == omnicrawl_ipc::method::MODEL_REPLY => {
                // 内核自带 provider runtime 发模型请求，宿主代答路径不再需要。
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            Some(method) if method == omnicrawl_ipc::method::MODEL_HOOK => {
                self.handle_model_hook(id, &frame);
            }
            Some(method) => {
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            // 响应帧：宿主命令（`turn.undo`、`session.compact`）与 `session.settings`
            // 的结果要回填界面，其余（如 `turn.submit` 的确认）由通知驱动。
            None => {
                if let Some(command) = self.pending_commands.remove(&id) {
                    self.finish_kernel_command(command, &frame);
                } else if self.prompt_request.as_ref() == Some(&id) {
                    self.prompt_request = None;
                    if let Some(error) = frame.error.as_ref() {
                        self.state.notice(format!(
                            "内核未接受提示词更新（{}），模式提示词只在本宿主生效。",
                            error.message
                        ));
                    }
                } else if let Some((pending, change)) = self.settings_request.clone() {
                    if pending == id {
                        self.settings_request = None;
                        self.apply_settings_response(&change, &frame);
                    }
                }
            }
        }
    }

    /// 服务内核的 `model.hook`：跑 `model.request.before`，回改写后的消息或拒绝文案。
    fn handle_model_hook(&mut self, id: Id, frame: &Frame) {
        let request = match ModelHookRequest::from_frame(frame) {
            Ok(request) => request,
            Err(error) => {
                let _ = self.kernel.respond_error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!("model.hook 负载不符：{error}"),
                );
                return;
            }
        };
        let ModelHookRequest {
            mut messages,
            model,
        } = request;
        let session_id = self.current_session_id();
        let session_id = (!session_id.is_empty()).then_some(session_id);
        match self
            .plugins
            .model_request_before(&mut messages, &model, session_id.as_deref())
        {
            Ok(()) => {
                let result = ModelHookResult { messages }.to_result();
                let _ = self.kernel.respond(&id, result);
            }
            // 插件拒绝：错误响应带上拒绝文案，内核据此中止本轮。
            Err(error) => {
                let _ =
                    self.kernel
                        .respond_error(&id, error_code::INVALID_REQUEST, error.message());
            }
        }
    }

    fn handle_tool_batch(&mut self, id: Id, frame: &Frame) {
        // 协议上内核会等前一批的响应再发下一批；真的撞上时明确拒绝，
        // 而不是让新批次覆盖旧批次（那会让前一批永远等不到响应）。
        if self.state.batch_request_id().is_some() {
            let _ = self.kernel.respond_error(
                &id,
                error_code::INTERNAL_ERROR,
                "宿主仍有未完成的工具批次，拒绝并发批次。",
            );
            return;
        }
        let batch = match ToolBatch::from_frame(frame) {
            Ok(batch) => batch,
            Err(error) => {
                let _ = self.kernel.respond_error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!("tool.batch 负载不符：{error}"),
                );
                return;
            }
        };
        // 子任务的工具批次带着隔离根：本批改用「根在该目录」的临时工具表，
        // 路径保护与命令工作目录因此都落在隔离区里；没有根的批次照旧用共享表。
        self.batch_registry = match batch.workspace_root.as_deref() {
            Some(root) if !root.trim().is_empty() => {
                match ToolRegistry::new(
                    root,
                    &self.registry_options,
                    self.options.command_timeout_seconds,
                ) {
                    Ok(registry) => Some(Arc::new(registry)),
                    Err(_) => None,
                }
            }
            _ => None,
        };
        let step = self.state.start_batch(id.clone(), batch.calls);
        self.handle_batch_step(id, step);
    }

    fn handle_batch_step(&mut self, request_id: Id, step: BatchStep) {
        // 批次推进可能已经在宿主内定调了某些调用（update_todos / pause_work / ask_user）：
        // 它们不经过执行层，卡片要在这里补上终态，否则会一直停在「调用中」并继续计时。
        self.state.settle_internal_batch_calls();
        match step {
            BatchStep::Awaiting => {
                // 插件守卫先于人工审批：被拒的调用不再弹审批面板。
                let plugins = Arc::clone(&self.plugins);
                let mode = self.options.approval.as_str();
                if let Some(next) = self
                    .state
                    .guard_pending_approval(|call| plugin_tool_guards(&plugins, call, true, mode))
                {
                    self.handle_batch_step(request_id, next);
                }
            }
            BatchStep::Execute(jobs) => {
                self.tool_deadline = Some(
                    Instant::now()
                        + Duration::from_secs(self.options.tool_timeout_seconds.max(1) as u64),
                );
                // 本批的工具属于当前回合：回合取消时只回收它自己的后台任务。
                self.refresh_advisor_context();
                self.batch().set_monitor_scope(self.state.turn.turn_id());
                let mode = self.options.approval.as_str();
                for (index, mut call) in jobs {
                    // 插件守卫先于执行：被挡下的调用不进执行层，按拒绝结果回填。
                    match plugin_tool_guards(&self.plugins, &call, false, mode) {
                        Ok(Some(arguments)) => {
                            self.state.rewrite_call_arguments(index, arguments.clone());
                            call.arguments = arguments;
                        }
                        Ok(None) => {}
                        Err(reason) => {
                            self.state.begin_tool_run(&call, Instant::now());
                            self.plugin_approval_after(&call.name, false, &reason);
                            let _ = self.completion_sender.send(ToolCompletion {
                                index,
                                call,
                                result: host::denied_with_reason(&reason),
                                vision: None,
                            });
                            continue;
                        }
                    }
                    // 审查闸：`review` 模式下判定为 Review 的调用交给审查模型（与
                    // `omnicrawl-host::turn` 同序）。审查是一次网络请求，同步跑会卡界面，
                    // 因此这里只做判定：需要审查的调用单独起一个后台线程跑模型，
                    // 结果经 [`ToolCompletion`] 通道回填（拒绝时写拒绝结果）。
                    if self.needs_review_gate(&call) {
                        self.plugin_approval_after(&call.name, true, "");
                        self.state.begin_tool_run(&call, Instant::now());
                        self.spawn_review_job(index, call);
                        continue;
                    }
                    // 走到执行层的调用都已被放行（需人工审批的调用已在 `Awaiting` 分支消费）。
                    self.plugin_approval_after(&call.name, true, "");
                    self.state.begin_tool_run(&call, Instant::now());
                    self.spawn_tool_job(index, call);
                }
            }
            BatchStep::Complete => {
                self.tool_deadline = None;
                self.batch_registry = None;
                let attach_images = self.attach_vision_images();
                if let Some(observations) = self.state.take_observations(attach_images) {
                    self.respond_batch(&request_id, observations);
                }
            }
        }
    }

    /// 回答待决提问：先把问答记进审查上下文（授权边界的最新事实），再推进批次。
    ///
    /// 与 `omnicrawl-host::turn` 的 `record_ask_user` 同位：问答是审查模型判断授权
    /// 范围的最新依据，先记下来再继续推进。
    fn answer_pending_question(&mut self, answer: String) -> Option<host::BatchStep> {
        if let Some(Waiting::Question(panel)) = self.state.waiting() {
            self.review_context.record_ask_user(&panel.prompt, &answer);
        }
        self.state.answer_question(answer)
    }

    /// 审查闸判定：`review` 模式下删除类 / 下载并执行类 / 高风险 Git 调用需要审查。
    ///
    /// 判定复用 `omnicrawl-host::review::needs_review`（语义基准是 Python
    /// `_approve_tool_call` 的 `Review` 分支）；工具说明与参数 schema 取自本批工具表。
    fn needs_review_gate(&self, call: &ToolCall) -> bool {
        if self.options.approval != ApprovalMode::Review {
            return false;
        }
        let (description, schema) = self.batch().approval_facts(&call.name).unwrap_or_default();
        omnicrawl_host::review::needs_review(
            self.options.approval,
            &call.name,
            &description,
            &schema,
            &call.arguments,
        )
    }

    /// 后台线程跑审查模型，结果经 [`ToolCompletion`] 回填：批准则转正常执行，
    /// 拒绝（含审查运行期不可用）则写拒绝结果。
    ///
    /// 审查模型调用是网络请求（与主回合的模型请求同一档超时），不能放在 UI 线程上；
    /// 与 [`Self::spawn_tool_job`] 一样，一个调用一个线程。
    fn spawn_review_job(&self, index: usize, call: ToolCall) {
        let review = self.review.clone();
        let context = self.review_context.clone();
        let registry = Arc::clone(self.batch());
        let plugins = Arc::clone(&self.plugins);
        let sender = self.completion_sender.clone();
        let workspace_root = self.workspace.to_string_lossy().to_string();
        thread::spawn(move || {
            let verdict = match review.as_ref() {
                Some(options) => {
                    let (description, schema) =
                        registry.approval_facts(&call.name).unwrap_or_default();
                    let _ = schema;
                    omnicrawl_host::review::review_tool_call(
                        options,
                        &omnicrawl_host::review::ReviewRequest {
                            tool_name: &call.name,
                            description: &description,
                            arguments: &call.arguments,
                            workspace_root: &workspace_root,
                            context: &context,
                        },
                    )
                }
                // 审查运行期缺失：与 host 侧同一 fail-closed 文案。
                None => Err(
                    omnicrawl_controllers::approval::review_request_failed_reason(
                        "审查模型不可用：宿主未装配审查运行期。",
                    ),
                ),
            };
            match verdict {
                Ok(()) => {
                    // 批准：接着走正常执行（插件执行前守卫与工具执行体）。
                    plugins.tool_approval_after(&call.name, true, "", "review");
                    let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        registry.execute_with_vision(&call)
                    }));
                    let (mut result, vision) = match executed {
                        Ok(Some(execution)) => {
                            let vision = if execution.images.is_empty() {
                                None
                            } else {
                                Some(host::VisionPayload {
                                    prompt: execution.vision_prompt,
                                    images: execution.images,
                                })
                            };
                            (execution.result, vision)
                        }
                        Ok(None) => (host::unavailable_result(&call.name), None),
                        Err(_) => {
                            plugins.tool_execute_error(&call.name, "工具执行线程 panic");
                            (host::panicked_result(&call.name), None)
                        }
                    };
                    let base = if result.full_output.is_empty() {
                        result.output.clone()
                    } else {
                        result.full_output.clone()
                    };
                    result.full_output = plugins.tool_execute_after(&call.name, result.ok, &base);
                    let _ = sender.send(ToolCompletion {
                        index,
                        call,
                        result,
                        vision,
                    });
                }
                Err(reason) => {
                    // 审查拒绝也属「未批准」：MCP 调用落一条拒绝审计。
                    host::record_mcp_denial(&registry, &call.name, &call.arguments, &reason);
                    plugins.tool_approval_after(&call.name, false, &reason, "review");
                    let _ = sender.send(ToolCompletion {
                        index,
                        call,
                        result: host::denied_with_reason(&reason),
                        vision: None,
                    });
                }
            }
        });
    }

    /// 一个调用一个线程：同批工具并发执行（与 Python 宿主一致）。
    ///
    /// 执行体的 panic 必须转成一条失败观察：线程静默消失会让整批永远凑不齐，
    /// 内核就会一直等这个 `tool.batch` 的响应。
    fn spawn_tool_job(&self, index: usize, call: ToolCall) {
        let registry = Arc::clone(self.batch());
        let plugins = Arc::clone(&self.plugins);
        let sender = self.completion_sender.clone();
        thread::spawn(move || {
            let tool_name = call.name.clone();
            let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.execute_with_vision(&call)
            }));
            let (mut result, vision) = match executed {
                Ok(Some(execution)) => {
                    let vision = if execution.images.is_empty() {
                        None
                    } else {
                        Some(host::VisionPayload {
                            prompt: execution.vision_prompt,
                            images: execution.images,
                        })
                    };
                    (execution.result, vision)
                }
                Ok(None) => (host::unavailable_result(&tool_name), None),
                Err(_) => {
                    plugins.tool_execute_error(&tool_name, "工具执行线程 panic");
                    (host::panicked_result(&tool_name), None)
                }
            };
            // `tool.execute.after` 可改写展示文本（`displayText`）；模型看到的输出不变。
            let base = if result.full_output.is_empty() {
                result.output.clone()
            } else {
                result.full_output.clone()
            };
            result.full_output = plugins.tool_execute_after(&tool_name, result.ok, &base);
            let _ = sender.send(ToolCompletion {
                index,
                call,
                result,
                vision,
            });
        });
    }

    /// `tool.approval.after`：审批结论通知。
    fn plugin_approval_after(&self, tool: &str, approved: bool, reason: &str) {
        self.plugins
            .tool_approval_after(tool, approved, reason, self.options.approval.as_str());
    }

    /// 批次执行超过 `tool_timeout_seconds` 时按超时收口：未回填的调用写成超时结果，
    /// 后台线程继续运行但结果被丢弃（与 Python 的批次截止时间语义一致）。
    fn enforce_tool_deadline(&mut self) {
        let Some(deadline) = self.tool_deadline else {
            return;
        };
        if self.state.batch_request_id().is_none() {
            self.tool_deadline = None;
            return;
        }
        if Instant::now() < deadline {
            return;
        }
        let seconds = self.options.tool_timeout_seconds;
        let ready = self.state.fill_tool_timeout(seconds);
        // 不再直接写终端（会盖住输入框）：作为会话区提示显示。
        self.state.notice(format!(
            "工具执行超过 {seconds} 秒仍未完成，已按超时回收等待（后台线程的结果会被丢弃）。"
        ));
        self.tool_deadline = None;
        if ready {
            self.flush_ready_batch();
        }
    }

    fn respond_batch(&mut self, id: &Id, observations: Vec<omnicrawl_core::AgentLoopObservation>) {
        let result = omnicrawl_ipc::ToolBatchResult { observations }.to_result();
        if let Err(error) = self.kernel.respond(id, result) {
            self.state.notice(format!("回工具批次失败：{error}"));
        }
    }

    /// 处理一个终端事件。
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // 文件选择弹层是最上层模态：它开着时按键都归它。
                if self.file_picker.is_some() {
                    self.handle_file_picker_key(key);
                    return;
                }
                // 配置对话弹层同为最上层模态（`/settings --chat` 打开）。
                if self.config_chat.is_some() {
                    self.handle_config_chat_key(key);
                    return;
                }
                // 设置面板是模态页：它开着时按键都归它（与 Python 的 `SettingsScreen` 一致）；
                // 鼠标同理——面板铺满整屏，落点只能按面板自己的命中区判。
                if self.settings.is_some() {
                    self.handle_settings_key(key);
                    return;
                }
                self.handle_key(key);
            }
            Event::Mouse(_) if self.file_picker.is_some() => {}
            Event::Mouse(_) if self.config_chat.is_some() => {}
            Event::Mouse(mouse) if self.settings.is_some() => self.handle_settings_mouse(mouse),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            // 弹层开着时粘贴不落到主输入框（路径输入用键盘）。
            Event::Paste(_) if self.file_picker.is_some() => {}
            Event::Paste(text) if self.config_chat.is_some() => {
                // 配置对话接受粘贴的整句描述（本机输入法长句常用）。
                if let Some(chat) = self.config_chat.as_mut() {
                    for character in text.chars() {
                        chat.handle_key(KeyCode::Char(character));
                    }
                }
            }
            Event::Paste(text) if self.settings.is_none() => {
                // 多行粘贴折成 `[粘贴 #n +N 行]`（提交时还原），否则大段文本会把输入框撑爆。
                self.state.composer.insert_paste(&text)
            }
            Event::Resize(..) => {}
            _ => {}
        }
    }

    /// 鼠标事件：滚轮滚动消息区，左键拖选复制、按落点分派到排队预览或消息流。
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // 滚动条拖动优先，且不受「等审批」限制：按住右缘那格上下滑就是滚会话。
        if self.handle_scrollbar_mouse(&mouse) {
            return;
        }
        // 提问面板开着时不允许点击展开/撤回：回填内容会被提问模式吞掉
        // （与 Python `_withdraw_pending_input` 的守卫同源）。
        let interactive = self.state.waiting().is_none();
        // 按住 Shift 的拖选留给终端自己做（Windows Terminal 支持），否则会与应用抢选择。
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        // 点输入区边框上的 `[ ESC ]` 等同于按键盘 Esc（取消回合）——不受「等审批」限制。
        let on_esc = self
            .state
            .runtime_esc_area()
            .is_some_and(|area| area.contains(Position::new(mouse.column, mouse.row)));
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if on_esc => {
                if self.state.selection().is_some() {
                    self.state.clear_selection();
                } else if self.state.turn.is_running() {
                    self.cancel_turn();
                }
            }
            MouseEventKind::Moved => self.update_conversation_hover(mouse.column, mouse.row),
            MouseEventKind::ScrollUp => self.state.scroll_by(-WHEEL_STEP),
            MouseEventKind::ScrollDown => self.state.scroll_by(WHEEL_STEP),
            MouseEventKind::Down(MouseButton::Left) if interactive => {
                if let Some(point) = self.conversation_point(mouse.column, mouse.row) {
                    if !shift {
                        self.state.begin_selection(point.0, point.1);
                    }
                }
                self.handle_click(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(MouseButton::Left) if interactive && !shift => {
                if let Some(point) = self.conversation_point(mouse.column, mouse.row) {
                    self.state.extend_selection(point.0, point.1);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if interactive => self.finish_selection(),
            _ => {}
        }
    }

    /// 屏幕坐标 → 会话流的（显示行下标, 行内显示列）；不在会话文本区时返回 `None`。
    ///
    /// 与渲染、命中测试共用[`ui::layout`]与会话区换算，滚动/折行/宽字符都不会错位。
    fn conversation_point(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        let areas = ui::layout(self.viewport, &self.state);
        let area = areas.conversation;
        if !area.contains(Position::new(column, row)) {
            return None;
        }
        let relative = (row - area.y) as usize;
        let line = conversation::line_index(
            &self.state,
            area,
            self.state.scroll_from_bottom,
            relative,
        )?;
        let text = conversation::text_area(area);
        Some((line, usize::from(column.saturating_sub(text.x))))
    }

    /// 松左键：有选区就写剪切板并清掉高亮；没拖动（空选区）就当普通点击。
    /// 滚动条拖动：命中返回 `true`（这一下不再当点选/拖选处理）。
    fn handle_scrollbar_mouse(&mut self, mouse: &MouseEvent) -> bool {
        let areas = ui::layout(self.viewport, &self.state);
        let Some(column) = conversation::scrollbar_column(areas.conversation) else {
            return false;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column == column => {
                self.scrollbar_drag = true;
                self.drag_scrollbar_to(mouse.row);
                true
            }
            MouseEventKind::Drag(MouseButton::Left) if self.scrollbar_drag => {
                self.drag_scrollbar_to(mouse.row);
                true
            }
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_drag => {
                self.scrollbar_drag = false;
                true
            }
            _ => false,
        }
    }

    fn drag_scrollbar_to(&mut self, row: u16) {
        let areas = ui::layout(self.viewport, &self.state);
        if let Some(offset) = conversation::scroll_offset_for_row(&self.state, areas.conversation, row)
        {
            self.state.scroll_from_bottom = offset;
        }
    }

    fn finish_selection(&mut self) {
        let Some(selection) = self.state.selection() else {
            return;
        };
        if selection.is_empty() {
            self.state.clear_selection();
            return;
        }
        let width = conversation::text_area(ui::layout(self.viewport, &self.state).conversation).width;
        let Some(text) = conversation::selection_text(&self.state, width) else {
            self.state.clear_selection();
            return;
        };
        let rows = text.lines().count();
        match crate::clipboard::copy_text(&text) {
            Ok(()) => {
                self.state.clear_selection();
                self.state
                    .show_notice_line(format!("已复制 {rows} 行到剪切板。"), Instant::now());
            }
            Err(error) => self.state.show_notice_line(
                format!("复制失败：{error}（可按住 Shift 拖选，用终端自带的复制）"),
                Instant::now(),
            ),
        }
    }

    /// 鼠标移动：只关心输入区方框上边框里的 `[ ESC ]`，命中则点亮为淡黄色。
    ///
    /// 位置由渲染路径登记（`AppState::runtime_esc_area`），所以这里不必重算显示行——
    /// 长会话下那是一次不便宜的折行。
    fn update_conversation_hover(&mut self, column: u16, row: u16) {
        if !self.state.turn.is_running() {
            self.state.runtime_esc_hover = false;
            return;
        }
        let hover = self
            .state
            .runtime_esc_area()
            .is_some_and(|area| area.contains(Position::new(column, row)));
        self.state.runtime_esc_hover = hover;
    }

    /// 设置面板的鼠标事件：悬停只记录行（加亮交给渲染层），左键按命中区分派。
    ///
    /// 面板是整屏模态页，各页的滚动都跟着选中项走，因此滚轮不在这里接管。
    fn handle_settings_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Moved => {
                if let Some(settings) = self.settings.as_mut() {
                    let hover = settings.hit_at(mouse.column, mouse.row);
                    settings.set_hover(hover);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // 先取出事件再分派：`click` 要可变借用面板，而事件处理要可变借用 `App`。
                let event = {
                    let Some(settings) = self.settings.as_mut() else {
                        return;
                    };
                    let Some(action) = settings.hit_at(mouse.column, mouse.row) else {
                        return;
                    };
                    settings.click(action)
                };
                self.dispatch_settings_event(event);
            }
            _ => {}
        }
    }

    /// 左键点击：先在排队预览条上找命中，再到消息区找工具卡/思考段命中。
    fn handle_click(&mut self, column: u16, row: u16) {
        let areas = ui::layout(self.viewport, &self.state);
        let point = Position::new(column, row);

        if areas.queue.contains(point) {
            let index = (row - areas.queue.y) as usize;
            match queue::hit(&self.state, index) {
                Some(QueueHit::Withdraw(index)) => {
                    if !self.state.withdraw_pending(index) {
                        return;
                    }
                }
                Some(QueueHit::Toggle) => self.state.toggle_queue_expanded(),
                None => {}
            }
            return;
        }

        if !areas.conversation.contains(point) {
            return;
        }
        let relative = (row - areas.conversation.y) as usize;
        let hit = conversation::hit_test(
            &self.state,
            areas.conversation,
            self.state.scroll_from_bottom,
            relative,
        );
        match hit {
            Some(LineHit::ToolHint { call_id }) => self.state.expand_tool(&call_id),
            Some(LineHit::ToolCard { call_id }) => {
                // 对映 Python：缩略态点卡片无效，展开态点卡片才是收起。
                self.state.collapse_tool(&call_id);
            }
            Some(LineHit::Reasoning { index }) => {
                self.state.toggle_reasoning_expanded(index);
            }
            None => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                // 有选区时 Ctrl+C 复制（终端惯例）；否则维持「清空输入框」。
                if self.state.selection().is_some() {
                    self.finish_selection();
                } else {
                    self.state.composer.clear();
                }
                return;
            }
            KeyCode::Char('q') if ctrl => {
                self.start_shutdown();
                return;
            }
            _ => {}
        }

        if self.handle_waiting_key(key, ctrl) {
            return;
        }

        // `/sessions` 菜单开着时优先吃上下键 / Enter / Esc。
        if self.handle_sessions_menu_key(key) {
            return;
        }

        // 命令菜单开着时先吃选择键（对映 `_handle_composer_command_key`）：
        // 上下键在候选间移动，Enter/Tab 只补全；输入已是完整命令时 Enter 放行提交。
        if self.handle_command_menu_key(key) {
            return;
        }

        match key.code {
            // 生成期间按 Enter 排队（FIFO），空闲时直接提交：与 Python
            // `_submit_composer_text` 的分叉一致。
            KeyCode::Enter => self.submit_or_queue(),
            KeyCode::Char('j') if ctrl => self.state.composer.newline(),
            KeyCode::Char('l') if ctrl => self.state.records.clear(),
            KeyCode::Esc => {
                // 先清选区（有选区时 Esc 不该顺手取消回合）。
                if self.state.selection().is_some() {
                    self.state.clear_selection();
                } else if self.state.turn.is_running() {
                    self.cancel_turn();
                } else {
                    self.state.composer.clear();
                    self.state.scroll_to_bottom();
                }
            }
            KeyCode::Char(character) if !ctrl => self.state.composer.insert(&character.to_string()),
            KeyCode::Backspace => self.state.composer.backspace(),
            KeyCode::Delete => self.state.composer.delete(),
            KeyCode::Left => self.state.composer.move_left(),
            KeyCode::Right => self.state.composer.move_right(),
            KeyCode::Home => self.state.composer.move_home(),
            KeyCode::End => self.state.composer.move_end(),
            KeyCode::Up => self.state.scroll_by(-SCROLL_STEP),
            KeyCode::Down => self.state.scroll_by(SCROLL_STEP),
            KeyCode::PageUp => self.state.scroll_by(-PAGE_STEP),
            KeyCode::PageDown => self.state.scroll_by(PAGE_STEP),
            _ => {}
        }
    }

    /// `/sessions` 会话菜单的选择键；返回 `true` 表示事件已被菜单消费。
    ///
    /// 上下键循环移动，Enter 把 `/resume <session_id>` 填回输入框（不直接执行，
    /// 对映 Python `_handle_sessions_menu_key`），Esc 或其余按键收起菜单。
    fn handle_sessions_menu_key(&mut self, key: KeyEvent) -> bool {
        if !self.state.sessions_menu.is_open() {
            return false;
        }
        match key.code {
            KeyCode::Up => {
                self.state.sessions_menu.move_selection(-1);
                true
            }
            KeyCode::Down => {
                self.state.sessions_menu.move_selection(1);
                true
            }
            KeyCode::Enter => {
                if let Some(item) = self.state.sessions_menu.selected() {
                    let command = format!("/resume {}", item.session_id);
                    self.state.sessions_menu.close();
                    self.state.composer.set_text(&command);
                }
                true
            }
            KeyCode::Esc => {
                self.state.sessions_menu.close();
                true
            }
            // 其余键收起菜单，但不等同消费：让输入继续落到输入框。
            _ => {
                self.state.sessions_menu.close();
                false
            }
        }
    }

    /// 按钮命令菜单的选择键；返回 `true` 表示事件已被菜单消费。
    ///
    /// 菜单只在打开时吃键：其余按键继续交给输入框编辑。补全写入的文本会再走一次
    /// 菜单刷新（`Composer::set_text`），因此补全后的候选与光标位置总是一致的。
    fn handle_command_menu_key(&mut self, key: KeyEvent) -> bool {
        let menu_key = match key.code {
            KeyCode::Up => MenuKey::Up,
            KeyCode::Down => MenuKey::Down,
            KeyCode::Enter => MenuKey::Enter,
            KeyCode::Tab => MenuKey::Tab,
            _ => return false,
        };
        match self.state.composer.menu_handle_key(menu_key) {
            // 输入已是完整命令：放行给提交流程（否则 `/settings` 这类命令永远打不开）。
            MenuAction::Passthrough => false,
            MenuAction::Complete { insert } => {
                self.state.composer.set_text(&insert);
                true
            }
            MenuAction::Redraw | MenuAction::Hidden => true,
        }
    }

    /// 待决面板优先吃按键；返回 `true` 表示事件已被消费。
    fn handle_waiting_key(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        let Some(batch_id) = self.state.batch_request_id() else {
            return false;
        };
        match self.state.waiting() {
            Some(Waiting::Approval(_)) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                        if let Some(step) = self.state.decide_approval(true) {
                            self.handle_batch_step(batch_id, step);
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        // 拒绝的若是 MCP Tool，先落审计再推进批次（与 Python 的
                        // `record_denied_tool_call` 同一时点、同一文案）。
                        if let Some(call) = self.state.pending_approval_call() {
                            host::record_mcp_denial(
                                &self.registry,
                                &call.name,
                                &call.arguments,
                                &omnicrawl_controllers::approval::user_cancelled_reason(&call.name),
                            );
                        }
                        if let Some(step) = self.state.decide_approval(false) {
                            self.handle_batch_step(batch_id, step);
                        }
                    }
                    _ => {}
                }
                true
            }
            Some(Waiting::Question(panel)) => {
                let is_select = panel.is_select();
                match key.code {
                    KeyCode::Up if is_select => {
                        self.state.select_question(-1);
                        true
                    }
                    KeyCode::Down if is_select => {
                        self.state.select_question(1);
                        true
                    }
                    KeyCode::Enter if is_select => {
                        let answer = match self.state.waiting() {
                            Some(Waiting::Question(panel)) => panel.answer(),
                            _ => String::new(),
                        };
                        if let Some(step) = self.answer_pending_question(answer) {
                            self.handle_batch_step(batch_id, step);
                        }
                        true
                    }
                    KeyCode::Enter => {
                        if !self.state.composer.is_empty() {
                            let answer = self.state.composer.take();
                            if let Some(step) = self.answer_pending_question(answer) {
                                self.handle_batch_step(batch_id, step);
                            }
                        }
                        true
                    }
                    KeyCode::Esc => {
                        if let Some(step) = self.answer_pending_question(String::new()) {
                            self.handle_batch_step(batch_id, step);
                        }
                        true
                    }
                    _ => {
                        // 自由输入型提问复用输入框，其余按键继续交给它编辑。
                        self.edit_composer(key, ctrl);
                        true
                    }
                }
            }
            None => false,
        }
    }

    fn edit_composer(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Char(character) if !ctrl => self.state.composer.insert(&character.to_string()),
            KeyCode::Backspace => self.state.composer.backspace(),
            KeyCode::Delete => self.state.composer.delete(),
            KeyCode::Left => self.state.composer.move_left(),
            KeyCode::Right => self.state.composer.move_right(),
            KeyCode::Home => self.state.composer.move_home(),
            KeyCode::End => self.state.composer.move_end(),
            _ => {}
        }
    }

    /// 提交输入框：生成期间排队等待，空闲时直接发给内核。
    ///
    /// 「立即命令」在生成期间也当场处理、不入队（对映 Python 的
    /// `_command_dispatcher.is_immediate`）：本批只有 `/settings`，它是纯界面
    /// 模态页、不产生模型回合，排队等反而让用户按了没反应。`/undo` 需要回合
    /// 空闲才能执行，因此照常排队，轮到时由 [`Self::request_undo`] 给出提示。
    fn submit_or_queue(&mut self) {
        let Some(text) = self.state.submit() else {
            return;
        };
        // `/sessions` 特殊处理（对映 Python `_handle_command`）：不把列表追加进对话区，
        // 而是在输入框上方弹可导航的会话菜单。放在这里而不是命令派发层，是为了让它
        // 在生成中（立即命令分支）与空闲时走同一条路径。
        if text.trim() == "/sessions" {
            self.open_sessions_menu();
            return;
        }
        if self.state.turn.is_running() {
            // 生成期间的命令调度按声明类型走（对映 Python 的 `is_immediate`：
            // 纯界面与只读查询当场执行，状态变更类排队等回合结束）。
            if let Some(parsed) = command_registry().parse(&text) {
                if parsed.command.command_type.immediate() {
                    self.dispatch_command(text, parsed);
                } else {
                    self.state.queue_pending(text);
                }
                return;
            }
            self.state.queue_pending(text);
            return;
        }
        self.dispatch_submission(text);
    }

    /// 把一次提交派发到斜杠命令或内核。
    ///
    /// 命中注册表即交给命令层（未命中的输入照旧当成一轮对话）；需要内核往返的两条
    /// （`/undo`、`/compact`）先在宿主侧拦下来异步下发。
    fn dispatch_submission(&mut self, text: String) {
        if let Some(parsed) = command_registry().parse(&text) {
            self.dispatch_command(text, parsed);
            return;
        }
        let turn_id = format!("turn-{}", self.next_turn);
        self.next_turn += 1;
        // `turn.start` 在进内核之前分发：transform 类 Handler 可改写 `userText`，
        // 守卫类 Handler 拒绝时这一轮根本不发出去（与 Python 的 `turn_payload` 同位）。
        let text = match self.plugins.turn_start(&text, None, Some(&turn_id)) {
            Ok(rewritten) => rewritten,
            Err(error) => {
                self.state.notice(error.to_string());
                return;
            }
        };
        self.state.begin_turn(turn_id.clone(), text.clone());
        // 用户提交的文本是审查模型判断授权边界的最新事实（与 host 侧 `record_user_text` 同位）。
        self.review_context.record_user_text(&text);
        self.send(Command::TurnSubmit(omnicrawl_ipc::TurnSubmitParams {
            turn_id,
            user_text: text,
        }));
    }

    /// 执行一条已解析的斜杠命令。
    ///
    /// `text` 是原始输入（命令层按它解析参数），`parsed` 只用来判断调度方式：
    /// 需要内核往返的命令在这里分叉到 [`Self::start_kernel_command`]。
    fn dispatch_command(&mut self, text: String, parsed: ParsedCommand) {
        if let Some(kind) = KernelCommand::from_parsed(&parsed) {
            self.start_kernel_command(text, kind);
            return;
        }
        // 慢命令在宿主侧接管（对映 Python `CommandOutcome.execution == "slow"`）：
        // 延迟执行体是同步接口，内联跑会冻结界面，因此只接 `/workspace` 与 `/mcp`
        // 两条真正慢的，剩下的照旧交命令层。
        match parsed.command.name.as_str() {
            "workspace" if !parsed.args.trim().is_empty() => {
                let path = parsed.args.trim().to_string();
                self.start_workspace_switch(&path);
                return;
            }
            "mcp" => {
                self.start_mcp_status();
                return;
            }
            _ => {}
        }
        let result = {
            let agent = TuiHostAgent::new(self);
            command_registry().dispatch(&text, &agent, Channel::Tui, None)
        };
        self.apply_command_result(result);
    }

    /// 把命令结果落到界面上：消息进消息流，调度标记交给对应的界面动作。
    ///
    /// 延迟执行体（`/workspace` 一类慢命令）目前在本线程内联跑完：宿主还没有慢命令
    /// worker，而本批能真正执行的命令都不产生延迟体（见 README「斜杠命令接线」）。
    fn apply_command_result(&mut self, mut result: CommandResult) {
        // 命令层要求重放会话视图（如 `/undo`）：先按消息投影立即对齐，再请内核给
        // 事件流做完整重建（工具卡与子 Agent 进度树只有事件流里才有）。
        if result.replay_conversation {
            self.request_session_replay(Vec::new(), None);
        }
        // 慢命令期间把子代理事件渲染成 │ 对话面板（Python
        // `_start_slow_command(stream_subagent_conversation=...)`）：延迟执行体在
        // 本地内联，因此在体的前后开关流式态；体内部的同步内核往返仍会收帧，
        // 期间到达的子代理事件照 `subagent_stream` 口径进面板。
        if result.stream_subagent_conversation {
            self.state.subagent_stream = true;
        }
        if let Some(run) = result.deferred.take() {
            let deferred = {
                let agent = TuiHostAgent::new(self);
                run(&agent)
            };
            if let Some(message) = deferred.message {
                self.state.notice(message);
            }
            if let Some(error) = deferred.error {
                self.state.notice(error);
            }
        }
        if result.stream_subagent_conversation {
            self.state.subagent_stream = false;
        }
        if let Some(status) = result.working_status.take() {
            self.state.status = Some(status);
        }
        // 打开类命令（`/settings`、`/settings --chat`）不追加提示：Python 在
        // `open_settings` / `open_config_chat` 分支里直接 return，`message`（如
        // 「打开设置面板」）从来不会进消息流；这里同样先处理再 return，
        // 否则会多出一条「· 打开设置面板」这类纯提醒。
        if result.clear_conversation {
            self.state.records.clear();
            self.state.scroll_to_bottom();
        }
        if result.open_config_chat {
            self.open_config_chat();
            return;
        }
        if result.open_settings {
            self.open_settings();
            return;
        }
        if let Some(message) = result.message.take() {
            self.state.notice(message);
        }
        if let Some(error) = result.error.take() {
            self.state.notice(error);
        }
        // `refresh_context` 对内核侧上下文没有可刷新的东西：模型上下文由内核持有，
        // 宿主这里没有缓存可失效。
        if result.exit_requested {
            self.start_shutdown();
        }
    }

    /// 需要内核往返的宿主命令：命令层是同步接口、内核链路是异步帧，因此由宿主
    /// 先拦下并异步下发（响应在 `handle_frame` 里回填）。
    fn start_kernel_command(&mut self, text: String, kind: KernelCommand) {
        // 状态变更类命令在回合进行中等回合结束（与排队语义一致）。
        if self.state.turn.is_running() {
            self.state.queue_pending(text);
            return;
        }
        // 同类命令不并发：`/review HEAD~3` 与 `/review` 算同类（label 相同）。
        if self
            .pending_commands
            .values()
            .any(|pending| pending.label() == kind.label())
        {
            self.state
                .notice(format!("上一条 {} 还在执行中，稍候再试。", kind.label()));
            return;
        }
        let command = match kind.to_command(&text) {
            Ok(Some(command)) => command,
            // 内部后继命令（如评审报告注入）不走这个入口。
            Ok(None) => return,
            Err(message) => {
                self.state.notice(message);
                return;
            }
        };
        // `/review` 是唯一需要在进内核之前先做本地预检的：没有 git 或没有可评审改动时
        // 连子 Agent 都不必拉起来（预检用与命令层同一个函数，文案不会两份）。
        if let KernelCommand::Review { scope } = &kind {
            let workspace = self.workspace.clone();
            let scope = scope.clone();
            let precheck = {
                let agent = TuiHostAgent::new(self);
                check_review_preconditions(&agent, &workspace, &scope)
            };
            if let Some(message) = precheck {
                self.state.notice(message);
                return;
            }
        }
        self.state.status = Some(kind.working_status().to_string());
        // `/review` 运行期间子代理事件进流式对话面板（面板按 batch_id 挂进消息流）。
        if matches!(kind, KernelCommand::Review { .. }) {
            self.state.subagent_stream = true;
        }
        let id = self.send(command);
        self.pending_commands.insert(id, kind);
    }

    /// 当前回合 id（无在途回合时为空）。
    fn current_turn_id(&self) -> Option<String> {
        self.state.turn.turn_id().map(|value| value.to_string())
    }

    /// 回合结束后按 FIFO 排空排队消息（对映 Python `_drain_pending_inputs`）。
    ///
    /// 模态页（设置面板）打开时不排空，与 Python 的 `len(self.screen_stack) == 1`
    /// 守卫同义。每次成功派发都会开启新回合，循环条件随之结束，因此不会出现
    /// 重叠的回合 worker。
    fn drain_pending_inputs(&mut self) {
        loop {
            if self.settings.is_some() || self.state.turn.is_running() {
                return;
            }
            let Some(text) = self.state.take_next_pending() else {
                return;
            };
            self.dispatch_submission(text);
        }
    }

    /// 内核对宿主命令的响应 → 界面消息；失败时原样透出内核的中文原因。
    fn finish_kernel_command(&mut self, command: KernelCommand, frame: &Frame) {
        // 回执到了就收起状态行：命令已经不在执行中。
        self.state.status = None;
        if matches!(command, KernelCommand::Review { .. }) {
            self.state.subagent_stream = false;
        }
        match (&frame.error, &frame.result) {
            (Some(error), _) => {
                // 回放读不到事件时不把错误摆到消息流：对话视图已经有消息投影可看
                // （Python `_replay_session_conversation` 同样静默跳过读取失败）。
                if let KernelCommand::Replay { fallback, notice } = &command {
                    if !fallback.is_empty() {
                        self.state.replay_history(fallback);
                    }
                    if let Some(notice) = notice {
                        self.state.notice(notice.clone());
                    }
                    return;
                }
                let label = command.label();
                self.state.notice(format!("{label}失败：{}", error.message));
            }
            (None, Some(result)) => {
                // `/undo` 的回执带重建后的历史：该提示要等回放完成后才进消息流，
                // 否则回放的清空会把刚追加的提示一并抹掉。
                if let Some(history) = command.replay_history(result) {
                    self.request_session_replay(history, Some(command.success_message(result)));
                    // 回放请求已经接过这条命令的提示，别再重复追加。
                    return;
                }
                // 回放回执的结果就是对话视图，不另发提示。
                if let Some(events) = command.replay_events(result) {
                    self.state.replay_events(&events);
                    if let KernelCommand::Replay { notice, .. } = &command {
                        if let Some(notice) = notice {
                            self.state.notice(notice.clone());
                        }
                    }
                } else {
                    let message = command.success_message(result);
                    if !message.trim().is_empty() {
                        self.state.notice(message);
                    }
                }
                // `/review` 的第二段：报告先展示，再注入内核上下文供下一轮请求使用。
                if let Some(report) = command.review_report(result) {
                    let id = self.send(Command::SessionAppend(SessionAppendParams {
                        role: "assistant".to_string(),
                        content: report,
                    }));
                    self.pending_commands
                        .insert(id, KernelCommand::ReviewInject);
                }
            }
            (None, None) => {
                self.state
                    .notice(format!("{}失败：内核没有返回结果。", command.label()));
            }
        }
    }

    // ---------- 设置面板 ----------

    /// 打开设置面板：初始值来自配置与当前工具表，读不出来就只提示、不打开。
    fn open_settings(&mut self) {
        match self.settings_values() {
            Ok(values) => self.settings = Some(SettingsState::new(values)),
            Err(message) => self.state.notice(format!("设置面板打不开：{message}")),
        }
    }

    /// 读设置面板的初始值：上下文两页、四个单选页与内置工具开关。
    ///
    /// 上下文取自「命令行/环境显式给定 > config.toml 的 llm 段」，压缩阈值优先用配置里记着的
    /// 百分比，没有就按阈值 Token 反推（与 Python 的 `_context_compaction_percent` 同口径）；
    /// 记忆开关取运行期现值（`registry_options.memory_enabled`），插件开关取配置值。
    fn settings_values(&self) -> Result<SettingsValues, String> {
        let environment = ConfigEnvironment::from_process();
        let llm = load_llm_config(&environment)
            .map_err(|error| format!("读取模型配置失败：{}", error.message()))?;
        let context_window_tokens =
            effective_context_window(self.options.context_window_tokens, &llm);
        let compaction = load_context_compaction_config(&environment, None)
            .map_err(|error| format!("读取上下文压缩配置失败：{}", error.message()))?;
        let percent = match compaction.trigger_context_percent {
            Some(percent) => nearest_compaction_percent(percent),
            None => nearest_compaction_percent(
                compaction.trigger_context_tokens * 100 / context_window_tokens.max(1),
            ),
        };
        let show_thinking = load_show_thinking(&environment, None).unwrap_or(true);
        let plugins =
            load_feature_enabled(&environment, "plugins", false, None, None).unwrap_or(false);
        let (model_options, model_key) = self.model_candidates(&environment, &llm);
        let (channel_rows, default_channel_key) = self.channel_views(&environment);
        let channel_template = default_channel(provider_options()[0], None)
            .map(|channel| channel_row_from_config(&channel))
            .unwrap_or_default();
        // 表单页的初值：读不出来就退回配置默认值，保存时会再校验一次。
        let advisor = load_advisor_config(&environment, None).unwrap_or_default();
        let compression =
            load_tool_output_compression_config(&environment, None).unwrap_or_default();
        let desensitization = load_desensitization_config(&environment, None).unwrap_or_default();
        let run_guard = load_run_guard_config(&environment, None).unwrap_or_default();
        let workspace = load_agent_workspace_config(&environment, None).unwrap_or_default();
        let image_gen = load_image_gen_configuration(&environment, None).unwrap_or_default();
        let subagent_config = load_subagent_config(&environment, None).unwrap_or_default();
        // 视觉页：代理开关 + 故障转移列表 + 当前主模型的模型原生视觉三态值。
        let vision = load_vision_configuration(&environment, None).unwrap_or(VisionConfiguration {
            enabled: false,
            models: Vec::new(),
        });
        let vision_native =
            resolve_native_vision(&environment, &llm.catalog_key, &llm.profile_id, None, None)
                .map(|setting| setting.value)
                .unwrap_or(None);
        // TTS 页：配置 + 音色库（内置 manifest 优先）+ 模型状态行 + 接口后端。
        let tts_config = load_tts_configuration(&environment, None).unwrap_or_default();
        let tts_model_dir = tts_config.resolved_model_dir(&environment);
        let tts_root = default_root();
        let tts_api = load_tts_api_configuration(&environment, None).unwrap_or_default();
        let tts_api_ready = tts_api.resolve_api_key(&environment).trim().to_string();
        let tts_values = TtsValues {
            enabled: tts_config.enabled,
            auto_play: tts_config.auto_play,
            voice: tts_config.voice.clone(),
            model_dir: tts_config.model_dir.clone(),
            device: tts_config.device.clone(),
            thread_count: tts_config.thread_count,
            voices: all_voice_names(Some(&tts_model_dir), &tts_root),
            custom_voices: list_custom_voice_names(&tts_root),
            model_status: if models_ready(Some(&tts_model_dir)) {
                "模型已就绪。".to_string()
            } else {
                "模型未下载，请先点「下载 ONNX 模型（约 763MB）」。".to_string()
            },
            api_enabled: tts_api.enabled,
            api_base_url: tts_api.base_url.clone(),
            api_model: tts_api.model.clone(),
            api_voice: tts_api.voice.clone(),
            // 界面上只展示是否已配密钥，不回显明文。
            api_key: String::new(),
            api_key_env: tts_api.api_key_env.clone(),
            api_speed: format_speed(tts_api.speed),
            local_engine_available: omnicrawl_tts::local_engine_available(),
            api_ready: !tts_api_ready.is_empty(),
        };
        Ok(SettingsValues::new(
            context_window_tokens,
            percent,
            tool_switch_rows(&self.registry),
        )
        .with_model(model_options, &model_key)
        .with_channels(channel_rows, &default_channel_key, channel_template)
        .with_choices(
            &llm.reasoning_effort,
            show_thinking,
            self.registry_options.memory_enabled,
            plugins,
        )
        .with_form(FormKind::Advisor, advisor_form_values(&advisor))
        .with_form(
            FormKind::ToolOutputCompression,
            compression_form_values(&compression),
        )
        .with_form(
            FormKind::Desensitization,
            desensitization_form_values(&desensitization),
        )
        .with_form(FormKind::RunGuard, run_guard_form_values(&run_guard))
        .with_form(
            FormKind::AgentWorkspace,
            agent_workspace_form_values(&workspace),
        )
        .with_form(FormKind::ImageGen, image_gen_form_values(&image_gen))
        .with_subagents(subagent_rows(&subagent_config))
        .with_mcp(self.mcp_settings_values(&environment))
        .with_vision(
            vision.enabled,
            vision.models.iter().map(vision_ref_from_config).collect(),
            vision_native,
        )
        .with_tts(tts_values))
    }

    /// 渠道页的初始列表与默认渠道（读不出来就留空，页面会提示按 N 新建）。
    fn channel_views(&self, environment: &ConfigEnvironment) -> (Vec<ChannelRow>, String) {
        match load_channel_configuration(environment, None, None) {
            Ok(configuration) => (
                configuration
                    .channels
                    .iter()
                    .map(channel_row_from_config)
                    .collect(),
                configuration.default_key.clone(),
            ),
            Err(error) => {
                eprintln!("[tui] 渠道配置读取失败，渠道页留空：{error}");
                (Vec::new(), String::new())
            }
        }
    }

    /// 模型页的候选与当前项。
    ///
    /// 候选来自 config.toml 的 profiles + models.toml 的条目（`load_channel_configuration`
    /// 已把两侧合成渠道视图）；单模型（legacy）配置没有渠道可切，只放当前模型一项。
    fn model_candidates(
        &self,
        environment: &ConfigEnvironment,
        llm: &LlmConfig,
    ) -> (Vec<(String, String)>, String) {
        match load_channel_configuration(environment, None, None) {
            Ok(configuration) if !configuration.channels.is_empty() => (
                configuration
                    .channels
                    .iter()
                    .map(|channel| (channel.name.clone(), channel.key.clone()))
                    .collect(),
                configuration.default_key.clone(),
            ),
            Ok(_) => (
                vec![(llm.model.clone(), llm.model.clone())],
                llm.model.clone(),
            ),
            Err(error) => {
                eprintln!("[tui] 渠道配置读取失败，模型页只显示当前模型：{error}");
                (
                    vec![(llm.model.clone(), llm.model.clone())],
                    llm.model.clone(),
                )
            }
        }
    }

    /// 设置面板的按键：面板自己认的键位归它，`Ctrl+Q`/`Ctrl+C` 仍由宿主兜底。
    fn handle_settings_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => {
                    self.start_shutdown();
                    return;
                }
                KeyCode::Char('c') => {
                    self.settings = None;
                    return;
                }
                _ => {}
            }
            // 其余 Ctrl 组合归设置面板自己（渠道表单的 Ctrl+S 保存）。
            let event = self
                .settings
                .as_mut()
                .and_then(|settings| settings.handle_ctrl_key(key.code));
            self.dispatch_settings_event(event);
            return;
        }
        let event = self
            .settings
            .as_mut()
            .and_then(|settings| settings.handle_key(key.code));
        self.dispatch_settings_event(event);
    }

    /// 面板事件 → 宿主动作：关闭、交出键盘给配置对话，或执行一次设置变更并回填结果。
    fn dispatch_settings_event(&mut self, event: Option<SettingsEvent>) {
        match event {
            None => {}
            Some(SettingsEvent::Close) => self.settings = None,
            Some(SettingsEvent::OpenConfigChat) => {
                // 与 `/settings --chat` 同一入口：面板让位，弹层接管键盘。
                self.settings = None;
                self.open_config_chat();
            }
            Some(SettingsEvent::Apply(change)) => {
                let outcome = self.apply_settings_change(&change);
                if let Some(settings) = self.settings.as_mut() {
                    match outcome {
                        Ok(message) => settings.apply_succeeded(&change, message),
                        Err(message) => settings.apply_failed(message),
                    }
                }
            }
            Some(SettingsEvent::DiscoverChannelModels {
                profile_id,
                provider,
                protocol,
                base_url,
                api_key,
                api_key_env,
                user_agent,
            }) => self.start_channel_model_discovery(
                profile_id,
                provider,
                protocol,
                base_url,
                api_key,
                api_key_env,
                user_agent,
            ),
        }
    }

    /// 后台自动检测一个渠道的模型列表（网络 I/O 不能占用界面线程）。
    ///
    /// 凭据优先取 `api_key_env` 指向的环境变量，没设时退回渠道里的内联密钥
    /// （渠道页新录入的那一行；否则只把密钥留在 config.toml 的用户会看到
    /// 「缺少 API Key，无法发现模型」）；协议字符串先解析成内核枚举，
    /// 无法识别或缺少凭据时直接把原因回填给面板。
    fn start_channel_model_discovery(
        &mut self,
        profile_id: String,
        provider: String,
        protocol: String,
        base_url: String,
        api_key: String,
        api_key_env: String,
        user_agent: String,
    ) {
        // 与 config 层 `ProviderProfile.resolve_api_key` 同口径：环境变量优先，其次内联密钥。
        let api_key = match std::env::var(&api_key_env) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => api_key.trim().to_string(),
        };
        let timeout = (self.options.tool_timeout_seconds as f64).clamp(1.0, 10.0);
        let (sender, receiver) = mpsc::channel::<ChannelModelsResult>();
        self.channel_models_task = Some(receiver);
        thread::spawn(move || {
            let Some(parsed) = omnicrawl_protocol::Protocol::parse(&protocol) else {
                let _ = sender.send(ChannelModelsResult {
                    models: Vec::new(),
                    message: format!("未知的协议：{protocol}"),
                });
                return;
            };
            let profile = omnicrawl_llm::ProviderProfile {
                id: profile_id,
                provider,
                base_url,
                api_key,
                user_agent,
                default_protocol: parsed.as_str().to_string(),
            };
            let result = omnicrawl_llm::discover_models(&profile, parsed, timeout);
            let models: Vec<String> = result
                .models
                .iter()
                .map(|model| model.model_id.clone())
                .collect();
            let _ = sender.send(ChannelModelsResult {
                models,
                message: result.message,
            });
        });
    }

    /// 取回渠道模型检测结果并回填到面板。
    fn drain_channel_models(&mut self) {
        let Some(receiver) = self.channel_models_task.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_channel_models(result.models, result.message);
                }
            }
            Err(mpsc::TryRecvError::Empty) => self.channel_models_task = Some(receiver),
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// 应用一次设置：先写盘并让宿主侧生效，再把变更推给内核做热更新。
    ///
    /// 写盘失败即视为「设置未完成」，值不变；内核拒绝即时更新时配置已落盘、
    /// 宿主侧已生效，只补一句「下次会话生效」，不回滚（两件事分开）。
    fn apply_settings_change(&mut self, change: &SettingsChange) -> Result<String, String> {
        let message = match change {
            SettingsChange::Model { key } => self.apply_model_channel(key)?,
            SettingsChange::Channels { rows, default_key } => {
                self.apply_channels(rows, default_key)?
            }
            SettingsChange::ToolSwitch { name, enabled } => {
                self.apply_tool_switch(name, *enabled)?
            }
            SettingsChange::ContextWindow { tokens } => self.apply_context_window(*tokens)?,
            SettingsChange::CompactionPercent { percent } => {
                self.apply_compaction_percent(*percent)?
            }
            SettingsChange::Reasoning { effort } => self.apply_reasoning_effort(effort)?,
            SettingsChange::ShowThinking { enabled } => self.apply_show_thinking(*enabled)?,
            SettingsChange::Feature { key, enabled } => self.apply_feature(key, *enabled)?,
            SettingsChange::Form { kind, values } => self.apply_form(*kind, values)?,
            SettingsChange::Subagent(subagent) => self.apply_subagent(subagent)?,
            SettingsChange::Mcp(change) => self.apply_mcp(change)?,
            SettingsChange::Vision(vision) => self.apply_vision(vision)?,
            SettingsChange::Tts(change) => self.apply_tts(change)?,
        };
        self.push_session_settings(change);
        Ok(message)
    }

    // ---------- TTS 页与音频选择弹层 ----------

    /// TTS 页的一次动作：保存 / 下载模型 / 克隆音色 / 删除音色 / 打开音频选择弹层。
    fn apply_tts(&mut self, change: &TtsChange) -> Result<String, String> {
        match change {
            TtsChange::Save(draft) => self.apply_tts_save(draft),
            TtsChange::Download => self.start_tts_download(),
            TtsChange::CloneVoice { voice, audio } => self.start_tts_clone(voice, audio),
            TtsChange::DeleteVoice { voice } => self.apply_tts_delete(voice),
            TtsChange::BrowseAudio => {
                self.open_tts_file_picker();
                Ok(String::new())
            }
        }
    }

    /// 保存 `[tts]` 段并重建工具表：启用状态决定 `tts_synthesize` 是否注册。
    fn apply_tts_save(&mut self, draft: &TtsDraft) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_tts_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 界面上不编辑的两项（流式、输出目录）按磁盘上的原值保留。
        let configuration = TtsConfiguration {
            enabled: draft.enabled,
            model_dir: draft.model_dir.clone(),
            voice: draft.voice.clone(),
            auto_play: draft.auto_play,
            thread_count: draft.thread_count,
            device: draft.device.clone(),
            streaming: previous.streaming,
            output_dir: previous.output_dir.clone(),
        }
        .normalize()
        .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let path = save_tts_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 接口后端：面板上不编辑的 response_format / timeout_seconds 按磁盘值保留。
        let previous_api = load_tts_api_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let speed = draft.api_speed.trim().parse::<f64>().map_err(|_| {
            format!(
                "设置未完成：接口语速必须是数值，当前为「{}」。",
                draft.api_speed
            )
        })?;
        let api_configuration = TtsApiConfiguration {
            enabled: draft.api_enabled,
            base_url: draft.api_base_url.clone(),
            // 密钥空字符串 = 保留磁盘上的旧值（界面上不回显明文）。
            api_key: if draft.api_key.trim().is_empty() {
                previous_api.api_key.clone()
            } else {
                draft.api_key.trim().to_string()
            },
            api_key_env: draft.api_key_env.clone(),
            model: draft.api_model.clone(),
            voice: draft.api_voice.clone(),
            response_format: previous_api.response_format.clone(),
            speed,
            timeout_seconds: previous_api.timeout_seconds,
        }
        .normalize()
        .map_err(|error| format!("设置未完成：{}", error.message()))?;
        save_tts_api_configuration(&environment, &api_configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.registry_options.tts = tts_options_from(configuration.clone(), &self.workspace);
        self.rebuild_registry()?;
        Ok(format!(
            "TTS 设置已保存到 {}（{}）。",
            path.display(),
            if !configuration.enabled {
                "语音合成已停用"
            } else if api_configuration.enabled {
                "本会话即刻生效，后端：接口合成"
            } else {
                "本会话即刻生效，后端：本地推理"
            }
        ))
    }

    /// 后台下载 ONNX 模型到配置的模型目录（大文件，不阻塞 UI）。
    fn start_tts_download(&mut self) -> Result<String, String> {
        if self.tts_task.is_some() {
            return Err("已有 TTS 后台任务在跑，请稍候。".to_string());
        }
        let environment = ConfigEnvironment::from_process();
        let configuration = load_tts_configuration(&environment, None)
            .map_err(|error| format!("读取 TTS 配置失败：{}", error.message()))?;
        let model_dir = configuration.resolved_model_dir(&environment);
        let status = format!("正在下载模型到 {}…", model_dir.display());
        let (sender, receiver) = mpsc::channel();
        self.tts_task = Some(receiver);
        if let Some(settings) = self.settings.as_mut() {
            settings.set_tts_busy(true, status.clone());
        }
        thread::spawn(move || {
            let result = download_models_into(&model_dir, None).map(|_| ());
            let _ = sender.send(TtsTaskResult::Download(result));
        });
        Ok(status)
    }

    /// 后台把参考音频克隆为一条自定义音色（首次会加载 ONNX 模型）。
    fn start_tts_clone(&mut self, voice: &str, audio: &str) -> Result<String, String> {
        if self.tts_task.is_some() {
            return Err("已有 TTS 后台任务在跑，请稍候。".to_string());
        }
        let environment = ConfigEnvironment::from_process();
        let configuration = load_tts_configuration(&environment, None)
            .map_err(|error| format!("读取 TTS 配置失败：{}", error.message()))?;
        let model_dir = configuration.resolved_model_dir(&environment);
        let voice = voice.to_string();
        let audio = PathBuf::from(audio);
        let status = format!("正在克隆音色 {voice}…");
        let (sender, receiver) = mpsc::channel();
        self.tts_task = Some(receiver);
        if let Some(settings) = self.settings.as_mut() {
            settings.set_tts_busy(true, status.clone());
        }
        thread::spawn(move || {
            let result = clone_tts_voice(&configuration, &model_dir, &voice, &audio);
            let _ = sender.send(TtsTaskResult::Clone(result));
        });
        Ok(status)
    }

    /// 从自定义音色库删除一条音色（同步、无网络）。
    fn apply_tts_delete(&mut self, voice: &str) -> Result<String, String> {
        let removed = delete_custom_voice(&default_root(), voice)
            .map_err(|error| format!("删除音色失败：{error}"))?;
        if !removed {
            return Err(format!("自定义音色库里没有 {voice}。"));
        }
        self.refresh_tts_voices();
        Ok(format!("已删除自定义音色 {voice}。"))
    }

    /// 重读音色库（内置 manifest 优先，模型缺失时留空由界面回落到兜底表）。
    fn refresh_tts_voices(&mut self) {
        let environment = ConfigEnvironment::from_process();
        let configuration = load_tts_configuration(&environment, None).unwrap_or_default();
        let model_dir = configuration.resolved_model_dir(&environment);
        let root = default_root();
        let voices = all_voice_names(Some(&model_dir), &root);
        let custom = list_custom_voice_names(&root);
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_tts_voices(voices, custom);
        }
    }

    /// 打开参考音频选择弹层（从工作区起步）。
    fn open_tts_file_picker(&mut self) {
        let start = self.workspace.clone();
        self.file_picker = Some(FilePickerState::new(&start, "选择参考音频"));
    }

    /// 打开配置对话弹层：工作线程在后台装载路由器（约一分钟），弹层里给出进度提示。
    fn open_config_chat(&mut self) {
        let chat = ConfigChatState::start();
        if !chat.available() {
            self.state
                .notice(format!("配置对话不可用：{}", chat.unavailable_reason()));
        }
        self.config_chat = Some(chat);
    }

    /// 配置对话弹层的按键：Esc 关闭，Enter 把一句话交给工作线程。
    fn handle_config_chat_key(&mut self, key: KeyEvent) {
        let event = self
            .config_chat
            .as_mut()
            .and_then(|chat| chat.handle_key(key.code));
        match event {
            None => {}
            Some(ConfigChatEvent::Close) => {
                self.config_chat = None;
                // 配置可能已改（模型/上下文/工具开关等），设置面板下次打开时重新取初值。
                self.settings = None;
            }
            Some(ConfigChatEvent::Submit(text)) => {
                if let Some(chat) = self.config_chat.as_mut() {
                    chat.push(true, text.clone());
                    if let Some(message) = chat.submit(&text) {
                        chat.push(false, message);
                    }
                }
            }
        }
    }

    /// 弹层按键：取消 / 选中都关闭弹层，选中时回填参考音频路径。
    fn handle_file_picker_key(&mut self, key: KeyEvent) {
        let event = self
            .file_picker
            .as_mut()
            .and_then(|picker| picker.handle_key(key.code));
        match event {
            None => {}
            Some(FilePickerEvent::Cancel) => self.file_picker = None,
            Some(FilePickerEvent::Chosen(path)) => {
                self.file_picker = None;
                if let Some(settings) = self.settings.as_mut() {
                    settings.set_tts_clone_audio(&path.to_string_lossy());
                    settings.set_tts_busy(false, format!("已选择参考音频：{}", path.display()));
                }
            }
        }
    }

    /// 每帧轮询配置对话工作线程：就绪状态与结论行都从这里回到弹层。
    pub fn tick_config_chat(&mut self) {
        if let Some(chat) = self.config_chat.as_mut() {
            chat.tick();
        }
    }

    /// 按 `MONITOR_POLL_INTERVAL` 节流轮询后台任务日志，把增量追加成工具卡。
    ///
    /// 对映 Python `ConversationViewMixin._refresh_monitor_events`：只往消息流追加，
    /// 不改动回合状态，不干扰正在跑的模型回合或其他后台任务。游标只在本适配器里，
    /// 本地 API 的 `/monitors` 与模型侧的 `monitor` 工具各有自己的消费位置。
    pub fn tick_monitor_events(&mut self, now: Instant) {
        if now.duration_since(self.monitor_polled_at) < crate::monitor::MONITOR_POLL_INTERVAL {
            return;
        }
        self.monitor_polled_at = now;
        for batch in self.monitor_state.refresh(self.registry.monitors()) {
            let text = crate::monitor::format_monitor_display_batch(&batch);
            self.state
                .push_monitor_batch(&batch.monitor_id, &batch.status, text);
        }
    }

    /// 每帧轮询后台 TTS 任务（下载 / 克隆）：取回结果后回填状态并按需重建工具表。
    pub fn tick_tts_tasks(&mut self) {
        let received = match self.tts_task.as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return,
        };
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.tts_task = None;
                return;
            }
        };
        self.tts_task = None;
        let (message, ok) = match outcome {
            TtsTaskResult::Download(Ok(())) => {
                ("模型下载完成，可以开始语音合成。".to_string(), true)
            }
            TtsTaskResult::Download(Err(error)) => (format!("模型下载失败：{error}"), false),
            TtsTaskResult::Clone(Ok(())) => {
                ("音色克隆完成，已加入自定义音色库。".to_string(), true)
            }
            TtsTaskResult::Clone(Err(error)) => (format!("音色克隆失败：{error}"), false),
        };
        if ok {
            self.refresh_tts_voices();
            self.registry_options.tts = tts_options(&self.workspace);
            if let Err(error) = self.rebuild_registry() {
                self.state
                    .notice(format!("TTS 任务后重建工具表失败：{error}"));
            }
        }
        if let Some(settings) = self.settings.as_mut() {
            settings.set_tts_busy(false, message);
        }
    }

    /// 切换模型渠道：写 `llm.active_model`（legacy 配置写 `llm.model`），并同步运行视图。
    ///
    /// 渠道信息（Provider/协议/基地址/凭据变量名、模型 id）整条换掉，随后的
    /// `session.settings` 会把它推给内核，本会话即刻生效。
    fn apply_model_channel(&mut self, key: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let configuration = load_channel_configuration(&environment, None, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let Some(channel) = configuration
            .channels
            .iter()
            .find(|channel| channel.key == key)
        else {
            return Err(format!("设置未完成：配置里没有渠道 {key}。"));
        };
        let reference = ActiveModelRef {
            source: "custom".to_string(),
            key: channel.key.clone(),
            profile: channel.profile_id.clone(),
            model_id: channel.model_id.clone(),
            protocol: channel.protocol.clone(),
        };
        let path = save_active_model_ref(&environment, &reference, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘后重新解析一次整条模型视图：上下文窗口、生成选项、Provider、基地址与凭据
        // 变量名都跟着新渠道走（读不出来时退回渠道记录里的那几个字段）。
        match load_llm_config(&environment) {
            Ok(refreshed) => self.llm = refreshed,
            Err(error) => {
                self.state.notice(format!(
                    "切换渠道后重新解析模型配置失败，沿用渠道记录：{error}"
                ));
                self.llm.model = channel.model_id.clone();
                self.llm.provider = channel.provider.clone();
                self.llm.protocol = channel.protocol.clone();
                self.llm.base_url = channel.base_url.clone();
                self.llm.api_key_env = channel.api_key_env.clone();
                self.llm.profile_id = channel.profile_id.clone();
                self.llm.catalog_key = channel.key.clone();
                self.llm.model_source = "custom".to_string();
            }
        }
        self.state.model = self.llm.model.clone();
        Ok(format!(
            "模型已切到 {}（{}），已保存到 {}。",
            channel.name,
            self.llm.model,
            path.display()
        ))
    }

    /// 保存整份渠道配置：写 config.toml 与 models.toml，并让当前渠道跟着默认渠道走。
    ///
    /// 面板里填了新密钥（非空）就用新值，留空则按 key 从磁盘上的同一条渠道继承——
    /// 既能录入内联 `api_key`（Python 渠道编辑器同能力），也不会因为没碰这一行把它抹掉
    /// （`save_channel_configuration` 对空 `api_key` 会移除该字段）。
    fn apply_channels(&mut self, rows: &[ChannelRow], default_key: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let existing = load_channel_configuration(&environment, None, None)
            .map(|configuration| configuration.channels)
            .unwrap_or_default();
        let channels: Vec<ChannelConfig> = rows
            .iter()
            .map(|row| {
                let mut channel = channel_config_from_row(row);
                if channel.api_key.is_empty() {
                    if let Some(previous) = existing.iter().find(|item| item.key == row.key) {
                        channel.api_key = previous.api_key.clone();
                    }
                }
                channel
            })
            .collect();
        let configuration = ChannelConfiguration {
            channels,
            default_key: default_key.to_string(),
        };
        let (config_path, models_path) =
            save_channel_configuration(&environment, &configuration, None, None)
                .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 配置侧会规范默认渠道与凭据变量名：重新解析一次模型视图并读回磁盘上的渠道。
        match load_llm_config(&environment) {
            Ok(refreshed) => self.llm = refreshed,
            Err(error) => self
                .state
                .notice(format!("保存渠道后重新解析模型配置失败：{error}")),
        }
        self.state.model = self.llm.model.clone();
        let (reloaded, reloaded_default) = self.channel_views(&environment);
        if let Some(settings) = self.settings.as_mut() {
            settings.sync_channels(reloaded, &reloaded_default);
        }
        Ok(format!(
            "渠道配置已保存到 {} 与 {}；当前渠道 {}。",
            config_path.display(),
            models_path.display(),
            self.llm.model
        ))
    }

    /// 推理强度：写 config.toml 的 `llm.reasoning_effort`，并把档位推给内核的生成选项。
    fn apply_reasoning_effort(&mut self, effort: &str) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_reasoning_effort(&environment, effort, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "推理强度已设为 {}，已保存到 {}。",
            reasoning_label(effort),
            path.display()
        ))
    }

    /// 思考显示：只影响本机界面（消息流里要不要显示思考段），写 `ui.show_thinking`。
    fn apply_show_thinking(&mut self, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_show_thinking(&environment, enabled, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.state.show_thinking = enabled;
        Ok(format!(
            "思考显示已{}，已保存到 {}。",
            if enabled { "开启" } else { "关闭" },
            path.display()
        ))
    }

    /// 功能开关：`memory` 立即重建工具表（记忆整组进/出表），`plugins` 立即重建插件
    /// 运行期（Worker 与执行计划一并回收 / 装配，对应 Python 设置面板的即时生效）。
    fn apply_feature(&mut self, key: &str, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_feature_enabled(&environment, key, enabled, None, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let label = feature_label(key);
        match key {
            "memory" => {
                self.registry_options.memory_enabled = enabled;
                self.rebuild_registry()?;
                if let Some(settings) = self.settings.as_mut() {
                    settings.refresh_tools(tool_switch_rows(&self.registry));
                }
                Ok(format!(
                    "{label}已{}，已保存到 {}。",
                    if enabled { "开启" } else { "关闭" },
                    path.display()
                ))
            }
            "plugins" => {
                // 运行期热更新：与 API 的 `PUT /settings/features` 同一入口
                // （`PluginHost::set_enabled` 事务式重建 Worker 与执行计划）。
                let diagnostics = self
                    .plugins
                    .set_enabled(enabled)
                    .map_err(|error| format!("设置未完成：{error}"))?;
                let detail = if diagnostics.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", diagnostics.join("\n"))
                };
                Ok(format!(
                    "{label}已{}，已保存到 {}。{detail}",
                    if enabled { "开启" } else { "关闭" },
                    path.display()
                ))
            }
            _ => Ok(format!(
                "{label}已{}，已保存到 {}；重启后生效。",
                if enabled { "开启" } else { "关闭" },
                path.display()
            )),
        }
    }

    /// 上下文长度：写窗口 + 按当前百分比重算阈值，两者都写盘。
    ///
    /// 第二部分失败时把磁盘上的窗口值改回旧值——否则会留下「新窗口 + 旧百分比」
    /// 的自相矛盾配置（与 Python 的 `_apply_setting_value("context")` 一致）。
    fn apply_context_window(&mut self, tokens: i64) -> Result<String, String> {
        let percent = self
            .settings
            .as_ref()
            .map(|settings| settings.compaction_percent())
            .unwrap_or(80);
        let environment = ConfigEnvironment::from_process();
        let llm = load_llm_config(&environment)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous = effective_context_window(self.options.context_window_tokens, &llm);
        let path = save_context_window_tokens(
            &environment,
            tokens,
            &llm.model_source,
            &llm.catalog_key,
            None,
            None,
        )
        .map_err(|error| format!("设置未完成：{}", error.message()))?;
        if let Err(error) =
            save_context_compaction_trigger_percent(&environment, percent, tokens, None)
        {
            let _ = save_context_window_tokens(
                &environment,
                previous,
                &llm.model_source,
                &llm.catalog_key,
                None,
                None,
            );
            return Err(format!("设置未完成：{}", error.message()));
        }
        self.state.telemetry.context_window = Some(tokens.max(0) as u64);
        let threshold = context_compaction_trigger_tokens(tokens, percent);
        Ok(format!(
            "上下文长度已设为 {}K，已保存到 {}；压缩阈值已同步为 {percent}%（{threshold} Token）。",
            tokens / 1000,
            path.display()
        ))
    }

    /// 压缩阈值：按当前上下文长度换算 Token 后写盘。
    fn apply_compaction_percent(&mut self, percent: i64) -> Result<String, String> {
        let window = self
            .settings
            .as_ref()
            .map(|settings| settings.context_window_tokens())
            .unwrap_or(128_000);
        let environment = ConfigEnvironment::from_process();
        let path = save_context_compaction_trigger_percent(&environment, percent, window, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let tokens = context_compaction_trigger_tokens(window, percent);
        Ok(format!(
            "上下文压缩阈值已设为 {percent}%（{tokens} Token），已保存到 {}。",
            path.display()
        ))
    }

    /// 工具开关：写盘 → 重建宿主工具表 → 刷新设置面板里的注册标记。
    ///
    /// 开关即「模型可见性」：禁用的工具不进声明，模型也就调不到它。
    fn apply_tool_switch(&mut self, name: &str, enabled: bool) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let path = save_tool_switch(&environment, name, enabled, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let disabled = load_disabled_tools(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        self.registry_options.disabled_tools = disabled;
        self.rebuild_registry()?;
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_tools(tool_switch_rows(&self.registry));
        }
        Ok(format!(
            "{}已{}，已保存到 {}。",
            tool_label(name),
            if enabled { "启用" } else { "关闭" },
            path.display()
        ))
    }

    /// 读 MCP 设置页的初值：全局开关/策略 + Server 列表；读不出来用保守默认值。
    fn mcp_settings_values(&self, environment: &ConfigEnvironment) -> McpSettingsValues {
        match load_mcp_config(environment, None) {
            Ok(config) => mcp_values_from_config(&config),
            Err(error) => {
                eprintln!("[tui] MCP 配置读取失败，MCP 设置页用默认值：{error}");
                McpSettingsValues::default()
            }
        }
    }

    /// 应用一次 MCP 设置：把界面变更写回 `[mcp]` 段，再重建 MCP 连接与工具表。
    ///
    /// MCP 能力注册进工具表，所以保存后必须重建注册表；旧管理器先 `close()`，
    /// 不给系统留下孤儿子进程（与退出路径同一处理）。
    fn apply_mcp(&mut self, change: &McpChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let config = load_mcp_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let mut data = mcp_config_data(&config);
        match change {
            McpChange::Globals {
                enabled,
                allow_external_network_tools,
                require_confirmation_for_write,
                require_confirmation_for_command,
                audit_log_enabled,
                timeout_seconds,
            } => {
                data.enabled = *enabled;
                data.default_timeout_seconds = *timeout_seconds;
                data.policy = McpPolicyData {
                    require_confirmation_for_write: *require_confirmation_for_write,
                    require_confirmation_for_command: *require_confirmation_for_command,
                    allow_external_network_tools: *allow_external_network_tools,
                    audit_log_enabled: *audit_log_enabled,
                };
            }
            McpChange::SaveServer(draft) => {
                let name = draft.name.trim().to_string();
                if name.is_empty() {
                    return Err("设置未完成：Server 名称不能为空。".to_string());
                }
                let original = draft.original_name.as_deref();
                let duplicate = data.servers.iter().any(|(key, _)| key == &name)
                    && original != Some(name.as_str());
                if duplicate {
                    return Err("设置未完成：Server 名称不能重复。".to_string());
                }
                let previous = original
                    .and_then(|key| data.servers.iter().find(|(name, _)| name == key))
                    .map(|(_, server)| server.clone());
                let server = mcp_server_data(draft.as_ref(), previous.as_ref())?;
                if let Some(original) = original {
                    if original != name {
                        data.servers.retain(|(key, _)| key != original);
                    }
                }
                match data.servers.iter_mut().find(|(key, _)| key == &name) {
                    Some(entry) => entry.1 = server,
                    None => data.servers.push((name, server)),
                }
            }
            McpChange::SetServerEnabled { name, enabled } => {
                if let Some(entry) = data.servers.iter_mut().find(|(key, _)| key == name) {
                    entry.1.enabled = *enabled;
                }
            }
            McpChange::DeleteServer { name } => {
                data.servers.retain(|(key, _)| key != name);
            }
        }
        let path = save_mcp_config(&environment, &data, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 旧连接先关，再按新配置重新发现并重建工具表。
        if let Some(manager) = self.registry.mcp() {
            manager.close();
        }
        self.registry_options.mcp = mcp_manager(&self.workspace);
        self.rebuild_registry()?;
        Ok(format!("MCP 设置已保存：{}。", path.display()))
    }

    /// 保存一页表单：按表单页把字段值拼回配置结构，写盘后再让宿主侧生效。
    fn apply_form(&mut self, kind: FormKind, values: &[FieldValue]) -> Result<String, String> {
        match kind {
            FormKind::Advisor => self.apply_advisor_form(values),
            FormKind::ToolOutputCompression => self.apply_compression_form(values),
            FormKind::Desensitization => self.apply_desensitization_form(values),
            FormKind::RunGuard => self.apply_run_guard_form(values),
            FormKind::AgentWorkspace => self.apply_agent_workspace_form(values),
            FormKind::ImageGen => self.apply_image_gen_form(values),
        }
    }

    /// 顾问设置：写 `config.toml [advisor]`，再用新配置重建顾问工具。
    ///
    /// 顾问工具是否注册取决于「启用且选了模型」以及当前模型是否命中黑名单，
    /// 所以保存后必须重建工具表，让 advisor 工具即时出现/消失
    /// （与 Python 的 `set_advisor_configuration` 同义）。
    fn apply_advisor_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let previous = load_advisor_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let enabled = values.first().map(|value| value.flag()).unwrap_or(false);
        let effort = field_text(values, 1);
        let model_key = field_text(values, 2).trim().to_string();
        if enabled && model_key.is_empty() {
            return Err("设置未完成：启用顾问前请先选择顾问模型。".to_string());
        }
        let configuration = AdvisorConfig {
            enabled,
            model_key,
            effort,
            // 黑名单不在界面里编辑，按磁盘上的原值保留。
            disabled_for_models: previous.disabled_for_models.clone(),
        };
        let path = save_advisor_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 与 Python 页面同义：配置已写盘，但宿主侧生效失败时把磁盘与运行期一起回滚，
        // 不留「磁盘是新值、运行期还是旧值」的错配。
        let previous_options = self.registry_options.advisor.clone();
        self.sync_advisor_options(&environment, &configuration);
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_advisor_config(&environment, &previous, None) {
                self.state
                    .notice(format!("顾问设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.advisor = previous_options;
            return Err(error);
        }
        Ok(format!(
            "顾问设置已保存到 {}（{}）。",
            path.display(),
            if configuration.active() {
                "本会话即刻生效"
            } else {
                "顾问已停用"
            }
        ))
    }

    /// 把顾问配置映射到顾问工具的运行期选项：模型项按渠道解析出接口与凭据。
    fn sync_advisor_options(&mut self, environment: &ConfigEnvironment, config: &AdvisorConfig) {
        let mut advisor = self.registry_options.advisor.clone();
        advisor.enabled = config.active();
        advisor.effort = config.display_effort();
        advisor.disabled_for_models = config.disabled_for_models.clone();
        match channel_for_key(environment, &config.model_key) {
            Some(channel) => {
                advisor.model = channel.model_id.clone();
                advisor.base_url = if channel.base_url.trim().is_empty() {
                    self.options.base_url.clone()
                } else {
                    channel.base_url.clone()
                };
                advisor.api_key_env = if channel.api_key_env.trim().is_empty() {
                    self.options.api_key_env.clone()
                } else {
                    channel.api_key_env.clone()
                };
            }
            // 没配渠道（单模型 legacy 配置）时，顾问模型就是配置里的引用本身。
            None => {
                advisor.model = config.model_key.clone();
                advisor.base_url = self.options.base_url.clone();
                advisor.api_key_env = self.options.api_key_env.clone();
            }
        }
        self.registry_options.advisor = advisor;
    }

    /// `/advisor` 的运行期落地：把命令层已写盘的顾问配置同步到工具选项并重建工具表。
    ///
    /// 与设置页（[`App::apply_advisor_form`]）不同的是，写盘由命令处理器负责
    /// （它先调 `save_advisor_config` 再调这里），所以宿主这一侧只做两件事：
    /// 把配置映射成运行期选项，再重建工具表让 advisor 工具即时出现/消失。
    ///
    /// 重建失败时把运行期选项回滚成原值再报错：调用方会看到「已保存但未生效」，
    /// 而不是「磁盘是新值、运行期还是旧值」这种看不见的错配。
    ///
    /// 公开是为了让集成测试能直接驱动这条落地路径（`/advisor <model>` 的写盘会碰
    /// 真实 `config.toml`，不经写盘就验证「选项同步 + 重建工具表」只能走这里）。
    pub fn command_apply_advisor_config(&mut self, config: &AdvisorConfig) -> Result<(), String> {
        let environment = ConfigEnvironment::from_process();
        let previous_options = self.registry_options.advisor.clone();
        self.sync_advisor_options(&environment, config);
        if let Err(error) = self.rebuild_registry() {
            self.registry_options.advisor = previous_options;
            return Err(error);
        }
        Ok(())
    }

    /// 工具输出压缩：写 `config.toml [tool_output_compression]`。
    ///
    /// 压缩发生在内核侧的工具批次收口时，宿主这边没有运行期句柄，所以这里只写盘，
    /// 并在状态文本里说明生效时机（与「插件功能」同款处理）。
    fn apply_compression_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let enabled = values.first().map(|value| value.flag()).unwrap_or(false);
        let model_key = field_text(values, 7).trim().to_string();
        if enabled && model_key.is_empty() {
            return Err("设置未完成：启用压缩前请先选择压缩模型。".to_string());
        }
        let configuration = ToolOutputCompressionConfig {
            enabled,
            model_key,
            thinking_enabled: values.get(1).map(|value| value.flag()).unwrap_or(false),
            reasoning_effort: field_text(values, 2),
            min_chars: positive_int(values, 3, "最小压缩字符数")?,
            max_input_chars: positive_int(values, 4, "单次压缩输入上限")?,
            max_output_chars: positive_int(values, 5, "压缩结果上限")?,
            timeout_seconds: positive_int(values, 6, "单条压缩超时（秒）")?,
        };
        configuration
            .clone()
            .validate()
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let path = save_tool_output_compression_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "工具输出压缩设置已保存到 {}（压缩在内核侧随每轮工具批次重读，下一轮起生效）。",
            path.display()
        ))
    }

    /// 消息脱敏：写 `config.toml` 的 `[desensitization]` 段。
    ///
    /// 该段在构建模型运行时读取，切换模型或重启 TUI 才生效，所以这里只写盘
    /// （与 Python 面板的提示一致）。
    fn apply_desensitization_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_desensitization_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `DESENSITIZATION_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.fail_closed = field_flag(values, 1);
        configuration.strict_restore = field_flag(values, 2);
        configuration.entropy_enabled = field_flag(values, 3);
        configuration.entropy_pure_letters = field_flag(values, 4);
        configuration.entropy_pure_digits = field_flag(values, 5);
        configuration.entropy_min_length = positive_int(values, 6, "熵兜底长度下限")?;
        configuration.entropy_min_bits = field_float(values, 7, "熵兜底阈值")?;
        configuration.extra_sensitive_keys = field_list(values, 8, ',');
        configuration.exempt_keys = field_list(values, 9, ',');
        configuration.detect_pem_private_key = field_flag(values, 10);
        configuration.detect_db_connection_string = field_flag(values, 11);
        configuration.detect_email = field_flag(values, 12);
        configuration.detect_bank_card = field_flag(values, 13);
        configuration.detect_internal_ip = field_flag(values, 14);
        configuration.detect_external_ip = field_flag(values, 15);
        configuration.detect_url = field_flag(values, 16);
        configuration.detect_mac_address = field_flag(values, 17);
        configuration.detect_license_plate = field_flag(values, 18);
        configuration.gitleaks_enabled = field_flag(values, 19);
        configuration.gitleaks_config_path = field_text(values, 20).trim().to_string();
        configuration.ner_enabled = field_flag(values, 21);
        configuration.ner_device = field_text(values, 22);
        configuration.ner_model_path = field_text(values, 23).trim().to_string();
        configuration.ner_entity_types = field_list(values, 24, ',');
        configuration.ner_min_entity_chars = positive_int(values, 25, "NER 最小实体长度")?;
        // 0 表示关闭 NER 结果缓存，所以这里允许 0。
        configuration.ner_cache_size = non_negative_int(values, 26, "NER 缓存容量")?;
        let path = save_desensitization_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "消息脱敏设置已保存到 {}（切换模型或重启 TUI 后生效）。",
            path.display()
        ))
    }

    /// 持续运转：写 `config.toml` 的 `[run_guard]` 段。
    ///
    /// 与 Python 一致：保存后从下一个 Agent 回合起生效，正在跑的回合不变。
    fn apply_run_guard_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_run_guard_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `RUN_GUARD_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.guard.enabled = field_flag(values, 1);
        configuration.guard.window_chars = positive_int(values, 2, "窗口字符数")?;
        configuration.guard.substr_len = positive_int(values, 3, "重复子串长度")?;
        configuration.guard.repeat_ratio = field_float(values, 4, "重复率阈值")?;
        configuration.guard.check_every = positive_int(values, 5, "检查间隔")?;
        configuration.guard.max_blocks = positive_int(values, 6, "推理块上限")?;
        configuration.guard.max_chars = positive_int(values, 7, "推理字符上限")?;
        configuration.guard.max_guard_retries = non_negative_int(values, 8, "护栏重试次数")?;
        configuration.guard.auto_retry_errors = field_list(values, 9, ',');
        configuration.continuation.enabled = field_flag(values, 10);
        configuration.continuation.max_auto_followups =
            non_negative_int(values, 11, "续跑次数上限")?;
        let path = save_run_guard_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "持续运转设置已保存到 {}（从下一个回合起生效）。",
            path.display()
        ))
    }

    /// 隔离工作区：写 `config.toml` 的 `[agent_workspace]` 段。
    ///
    /// 与 Python 一致：保存后从下一次启动生效，正在运行的进程不受影响。
    /// 「基线分支」不在界面里，保留磁盘原值。
    fn apply_agent_workspace_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_agent_workspace_config(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `AGENT_WORKSPACE_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.mode = field_text(values, 1);
        let base_ref = field_text(values, 2).trim().to_string();
        configuration.base_ref = if base_ref.is_empty() {
            "HEAD".to_string()
        } else {
            base_ref
        };
        configuration.detached = field_flag(values, 3);
        configuration.sync_uncommitted = field_flag(values, 4);
        configuration.apply_on_exit = field_flag(values, 5);
        configuration.cleanup_on_exit = field_text(values, 6);
        configuration.copy_dirs = field_list(values, 7, ',');
        configuration.env_scripts = field_list(values, 8, ';');
        let path = save_agent_workspace_config(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "隔离工作区设置已保存到 {}（从下一次启动生效）。",
            path.display()
        ))
    }

    /// 图像生成：写 `config.toml [image_gen]`，再按新配置重建图像生成工具。
    ///
    /// 界面不编辑 `api_key`（凭据不进界面状态）：整段写盘时按磁盘原值继承，
    /// 否则会把已存的凭据抹掉；重建工具表失败时磁盘与运行期一起回滚。
    fn apply_image_gen_form(&mut self, values: &[FieldValue]) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        let mut configuration = load_image_gen_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 写盘前再读一份原始值：生效失败时要把它写回去（这一份还没被覆盖）。
        let previous = load_image_gen_configuration(&environment, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        // 下标与 `form.rs` 的 `IMAGE_GEN_FIELDS` 一一对应。
        configuration.enabled = field_flag(values, 0);
        configuration.base_url = field_text_or(values, 1, "https://api.openai.com/v1");
        configuration.api_key_env = field_text_or(values, 2, "OPENAI_API_KEY");
        configuration.model = field_text_or(values, 3, "gpt-image-2");
        configuration.size = field_text(values, 4);
        configuration.quality = field_text(values, 5);
        configuration.output_format = field_text(values, 6);
        configuration.n = non_negative_int(values, 7, "默认张数")?;
        configuration.timeout_seconds = non_negative_int(values, 8, "请求超时")?;
        let path = save_image_gen_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        let previous_options = self.registry_options.image_gen.clone();
        self.sync_image_gen_options(&configuration);
        if let Err(error) = self.rebuild_registry() {
            if let Err(rollback) = save_image_gen_configuration(&environment, &previous, None) {
                self.state
                    .notice(format!("图像生成设置回滚写盘失败：{rollback}"));
            }
            self.registry_options.image_gen = previous_options;
            return Err(error);
        }
        Ok(format!(
            "图像生成设置已保存到 {}（本会话即刻生效）。",
            path.display()
        ))
    }

    /// 把图像生成配置映射到运行期选项（图像生成工具按它发请求）。
    fn sync_image_gen_options(&mut self, config: &ImageGenConfiguration) {
        let mut image_gen = self.registry_options.image_gen.clone();
        image_gen.enabled = config.enabled;
        image_gen.base_url = config.base_url.clone();
        image_gen.model = config.model.clone();
        image_gen.api_key_env = config.api_key_env.clone();
        self.registry_options.image_gen = image_gen;
    }

    /// 子任务设置：总开关走功能开关，高级参数写 `subagents.toml`。
    ///
    /// 子任务的运行期在内核，宿主这边没有句柄，所以只写盘并说明生效时机。
    fn apply_subagent(&mut self, change: &SubagentChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        match change {
            SubagentChange::Enabled(enabled) => {
                let path = save_feature_enabled(&environment, "subagents", *enabled, None, None)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                Ok(format!(
                    "子任务功能已{}，已保存到 {}；子任务运行期在内核，重新开会话后生效。",
                    if *enabled { "开启" } else { "关闭" },
                    path.display()
                ))
            }
            SubagentChange::Advanced { key, value } => {
                let spec = SUBAGENT_ADVANCED_SPECS
                    .iter()
                    .find(|spec| spec.key == key.as_str())
                    .ok_or_else(|| format!("设置未完成：未知的子任务参数 {key}。"))?;
                // 子任务超时在配置里是浮点：写成整数会被类型校验拒绝。
                let raw = if spec.is_float {
                    omnicrawl_config::toml::Value::Float(*value as f64)
                } else {
                    omnicrawl_config::toml::Value::Integer(*value)
                };
                validate_subagent_advanced_setting(key, &raw)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                let path = save_subagent_setting(&environment, key, &raw, None)
                    .map_err(|error| format!("设置未完成：{}", error.message()))?;
                Ok(format!(
                    "{}已设为 {value}，已保存到 {}。",
                    spec.label,
                    path.display()
                ))
            }
        }
    }

    /// 视觉设置：写 `config.toml [vision]`；模型原生视觉有改动时连带写回 models.toml 或渠道。
    ///
    /// 与 Python 一致：启用但列表为空时拒绝保存；模型原生视觉先写（它自身失败会回滚），
    /// 再写代理配置。
    fn apply_vision(&mut self, change: &VisionChange) -> Result<String, String> {
        let environment = ConfigEnvironment::from_process();
        if change.enabled && change.models.is_empty() {
            return Err("设置未完成：启用视觉模型代理前，至少添加一个视觉模型。".to_string());
        }
        if let Some(native) = change.native {
            let (scope, label) = vision_native_target(&self.llm);
            if label.is_empty() {
                return Err(
                    "设置未完成：当前模型缺少渠道或模型标识，无法保存模型原生视觉。".to_string(),
                );
            }
            // 自定义模型按 key 写 models.toml，其余按渠道标识写 config.toml。
            let (catalog_key, profile_id) = if scope == "model" {
                (label.as_str(), "")
            } else {
                ("", label.as_str())
            };
            save_native_vision(
                &environment,
                native,
                scope,
                catalog_key,
                profile_id,
                None,
                None,
            )
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        }
        let configuration = VisionConfiguration {
            enabled: change.enabled,
            models: change.models.iter().map(vision_ref_to_config).collect(),
        };
        let path = save_vision_configuration(&environment, &configuration, None)
            .map_err(|error| format!("设置未完成：{}", error.message()))?;
        Ok(format!(
            "视觉设置已保存到 {}（{} 个故障转移模型）。",
            path.display(),
            change.models.len()
        ))
    }

    /// 按当前 `registry_options` 重建工具表。
    ///
    /// 只换工具表：正在跑的后台命令与取消令牌从旧表带过去，否则一次开关
    /// 会把 `monitor` 启动的后台进程从宿主的账上抹掉。
    fn rebuild_registry(&mut self) -> Result<(), String> {
        let mut options = self.registry_options.clone();
        options.monitors = Some(self.registry.monitors().clone());
        options.cancel = Some(self.registry.cancel_token());
        let registry = ToolRegistry::new(
            &self.workspace,
            &options,
            self.options.command_timeout_seconds,
        )
        .map_err(|error| format!("设置未完成：{}", error.message))?;
        self.registry = Arc::new(registry);
        Ok(())
    }

    /// 运行中切换工作区（`/workspace <路径>` 的同步入口）。
    ///
    /// 窄口径对映 Python `WorkspaceSwitchingMixin.switch_workspace` 的主体：
    /// 解析校验 → 子 Agent 排空（仍有活跃任务且超期时拒绝）→ pending worktree 拦阻
    /// → 准备新子系统（工具表 / MCP 连接 / 提示词运行时）→ 插件
    /// `workspace.switch.before`（拒绝即中止）→ 收尾旧资源（旧工作区的 MCP 与
    /// 后台任务）→ 提交新状态 → 内核侧同一会话补写 `workspace_switched`。
    ///
    /// TUI 的 `/workspace` 走 [`Self::start_workspace_switch`]（装配在工作线程，界面
    /// 不冻）；这一条是同步实现，供不接管慢命令的宿主（测试替身等）使用。
    ///
    /// 与 Python 的已知差异（见 README「本阶段的边界」）：before/after 钩子由
    /// `PluginHost::switch_workspace` 一次发出，因此钩子相对「准备候选子系统」的
    /// 先后与 Python 不同源。
    ///
    /// 事务性：候选子系统先在本地装配，插件切换通过后才提交；任一步失败都不动
    /// 当前工作区与工具表（候选 MCP/后台任务在失败路径上主动 close，不留孤儿进程）。
    pub(crate) fn command_switch_workspace(&mut self, path: &str) -> Result<PathBuf, String> {
        let Some(new_root) = self.workspace_switch_target(path)? else {
            // 目标就是当前工作区：Python 同样直接返回，不重建、不转录事件。
            let current = self.workspace.clone();
            return Ok(current);
        };
        let (registry_options, options, environment, timeout) =
            self.workspace_prepare_inputs(&new_root);
        let prepared = match prepare_workspace(
            &environment,
            &options,
            &new_root,
            registry_options,
            timeout,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.notify_workspace_switch_error(&new_root, &error);
                return Err(error);
            }
        };
        self.commit_workspace_switch(&new_root, path, prepared)
    }

    /// `/workspace <路径>` 的异步入口（宿主接管慢命令）：装配在工作线程，提交在主线程。
    ///
    /// 对映 Python 把延迟执行体交给工作线程：前置检查是几次本地内核往返（毫秒级，留在
    /// 主线程），真正慢的一段（MCP 连接与发现、工具表与提示词运行时重建）在线程里跑。
    fn start_workspace_switch(&mut self, path: &str) {
        if let Some(task) = self.slow_task.as_ref() {
            self.state.notice(format!(
                "上一条慢命令（{}）还在执行中，稍候再试。",
                task.kind.working_status()
            ));
            return;
        }
        let new_root = match self.workspace_switch_target(path) {
            Ok(Some(root)) => root,
            Ok(None) => {
                let current = self.workspace.clone();
                self.state.notice(workspace_switch_success_message(&current));
                return;
            }
            Err(message) => {
                self.state.notice(message);
                return;
            }
        };
        let (registry_options, options, environment, timeout) =
            self.workspace_prepare_inputs(&new_root);
        let requested = path.trim().to_string();
        let thread_root = new_root.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let prepared = prepare_workspace(
                &environment,
                &options,
                &thread_root,
                registry_options,
                timeout,
            );
            let _ = sender.send(SlowOutcome::WorkspacePrepared {
                root: thread_root,
                requested,
                prepared,
            });
        });
        self.state.status = Some(SlowTaskKind::WorkspaceSwitch.working_status().to_string());
        self.state
            .notice(WORKSPACE_SWITCH_PENDING_MESSAGE.to_string());
        self.slow_task = Some(SlowTask {
            kind: SlowTaskKind::WorkspaceSwitch,
            outcome: receiver,
        });
    }

    /// `/mcp` 的异步入口：状态文本在工作线程读（`format_status` 会触发 MCP 发现与连接）。
    fn start_mcp_status(&mut self) {
        if let Some(task) = self.slow_task.as_ref() {
            self.state.notice(format!(
                "上一条慢命令（{}）还在执行中，稍候再试。",
                task.kind.working_status()
            ));
            return;
        }
        let manager = self.registry.mcp().cloned();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            // MCP 清单由宿主工具表持有连接；未配置时给一句明确说明，而不是空串。
            let text = match manager {
                Some(manager) => manager.format_status(),
                None => {
                    "MCP 未配置：在 config.toml 的 [mcp] 段添加 Server 后可查看详情。".to_string()
                }
            };
            let _ = sender.send(SlowOutcome::McpStatus(text));
        });
        self.state.status = Some(SlowTaskKind::McpStatus.working_status().to_string());
        self.slow_task = Some(SlowTask {
            kind: SlowTaskKind::McpStatus,
            outcome: receiver,
        });
    }

    /// 每帧轮询慢命令的产出：回填消息、收起状态行，工作区切换在这里提交。
    pub fn tick_slow_command(&mut self) {
        let received = match self.slow_task.as_ref() {
            Some(task) => task.outcome.try_recv(),
            None => return,
        };
        let outcome = match received {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // 线程崩了：不能把状态行永远留在「正在准备…」。
                self.slow_task = None;
                self.state.status = None;
                self.state
                    .notice("慢命令后台任务意外结束，请重试。".to_string());
                return;
            }
        };
        self.slow_task = None;
        self.state.status = None;
        match outcome {
            SlowOutcome::McpStatus(text) => self.state.notice(text),
            SlowOutcome::WorkspacePrepared {
                root,
                requested,
                prepared,
            } => match prepared {
                Ok(prepared) => {
                    if let Err(message) = self.commit_workspace_switch(&root, &requested, prepared) {
                        self.state.notice(message);
                    }
                }
                Err(message) => {
                    // 装配阶段失败：通知插件 `workspace.switch.error`（对映 Python
                    // `_prepare_workspace_switch` 的 except），旧工作区一律不动。
                    self.notify_workspace_switch_error(&root, &message);
                    self.state.notice(message);
                }
            },
        }
    }

    /// 退出前收尾在途的慢命令：短暂等一次结果，拿到的候选子系统要关干净。
    ///
    /// 不等就会把候选 MCP 子进程变成孤儿（线程随进程退出而消失，候选资源无人 close）。
    pub fn settle_slow_task(&mut self) {
        let Some(task) = self.slow_task.take() else {
            return;
        };
        if let Ok(SlowOutcome::WorkspacePrepared {
            prepared: Ok(prepared),
            ..
        }) = task
            .outcome
            .recv_timeout(Duration::from_millis(SLOW_TASK_SETTLE_MS))
        {
            prepared.registry.close_monitors();
            prepared.registry.close_mcp();
        }
    }

    /// `/workspace` 的公共前置：回合门禁 → 解析校验 → 子 Agent 排空 → worktree 拦阻。
    ///
    /// 返回 `Ok(None)` 表示目标就是当前工作区（调用方只需回一句成功文案）。
    fn workspace_switch_target(&mut self, path: &str) -> Result<Option<PathBuf>, String> {
        // 回合进行中不切换：本轮的工作区快照与工具批次已经按旧根展开。
        if self.state.turn.is_running() {
            return Err(
                "工作区切换失败：当前回合仍在执行，请等它结束或先取消后再切换。".to_string(),
            );
        }
        let new_root = resolve_switch_target(path).map_err(|error| error.message().to_string())?;
        if same_directory(&new_root, &self.workspace) {
            return Ok(None);
        }
        // 前置检查（对映 Python 的 `coordinator.cancel_and_wait` 与
        // `list_subagent_worktrees`）：先排空活跃子任务，再有未处理的 worktree 就拒绝。
        self.drain_subagents_before_switch()?;
        let pending_worktrees = self.pending_subagent_worktrees()?;
        if !pending_worktrees.is_empty() {
            return Err(pending_worktrees_error(&pending_worktrees)
                .message()
                .to_string());
        }
        Ok(Some(new_root))
    }

    /// 装配候选子系统所需的输入（同步路径与工作线程共用同一口径）。
    fn workspace_prepare_inputs(
        &self,
        root: &Path,
    ) -> (RegistryOptions, Options, ConfigEnvironment, i64) {
        let mut options = self.registry_options.clone();
        // 旧工作区的后台任务不带进新工作区（与 Python 关闭旧 MonitorManager 同义）：
        // 置空让 `ToolRegistry::new` 按新根建一个新管理器，避免把旧工作区的
        // monitor 进程记到新工作区账上。取消令牌与工作区无关，照旧沿用。
        options.monitors = None;
        options.cancel = Some(self.registry.cancel_token());
        options.mcp = mcp_manager(root);
        (
            options,
            self.options.clone(),
            ConfigEnvironment::from_process(),
            self.options.command_timeout_seconds,
        )
    }

    /// 提交候选子系统：插件门禁 → 收尾旧资源 → 换状态 → 内核下发。
    ///
    /// 失败路径上候选 MCP/后台任务主动 close，不留孤儿进程；提交开始后不再有可失败步骤。
    fn commit_workspace_switch(
        &mut self,
        new_root: &Path,
        requested: &str,
        prepared: PreparedWorkspace,
    ) -> Result<PathBuf, String> {
        let PreparedWorkspace {
            registry: candidate,
            prompt,
        } = prepared;
        // 插件侧重建是事务式的：`workspace.switch.before` 拒绝时保持旧 Worker 不动，
        // 工作区的 before/after 钩子也都在这一调用里（与 Python 不同源，但与
        // `PluginHost::switch_workspace` 的既定契约一致）。
        let diagnostics = match self.plugins.switch_workspace(new_root) {
            Ok(diagnostics) => diagnostics,
            Err(error) => {
                candidate.close_monitors();
                candidate.close_mcp();
                return Err(format!("工作区切换失败：{error}"));
            }
        };
        let mut options = self.registry_options.clone();
        options.monitors = None;
        options.cancel = Some(self.registry.cancel_token());
        options.mcp = mcp_manager(new_root);
        // —— 提交：以下不再有可失败步骤 ——
        // 先暂停轮询并废弃旧工作区的游标（任务 ID 可能在新工作区复用），
        // 提交完再恢复；旧后台任务随旧工具表一起关闭。
        self.monitor_state.suspend_for_workspace_switch();
        self.registry.close_monitors();
        self.registry.close_mcp();
        self.workspace = new_root.to_path_buf();
        self.registry_options = options;
        self.registry = Arc::new(candidate);
        self.prompt = prompt;
        self.monitor_state.resume_polling();
        if let Some(settings) = self.settings.as_mut() {
            settings.refresh_tools(tool_switch_rows(&self.registry));
        }
        // 跳进程同步：把新工作区写回 config.toml（远程入口在任务开始前重读并跟随）；
        // 失败只提示不阻断，与命令层 `persist_workspace_root` 同口径。
        if let Err(error) = save_workspace_root(
            &ConfigEnvironment::from_process(),
            Path::new(requested),
            None,
        ) {
            self.state.notice(format!(
                "工作区持久化到 config.toml 失败：{}",
                error.message()
            ));
        }
        // 内核侧：先把新工具表与上下文消息推给内核（与新工作区的稳定前缀一致），
        // 再让它在同一会话里转录 `workspace_switched`。两者都不等回包：
        // `handle_frame` 对未匹配的响应帧只丢弃，不阻断界面。
        let context_messages = match self.prompt.context_messages_with_plugins(
            true,
            Some(self.plugins.as_ref()),
            None,
            None,
        ) {
            Ok(messages) => messages,
            Err(error) => {
                // 同上：切完工作区装不出上下文时，界面必须看得到原因。
                self.state
                    .notice(format!("上下文装配失败：{error}（只推送了工具表）"));
                Vec::new()
            }
        };
        let _ = self.send(Command::SessionSettings(Box::new(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                tools: Some(self.registry.declarations()),
                context_messages: Some(context_messages),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        })));
        let _ = self.send(Command::WorkspaceSwitch(omnicrawl_ipc::bridge::WorkspaceSwitchParams {
            path: new_root.to_string_lossy().to_string(),
        }));
        for line in diagnostics {
            self.state.notice(format!("插件：{line}"));
        }
        let message = workspace_switch_success_message(new_root);
        self.state.notice(message);
        Ok(new_root.to_path_buf())
    }

    /// 切换前的子 Agent 排空（对映 Python `coordinator.cancel_and_wait(reason, timeout, permanent=False)`）。
    ///
    /// 仍活跃的后台子任务逐个取消，然后轮询到它们离开任务表；超期未退出就报
    /// `subagent_drain_error`（原工作区与共享资源一律不动）。任务表本身在内核里，
    /// 因此这里走宿主→内核的同步往返（与 `/tasks` 同一条路径）。
    fn drain_subagents_before_switch(&mut self) -> Result<(), String> {
        let mut active = self.active_subagent_tasks()?;
        if active.is_empty() {
            return Ok(());
        }
        for task_id in &active {
            // 逐个取消：单个取消失败不阻断其余任务（与 Python 让协调器统一取消同效）。
            let _ = self.command_subagent_query("cancel", task_id);
        }
        let deadline = Instant::now()
            + Duration::from_secs_f64(
                omnicrawl_controllers::shared::SUBAGENT_LIFECYCLE_WAIT_SECONDS,
            );
        while Instant::now() < deadline {
            active = self.active_subagent_tasks()?;
            if active.is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(subagent_drain_error().message().to_string())
    }

    /// 仍未离开任务表（非终态）的子任务 id。
    fn active_subagent_tasks(&mut self) -> Result<Vec<String>, String> {
        let value = self.command_subagent_query("list", "")?;
        // 后台任务管理器未启用时没有任务，与「空表」同义。
        if value
            .get("unavailable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(Vec::new());
        }
        let tasks = value
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(tasks
            .iter()
            .filter(|task| {
                let status = task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                !is_terminal(status)
            })
            .filter_map(|task| {
                task.get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    /// 当前工作区仍未处理的 SubAgent worktree（切换前的拦阻项）。
    ///
    /// 托管根是全局的（`~/.omnicrawl/agent-worktrees`），可能还留着别的仓库的残留，
    /// 因此按元数据里的 `repo_root` 对齐当前工作区（对映 Python 的「本进程登记的会话」）。
    fn pending_subagent_worktrees(&mut self) -> Result<Vec<WorktreeRef>, String> {
        let value = self.command_subagent_query("list_worktrees", "")?;
        let items = value
            .get("worktrees")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let current = self.workspace.clone();
        Ok(items
            .iter()
            .filter(|item| match item.get("repo_root").and_then(Value::as_str) {
                Some(root) if !root.is_empty() => same_directory(std::path::Path::new(root), &current),
                // 元数据缺 repo_root（旧版本）：宁多拦一个，也不让未处理的 worktree 静默丢失。
                _ => true,
            })
            .map(|item| WorktreeRef {
                branch: item
                    .get("branch")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                task_id: item
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect())
    }

    /// 准备阶段失败时通知插件 `workspace.switch.error`（对映 Python
    /// `_prepare_workspace_switch` 的 except 分支）。钩子是 NOTIFY 语义，失败不抛错。
    fn notify_workspace_switch_error(&self, root: &std::path::Path, error: &str) {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "workspace".to_string(),
            Value::String(root.to_string_lossy().to_string()),
        );
        payload.insert("error".to_string(), Value::String(error.to_string()));
        let _decision = self
            .plugins
            .dispatch("workspace.switch.error", payload, None, None);
    }

    /// 把本次变更推给内核做热更新；结果在 `handle_frame` 里回填。
    ///
    /// 只有内核持有的设置在协议上有对应字段：工具声明、上下文窗口与压缩阈值、推理强度。
    /// 思考显示与功能开关属于宿主/配置面，没有帧可发（`params` 为空时直接返回）。
    fn push_session_settings(&mut self, change: &SettingsChange) {
        let compaction = match change {
            SettingsChange::ContextWindow { .. } | SettingsChange::CompactionPercent { .. } => {
                let percent = self
                    .settings
                    .as_ref()
                    .map(|settings| settings.compaction_percent())
                    .unwrap_or(80);
                let window = self
                    .settings
                    .as_ref()
                    .map(|settings| settings.context_window_tokens())
                    .unwrap_or(128_000);
                Some(compaction_settings(window, percent))
            }
            _ => None,
        };
        let model = match change {
            // 换渠道：模型 id 与整条渠道一起下发（空串会被内核按「不改」忽略）。
            SettingsChange::Model { .. } | SettingsChange::Channels { .. } => {
                Some(SessionModelSettings {
                    model: Some(self.llm.model.clone()),
                    provider: Some(self.llm.provider.clone()),
                    protocol: Some(self.llm.protocol.clone()),
                    base_url: Some(self.llm.base_url.clone()),
                    api_key_env: Some(self.llm.api_key_env.clone()),
                    ..SessionModelSettings::default()
                })
            }
            SettingsChange::ToolSwitch { .. } => Some(SessionModelSettings {
                tools: Some(self.registry.declarations()),
                ..SessionModelSettings::default()
            }),
            SettingsChange::ContextWindow { tokens } => Some(SessionModelSettings {
                context_window_tokens: Some(*tokens),
                ..SessionModelSettings::default()
            }),
            SettingsChange::Reasoning { effort } => Some(SessionModelSettings {
                reasoning_effort: Some(effort.clone()),
                ..SessionModelSettings::default()
            }),
            _ => None,
        };
        // 配置写盘已经成功，这里只为「本会话即时生效」；内核拒绝不是失败。
        let params = SessionSettingsParams {
            model: model.map(Box::new),
            compaction,
        };
        if params.model.is_none() && params.compaction.is_none() {
            return;
        }
        let id = self.send(Command::SessionSettings(Box::new(params)));
        self.settings_request = Some((id, change.clone()));
    }

    /// 内核对 `session.settings` 的响应：拒绝时在状态文本里如实补一句。
    fn apply_settings_response(&mut self, change: &SettingsChange, frame: &Frame) {
        let Some(error) = frame.error.as_ref() else {
            return;
        };
        self.state
            .notice(format!("内核未接受设置更新：{}", error.message));
        if let Some(settings) = self.settings.as_mut() {
            settings.note_kernel_rejection(
                change,
                &format!(
                    "内核未接受即时更新（{}），将在下次会话生效。",
                    error.message
                ),
            );
        }
    }

    fn cancel_turn(&mut self) {
        let Some(turn_id) = self.state.turn.turn_id().map(|value| value.to_string()) else {
            return;
        };
        self.send(Command::TurnCancel(omnicrawl_ipc::TurnCancelParams {
            turn_id: turn_id.clone(),
        }));
        // 取消也是回合结束：插件侧发 `turn.cancelled`（通知类，失败不阻断收尾）。
        self.plugins.turn_cancelled(None, Some(turn_id.as_str()));
        // 先回收正在跑的进程树与这个回合的后台任务，再卸下批次：迟到的结果会被忽略。
        self.registry.cancel_token().cancel();
        self.registry
            .stop_monitors_in_scope(&turn_id, crate::tools::monitor::CANCEL_REASON);
        self.tool_deadline = None;
        self.state.cancel_batch();
        self.state.fail_turn("已取消当前回合。".to_string());
        // 取消也是回合结束：排队消息接着按 FIFO 提交。
        self.drain_pending_inputs();
    }

    /// 退出收尾：回收后台进程、收尾隔离工作区后请内核退出。
    pub fn shutdown(&mut self) {
        // 在途的慢命令先收尾：候选子系统得 close，否则 MCP 子进程会成孤儿。
        self.settle_slow_task();
        self.registry.close_monitors();
        self.registry.close_mcp();
        self.finalize_isolation();
        self.request_shutdown();
    }

    /// `session.close.before` / `after`：内核补写 `session_closed` 的观察与通知钩子。
    ///
    /// 顺序与 Python `LocalToolAgent.close` 一致：退出收尾后先发 before，内核据此
    /// 在写事件与丢空占位之前得到一次观察机会；进程退出后再发 after。会话 id 未知
    /// （握手尚未回填、或本来就不自持会话）时跳过。
    fn dispatch_session_close(&self, before: bool) {
        let session_id = self.current_session_id();
        if session_id.is_empty() {
            return;
        }
        self.plugins.session_close(&session_id, before);
    }

    /// 内核退出后的 `session.close.after`：由 `main` 在 `wait_or_kill` 之后调用。
    pub fn finish_session_close(&self) {
        self.dispatch_session_close(false);
    }

    fn start_shutdown(&mut self) {
        // 退出前丢开未提交的排队消息（对映 Python `_pending_inputs.clear()`）。
        self.state.clear_pending();
        self.request_shutdown();
        self.quit = true;
    }

    /// 请内核退出；连接已断时什么都不做。
    pub fn request_shutdown(&mut self) {
        if self.kernel.is_closed() {
            return;
        }
        // 只在真正下发 shutdown 的那一次发 before 钩子。
        if !self.session_close_before_sent {
            self.session_close_before_sent = true;
            self.dispatch_session_close(true);
        }
        self.send(Command::Shutdown);
    }

    fn send(&mut self, command: Command) -> Id {
        let id = self.kernel.next_id();
        let frame = command.to_frame(id.clone());
        if let Err(error) = self.kernel.send_frame(&frame) {
            self.state
                .notice(format!("发送内核命令失败：{error}"));
        }
        id
    }

    // ---------- 内核同步往返 ----------

    /// 发一条命令并等它的响应帧（只用于毫秒级的只读查询）。
    ///
    /// 命令层的 `CommandAgent` 是同步接口，而内核链路是异步的帧，因此这里在等待期间
    /// 把「先到的其他帧」原样收进 [`App::deferred_frames`]，交给下一次 [`App::drain_frames`]
    /// 按原顺序处理：通知不丢，也不会在命令处理器里重入渲染或重入工具批次。
    fn request_kernel(&mut self, command: Command, timeout: Duration) -> Result<Frame, String> {
        if self.kernel.is_closed() {
            return Err("内核进程已退出。".to_string());
        }
        let id = self.send(command);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(KERNEL_POLL_INTERVAL);
            match self.kernel.recv_timeout(wait) {
                Some(frame) if frame.is_response() && frame.id() == Some(&id) => {
                    return match frame.error.as_ref() {
                        Some(error) => Err(format!("内核拒绝请求：{}", error.message)),
                        None => Ok(frame),
                    };
                }
                Some(frame) => self.deferred_frames.push_back(frame),
                None if self.kernel.is_closed() => {
                    return Err("内核进程在等待响应期间退出。".to_string())
                }
                None => {}
            }
        }
        Err(format!("等待内核响应超时（{} 秒）。", timeout.as_secs()))
    }

    // ---------- 命令层钩子 ----------
    //
    // `commands::TuiHostAgent` 只经这些入口碰宿主：内核链路、工具表与配置读写的
    // 细节都收在这里，命令层不必知道。

    pub(crate) fn command_workspace_root(&self) -> PathBuf {
        self.workspace.clone()
    }

    /// 当前审批模式名（`manual` / `review` / `auto`，与 Python 的 `approval_mode()` 同形）。
    pub(crate) fn command_approval_mode(&self) -> &'static str {
        self.state.approval.as_str()
    }

    /// 运行期切换审批模式；持久化由命令处理器自己写 `config.toml`。
    ///
    /// 审批由宿主执行层判定，内核不知道也不需要知道模式，因此这里只改本进程状态。
    pub(crate) fn command_set_approval_mode(&mut self, mode: &str) -> Result<(), String> {
        let parsed = ApprovalMode::parse(mode)?;
        self.state.approval = parsed;
        self.options.approval = parsed;
        Ok(())
    }

    pub(crate) fn command_llm_config(&self) -> LlmConfig {
        self.llm.clone()
    }

    /// 启用主 Agent 模式：重算 system prompt（含模式区块）并即时下发内核。
    ///
    /// 模式改的是 system prompt，而 system prompt 由宿主装配（模板 + AGENTS.md + Skill），
    /// 因此这里必须与握手走同一份装配结果；内核只按帧里的文本发请求。
    pub(crate) fn command_activate_mode(&mut self, mode: &str) -> Result<String, String> {
        let activated = self.prompt.activate_mode(mode)?;
        let session_id = self.current_session_id();
        let settings = SessionModelSettings {
            system_prompt: Some(self.prompt.system_prompt()),
            context_messages: Some(self.prompt.context_messages_with_plugins(
                !self.registry.declarations().is_empty(),
                Some(self.plugins.as_ref()),
                (!session_id.is_empty()).then_some(session_id.as_str()),
                None,
            )?),
            ..SessionModelSettings::default()
        };
        let params = SessionSettingsParams {
            model: Some(Box::new(settings)),
            compaction: None,
        };
        let id = self.send(Command::SessionSettings(Box::new(params)));
        // 模式提示词已在本进程生效（下一轮请求就带上）；内核拒绝时如实补一句，
        // 不能因为「配置已生效」就把拒绝吞掉。
        self.prompt_request = Some(id);
        Ok(activated)
    }

    /// 当前模型标识：命中 models.toml 的自定义模型时给 key，否则给真实 model_id
    /// （与 Python `current_model()` 同口径）。
    pub(crate) fn command_current_model(&self) -> String {
        let environment = ConfigEnvironment::from_process();
        if let Ok(store) = load_model_store(&environment, None) {
            if let Some(record) = store
                .models
                .iter()
                .find(|record| record.model_id == self.llm.model)
            {
                return record.key.clone();
            }
        }
        self.llm.model.clone()
    }

    /// 把选择解析成新的模型视图；解析失败即报错，不做静默降级。
    pub(crate) fn command_resolve_model(&self, selection: &str) -> Result<LlmConfig, AgentError> {
        let environment = ConfigEnvironment::from_process();
        apply_model_selection(&environment, &self.llm, selection)
            .map_err(|error| AgentError::new(error.message()))
    }

    /// 应用解析好的模型视图：宿主视图＋本会话热更新一并跟上。
    pub(crate) fn command_apply_model_view(&mut self, llm: LlmConfig) {
        self.llm = llm;
        self.state.model = self.llm.model.clone();
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                model: Some(self.llm.model.clone()),
                provider: Some(self.llm.provider.clone()),
                protocol: Some(self.llm.protocol.clone()),
                base_url: Some(self.llm.base_url.clone()),
                api_key_env: Some(self.llm.api_key_env.clone()),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
    }

    /// 校验并切换推理强度；返回归一化后的档位（写盘由命令处理器负责）。
    pub(crate) fn command_set_reasoning_effort(
        &mut self,
        effort: &str,
    ) -> Result<String, AgentError> {
        let normalized =
            normalize_reasoning_effort(effort).map_err(|error| AgentError::new(error.message()))?;
        self.llm.reasoning_effort = normalized.to_string();
        self.push_session_settings_raw(SessionSettingsParams {
            model: Some(Box::new(SessionModelSettings {
                reasoning_effort: Some(normalized.to_string()),
                ..SessionModelSettings::default()
            })),
            compaction: None,
        });
        Ok(normalized.to_string())
    }

    /// 插件子系统只读状态（与 Python `format_plugins_status` 的两个分支同形）。
    pub(crate) fn command_plugins_status(&self) -> String {
        if !self.plugins.configured() {
            return omnicrawl_controllers::control::plugins_status_text(None);
        }
        let rows: Vec<omnicrawl_controllers::control::PluginWorkerRow> = self
            .plugins
            .status_rows()
            .iter()
            .map(commands::plugin_row)
            .collect();
        omnicrawl_controllers::control::plugins_status_text(Some(&(self.plugins.enabled(), rows)))
    }

    /// 后台 SubAgent 任务的查询与取消：内核持有任务管理器，走一次快速往返。
    pub(crate) fn command_subagent_query(
        &mut self,
        action: &str,
        task_id: &str,
    ) -> Result<Value, String> {
        let params = omnicrawl_ipc::bridge::SubagentQueryParams {
            action: action.to_string(),
            task_id: task_id.to_string(),
        };
        let frame = self.request_kernel(Command::SubagentQuery(params), SUBAGENT_QUERY_TIMEOUT)?;
        Ok(frame.result.clone().unwrap_or(Value::Null))
    }

    // ---------- 会话生命周期（命令层钩子）----------
    //
    // 会话由内核持有，宿主这里是「薄客户端」：每个方法都是一次快速往返加一次视图同步。
    // 往返期间到达的其他帧先进 `deferred_frames`，因此不会丢通知。

    /// 当前会话 id（握手与各会话命令的回执里同步过来）。
    pub(crate) fn current_session_id(&self) -> String {
        self.session_id.clone()
    }

    /// 会话类命令的同步往返：内核只在本地会话目录上读写，毫秒级。
    ///
    /// 超时给得比子任务查询宽：`/resume` 要按转录重建历史，大会话的读盘量不小。
    fn session_request(&mut self, command: Command) -> Result<Value, String> {
        let frame = self.request_kernel(command, SESSION_COMMAND_TIMEOUT)?;
        Ok(frame.result.clone().unwrap_or(Value::Null))
    }

    /// 提交 `/sessions`：读最近会话并在输入框上方打开可导航菜单。
    ///
    /// 列表读取失败或没有会话时回退到对话区提示（对映 Python `_show_sessions_menu`）。
    fn open_sessions_menu(&mut self) {
        let entries = match self.command_session_list(false, 10) {
            Ok(entries) => entries,
            Err(message) => {
                self.state.notice(format!("会话列表读取失败：{message}"));
                return;
            }
        };
        if entries.is_empty() {
            self.state
                .notice("当前工作区还没有可恢复会话。".to_string());
            return;
        }
        let current = self.session_id.clone();
        let items: Vec<SessionMenuItem> = entries
            .iter()
            .map(|entry| SessionMenuItem {
                session_id: entry.session_id.clone(),
                updated_at: entry
                    .updated_at
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string(),
                message_count: entry.message_count,
                title: if entry.title.trim().is_empty() {
                    "未命名会话".to_string()
                } else {
                    entry.title.clone()
                },
                is_current: entry.session_id == current,
            })
            .collect();
        self.state.open_sessions_menu(items);
    }

    /// 会话列表（`/sessions` / `/archives`）：顺带把当前会话 id 对齐到内核。
    pub(crate) fn command_session_list(
        &mut self,
        archived: bool,
        limit: usize,
    ) -> Result<Vec<SessionIndexEntry>, String> {
        let params = SessionListParams {
            archived,
            limit: session_limit(limit),
        };
        let result = self.session_request(Command::SessionList(params))?;
        if let Some(current) = result.get("current_session_id").and_then(Value::as_str) {
            self.session_id = current.to_string();
        }
        session_entries(&result)
    }

    /// 重命名当前会话（`/rename`）。
    pub(crate) fn command_session_rename(&mut self, title: &str) -> Result<SessionSummary, String> {
        let params = SessionRenameParams {
            title: title.to_string(),
        };
        let result = self.session_request(Command::SessionRename(params))?;
        session_summary(&result)
    }

    /// 归档当前会话（`/archive`）：内核归档后已自动开好新会话，这里跟上新 id 并清空视图。
    pub(crate) fn command_session_archive(&mut self) -> Result<SessionSummary, String> {
        let result = self.session_request(Command::SessionArchive)?;
        if let Some(new_id) = result.get("new_session_id").and_then(Value::as_str) {
            self.session_id = new_id.to_string();
        }
        // 新会话没有历史：视图清空是「已归档并新开」的如实投影，不只是提示文案。
        self.state.replay_history(&[]);
        session_summary(&result)
    }

    /// 新开会话（`/new`）：返回旧会话 id 之外的新会话 id。
    pub(crate) fn command_session_new(&mut self) -> Result<String, String> {
        let result = self.session_request(Command::SessionNew)?;
        let Some(session_id) = result.get("session_id").and_then(Value::as_str) else {
            return Err("内核没有返回新会话 id。".to_string());
        };
        self.session_id = session_id.to_string();
        self.state.replay_history(&[]);
        Ok(self.session_id.clone())
    }

    /// `/resume`：切换内核会话，并用事件流重放对话视图。
    ///
    /// 先发一条 `session.events` 请内核给出有效事件流（与 Python
    /// `current_session_events()` 同源），读不到时才退回回执里的消息投影。
    pub(crate) fn command_session_resume(
        &mut self,
        session_id: &str,
    ) -> Result<SessionSummary, String> {
        let params = SessionResumeParams {
            session_id: session_id.to_string(),
        };
        let result = self.session_request(Command::SessionResume(params))?;
        if let Some(target) = result.get("session_id").and_then(Value::as_str) {
            self.session_id = target.to_string();
        }
        let history = result
            .get("history")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match self.session_events_result() {
            Some(events) => self.state.replay_events(&events),
            None => self.state.replay_history(&history),
        }
        session_summary(&result)
    }

    /// 请内核给出当前会话的有效事件流（`session.events`）；读不到时为 `None`。
    pub(crate) fn session_events_result(&mut self) -> Option<Vec<Value>> {
        let result = self.session_request(Command::SessionEvents).ok()?;
        Some(
            result
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        )
    }

    /// 异步请求一次会话回放（`/undo` 回执与命令层的 `replay_conversation` 标记）。
    ///
    /// `fallback` 是事件流读不到时用的消息投影，`notice` 是回放完成后再进消息流的
    /// 命令提示（顺序不能颠倒：回放会清空消息流）。已经有一条回放在途时不重复下发。
    fn request_session_replay(&mut self, fallback: Vec<Value>, notice: Option<String>) {
        if self
            .pending_commands
            .values()
            .any(|pending| matches!(pending, KernelCommand::Replay { .. }))
        {
            // 回放是幂等的整视图重建，在途时用回退投影先把视图对齐，不再排队。
            if !fallback.is_empty() {
                self.state.replay_history(&fallback);
            }
            if let Some(notice) = notice {
                self.state.notice(notice);
            }
            return;
        }
        self.state.status = Some("正在重放会话…".to_string());
        let id = self.send(Command::SessionEvents);
        self.pending_commands.insert(
            id,
            KernelCommand::Replay { fallback, notice },
        );
    }

    /// 提示历史（`/history`）：只读展示，不注入模型上下文。
    pub(crate) fn command_session_history(
        &mut self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PromptHistoryEntry>, String> {
        let params = SessionHistoryParams {
            query: query.to_string(),
            limit: session_limit(limit),
        };
        let result = self.session_request(Command::SessionHistory(params))?;
        let items = result
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items
            .iter()
            .map(|value| {
                PromptHistoryEntry::from_dict(value).map_err(|error| error.message().to_string())
            })
            .collect()
    }

    /// 把宿主产生的文本作为 assistant 消息注入内核上下文（`/review` 的报告）。
    pub(crate) fn command_session_append(&mut self, content: &str) -> Result<bool, String> {
        let params = SessionAppendParams {
            role: "assistant".to_string(),
            content: content.to_string(),
        };
        let result = self.session_request(Command::SessionAppend(params))?;
        Ok(result
            .get("appended")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    /// 下发一条 `session.settings`：设置面板与命令层共用同一条热更新路径。
    fn push_session_settings_raw(&mut self, params: SessionSettingsParams) {
        if params.model.is_none() && params.compaction.is_none() {
            return;
        }
        let id = self.send(Command::SessionSettings(Box::new(params)));
        // 命令层不跟踪内生推包的拒绝：设置面板自己会记（见 `settings_request`）。
        let _ = id;
    }
}

/// 同步等内核响应时的轮询间隔（只影响等待中的可打断粒度）。
const KERNEL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 后台子任务查询的超时：内核只是查内存里的任务表，毫秒级。
const SUBAGENT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// 会话类命令的超时：本地会话目录的读写，但 `/resume` 要按转录重建历史。
const SESSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 命令层给的会话条数 → 协议里的 `u32`（内核会自己再收敛到 1..=100）。
fn session_limit(limit: usize) -> u32 {
    limit.clamp(1, u32::MAX as usize) as u32
}

/// 内核回执里的 `sessions` 数组 → 会话索引条目。
fn session_entries(result: &Value) -> Result<Vec<SessionIndexEntry>, String> {
    let items = result
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .map(|value| {
            SessionIndexEntry::from_dict(value).map_err(|error| error.message().to_string())
        })
        .collect()
}

/// 内核回执里的 `session` 条目 → 命令层展示用的会话快照。
fn session_summary(result: &Value) -> Result<SessionSummary, String> {
    let entry = result.get("session").cloned().unwrap_or(Value::Null);
    let entry =
        SessionIndexEntry::from_dict(&entry).map_err(|error| error.message().to_string())?;
    Ok(SessionSummary {
        session_id: entry.session_id,
        title: entry.title,
        message_count: entry.message_count as usize,
    })
}

/// 需要内核往返的宿主命令：命令层的接口是同步的，内核链路是异步帧，
/// 因此这几条由宿主先拦下、异步下发，响应到了再回填界面。
///
/// 拦下来的是**不能阻塞界面**的三条：`/undo` 与 `/compact` 会改会话与工作区，
/// `/review` 期间工具批次还要回到宿主执行（同步等待会死锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum KernelCommand {
    /// `/undo`：整轮回退由内核执行（会改会话与工作区）。
    Undo,
    /// `/compact`：压缩要调用摘要模型，不能让界面等。
    Compact,
    /// `/review`：派生评审子 Agent，`scope` 是可选 git 范围。
    Review { scope: String },
    /// `/review` 的第二段：把渲染好的报告注入内核上下文（内部后继命令）。
    ReviewInject,
    /// 会话回放：向内核索取有效事件流，用它重建对话视图（`replay_conversation`
    /// 标记与 `/undo` 回执都复用这一条；`fallback` 是请求失败时的消息投影，
    /// `notice` 是回放完成后才进消息流的命令提示）。
    Replay {
        fallback: Vec<Value>,
        notice: Option<String>,
    },
}

impl KernelCommand {
    /// 已解析的命令 → 宿主命令；不需要内核往返的返回 `None`，照旧交命令层。
    fn from_parsed(parsed: &ParsedCommand) -> Option<Self> {
        match parsed.command.name.as_str() {
            "undo" => Some(Self::Undo),
            "compact" => Some(Self::Compact),
            "review" => Some(Self::Review {
                scope: parsed.args.trim().to_string(),
            }),
            _ => None,
        }
    }

    /// 这条命令要下发的协议方法；内部后继命令（[`Self::ReviewInject`]）没有入口。
    fn to_command(&self, text: &str) -> Result<Option<Command>, String> {
        Ok(match self {
            Self::Undo => Some(Command::TurnUndo),
            Self::Compact => {
                if !text.trim().trim_start_matches("/compact").trim().is_empty() {
                    return Err("参数错误：/compact。".to_string());
                }
                Some(Command::SessionCompact)
            }
            Self::Review { scope } => Some(Command::SubagentRun(SubagentRunParams {
                agent_type: "review".to_string(),
                description: REVIEW_TASK_DESCRIPTION.to_string(),
                prompt: build_review_task_prompt(scope),
            })),
            Self::ReviewInject => None,
            // 回放没有用户输入（不入命令层），只需一条协议请求。
            Self::Replay { .. } => Some(Command::SessionEvents),
        })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Undo => "撤销",
            Self::Compact => "压缩",
            Self::Review { .. } => "评审",
            Self::ReviewInject => "评审报告注入",
            Self::Replay { .. } => "会话回放",
        }
    }
    /// 执行期间的运行状态文案（对映命令层 `working_status`）。
    fn working_status(&self) -> &'static str {
        match self {
            Self::Undo => "正在撤销上一轮…",
            Self::Compact => "正在压缩上下文",
            Self::Review { .. } => "正在评审",
            Self::ReviewInject => "正在注入评审报告",
            Self::Replay { .. } => "正在重放会话…",
        }
    }

    /// 成功回执 → 界面消息。
    fn success_message(&self, result: &Value) -> String {
        match self {
            Self::Undo => undo_message(result),
            Self::Compact => compact_message(result),
            Self::Review { .. } => format_review_report(result_text(result, "output").as_str()),
            Self::ReviewInject => {
                if result
                    .get("appended")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "已把评审报告注入上下文，下一轮请求可见。".to_string()
                } else {
                    "评审报告未注入上下文（内容为空或写入失败）。".to_string()
                }
            }
            // 回放本身不产生提示：它的结果是对话视图。
            Self::Replay { .. } => String::new(),
        }
    }

    /// `/review` 回执里要注入内核上下文的报告（其余命令返回 `None`）。
    ///
    /// 注入的是**渲染后**的报告（与 Python `remember_review_report(rendered)` 同口径）：
    /// 模型看到的是可读结论，而不是原始 JSON。
    fn review_report(&self, result: &Value) -> Option<String> {
        match self {
            Self::Review { .. } => {
                Some(format_review_report(result_text(result, "output").as_str()))
            }
            _ => None,
        }
    }

    /// 回执里要重放的内核历史（`/undo` 用：撤回的消息与工具卡不能留在视图里）。
    fn replay_history(&self, result: &Value) -> Option<Vec<Value>> {
        match self {
            Self::Undo => result.get("history").and_then(Value::as_array).cloned(),
            _ => None,
        }
    }

    /// 回执里的有效事件流（`session.events` 用）。
    fn replay_events(&self, result: &Value) -> Option<Vec<Value>> {
        match self {
            Self::Replay { .. } => result.get("events").and_then(Value::as_array).cloned(),
            _ => None,
        }
    }
}

/// 取字符串字段；缺失或不是字符串时为空串（避免把 JSON 拼进文案）。
fn result_text(result: &Value, key: &str) -> String {
    result
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 表单字段的文本取值；字段缺失时为空串（保存时由校验兜住）。
fn field_text(values: &[FieldValue], index: usize) -> String {
    values
        .get(index)
        .map(|value| value.text().to_string())
        .unwrap_or_default()
}

/// 表单字段的正整数取值（对映 Python 的 `_read_positive_int`）。
fn positive_int(values: &[FieldValue], index: usize, label: &str) -> Result<i64, String> {
    let raw = field_text(values, index);
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是正整数。"))?;
    if value <= 0 {
        return Err(format!("设置未完成：{label}必须是正整数。"));
    }
    Ok(value)
}

/// 表单字段的非负整数取值（0 合法，如「NER 缓存容量 0 关闭」）。
fn non_negative_int(values: &[FieldValue], index: usize, label: &str) -> Result<i64, String> {
    let raw = field_text(values, index);
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是整数。"))?;
    if value < 0 {
        return Err(format!("设置未完成：{label}不能为负数。"));
    }
    Ok(value)
}

/// 表单字段的小数取值（重复率阈值、熵兜底阈值这类 0～1 / 0～8 的比例）。
fn field_float(values: &[FieldValue], index: usize, label: &str) -> Result<f64, String> {
    field_text(values, index)
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是数字。"))
}

/// 表单字段的开关取值。
fn field_flag(values: &[FieldValue], index: usize) -> bool {
    values.get(index).map(|value| value.flag()).unwrap_or(false)
}

/// 表单字段的文本取值；空串回落到默认值（对映 Python 的 `_read_input(fallback=...)`）。
fn field_text_or(values: &[FieldValue], index: usize, fallback: &str) -> String {
    let text = field_text(values, index).trim().to_string();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

/// 表单字段的分隔列表取值：切分后去掉空白与空项（对映 Python 的 `_read_keys`）。
fn field_list(values: &[FieldValue], index: usize, separator: char) -> Vec<String> {
    field_text(values, index)
        .split(separator)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 按渠道 key 取渠道记录；配置里没有这条渠道时返回 `None`。
fn channel_for_key(environment: &ConfigEnvironment, key: &str) -> Option<ChannelConfig> {
    if key.trim().is_empty() {
        return None;
    }
    load_channel_configuration(environment, None, None)
        .ok()?
        .channels
        .into_iter()
        .find(|channel| channel.key == key)
}

/// 顾问设置页的字段初值；顺序与 `FormKind::Advisor` 的字段表一致。
fn advisor_form_values(config: &AdvisorConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.display_effort()),
        FieldValue::Text(config.model_key.clone()),
    ]
}

/// 工具输出压缩页的字段初值；顺序与 `FormKind::ToolOutputCompression` 的字段表一致。
fn compression_form_values(config: &ToolOutputCompressionConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.thinking_enabled),
        FieldValue::Text(config.reasoning_effort.clone()),
        FieldValue::Text(config.min_chars.to_string()),
        FieldValue::Text(config.max_input_chars.to_string()),
        FieldValue::Text(config.max_output_chars.to_string()),
        FieldValue::Text(config.timeout_seconds.to_string()),
        FieldValue::Text(config.model_key.clone()),
    ]
}

/// 消息脱敏页的字段初值；顺序与 `FormKind::Desensitization` 的字段表一致。
fn desensitization_form_values(config: &DesensitizationConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.fail_closed),
        FieldValue::Flag(config.strict_restore),
        FieldValue::Flag(config.entropy_enabled),
        FieldValue::Flag(config.entropy_pure_letters),
        FieldValue::Flag(config.entropy_pure_digits),
        FieldValue::Text(config.entropy_min_length.to_string()),
        FieldValue::Text(config.entropy_min_bits.to_string()),
        FieldValue::Text(config.extra_sensitive_keys.join(", ")),
        FieldValue::Text(config.exempt_keys.join(", ")),
        FieldValue::Flag(config.detect_pem_private_key),
        FieldValue::Flag(config.detect_db_connection_string),
        FieldValue::Flag(config.detect_email),
        FieldValue::Flag(config.detect_bank_card),
        FieldValue::Flag(config.detect_internal_ip),
        FieldValue::Flag(config.detect_external_ip),
        FieldValue::Flag(config.detect_url),
        FieldValue::Flag(config.detect_mac_address),
        FieldValue::Flag(config.detect_license_plate),
        FieldValue::Flag(config.gitleaks_enabled),
        FieldValue::Text(config.gitleaks_config_path.clone()),
        FieldValue::Flag(config.ner_enabled),
        FieldValue::Text(config.ner_device.clone()),
        FieldValue::Text(config.ner_model_path.clone()),
        FieldValue::Text(config.ner_entity_types.join(", ")),
        FieldValue::Text(config.ner_min_entity_chars.to_string()),
        FieldValue::Text(config.ner_cache_size.to_string()),
    ]
}

/// 持续运转页的字段初值；顺序与 `FormKind::RunGuard` 的字段表一致。
fn run_guard_form_values(config: &RunGuardConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.guard.enabled),
        FieldValue::Text(config.guard.window_chars.to_string()),
        FieldValue::Text(config.guard.substr_len.to_string()),
        FieldValue::Text(config.guard.repeat_ratio.to_string()),
        FieldValue::Text(config.guard.check_every.to_string()),
        FieldValue::Text(config.guard.max_blocks.to_string()),
        FieldValue::Text(config.guard.max_chars.to_string()),
        FieldValue::Text(config.guard.max_guard_retries.to_string()),
        FieldValue::Text(config.guard.auto_retry_errors.join(", ")),
        FieldValue::Flag(config.continuation.enabled),
        FieldValue::Text(config.continuation.max_auto_followups.to_string()),
    ]
}

/// 隔离工作区页的字段初值；顺序与 `FormKind::AgentWorkspace` 的字段表一致。
fn agent_workspace_form_values(config: &AgentWorkspaceConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.mode.clone()),
        FieldValue::Text(config.base_ref.clone()),
        FieldValue::Flag(config.detached),
        FieldValue::Flag(config.sync_uncommitted),
        FieldValue::Flag(config.apply_on_exit),
        FieldValue::Text(config.cleanup_on_exit.clone()),
        FieldValue::Text(config.copy_dirs.join(", ")),
        FieldValue::Text(config.env_scripts.join("; ")),
    ]
}

/// 图像生成页的字段初值；顺序与 `FormKind::ImageGen` 的字段表一致。
///
/// `api_key` 不进界面：编辑期间按磁盘原值保留，界面只显示凭据的环境变量名。
fn image_gen_form_values(config: &ImageGenConfiguration) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.base_url.clone()),
        FieldValue::Text(config.api_key_env.clone()),
        FieldValue::Text(config.model.clone()),
        FieldValue::Text(config.size.clone()),
        FieldValue::Text(config.quality.clone()),
        FieldValue::Text(config.output_format.clone()),
        FieldValue::Text(config.n.to_string()),
        FieldValue::Text(config.timeout_seconds.to_string()),
    ]
}

/// 配置侧的模型引用 → 视觉设置页的投影。
fn vision_ref_from_config(reference: &ActiveModelRef) -> VisionModelRef {
    VisionModelRef {
        source: reference.source.clone(),
        key: reference.key.clone(),
        profile: reference.profile.clone(),
        model_id: reference.model_id.clone(),
        protocol: reference.protocol.clone(),
    }
}

/// 界面投影 → 配置侧的模型引用。
fn vision_ref_to_config(reference: &VisionModelRef) -> ActiveModelRef {
    ActiveModelRef {
        source: reference.source.clone(),
        key: reference.key.clone(),
        profile: reference.profile.clone(),
        model_id: reference.model_id.clone(),
        protocol: reference.protocol.clone(),
    }
}

/// 模型原生视觉的写入范围：自定义模型写 models.toml，其余写渠道 Profile
/// （对映 Python 的 `_native_write_target`）。
fn vision_native_target(llm: &LlmConfig) -> (&'static str, String) {
    let catalog_key = llm.catalog_key.trim();
    if !catalog_key.is_empty() {
        return ("model", catalog_key.to_string());
    }
    ("channel", llm.profile_id.trim().to_string())
}

/// 子任务设置页的行：总开关 + 六个高级参数（对映 Python 的 `_SubagentsPane.sections`）。
fn subagent_rows(config: &SubAgentConfig) -> Vec<SubagentRow> {
    let mut rows = vec![SubagentRow {
        key: "enabled".to_string(),
        label: "功能总开关".to_string(),
        value: if config.enabled {
            "已开启"
        } else {
            "已关闭"
        }
        .to_string(),
        toggle: true,
        active: config.enabled,
        section: "子任务功能",
    }];
    let current: [(&str, i64); 6] = [
        ("max_concurrency", config.max_concurrency),
        ("max_tasks_per_batch", config.max_tasks_per_batch),
        (
            "default_timeout_seconds",
            config.default_timeout_seconds as i64,
        ),
        (
            "model_request_concurrency",
            config.model_request_concurrency,
        ),
        (
            "verify_command_timeout_seconds",
            config.verify_command_timeout_seconds,
        ),
        ("task_retention_minutes", config.task_retention_minutes),
    ];
    for (key, value) in current {
        let Some(spec) = SUBAGENT_ADVANCED_SPECS.iter().find(|spec| spec.key == key) else {
            continue;
        };
        rows.push(SubagentRow {
            key: key.to_string(),
            label: spec.label.to_string(),
            value: value.to_string(),
            toggle: false,
            active: false,
            section: "高级参数",
        });
    }
    rows
}

/// 功能开关的中文名（对映 Python 的 `_FEATURES` 表）。
fn feature_label(key: &str) -> String {
    match key {
        "memory" => "记忆功能".to_string(),
        "plugins" => "插件功能".to_string(),
        other => other.to_string(),
    }
}

/// 配置里的渠道视图 → 界面行。
fn channel_row_from_config(channel: &ChannelConfig) -> ChannelRow {
    ChannelRow {
        key: channel.key.clone(),
        profile_id: channel.profile_id.clone(),
        name: channel.name.clone(),
        provider: channel.provider.clone(),
        protocol: channel.protocol.clone(),
        base_url: channel.base_url.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        api_key_env: channel.api_key_env.clone(),
        model_id: channel.model_id.clone(),
        user_agent: channel.user_agent.clone(),
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置渠道。
///
/// `api_key` 只在面板里被改过（非空）时才是新值；留空表示「不改动」，由调用方按 key
/// 从磁盘上的同一条渠道继承（否则会把已配置的密钥抹掉）。
fn channel_config_from_row(row: &ChannelRow) -> ChannelConfig {
    ChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        profile_id: row.profile_id.trim().to_string(),
        provider: row.provider.trim().to_string(),
        protocol: row.protocol.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model_id: row.model_id.trim().to_string(),
        enabled: row.enabled,
        api_key_env: row.api_key_env.trim().to_string(),
        user_agent: row.user_agent.trim().to_string(),
    }
}

/// 当前生效的上下文窗口：显式 CLI/环境值优先，其次配置文件。
fn effective_context_window(explicit: Option<u64>, llm: &LlmConfig) -> i64 {
    match explicit {
        Some(value) if value > 0 => value as i64,
        _ => llm.context_window_tokens,
    }
}

/// 生成选项（`GenerationOptions` 的 JSON 形状）：只给配置里真正有值的字段。
///
/// 内核按 `GenerationOptions` 解析，结构体带 `#[serde(default)]`，缺字段取默认值，
/// 因此这里不必把每个字段都填满。
fn generation_options(llm: &LlmConfig) -> Value {
    let mut options = serde_json::Map::new();
    if !llm.reasoning_effort.trim().is_empty() {
        options.insert(
            "reasoning_effort".to_string(),
            Value::String(llm.reasoning_effort.clone()),
        );
    }
    if llm.max_output_tokens > 0 {
        options.insert(
            "max_output_tokens".to_string(),
            json!(llm.max_output_tokens),
        );
    }
    if let Some(temperature) = llm.temperature {
        options.insert("temperature".to_string(), json!(temperature));
    }
    if llm.request_timeout_seconds > 0 {
        options.insert(
            "request_timeout_seconds".to_string(),
            json!(llm.request_timeout_seconds as f64),
        );
    }
    if llm.request_retry_count > 0 {
        options.insert(
            "request_retry_count".to_string(),
            json!(llm.request_retry_count as u32),
        );
    }
    if !llm.provider_options.is_empty() {
        // `provider_options` 在配置侧是 TOML 表（`toml::Table`），而协议只认 JSON。
        // 走 serde 序列化而不手写逐类型转换，避免漏掉日期、数组与嵌套表分支。
        match serde_json::to_value(&llm.provider_options) {
            Ok(value) if value.is_object() => {
                options.insert("provider_options".to_string(), value);
            }
            _ => {}
        }
    }
    Value::Object(options)
}

/// 工具开关行：按 `TOOL_SWITCH_KEYS` 的顺序，注册状态来自当前工具表。
/// MCP 配置 → 设置面板初值。
fn mcp_values_from_config(config: &omnicrawl_mcp::config::McpConfig) -> McpSettingsValues {
    McpSettingsValues {
        enabled: config.enabled,
        allow_external_network_tools: config.policy.allow_external_network_tools,
        require_confirmation_for_write: config.policy.require_confirmation_for_write,
        require_confirmation_for_command: config.policy.require_confirmation_for_command,
        audit_log_enabled: config.policy.audit_log_enabled,
        timeout_seconds: config.default_timeout_seconds,
        servers: config
            .servers
            .iter()
            .map(|(_, server)| McpServerRow {
                name: server.name.clone(),
                enabled: server.enabled,
                transport: server.transport.clone(),
                risk_level: server.risk_level.clone(),
            })
            .collect(),
    }
}

/// MCP 配置（内核/客户端的形态）→ 可写回配置（`omnicrawl-config` 的形态）。
fn mcp_config_data(config: &omnicrawl_mcp::config::McpConfig) -> McpConfigData {
    McpConfigData {
        enabled: config.enabled,
        default_timeout_seconds: config.default_timeout_seconds,
        servers: config
            .servers
            .iter()
            .map(|(_, server)| {
                let mut env = omnicrawl_config::toml::Table::new();
                for (key, value) in &server.env {
                    env.insert(
                        key.clone(),
                        omnicrawl_config::toml::Value::String(value.clone()),
                    );
                }
                let mut headers = omnicrawl_config::toml::Table::new();
                for (key, value) in &server.headers {
                    headers.insert(
                        key.clone(),
                        omnicrawl_config::toml::Value::String(value.clone()),
                    );
                }
                (
                    server.name.clone(),
                    McpServerData {
                        enabled: server.enabled,
                        transport: server.transport.clone(),
                        command: server.command.clone().unwrap_or_default(),
                        args: server.args.clone(),
                        url: server.url.clone().unwrap_or_default(),
                        env,
                        headers,
                        timeout_seconds: server.timeout_seconds,
                        risk_level: server.risk_level.clone(),
                    },
                )
            })
            .collect(),
        policy: McpPolicyData {
            require_confirmation_for_write: config.policy.require_confirmation_for_write,
            require_confirmation_for_command: config.policy.require_confirmation_for_command,
            allow_external_network_tools: config.policy.allow_external_network_tools,
            audit_log_enabled: config.policy.audit_log_enabled,
        },
    }
}

/// 编辑草稿 → 可写回的 Server 数据。
///
/// 环境变量与请求头在界面上是 `KEY=VALUE;…`：留空表示保持原值（凭据不在界面回显，
/// 因此不能因为一次编辑把它们抹掉）；参数按 `shlex.split(posix=True)` 切分，
/// 与 Python 编辑器同口径。
fn mcp_server_data(
    draft: &McpServerDraft,
    previous: Option<&McpServerData>,
) -> Result<McpServerData, String> {
    let transport = if draft.transport.trim().is_empty() {
        "stdio".to_string()
    } else {
        draft.transport.trim().to_string()
    };
    let args = omnicrawl_controllers::subagents::shlex_split(draft.args.trim(), true)
        .unwrap_or_else(|_| draft.args.split_whitespace().map(str::to_string).collect());
    let env = parse_key_value_list(&draft.env, previous.map(|server| &server.env), "环境变量")?;
    let headers = parse_key_value_list(
        &draft.headers,
        previous.map(|server| &server.headers),
        "请求头",
    )?;
    let risk_level = if draft.risk_level.trim().is_empty() {
        "restricted".to_string()
    } else {
        draft.risk_level.trim().to_string()
    };
    Ok(McpServerData {
        enabled: draft.enabled,
        transport,
        command: draft.command.trim().to_string(),
        args,
        url: draft.url.trim().to_string(),
        env,
        headers,
        timeout_seconds: draft.timeout_seconds,
        risk_level,
    })
}

/// 解析 `KEY=VALUE;KEY2=VALUE` 形式的表；留空保持原值，格式错时拒绝保存。
fn parse_key_value_list(
    text: &str,
    previous: Option<&omnicrawl_config::toml::Table>,
    label: &str,
) -> Result<omnicrawl_config::toml::Table, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(previous.cloned().unwrap_or_default());
    }
    let mut table = omnicrawl_config::toml::Table::new();
    for item in trimmed.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((key, value)) = item.split_once('=') else {
            return Err(format!("设置未完成：{label}格式必须是 KEY=VALUE。"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("设置未完成：{label}格式必须是 KEY=VALUE。"));
        }
        table.insert(
            key.to_string(),
            omnicrawl_config::toml::Value::String(value.to_string()),
        );
    }
    Ok(table)
}

fn tool_switch_rows(registry: &ToolRegistry) -> Vec<ToolSwitchRow> {
    let environment = ConfigEnvironment::from_process();
    let switches = load_tool_switches(&environment, None).unwrap_or_default();
    let registered = registered_tool_names(registry);
    TOOL_SWITCH_KEYS
        .iter()
        .map(|name| ToolSwitchRow {
            name: (*name).to_string(),
            label: tool_label(name),
            enabled: switches.get(*name).copied().unwrap_or(true),
            registered: registered.contains(*name),
        })
        .collect()
}

/// 已注册进工具表的工具名（声明里的 `function.name`）。
fn registered_tool_names(registry: &ToolRegistry) -> std::collections::BTreeSet<String> {
    registry
        .declarations()
        .iter()
        .filter_map(|declaration| {
            declaration
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// 工具开关的中文名；未知名字回落为工具名本身。
fn tool_label(name: &str) -> String {
    TOOL_SWITCH_LABELS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| name.to_string())
}

/// 上下文设置对应的内核压缩配置（阈值 Token 由窗口与百分比换算）。
fn compaction_settings(window: i64, percent: i64) -> KernelCompactionConfig {
    KernelCompactionConfig {
        trigger_context_tokens: Some(context_compaction_trigger_tokens(window, percent)),
        context_window_tokens: Some(window),
        ..KernelCompactionConfig::default()
    }
}

/// 压缩成功回执 → 界面消息（与命令层 `handle_compact_command` 的收尾文案一致）。
fn compact_message(result: &Value) -> String {
    let compacted = result["compacted"].as_bool().unwrap_or(true);
    if !compacted {
        return "当前会话不需要压缩（上下文里还没有可压缩的轮次）。".to_string();
    }
    "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。".to_string()
}

/// 把内核的撤销结果渲染成一行界面消息。
fn undo_message(result: &Value) -> String {
    let count = result["message_count"].as_u64().unwrap_or_default();
    let mut message = match result["kind"].as_str().unwrap_or_default() {
        "incomplete" => format!("已撤销最近一轮的未完成部分（回退 {count} 条消息）"),
        _ => format!("已撤销最近一轮（回退 {count} 条消息）"),
    };
    if result["side_effects_reverted"] == Value::Bool(true) {
        message.push_str("，工作区已恢复");
    }
    let unrestorable: Vec<&str> = result["unrestorable"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !unrestorable.is_empty() {
        message.push_str(&format!(
            "；以下文件没有内容副本、未能恢复：{}",
            unrestorable.join("、")
        ));
    }
    message.push('。');
    message
}

/// 可用角色名：只有内核侧 SubAgent 启用时才有；宿主据此把 `subagent` 声明给模型。
fn subagent_role_names() -> Vec<String> {
    let env = ConfigEnvironment::from_process();
    let Ok(config) = load_subagent_config(&env, None) else {
        return Vec::new();
    };
    if !config.enabled {
        return Vec::new();
    }
    let builtin = match env.get("OMNICRAWL_SUBAGENTS_DIR") {
        Some(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => env.home().join(".OmniCrawl").join("agents"),
    };
    let mut registry = AgentDefinitionRegistry::new(builtin, Some(env.home().to_path_buf()));
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    registry.discover(&workspace, &[]);
    registry
        .list_all()
        .iter()
        .map(|item| item.name.clone())
        .collect()
}

/// TTS 工具配置：读 config.toml 的 `[tts]` 段，未启用时不给这个工具（与 Python 一致）。
fn tts_options(workspace: &Path) -> Option<Arc<TtsOptions>> {
    let environment = ConfigEnvironment::from_process();
    let configuration = match load_tts_configuration(&environment, None) {
        Ok(configuration) => configuration,
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("TTS 配置读取失败，已跳过语音合成：{error}"),
            );
            return None;
        }
    };
    tts_options_from(configuration, workspace)
}

/// 把 `[tts_api]` 段映射成接口后端配置（密钥在这里解析）；未启用时返回 `None`（走本地推理）。
fn tts_api_backend(environment: &ConfigEnvironment) -> Option<ApiTtsConfig> {
    let configuration = match load_tts_api_configuration(environment, None) {
        Ok(configuration) => configuration,
        Err(error) => {
            // 读不出来时按「接口未启用」处理：后面会落到本地推理，用户在日志里能看到真正原因。
            report_startup_log(
                LogLevel::Warning,
                &format!("TTS 接口配置读取失败，已改按本地推理处理：{error}"),
            );
            return None;
        }
    };
    if !configuration.enabled {
        return None;
    }
    // 先解析密钥再搬字段：`resolve_api_key` 要借整份配置。
    let api_key = configuration.resolve_api_key(environment);
    Some(ApiTtsConfig {
        base_url: configuration.base_url,
        api_key,
        model: configuration.model,
        voice: configuration.voice,
        response_format: configuration.response_format,
        speed: configuration.speed,
        timeout_seconds: configuration.timeout_seconds.max(1) as u64,
    })
}

/// 语速的界面显示：整数去掉小数点（`1.0` → `1`），其余保留原样（`1.25`）。
fn format_speed(speed: f64) -> String {
    if speed.fract().abs() < f64::EPSILON {
        format!("{}", speed as i64)
    } else {
        format!("{speed}")
    }
}

/// 用一份已读出的配置构造 TTS 工具选项；未启用时不给工具。
fn tts_options_from(configuration: TtsConfiguration, workspace: &Path) -> Option<Arc<TtsOptions>> {
    if !configuration.enabled {
        return None;
    }
    let environment = ConfigEnvironment::from_process();
    let model_dir = configuration.resolved_model_dir(&environment);
    let api = tts_api_backend(&environment);
    Some(Arc::new(TtsOptions::new(
        configuration,
        api,
        model_dir,
        workspace.to_path_buf(),
    )))
}

/// 克隆一条音色：用参考音频编码出 prompt audio codes 并写入自定义音色库。
fn clone_tts_voice(
    configuration: &TtsConfiguration,
    model_dir: &Path,
    voice: &str,
    audio: &Path,
) -> Result<(), String> {
    let config = TtsConfig {
        model_dir: Some(model_dir.to_path_buf()),
        thread_count: configuration.thread_count,
        device: Some(configuration.device.clone()),
        ..TtsConfig::default()
    };
    let mut engine = TtsEngine::new(config)?;
    let result = engine.clone_voice(voice, audio, "");
    engine.close();
    result.map(|_| ())
}

/// 装配 MCP 管理器：读配置、发现能力，失败只警告不阻断启动。
///
/// 协议 v1 的工具声明固定在 `initialize`，所以能力发现必须发生在握手之前——
/// Python 侧是「首次使用时懒加载」，Rust 宿主把它提前到启动期（见 crate README）。
fn mcp_manager(workspace: &Path) -> Option<Arc<McpClientManager>> {
    let config = match load_mcp_config(&ConfigEnvironment::from_process(), None) {
        Ok(config) => config,
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("MCP 配置读取失败，已跳过 MCP：{error}"),
            );
            return None;
        }
    };
    if !config.enabled {
        return None;
    }
    let manager = Arc::new(McpClientManager::new(config, workspace));
    manager.discover();
    for diagnostic in manager.diagnostics() {
        let prefix = diagnostic
            .server_name
            .as_deref()
            .map(|name| format!("{name}: "))
            .unwrap_or_default();
        report_startup_log(
            LogLevel::Warning,
            &format!(
                "MCP [{}] {prefix}{}",
                diagnostic.severity, diagnostic.message
            ),
        );
    }
    Some(manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::models::llm::provider_options_from_json;

    #[test]
    fn api_key_env_follows_channel_then_provider_default() {
        // 命令行渠道（`--base-url`）：用命令行那个名字。
        assert_eq!(
            effective_api_key_env(true, "openai", "OPENAI_API_KEY", "MY_KEY"),
            "MY_KEY"
        );
        // 配置渠道：优先渠道里的名字。
        assert_eq!(
            effective_api_key_env(false, "openai", "CHANNEL_KEY", "OPENAI_API_KEY"),
            "CHANNEL_KEY"
        );
        // 渠道里没写名字：退回 Provider 默认名（不能退回空串：内核只看这个名字）。
        assert_eq!(
            effective_api_key_env(false, "anthropic", "  ", "OPENAI_API_KEY"),
            "ANTHROPIC_API_KEY"
        );
        assert_eq!(
            effective_api_key_env(false, "gemini", "", "OPENAI_API_KEY"),
            "GEMINI_API_KEY"
        );
        assert_eq!(
            effective_api_key_env(true, "", "", ""),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn undo_message_reports_rollback_and_workspace_restore() {
        let message = undo_message(&json!({
            "kind": "complete",
            "message_count": 4,
            "side_effects_reverted": true,
            "unrestorable": [],
        }));

        assert_eq!(message, "已撤销最近一轮（回退 4 条消息），工作区已恢复。");
    }

    #[test]
    fn undo_message_reports_unrestorable_files() {
        let message = undo_message(&json!({
            "kind": "incomplete",
            "message_count": 2,
            "side_effects_reverted": false,
            "unrestorable": ["a.txt", "b/c.txt"],
        }));

        assert_eq!(
            message,
            "已撤销最近一轮的未完成部分（回退 2 条消息）；以下文件没有内容副本、未能恢复：a.txt、b/c.txt。"
        );
    }

    #[test]
    fn kernel_commands_route_by_registry_name() {
        let parsed = |text: &str| command_registry().parse(text).expect("命令应当可解析");
        assert_eq!(
            KernelCommand::from_parsed(&parsed("/undo")),
            Some(KernelCommand::Undo)
        );
        assert_eq!(
            KernelCommand::from_parsed(&parsed("/compact")),
            Some(KernelCommand::Compact)
        );
        // `/review` 的 git 范围原样带上；范围是空串时用默认范围（由命令层构造提示词）。
        assert_eq!(
            KernelCommand::from_parsed(&parsed("/review HEAD~3")),
            Some(KernelCommand::Review {
                scope: "HEAD~3".to_string()
            })
        );
        assert_eq!(
            KernelCommand::from_parsed(&parsed("/review")),
            Some(KernelCommand::Review {
                scope: String::new()
            })
        );
        // 其余命令走命令层（不在宿主侧拦），名字对不上就不拦。
        assert_eq!(KernelCommand::from_parsed(&parsed("/settings")), None);
        assert_eq!(KernelCommand::from_parsed(&parsed("/sessions")), None);
        assert_eq!(
            KernelCommand::Undo.success_message(&json!({"message_count": 2})),
            "已撤销最近一轮（回退 2 条消息）。"
        );
    }

    #[test]
    fn kernel_commands_map_to_protocol_methods() {
        assert_eq!(
            KernelCommand::Undo.to_command("/undo").expect("撤销无参数"),
            Some(Command::TurnUndo)
        );
        // `/compact` 的非法参数在宿主侧就拦下，不下发内核。
        assert!(KernelCommand::Compact.to_command("/compact now").is_err());
        assert_eq!(
            KernelCommand::Compact
                .to_command("/compact")
                .expect("无参数合法"),
            Some(Command::SessionCompact)
        );
        let review = KernelCommand::Review {
            scope: "HEAD~3".to_string(),
        };
        match review.to_command("/review HEAD~3").expect("评审无参数校验") {
            Some(Command::SubagentRun(params)) => {
                assert_eq!(params.agent_type, "review");
                assert_eq!(params.description, REVIEW_TASK_DESCRIPTION);
                assert!(params.prompt.contains("HEAD~3"), "范围要进任务提示词");
            }
            other => panic!("评审应当派生 subagent.run：{other:?}"),
        }
        // 内部后继命令没有下发入口（它的请求由 `/review` 的回执直接发出）。
        assert_eq!(
            KernelCommand::ReviewInject
                .to_command("/review")
                .expect("不是参数错误"),
            None
        );
    }

    #[test]
    fn review_reply_renders_and_undo_reply_replays() {
        let review = KernelCommand::Review {
            scope: String::new(),
        };
        let result = json!({
            "output": "{\"overall_correctness\":\"correct\",\"findings\":[]}"
        });
        assert!(review.success_message(&result).contains("patch is correct"));
        assert!(review.review_report(&result).is_some(), "评审要注入上下文");
        assert!(
            KernelCommand::Undo.review_report(&result).is_none(),
            "只有 /review 有报告注入这一段"
        );

        let undo = json!({
            "message_count": 1,
            "history": [{"role": "user", "content": "你好"}]
        });
        assert_eq!(
            KernelCommand::Undo
                .replay_history(&undo)
                .map(|items| items.len()),
            Some(1)
        );
        assert!(review.replay_history(&undo).is_none());
        // 注入回执的文案要区分成功与空内容。
        assert!(KernelCommand::ReviewInject
            .success_message(&json!({"appended": true}))
            .contains("已把评审报告注入上下文"));
        assert!(KernelCommand::ReviewInject
            .success_message(&json!({"appended": false}))
            .contains("未注入"));
    }

    #[test]
    fn compact_message_reports_both_outcomes() {
        assert_eq!(
            KernelCommand::Compact.success_message(&json!({"compacted": true})),
            "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。"
        );
        assert!(
            KernelCommand::Compact
                .success_message(&json!({"compacted": false}))
                .contains("不需要压缩"),
            "没有可压缩轮次时要如实说明"
        );
    }

    #[test]
    fn tool_label_uses_config_table_then_falls_back() {
        assert_eq!(tool_label("read"), "读取文件内容");
        assert_eq!(tool_label("powershell"), "执行 PowerShell 命令");
        assert_eq!(tool_label("自定义工具"), "自定义工具");
    }

    #[test]
    fn explicit_context_window_wins_over_config() {
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let llm = LlmConfig::with_environment(&environment);

        assert_eq!(effective_context_window(Some(64_000), &llm), 64_000);
        assert_eq!(
            effective_context_window(None, &llm),
            llm.context_window_tokens,
            "没有显式值时用配置里的窗口"
        );
        assert_eq!(
            effective_context_window(Some(0), &llm),
            llm.context_window_tokens,
            "0 不是有效窗口，按未给出处理"
        );
    }

    #[test]
    fn compaction_settings_convert_percent_to_tokens() {
        let settings = compaction_settings(200_000, 80);
        assert_eq!(settings.trigger_context_tokens, Some(160_000));
        assert_eq!(settings.context_window_tokens, Some(200_000));
        assert_eq!(
            settings.recent_turns, None,
            "只带本次要改的字段，其余交给内核保留原值"
        );
    }

    #[test]
    fn generation_options_carry_only_configured_fields() {
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let mut llm = LlmConfig::with_environment(&environment);
        llm.reasoning_effort = "high".to_string();
        llm.max_output_tokens = 0;
        llm.temperature = None;
        llm.request_timeout_seconds = 0;
        llm.request_retry_count = 3;
        llm.provider_options = provider_options_from_json(&json!({}));

        let options = generation_options(&llm);
        assert_eq!(options["reasoning_effort"], json!("high"));
        assert_eq!(options["request_retry_count"], json!(3));
        assert!(options.get("max_output_tokens").is_none(), "0 视为未配置");
        assert!(options.get("temperature").is_none());
        assert!(options.get("request_timeout_seconds").is_none());
        assert!(options.get("provider_options").is_none());
    }

    #[test]
    fn generation_options_keep_temperature_and_provider_options() {
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let mut llm = LlmConfig::with_environment(&environment);
        llm.reasoning_effort = String::new();
        llm.max_output_tokens = 4096;
        llm.temperature = Some(0.2);
        llm.request_timeout_seconds = 180;
        llm.request_retry_count = 0;
        llm.provider_options = provider_options_from_json(&json!({"top_k": 40}));

        let options = generation_options(&llm);
        assert_eq!(options["max_output_tokens"], json!(4096));
        assert_eq!(options["temperature"], json!(0.2));
        assert_eq!(options["request_timeout_seconds"], json!(180.0));
        assert_eq!(options["provider_options"]["top_k"], json!(40));
        assert!(options.get("reasoning_effort").is_none(), "空串视为未配置");
        assert!(options.get("request_retry_count").is_none());
    }
}
