//! 接线层：内核帧、用户按键、工具执行层与状态机之间的翻译。
//!
//! 工具执行放在独立线程上（一个调用一个线程，同批并发），主线程只跑事件循环：
//! 执行结果经通道回到主线程，再回填批次并按模型顺序回内核。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
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
use omnicrawl_config::core::runtime::{resolve_config_path, ConfigEnvironment};
use omnicrawl_config::core::settings::{
    load_feature_enabled, load_show_thinking, save_context_compaction_trigger_percent,
    save_context_window_tokens, save_feature_enabled, save_mcp_config, save_show_thinking,
    save_subagent_setting, McpConfigData, McpPolicyData, McpServerData,
};
use omnicrawl_config::core::workspace::save_workspace_root;
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
use omnicrawl_controllers::turn::skill_command::{
    parse_skill_command, skill_default_prompt, skill_loaded_status, skill_not_found_message,
    skill_not_found_status,
};
use omnicrawl_extensions::skill::{SkillDiagnostic, SkillMeta};

use omnicrawl_config::features::computer_use::{
    load_computer_use_config, save_computer_use_config, ComputerUseConfig, DEFAULT_THINKING_EFFORT,
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
use omnicrawl_config::features::tts_cloud::{
    load_tts_api_configuration, load_tts_cloud_configuration, load_tts_gemini_configuration,
    save_tts_api_configuration, save_tts_cloud_configuration, save_tts_gemini_configuration,
    GeminiTtsConfiguration, TtsApiConfiguration, TtsCloudConfiguration,
};
use omnicrawl_config::features::web_search::{
    load_web_search_configuration, save_web_search_configuration,
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
use omnicrawl_controllers::memory::user_data_root;
use omnicrawl_controllers::settings::{
    context_compaction_trigger_tokens, kernel_compaction_settings,
};
// `context.prune` 的送审形状在控制层：宿主只负责把它换算成裁决输入并回调用 ID。
use omnicrawl_config::features::decision_model::{
    load_decision_model_configuration, load_decision_switches, load_local_deployment,
    save_decision_model_configuration, save_decision_switch, save_local_deployment,
    DecisionChannelConfig, DecisionModelConfiguration, DecisionSwitches, LocalDeploymentConfig,
    SwitchPage, DECISION_MODE_ONEJEV, DECISION_SWITCHES, DECISION_SWITCH_KB_SEARCH,
    DECISION_SWITCH_MEMORY_SEARCH, DECISION_SWITCH_SKILL_SEARCH, DECISION_SWITCH_WEB_SEARCH,
    DEFAULT_DECISION_RETRY_COUNT, DEFAULT_LOCAL_DECISION_BASE_URL,
    DEFAULT_LOCAL_DECISION_CHANNEL_KEY,
};
use omnicrawl_config::features::embedding::{
    load_embedding_model_configuration, save_embedding_model_configuration, EmbeddingChannelConfig,
    EmbeddingModelConfiguration,
};
use omnicrawl_controllers::subagents::definitions::AgentDefinitionRegistry;
use omnicrawl_controllers::subagents::tasks::is_terminal;
use omnicrawl_controllers::turn::tool_prune::PruneCandidate;
use omnicrawl_controllers::vision_proxy::vision_proxy_configured;
use omnicrawl_controllers::workspace::{
    pending_worktrees_error, resolve_switch_target, subagent_drain_error, WorktreeRef,
};
use omnicrawl_controllers::AgentError;
use omnicrawl_core::diagnostics::{self, Level};
use omnicrawl_core::{ToolCall, ToolResult};
use omnicrawl_host::plugins::PluginHost;
use omnicrawl_host::prompt::{PromptOptions, PromptRuntime};
use omnicrawl_host::prompt_cache::build_prompt_cache_identity;
use omnicrawl_host::tools::evicted_call_ids_for;
use omnicrawl_host::tools::FetcherOptions;
use omnicrawl_ipc::{
    bridge::{
        Command, ContextPruneRequest, ContextPruneResult, HostEvent, InitializeParams,
        KernelCompactionConfig, KernelModelConfig, KernelSessionConfig, ModelHookRequest,
        ModelHookResult, SessionAppendParams, SessionHistoryParams, SessionListParams,
        SessionModelSettings, SessionRenameParams, SessionResumeParams, SessionSettingsParams,
        SubagentRunParams, ToolBatch,
    },
    error_code, Frame, Id, PROTOCOL_VERSION,
};
use omnicrawl_mcp::client::McpClientManager;
use omnicrawl_mcp::config::{load_mcp_config, valid_server_name};
use omnicrawl_onejev::paths::model_dir as onejev_model_dir;
use omnicrawl_onejev::{
    delete_model as delete_onejev_model, download_model as download_onejev_model,
    ensure_environment as ensure_onejev_environment, environment_state as onejev_environment_state,
    find_size as find_onejev_size, model_ready as onejev_model_ready,
    remove_environment as remove_onejev_env, resolve_root as resolve_onejev_root,
    LaunchSpec as OneJevLaunchSpec, OneJevServer, ServerState as OneJevServerState, ONEJEV_SIZES,
};
use omnicrawl_session::{PromptHistoryEntry, SessionIndexEntry};
use omnicrawl_version::USER_AGENT;
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, finalize_subagent_worktrees, IsolationSession,
};

use crate::args::{ApprovalMode, Options};
use crate::commands::{self, TuiHostAgent};
use crate::host::{self, BatchStep, Waiting};
use crate::kernel::KernelClient;
use crate::state::{AppState, PendingImage, Record};
use crate::tools::{
    tts_backend, AdvisorOptions, ImageGenOptions, MemoryOptions, RegistryOptions, ToolRegistry,
    TtsOptions,
};
use crate::ui;
use crate::ui::conversation::{self, LineHit};
use crate::ui::fullscreen::input::menu::{MenuAction, MenuKey};
use crate::ui::fullscreen::input::sessions_menu::SessionMenuItem;
use crate::ui::queue::{self, QueueHit};
use crate::ui::settings::{
    nearest_compaction_percent, reasoning_label, split_token, ChannelRow, DecisionLocalChange,
    DecisionLocalValues, DecisionRow, DecisionSwitchRow, EmbeddingRow, FieldValue, FormKind,
    McpChange, McpServerDraft, McpServerRow, McpSettingsValues, SettingsChange, SettingsEvent,
    SettingsState, SettingsValues, SubagentChange, SubagentRow, ToolSwitchRow, TtsChange,
    TtsChannelDraft, TtsChannelValues, TtsDraft, TtsValues, VisionChange, VisionModelRef,
    WebSearchRow, SUBAGENT_ADVANCED_SPECS,
};
use crate::ui::splash::{report_startup_log, LogLevel};

/// 握手响应最多等这么久；内核启动即刻回帧，卡住说明进程有问题。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 有活动（回合在跑、后台结果在途）时的等待上限：状态行 spinner 与工具耗时按 80ms 换帧，
/// 取 20ms 保证帧率充足，又不会在「回车 → 首字」区间白等一整个节拍。
const ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 真正空闲时的等待上限：没有任何东西会定时变（没有回合、没有动画、没有在途结果），
/// 键盘、鼠标与内核帧都会立刻唤醒 `event::poll`，因此这里可以放心久睡。
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 方向键滚动步长与翻页步长（行）。
const SCROLL_STEP: isize = 1;
const PAGE_STEP: isize = 10;
/// 鼠标滚轮一格滚动的行数（Textual 的滚轮一步也是按行推进，这里取同一观感）。
const WHEEL_STEP: isize = 3;

mod host_commands;
mod input;
mod kernel_flow;
mod onejev;
mod turn_flow;
mod workspace;
mod settings_apply;
mod settings_pages;
mod settings_view;
mod support;

mod kernel;

mod ticks;

mod settings;

mod lifecycle;

use support::*;
// 二进制入口与测试都按 `omnicrawl_tui::app::<name>` 取用这几个自由函数。
pub use support::is_ctrl_v_release;

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
        diagnostics::warn(format!(
            "[tui] {0} 被插件挡下（tool.call.before）：{error}",
            call.name
        ));
        return Err(format!("插件拒绝工具调用：{}。", call.name));
    }
    if let Err(error) =
        plugins.tool_approval_before(&call.name, &arguments, requires_confirmation, mode)
    {
        diagnostics::warn(format!(
            "[tui] {0} 被插件挡下（tool.approval.before）：{error}",
            call.name
        ));
        return Err(format!("插件在审批前拒绝：{}。", call.name));
    }
    if let Err(error) = plugins.tool_execute_before(&call.name, &arguments) {
        diagnostics::warn(format!(
            "[tui] {0} 被插件挡下（tool.execute.before）：{error}",
            call.name
        ));
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
    let agent_temp_dir = resolve_agent_temp_dir_display(environment, workspace);
    load_prompt_runtime_with_temp(environment, options, workspace, &agent_temp_dir)
}

/// 提示词里的临时目录展示路径：优先用 `[agent_temp] directory` 的实际落点。
///
/// 工作区切换时要按新工作区重算，因此这里单独成函数，而不是沿用旧工作区的句柄。
fn resolve_agent_temp_dir_display(environment: &ConfigEnvironment, workspace: &Path) -> String {
    let config = omnicrawl_workspace::load_agent_temp_workspace_config(environment, None)
        .unwrap_or_default();
    if !config.enabled {
        return omnicrawl_host::prompt::DEFAULT_AGENT_TEMP_DIR.to_string();
    }
    let resolved = omnicrawl_workspace::resolve_agent_temp_dir(workspace, &config.directory)
        .unwrap_or_else(|_| workspace.join(&config.directory));
    match resolved.strip_prefix(workspace) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => resolved.to_string_lossy().to_string(),
    }
}

fn load_prompt_runtime_with_temp(
    environment: &ConfigEnvironment,
    options: &Options,
    workspace: &Path,
    agent_temp_dir: &str,
) -> Result<PromptRuntime, String> {
    let mut prompt_options = PromptOptions::new(workspace.to_path_buf());
    prompt_options.agent_temp_dir = agent_temp_dir.to_string();
    prompt_options.workspace_detection_summary =
        omnicrawl_config::core::context::detect_project_context(environment, None)
            .detection_summary();
    // system prompt 是**显式覆盖**才传：`None` 时 `PromptRuntime` 读
    // `rust/assets/templates/system_prompt.md` 模板（工作守则、工具协议、文档索引）。
    // 曾经这里无条件传 `Some(options.system_prompt)`，而它有个一行默认值兜底，
    // 于是模板永远被那句话顶掉——模型收到的是占位文案而不是真正的系统提示词。
    prompt_options.system_prompt_override = options.system_prompt.clone();
    prompt_options.advisor_active = options.advisor.enabled;
    prompt_options.advisor_blacklisted = options
        .advisor
        .disabled_for_models
        .iter()
        .any(|name| name == &options.model);
    match PromptRuntime::load(environment, prompt_options) {
        Ok(prompt) => Ok(prompt),
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("系统提示词装配失败，改用命令行给的文本：{error}"),
            );
            let mut fallback = PromptOptions::new(workspace.to_path_buf());
            fallback.agent_temp_dir = agent_temp_dir.to_string();
            fallback.system_prompt_override = options.system_prompt.clone();
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
    let _ = model;
    // 审查通道的选择（对话模型 / 结构化决策模型）由 host 侧按 `[decision_models.features]`
    // 统一判定，TUI 只做装配入口的转发，避免两侧各判一次。
    omnicrawl_host::review::review_options_from_config(llm, environment)
}

/// 内核 stderr 行 → 会话区提示文案；不是报错行时返回 `None`。
///
/// 内核把 `[kernel] 回合失败：…` 一类诊断写到 stderr；只有这些行值得占用会话流，
/// 常规信息（如「会话已就绪」）直接丢弃。
///
/// `panicked` 必须收：release 档是 `panic = "abort"`，内核 panic 会**直接结束进程**，
/// Rust 的 panic 首行既不带「失败/错误/异常」也没有任何中文提示词——按老口径过滤掉，
/// 用户就只能看到「内核进程已退出」而看不到原因（已发生过一次：脱敏层字节切片越界）。
fn kernel_error_notice(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let is_error = ["失败", "错误", "异常", "无法", "panicked", "RUST_BACKTRACE"]
        .iter()
        .any(|keyword| trimmed.contains(keyword));
    is_error.then(|| trimmed.to_string())
}

/// 内核进程消失时的收尾文案。
///
/// 措辞保持原样：提示行、插件 `turn.error` 与流程测试都按它检索。
const KERNEL_LOST_MESSAGE: &str = "内核进程已退出。";

/// 内核消失后常显在运行状态行的退出指引。
///
/// 写在状态行而不是提示行：用户滚动回看现场时，退出方式不会跟着滚走。
const KERNEL_LOST_STATUS: &str =
    "内核已退出：按 Enter / Esc 退出（会话已保存，可用 --resume 恢复）";

pub struct App {
    pub state: AppState,
    pub options: Options,
    pub kernel: KernelClient,
    pub quit: bool,
    /// 内核异常退出（连接断开）的闩锁。
    ///
    /// 以前这里直接 `quit = true`：`main` 画完那一帧就离屏退出，用户只看到界面一闪，
    /// 既看不到原因、也留不住现场。现在只收尾一次并把界面停在原处，等用户按键再退。
    pub kernel_lost: bool,
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
    /// 在途 `turn.submit` 的请求 id：它的响应帧带回合失败原因，必须接住。
    ///
    /// 不登记就会像以前一样被当成「无关响应」丢掉——重试耗尽后界面停在
    /// 「正在自动重试」正是这么来的。
    pending_turn_submit: Option<Id>,
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
    /// `/skill:<名>` 本轮的活动技能上下文消息：提交回合前先 `session.settings` 下发，
    /// 回合收尾后清掉（下一轮回到常规装配）。
    pending_skill_context: Option<Vec<Value>>,
    /// 本轮下发过活动技能正文：回合收尾要把上下文消息换回常规装配。
    restore_skill_context_after_turn: bool,
    /// 启动期插件加载诊断：由 `main.rs` 写进启动页日志框（不进对话流，避免顶掉首屏 Logo）。
    pub startup_plugin_lines: Vec<String>,
    /// Agent 临时目录句柄：创建分类子目录并跑清理调度。
    ///
    /// 必须活到进程退出：后台清理线程随句柄存在，提前丢弃等于没接。
    agent_temp: Option<omnicrawl_workspace::AgentTempInstallation>,
    /// 启动期自部署决策服务的处理结论（同上，走启动页日志框）。
    pub startup_service_lines: Vec<String>,
    /// 启动期解析出来的模型/渠道视图（config.toml + models.toml + 环境变量）。
    ///
    /// 握手与设置面板都读它：`initialize.model` 的 Provider/协议/生成选项/上下文窗口
    /// 与真实配置同源，不再是空壳；切换渠道后这里会被同步更新。
    llm: LlmConfig,
    /// 最近一帧的终端区域：鼠标命中判定与渲染共用同一套布局计算（[`ui::layout`]）。
    viewport: Rect,
    /// 上一帧实际画出去的「三块刷新版本号」与终端尺寸。
    ///
    /// `None` 表示还没画过任何一帧：首帧必须画，不能用「没变化」把自己跳过。
    drawn: Option<(ui::RefreshVersions, Rect)>,
    /// 插件运行期：与 API 同源（配置 → `PluginRuntime`），Hook 在回合与工具边界上分发。
    plugins: Arc<PluginHost>,
    /// 审查运行期（`approval.mode = review` 时用）：与 Python `_review_tool_call` 同源。
    ///
    /// 审查模型不可用（未配模型或凭据）时为 `None`，此时需审查的调用按 fail-closed 拒绝。
    review: Option<omnicrawl_host::review::ReviewOptions>,
    /// 提问托管运行期（`[decision_models.features] ask_user_custody`）：开启后有选项的提问
    /// 交给决策模型自动作答（选中「我有自己的想法...」就退回人工提问）；
    /// `None` 或不可用时一律退回人工提问（fail-open）。
    custody: Option<omnicrawl_host::tools::CustodyOptions>,
    /// 工具调用淘汰运行期（`[decision_models.features] tool_call_prune`）：开启后「刚变老」
    /// 的那一批调用交决策模型裁决，无用的整组移出上下文；`None` 或不可用时一律保留原文。
    ///
    /// 与 `options` 一样对外可见：用例需要把这一步钉成「裁决不可用」，否则结果会随本机
    /// 配置（开关开没开、决策服务在不在）漂移。
    pub prune: Option<omnicrawl_host::tools::PruneOptions>,
    /// 工具建议运行期（`[decision_models.features] tool_suggestion`）：开启后提交前先问一次决策
    /// 模型「本次该用哪些工具」，把结果拼进 user 尾部；`None` 或不可用时原文照发。
    ///
    /// 与 `prune` 一样对外可见：用例需要把它钉成「不可用」，否则结果会随本机配置漂移。
    pub suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    /// 子代理建议运行期（`[decision_models.features] subagent_suggestion`）：开启后提交前同样先
    /// 问一次决策模型「本次该调度哪些子代理」。与工具建议互相独立，可单开也可同时开——同时开时
    /// 两条请求并发发起，总耗时取较大者而不是相加。
    ///
    /// 同样对外可见：用例需要把它钉成「不可用」，否则结果会随本机配置漂移。
    pub subagent_suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    /// 子代理建议的候选角色清单覆盖：`None`（默认）表示取工具表里的角色清单。
    ///
    /// 与 `suggest` 同一用意：用例需要钉住候选清单，否则结果会随本机的 `subagents.toml`
    /// 与 `~/.OmniCrawl/agents` 漂移。
    pub suggest_roles: Option<Vec<(String, String)>>,
    /// 提问托管的上下文：用户本回合的请求 + 本回合已有的顾问答复。
    custody_context: omnicrawl_host::tools::CustodyContext,
    /// 审查载荷里的两条会话事实（最近一条用户消息 + 最近一次 ask_user 问答）。
    review_context: omnicrawl_host::review::ReviewContext,
    /// 审查相关配置的变更探测器：外部改 `config.toml` / `decision_models.toml` 后，
    /// 主循环按文件修改时间发现并即时重建 [`App::review`] 与审批模式，不必重启。
    review_watch: omnicrawl_host::review::ReviewConfigWatch,
    /// 启动命令行的 `--approval` 是否显式给过模式。
    ///
    /// 给过时以命令行优先：配置里的 `[approval] mode` 只有在**它自己**被改写后才跟随，
    /// 别的键（如 `review_model`）改动不会顺手覆盖用户的显式选择。
    approval_from_cli: bool,
    /// 是否正有一轮「朗读回复」在跑：合成要几秒，期间落地的下一轮直接跳过，
    /// 免得积压一队过期的语音。
    reply_speaking: Arc<AtomicBool>,
    /// 后台自部署任务（环境安装 / 权重下载 / 服务启停）的结果通道。
    onejev_task: Option<Receiver<OneJevTaskResult>>,
    /// 正在跑的自部署任务名与进度（界面每帧取一次快照）。
    onejev_progress: Option<OneJevRunningTask>,
    /// 本地 OneJev 决策服务进程（切换尺寸即按新规格重启它）。
    onejev_server: OneJevServer,
    /// 本界面独有的 Monitor 日志消费游标（对映 Python `MonitorStateAdapter`）。
    /// 每批增量按 `monitor:` 前缀归并进同一张卡片，不再逐批新开卡片。
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
    /// 输入框里正在拖选（左键按在输入框正文上才算）：松手时复制并清掉高亮。
    ///
    /// 不能只看「当前有没有选区」——只按一下不拖是**空选区**，那一下的作用是定位插入点，
    /// 不是选取；有了这个标记，拖动中的「只剩光标」与「选区折叠」才能分开。
    composer_drag: bool,
    /// 内核 stderr 的逐行通道：运行期报错转成会话区提示，不再直接写终端盖住输入框。
    kernel_logs: Option<Receiver<String>>,
    /// 运行期诊断通道：库里（宿主与 TUI 自身）的诊断经它进会话流，不再直接写 stderr 盖住画布。
    ///
    /// 与 `kernel_logs` 同一手法，只是来源不同：那条是内核**子进程**的 stderr，
    /// 这条是**本进程**内库代码的诊断（`crate::diagnostics` 装接收端，见那里为何要搬）。
    diagnostic_lines: Option<Receiver<(Level, String)>>,
    /// 渠道页「模型 ID」的自动检测后台任务。
    channel_models_task: Option<Receiver<ChannelModelsResult>>,
    /// 工具调用淘汰裁决的请求通道：裁决是网络请求（超时 20s），不能占着 UI 线程等。
    prune_sender: Sender<PruneTaskResult>,
    prune_results: Receiver<PruneTaskResult>,
    /// 仍在后台跑的裁决数：>0 时主循环按活动帧率轮询，结果一到就应答内核。
    prune_in_flight: usize,
    /// 工具建议的请求通道：建议是网络请求（超时 20s），不能占着 UI 线程等。
    suggest_sender: Sender<SuggestTaskResult>,
    suggest_results: Receiver<SuggestTaskResult>,
    /// 仍在后台跑的建议数：>0 时主循环按活动帧率轮询，结果一到就提交回合。
    suggest_in_flight: usize,
}

/// 工具建议的产出：拼好建议后待提交的提问。
///
/// 提交由 UI 线程在 [`App::drain_suggest_results`] 里做——工作线程只负责算，
/// 不碰内核连接（那是 UI 线程独占的）。
struct SuggestTaskResult {
    text: String,
    images: Vec<PendingImage>,
}

/// 渠道模型列表自动检测的产出：候选模型 ID 与失败原因。
struct ChannelModelsResult {
    models: Vec<String>,
    message: String,
}

/// 淘汰裁决的产出：要回给内核的请求 id 与判为无用的调用 ID。
///
/// 应答由 UI 线程在 [`App::drain_prune_results`] 里发出——裁决线程只负责算，
/// 不碰内核连接（那是 UI 线程独占的）。
struct PruneTaskResult {
    id: Id,
    evicted_call_ids: Vec<String>,
}

/// 后台自部署任务的产出（环境安装 / 权重下载 / 服务启停），由 UI 线程的轮询取回。
///
/// 权重下载与环境安装都是 GB 级、服务启停要等健康检查，一律放后台线程，界面只收最终文案。
enum OneJevTaskResult {
    /// 环境安装：成功时是否装到了 CUDA 版 torch。
    Environment(Result<bool, String>),
    /// 权重下载：成功时给出尺寸键。
    Download {
        size: String,
        result: Result<(), String>,
    },
    /// 服务启停：句柄要连同结果一起送回宿主（它持有子进程与 Job 对象）。
    Service {
        server: OneJevServer,
        result: Result<String, String>,
    },
}

/// 正在跑的自部署任务：名字（显示用）+ 进度句柄（后台线程写、界面读）。
struct OneJevRunningTask {
    name: String,
    progress: omnicrawl_onejev::ProgressHandle,
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
        // 启动期只读一次环境：记忆开关、以及决策模型的三处检索重排都从这里取配置。
        let environment = ConfigEnvironment::from_process();
        // 记忆开关与 Python 侧同源：`entry.py` 读 `load_feature_enabled("memory", default=True)`，
        // 即缺省开启。此前这里直接走 `RegistryOptions::default()`（`memory_enabled: false`），
        // 导致记忆整组工具永远进不了表，而 Python 侧缺省是进的。
        let memory_enabled =
            load_feature_enabled(&environment, "memory", true, None, None).unwrap_or(true);
        // `[tools]` 段的禁用开关同样要在构造期装载：此前只在设置面板切换
        // （`apply_tool_switch`）时才赋值，启动路径漏装，配置里关掉的工具重启后照旧进表。
        let disabled_tools = load_disabled_tools(&environment, None).unwrap_or_default();
        // Agent 临时目录：创建分类子目录、按 `[agent_temp]` 间隔补清理并起后台线程。
        // 此前这部分只在对照测试里被调用，宿主侧实际只靠字符串常量拼路径，产物从不回收。
        let agent_temp = omnicrawl_workspace::install_agent_temp_workspace(&environment, workspace);
        if let Some(installation) = agent_temp.as_ref() {
            if let Some(note) = installation.note.as_ref() {
                report_startup_log(LogLevel::Warning, &format!("临时目录未就绪：{note}"));
            }
            if installation.cleaned_entries > 0 {
                report_startup_log(
                    LogLevel::Info,
                    &format!(
                        "已清理临时目录 {}（{} 项）",
                        installation.display_path(),
                        installation.cleaned_entries
                    ),
                );
            }
        }
        // 提示词里的临时目录取配置的实际落点（`[agent_temp] directory`），不再写死默认值。
        let agent_temp_dir = agent_temp
            .as_ref()
            .map(|installation| installation.display_path())
            .unwrap_or_else(|| omnicrawl_host::prompt::DEFAULT_AGENT_TEMP_DIR.to_string());
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
                // 记忆检索重排：开关在 `[decision_models.features]`，默认关闭。
                rerank: Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                    &environment,
                    DECISION_SWITCH_MEMORY_SEARCH,
                )),
                // 向量召回：`[memory_embedding] enabled` 开关，默认关闭。
                embedding: Arc::new(
                    omnicrawl_host::embedding_client::embedding_options_from_config(
                        &environment,
                        load_feature_enabled(&environment, "memory_embedding", false, None, None)
                            .unwrap_or(false),
                    ),
                ),
                ..MemoryOptions::default()
            },
            // 知识库检索重排：与记忆搜索同一套装配口径。
            knowledge_rerank: Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                &environment,
                DECISION_SWITCH_KB_SEARCH,
            )),
            // Skill 检索重排：同样一套装配口径（无关键词时按作用域列出，不送审）。
            skill_rerank: Arc::new(omnicrawl_host::tools::rerank_options_from_config(
                &environment,
                DECISION_SWITCH_SKILL_SEARCH,
            )),
            subagent_roles: subagent_roles(),
            disabled_tools,
            // 联网搜索：默认引擎与 Tavily/Exa/Firecrawl 凭据来自 `[web_search]` 段。
            web_search: omnicrawl_host::tools::web_search_options_from_config(&environment),
            mcp: mcp_manager(workspace),
            // 命令行只承载显式覆盖，未给的字段以 `[image_gen]` 配置为准（见 `args.rs`）。
            image_gen: image_gen_options_from(
                &load_image_gen_configuration(&environment, None).unwrap_or_default(),
                &options.image_gen,
            ),
            advisor: AdvisorOptions {
                enabled: options.advisor.enabled,
                model: options.advisor.model.clone(),
                base_url: if options.advisor.base_url.trim().is_empty() {
                    options.base_url.clone()
                } else {
                    options.advisor.base_url.clone()
                },
                api_key: String::new(),
                effort: options.advisor.effort.clone(),
                executor_model: options.model.clone(),
                disabled_for_models: options.advisor.disabled_for_models.clone(),
                timeout_seconds: options.tool_timeout_seconds,
                messages: advisor_messages,
                tools: advisor_inventory,
                ..AdvisorOptions::default()
            },
            tts: tts_options(workspace),
            // TLS 会话恢复：`[tls_resumption] enabled` 为真时才建缓存，之后 `fetcher`
            // 对同一站点的第二次连接会尝试恢复。默认关闭（改的是密钥调度）。
            fetcher: FetcherOptions {
                tls_session_cache: omnicrawl_host::tools::tls_session_cache_from_config(
                    &environment,
                ),
                ..FetcherOptions::default()
            },
            // 只受 `[computer_use] enabled` 约束：普通子代理关掉不影响桌面操作。
            computer_use: computer_use_enabled(&environment),
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
        // 子代理窗口由工作台自己拉起：可执行文件就是本进程，流水落在用户配置目录下。
        state.subagent_window_exe = std::env::current_exe().ok();
        state.telemetry.context_window = options.context_window_tokens;
        // 思考显示是纯界面开关：启动期按 `ui.show_thinking` 定初值，设置面板里可即时改。
        state.set_show_thinking(
            load_show_thinking(&ConfigEnvironment::from_process(), None).unwrap_or(true),
        );
        state.mcp_servers = registry
            .mcp()
            .map(|manager| manager.config().enabled_servers().len() as u64)
            .unwrap_or(0);
        let prompt = load_prompt_runtime_with_temp(
            &ConfigEnvironment::from_process(),
            &options,
            workspace,
            &agent_temp_dir,
        )?;
        let (completion_sender, completions) = mpsc::channel();
        let (prune_sender, prune_results) = mpsc::channel();
        let (suggest_sender, suggest_results) = mpsc::channel();
        // 模型/渠道视图：配置读不出来时退回环境变量默认值，界面照常可用（缺什么会由内核回错）。
        let llm = match load_llm_config(&ConfigEnvironment::from_process()) {
            Ok(config) => config,
            Err(error) => {
                report_startup_log(
                    LogLevel::Warning,
                    &format!("模型配置读取失败，按环境变量默认值启动：{error}"),
                );
                LlmConfig::with_environment(&ConfigEnvironment::from_process())
            }
        };
        // 上下文占用段用「CLI 显式值 → 配置」的同一套口径（与发给内核的 handshake 一致）；
        // 只认 CLI 值会出现总量未知的 `0/1 0%`，而且与内核的实际窗口不一致。
        let context_window = effective_context_window(options.context_window_tokens, &llm);
        state.telemetry.context_window = u64::try_from(context_window)
            .ok()
            .filter(|value| *value > 0);
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
        let review = build_review_options(&llm, &environment);
        // 审查配置变更探测：以启动时这份磁盘状态的生效值为基线，之后主循环发现外部改动即热应用。
        let mut review_watch = omnicrawl_host::review::ReviewConfigWatch::new();
        review_watch.baseline(&environment);
        let approval_from_cli = options.approval_from_cli;
        // 提问托管：与审查、检索重排同源（`[decision_models.features] ask_user_custody`）。
        let custody = Some(omnicrawl_host::tools::custody_options_from_config(
            &environment,
        ));
        // 工具调用淘汰：与审查、检索重排同源（`[decision_models.features] tool_call_prune`）。
        let prune = Some(omnicrawl_host::tools::prune_options_from_config(
            &environment,
        ));
        // 建议运行期：与审查、检索重排同源（`[decision_models.features] tool_suggestion` 与
        // `subagent_suggestion`；两者独立，可同时开启）。
        let suggest = Some(omnicrawl_host::tools::suggest_options_from_config(
            &environment,
        ));
        let subagent_suggest = Some(omnicrawl_host::tools::subagent_suggest_options_from_config(
            &environment,
        ));
        // 命令菜单的候选表来自统一命令源（注册表声明）加运行期 Skill；启动时装载一次，
        // 工作区切换与设置变更后由 `refresh_command_menu` 重装。
        state
            .composer
            .set_commands(commands::command_options_with_skills(&prompt.skill_metas()));
        Ok(Self {
            state,
            options,
            kernel,
            quit: false,
            kernel_lost: false,
            workspace: workspace.to_path_buf(),
            prompt,
            registry,
            registry_options,
            batch_registry: None,
            completions,
            completion_sender,
            tool_deadline: None,
            pending_commands: HashMap::new(),
            pending_turn_submit: None,
            deferred_frames: VecDeque::new(),
            next_turn: 1,
            advisor_context,
            settings: None,
            settings_request: None,
            prompt_request: None,
            pending_skill_context: None,
            restore_skill_context_after_turn: false,
            llm,
            viewport: Rect::default(),
            drawn: None,
            plugins,
            review,
            custody,
            prune,
            suggest,
            subagent_suggest,
            suggest_roles: None,
            custody_context: omnicrawl_host::tools::CustodyContext::default(),
            review_context: omnicrawl_host::review::ReviewContext::default(),
            review_watch,
            approval_from_cli,
            reply_speaking: Arc::new(AtomicBool::new(false)),
            onejev_task: None,
            onejev_progress: None,
            onejev_server: OneJevServer::new(),
            monitor_state: crate::monitor::MonitorStateAdapter::default(),
            monitor_polled_at: Instant::now(),
            isolation: None,
            session_id: String::new(),
            session_close_before_sent: false,
            slow_task: None,
            scrollbar_drag: false,
            composer_drag: false,
            startup_plugin_lines,
            agent_temp,
            startup_service_lines: Vec::new(),
            kernel_logs: None,
            diagnostic_lines: None,
            channel_models_task: None,
            prune_sender,
            prune_results,
            prune_in_flight: 0,
            suggest_sender,
            suggest_results,
            suggest_in_flight: 0,
        })
    }
    // ---------- 内核同步往返 ----------
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
            Self::Compact => "上下文压缩中…",
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


#[cfg(test)]
mod tests {
    use super::*;
    use omnicrawl_config::models::llm::provider_options_from_json;

    #[test]
    fn retryable_failures_are_the_ones_worth_resending() {
        // 网络类失败：原样重发有机会成功，给按钮。
        for message in [
            "上游错误已达到本回合自动重试上限，已停止本次请求。connection reset",
            "请求超时",
            "502 bad gateway",
        ] {
            assert!(is_retryable_failure(message), "{message} 应当可重试");
        }
        // 确定性失败：重发只会再失败一次，不给按钮。
        for message in [
            "超过上下文长度限制",
            "鉴权失败：401 unauthorized",
            "参数不合法：invalid params",
            "网关判定工具状态重复提交",
        ] {
            assert!(!is_retryable_failure(message), "{message} 不该重试");
        }
    }

    /// 钉住这次修复的触发条件：`Ctrl+V` 只认**抬起**。
    ///
    /// Windows Terminal 的 `ctrl+v` 是它自家的粘贴动作，按下被它吃掉、只有抬起透传；
    /// 若判据退回按下，图片粘贴在 WT 下又是死代码（这正是本次的故障现象）。
    #[test]
    fn ctrl_v_image_paste_hangs_on_the_release_event() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release,
        );
        assert!(is_ctrl_v_release(release), "WT 只透传抬起，判据必须认它");

        let press = KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );
        assert!(!is_ctrl_v_release(press), "WT 下按下根本到不了应用");

        // 普通 `v` 与别的 Ctrl 组合都不该命中。
        assert!(!is_ctrl_v_release(KeyEvent::new_with_kind(
            KeyCode::Char('v'),
            KeyModifiers::empty(),
            KeyEventKind::Release
        )));
        assert!(!is_ctrl_v_release(KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release
        )));

        // 大写锁打开时 crossterm 会把 `v` 折成 `'V'`：这是同一次 Ctrl+V，必须认。
        assert!(is_ctrl_v_release(KeyEvent::new_with_kind(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL,
            KeyEventKind::Release
        )));

        // Ctrl+Shift+V 是终端自家的文本粘贴，不能顺手再插一张图（否则文本与图各来一遍）。
        assert!(!is_ctrl_v_release(KeyEvent::new_with_kind(
            KeyCode::Char('V'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyEventKind::Release
        )));
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

    /// 原生视觉的三条来源：命令行/环境显式开启、配置里当前模型的覆盖、都没有时关闭。
    ///
    /// 回归的是「配置里配好了却看不到图」：以前只看 `Options::native_vision`，
    /// 于是 `models.toml` / 渠道里写的 `native_vision = true` 对 TUI 完全无效。
    #[test]
    fn native_vision_comes_from_flag_then_config() {
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let mut llm = LlmConfig::with_environment(&environment);
        llm.native_vision = None;
        assert!(!effective_native_vision(false, false, &llm), "都没给就是关");
        assert!(
            effective_native_vision(false, true, &llm),
            "命令行开关仍然算数"
        );

        llm.native_vision = Some(true);
        assert!(
            effective_native_vision(false, false, &llm),
            "配置开了就用配置"
        );

        // `--base-url` 指外部渠道：跑的是命令行那个模型，配置里的覆盖不跟过来。
        assert!(!effective_native_vision(true, false, &llm));
        assert!(
            effective_native_vision(true, true, &llm),
            "显式开关不受渠道影响"
        );

        llm.native_vision = Some(false);
        assert!(
            !effective_native_vision(false, false, &llm),
            "配置明确关掉就是关"
        );
    }

    /// 未配置原生视觉、但模型自身声明支持视觉时按原生视觉走（不能压成关闭）。
    ///
    /// 这是「原生视觉优先于视觉代理」的一半：显式开关决定另一半，未配置的一半靠模型能力
    /// 回落。少了这条回落，支持视觉的模型会被误送去视觉代理。
    #[test]
    fn native_vision_falls_back_to_the_model_capability() {
        let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
        let mut llm = LlmConfig::with_environment(&environment);

        llm.native_vision = None;
        llm.vision_capability = Some(true);
        assert!(
            effective_native_vision(false, false, &llm),
            "未配置 + 模型支持视觉 = 用原生视觉"
        );

        llm.native_vision = Some(false);
        assert!(
            !effective_native_vision(false, false, &llm),
            "显式关掉优先于模型能力"
        );

        llm.native_vision = None;
        llm.vision_capability = Some(false);
        assert!(
            !effective_native_vision(false, false, &llm),
            "未配置 + 模型不支持视觉 = 交给代理"
        );

        llm.vision_capability = None;
        assert!(
            !effective_native_vision(false, false, &llm),
            "未配置 + 能力未知 = 交给代理"
        );
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

    /// 握手必须把 `[context_compaction]` 整段转发：映射本身在
    /// `omnicrawl_controllers::settings::kernel_compaction_settings`（那边有用例钉住逐字段），
    /// 这里钉住「热更新只带本次要改的两个字段」这一条，避免把握手时的字段覆盖回空值。
    #[test]
    fn compaction_settings_convert_percent_to_tokens() {
        let settings = compaction_settings(200_000, 80);
        assert_eq!(settings.trigger_context_tokens, Some(160_000));
        assert_eq!(settings.context_window_tokens, Some(200_000));
        assert_eq!(
            settings.recent_turns, None,
            "只带本次要改的字段，其余交给内核保留原值"
        );
        assert_eq!(
            settings.target_summary_tokens, None,
            "热更新以不覆盖握手时下发的摘要预算"
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
