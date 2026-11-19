//! Agent 服务编排：单活动运行的宿主驱动、审批/提问的阻塞等待与事件翻译。
//!
//! 对应 `omnicrawl/api/service.py` 的 `AgentAPIService`。差别只在宿主形态：Python 侧
//! Agent 就在本进程，这里由 `omnicrawl-host` 起内核子进程、用协议 v1 驱动一个回合，
//! 并把内核通知翻成 `omnicrawl://docs/API.md` 列出的 SSE 事件。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use omnicrawl_config::core::context::detect_project_context;
use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_config::core::settings::load_feature_enabled;
use omnicrawl_config::features::agent_workspace::load_agent_workspace_config;
use omnicrawl_config::features::approval::{
    load_approval_mode, APPROVAL_MODE_AUTO, APPROVAL_MODE_MANUAL, APPROVAL_MODE_REVIEW,
};
use omnicrawl_config::features::context_compaction::load_context_compaction_config;
use omnicrawl_config::features::decision_model::{
    DECISION_SWITCH_KB_SEARCH, DECISION_SWITCH_MEMORY_SEARCH, DECISION_SWITCH_SKILL_SEARCH,
};
use omnicrawl_config::features::tools::load_disabled_tools;
use omnicrawl_config::models::llm::{load_llm_config, LlmConfig};
use omnicrawl_config::models::model_catalog::{
    build_catalog, detect_model_options, ensure_current_model_option, Catalog, CatalogPorts,
    CatalogRequest, DiscoveryCache, ModelOption, MODEL_LIST_TIMEOUT_SECONDS,
};
use omnicrawl_controllers::memory::{session_memory_root, user_data_root, user_memory_root};
use omnicrawl_controllers::settings::{
    context_compaction_trigger_tokens, kernel_compaction_settings,
};
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_controllers::vision_proxy::vision_proxy_configured;
use omnicrawl_extensions::skill::SkillManager;
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::prompt::{PromptOptions, PromptRuntime};
use omnicrawl_host::tools::monitor::{MonitorManager, MonitorPollView, MonitorTaskView};
use omnicrawl_host::tools::{MemoryOptions, RegistryOptions};
use omnicrawl_host::turn::{Interactor, RunnerOptions, TurnControl, TurnError, TurnRunner};
use omnicrawl_ipc::bridge::{
    HostEvent, KernelCompactionConfig, KernelModelConfig, KernelSessionConfig,
    SessionModelSettings, SessionSettingsParams, SubagentEventPayload,
};
use omnicrawl_mcp::client::McpClientManager;
use omnicrawl_session::redaction::redact_sensitive_values;
use omnicrawl_session::{
    format_datetime,
    project::{OverviewSessionEntry, ProjectOverview, ProjectStore},
    project_session_history,
    prompt_history::PromptHistoryEntry,
    recover_run_guard_state,
    store::{SessionListQuery, SessionStore},
    utc_now, MemoryStore, ProjectEntry, SessionEvent, SessionIndexEntry, SessionRecordDiagnostic,
};
use omnicrawl_version::USER_AGENT;
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, finalize_subagent_worktrees, prepare_isolated_workspace,
    start_background_isolation_sweep, IsolationOptions, IsolationSession,
};
use omnicrawl_workspace::process_control::pid_is_running;
use serde_json::{json, Map, Value};

use crate::error::ApiError;
use crate::model_discovery;
use crate::runs::{
    random_id, RunBackend, RunEvent, RunRecord, RunStatus, TodoSnapshot,
    DEFAULT_MAX_EVENTS_PER_RUN, DEFAULT_MAX_RETAINED_RUNS,
};
use crate::shared_store::{SharedRunStore, DECISION_ANSWER, DECISION_CANCEL, DECISION_CONFIRM};

mod errors;
mod options;
mod util;

// errors / util 里的项都是 `pub(super)`，只到 service 模块可见；这里用普通 `use`
// 把它们带进本模块，不要写成 `pub(super) use`——那会试图放大可见性并触发 unused_imports。
use errors::*;
use options::*;
use util::*;

// 这几个原本就是 `pub`，crate 内其它模块用 `crate::service::` 取。
pub use options::{
    connector_config_data, connector_options_from_process, options_from_process,
    resolve_kernel_program,
};

/// 审批与提问的等待上限（Python 的 `confirmation_timeout_seconds`）。
pub const DEFAULT_CONFIRMATION_TIMEOUT_SECONDS: f64 = 300.0;
/// 决策轮询粒度：HTTP 处理器只改状态，回合线程在这里醒来（Python 用 0.1s）。
const DECISION_POLL: Duration = Duration::from_millis(50);
/// 回合外事件泵的轮询间隔：后台任务事件不需要毫秒级实时，也不该频繁争回合锁。
const EVENT_PUMP_INTERVAL: Duration = Duration::from_millis(200);
/// 共享存储上的等待轮询粒度：跨进程没有条件变量可用（对应 Python `SHARED_WAIT_POLL_SECONDS`）。
const SHARED_WAIT_POLL: Duration = Duration::from_millis(100);
/// 退出时等内核把 `session_closed` 写完并自己退出的上限。
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
/// 子代理进入终态的事件名：到达后清除任务来源映射（对应 Python 的三件套）。
const TERMINAL_SUBAGENT_EVENTS: [&str; 3] = [
    "subagent.task.completed",
    "subagent.task.failed",
    "subagent.task.cancelled",
];
/// `on_protocol_wait` 的状态文案（Python 同名回调）。
const PROTOCOL_WAIT_STATUS: &str = "正在继续";
/// `on_stream_rollback` 的状态文案（Python 同名回调）。
const STREAM_ROLLBACK_STATUS: &str = "模型流中断，先前输出已作废，正在自动重试";
/// 生成失败的对外文案；第三方异常正文不外泄（Python 侧同样只记日志）。
const RUN_FAILED_MESSAGE: &str = "生成任务失败。";
/// 取消的对外文案。
const RUN_CANCELLED_MESSAGE: &str = "用户取消生成。";
/// 未配置系统提示词时的兜底文本（与 TUI 的默认值同源；完整提示词的组装见 README）。
const DEFAULT_SYSTEM_PROMPT: &str = "你是 OmniCrawl 助手，回答保持简洁。";

/// MCP 热更新的结果：连接诊断、重建后工具表规模与「是否要重启才生效」。
pub struct McpReload {
    pub diagnostics: Vec<String>,
    pub tool_count: usize,
    /// 嵌入模式（`with_runner`）没有可重建的运行器，只能等下次启动装配新配置。
    pub requires_restart: bool,
}

/// 服务启动所需的固定输入。
pub struct ServiceOptions {
    pub workspace_root: PathBuf,
    /// 会话根目录；`None` 表示不落会话（内核按无会话行为跑）。
    pub session_root: Option<PathBuf>,
    /// 模型端点；`tools` 由运行器覆盖。
    pub model: KernelModelConfig,
    pub approval: ApprovalMode,
    pub confirmation_timeout_seconds: f64,
    pub command_timeout_seconds: i64,
    pub tool_timeout_seconds: i64,
    /// 启动时算好的「原生视觉是否生效」（显式开关优先，未配置按模型能力回落）。
    ///
    /// 只在装配 `RunnerOptions` 时读一次；运行期换模型由 `apply_model_switch` 直接更新
    /// 运行器与重起快照，不回写这里——`options` 在装配完成后不可变。
    pub native_vision: bool,
    pub registry_options: RegistryOptions,
    pub max_retained_runs: usize,
    pub max_events_per_run: usize,
    /// 进程环境：管理面按请求读取 `config.toml`（设置端点与运行态展示都要用）。
    pub env: ConfigEnvironment,
    /// MCP 管理器：`/mcp` 状态与工具表共用同一个句柄；未配置时为 `None`。
    pub mcp: Option<Arc<McpClientManager>>,
    /// 后台任务管理器：`/monitors` 与 `monitor` 工具共用同一个句柄；缺省为 `None`。
    pub monitors: Option<MonitorManager>,
    /// 审查运行期（`approval.mode = review` 时用）；`None` 表示没有审查运行期，
    /// 此时需审查的调用按 fail-closed 拒绝。
    pub review: Option<omnicrawl_host::review::ReviewOptions>,
    /// 提问托管运行期（`[decision_models.features] ask_user_custody`）；`None` 表示不托管，
    /// 有选项的提问照旧等 HTTP 提交的答案。
    pub custody: Option<omnicrawl_host::tools::CustodyOptions>,
    /// 工具调用淘汰运行期（`[decision_models.features] tool_call_prune`）；`None` 表示不淘汰，
    /// 「刚变老」的那一批调用照旧全部留在上下文里。
    pub prune: Option<omnicrawl_host::tools::PruneOptions>,
    /// Skill 建议运行期（`[suggestion] skills`）：开启后由主模型识别技能词、检索本机 Skill
    /// 索引，把命中的前几名注入 user 尾部；`None` 表示不建议。
    pub skill_suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    /// 模型发现缓存：跨请求存活，`POST /models/refresh` 清空它。
    pub discovery_cache: Arc<DiscoveryCache>,
    /// 内核可执行文件：运行期重起内核（切换会话 / 工作区）需要它；嵌入模式可以不给。
    pub kernel_program: Option<PathBuf>,
    /// 主 Agent 隔离区会话：`close()` 时 apply 变更 + 按策略清理（对映 Python `attach_isolation_session`）。
    /// 用 `Mutex` 承载是为了在 `&self` 的收尾路径里取走它，保证只收尾一次。
    pub isolation: Mutex<Option<IsolationSession>>,
    /// 提示词装配结果（模板 + AGENTS.md + Skill 索引 + 运行环境），交给 `TurnRunner` 使用；
    /// `None` 表示本进程不做装配（嵌入与测试），system prompt 取 `model.system_prompt`。
    pub prompt: Option<Arc<PromptRuntime>>,
}

impl ServiceOptions {
    /// 只填工作区与模型的骨架；其余取默认值，供嵌入与测试起步用。
    pub fn new(workspace_root: impl Into<PathBuf>, model: KernelModelConfig) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            session_root: None,
            model,
            approval: ApprovalMode::Manual,
            confirmation_timeout_seconds: DEFAULT_CONFIRMATION_TIMEOUT_SECONDS,
            command_timeout_seconds: 600,
            tool_timeout_seconds: 300,
            native_vision: false,
            registry_options: RegistryOptions::default(),
            max_retained_runs: DEFAULT_MAX_RETAINED_RUNS,
            max_events_per_run: DEFAULT_MAX_EVENTS_PER_RUN,
            env: ConfigEnvironment::from_process(),
            mcp: None,
            monitors: None,
            review: None,
            custody: None,
            prune: None,
            skill_suggest: None,
            discovery_cache: Arc::new(DiscoveryCache::new()),
            kernel_program: None,
            isolation: Mutex::new(None),
            prompt: None,
        }
    }
}

/// 单 Agent、单活动运行的线程安全服务。
pub struct AgentService {
    store: Arc<RunBackend>,
    /// 内核连接与工具表；持有整段回合，因此同时只有一个回合在跑。
    runner: Mutex<Option<TurnRunner>>,
    control: Mutex<Option<TurnControl>>,
    session_id: Mutex<String>,
    /// 连接器注入的 system prompt 追加段（如 QQ 机器人的预设提示词）。
    ///
    /// 装配结果被重建（切模式、换工作区、重起内核）时要原样补回去，否则一次重建就会
    /// 让「我是谁」的设定悄悄消失。
    prompt_append: Mutex<String>,
    /// 当前模型：启动时取配置值，运行期切换后立即更新（`PUT /models/current`）。
    model: Mutex<String>,
    /// 当前工作区：`POST /projects/switch` 之后立即更新，工具与会话快照都跟着走。
    workspace: Mutex<PathBuf>,
    /// 重起内核所需的固定输入；嵌入模式（`with_runner`）为 `None`，不支持运行期切换。
    spawner: Mutex<Option<KernelSpawn>>,
    /// 当前会话的后台任务事件流：回合内由 sink 喂、回合外由事件泵喂。
    subagent_feed: SubagentFeed,
    /// 审查相关配置的变更探测器：事件泵按文件修改时间发现外部改动后即时热应用。
    review_watch: Mutex<omnicrawl_host::review::ReviewConfigWatch>,
    /// 服务是否已关闭（事件泵据此退出）。
    closed: AtomicBool,
    /// MCP 管理器句柄：`PUT /settings/mcp` 会整体换掉它，`/mcp` 状态与工具表共用同一份。
    ///
    /// 放在 `ServiceOptions` 之外是因为设置写端点在 `&self` 上运行，而选项结构在装配
    /// 完成后不再可变；这里只承载「当前是哪份配置」，不改启动输入。
    mcp: Mutex<Option<Arc<McpClientManager>>>,
    /// Agent 临时目录句柄：创建分类子目录并跑清理调度。
    ///
    /// 放在 `ServiceOptions` 之外与 `mcp` 同理：`POST /projects/switch` 换工作区后要按新根
    /// 重建，而 `options` 在装配完成后不再可变。`None` 表示 `[agent_temp] enabled` 为假
    /// 或嵌入模式没装（嵌入/测试不碰真实磁盘）。
    agent_temp: Mutex<Option<omnicrawl_workspace::AgentTempInstallation>>,
    options: ServiceOptions,
}

/// 会话级后台任务事件流（对应 Python 的 `_SubAgentEventStream`）。
///
/// 事件 ID 按会话单调递增；会话切换时整条流重置（与 Python 的「会话级独立控制流」同义）。
struct SubagentFeed {
    locked: Mutex<SubagentFeedState>,
    wake: Condvar,
}

#[derive(Default)]
struct SubagentFeedState {
    session_id: String,
    next_id: u64,
    events: Vec<RunEvent>,
}

/// 重起内核的固定输入：可执行文件、运行器选项与工具表选项。
struct KernelSpawn {
    program: PathBuf,
    runner: RunnerOptions,
    registry_options: RegistryOptions,
}

impl SubagentFeed {
    fn new() -> Self {
        Self {
            locked: Mutex::new(SubagentFeedState::default()),
            wake: Condvar::new(),
        }
    }

    /// 推一条事件；会话变化时整条流从 1 重新开始（会话级控制流不跨会话）。
    fn push(&self, session_id: &str, name: &str, data: Value, max_events: usize) {
        let mut state = match self.locked.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.session_id != session_id {
            state.session_id = session_id.to_string();
            state.events.clear();
            state.next_id = 0;
        }
        state.next_id += 1;
        // 先把 id 取出来：直接写在 `push` 的表达式里会让 `state.events` 的可变借
        // 与 `state.next_id` 的读取同时存在。
        let event_id = state.next_id;
        state.events.push(RunEvent {
            id: event_id,
            event: name.to_string(),
            data,
        });
        let max_events = max_events.max(10);
        if state.events.len() > max_events {
            let excess = state.events.len() - max_events;
            state.events.drain(..excess);
        }
        drop(state);
        self.wake.notify_all();
    }

    /// 读取指定会话在游标之后的事件；会话已切换时没有可读事件。
    pub fn events_after(&self, session_id: &str, cursor: u64) -> Vec<RunEvent> {
        let state = match self.locked.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.session_id != session_id {
            return Vec::new();
        }
        state
            .events
            .iter()
            .filter(|event| event.id > cursor)
            .cloned()
            .collect()
    }

    /// 等到指定会话在游标之后有新事件或超时；返回是否观测到变化。
    fn wait_for_events(&self, session_id: &str, cursor: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = match self.locked.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if state.session_id == session_id && state.events.iter().any(|event| event.id > cursor)
            {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }
}

impl AgentService {
    /// 生产路径：起内核子进程、握手、建工具表。
    ///
    /// 新会话的 ID 由内核经 stderr 报出，这里等它一小会儿；等不到就按空值继续
    /// （无会话行为与「有会话但 ID 未知」都不会让回合跑不起来）。
    pub fn spawn(
        options: ServiceOptions,
        kernel_program: &std::path::Path,
    ) -> Result<Self, String> {
        let ready = Arc::new(Mutex::new(String::new()));
        let slot = Arc::clone(&ready);
        let client =
            KernelClient::spawn_with_stderr(kernel_program, move |line| {
                if let Some(session_id) = parse_session_ready(line) {
                    if let Ok(mut guard) = slot.lock() {
                        *guard = session_id;
                    }
                }
            })
            .map_err(|error| format!("启动内核进程失败：{error}"))?;

        let mut runner =
            TurnRunner::new(client, runner_options(&options), &options.registry_options)?;
        runner
            .handshake(&mut |_| {})
            .map_err(|error| format!("内核握手失败：{error}"))?;

        let session_id = wait_for_session_id(&ready);
        // 会话 id 只有内核报出之后才知道：这里先按它绑一次，再让正在跑的运行器换表，
        // 否则首个回合的工具表里会话级记忆仍是关的。
        let mut registry_options = options.registry_options.clone();
        bind_session_memory(&mut registry_options, &session_id);
        runner
            .rebuild_registry(&registry_options)
            .map_err(|error| format!("会话级记忆未接上：{error}"))?;
        let spawner = KernelSpawn {
            program: kernel_program.to_path_buf(),
            runner: runner_options(&options),
            registry_options,
        };
        let service = Self::with_runner(options, runner)
            .with_session_id(session_id.clone())
            .with_spawner(spawner);
        // 审查配置热应用的基线：以启动时读到的生效值为准，之后的改动才算「外部变更」。
        if let Ok(mut watch) = service.review_watch.lock() {
            watch.baseline(&service.options.env);
        }
        // Agent 临时目录：建分类子目录、按 `[agent_temp]` 间隔补清理并起后台线程。
        // 此前这部分只有对照测试在调，无头服务从不回收临时产物。
        service.install_agent_temp();
        Ok(service)
    }

    /// 按当前工作区装/换 Agent 临时目录句柄；`[agent_temp] enabled` 为假时不装。
    ///
    /// 失败只告警不阻断：临时目录清理属于运维行为，配置写坏不该让服务起不来。
    fn install_agent_temp(&self) {
        let root = self.workspace_root();
        let installation =
            omnicrawl_workspace::install_agent_temp_workspace(&self.options.env, &root);
        if let Some(note) = installation
            .as_ref()
            .and_then(|entry| entry.note.as_ref())
        {
            eprintln!("[api] 临时目录未就绪：{note}");
        }
        if let Some(count) = installation
            .as_ref()
            .map(|entry| entry.cleaned_entries)
            .filter(|count| *count > 0)
        {
            eprintln!("[api] 已清理 Agent 临时目录（{count} 项）");
        }
        let mut guard = self
            .agent_temp
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(mut previous) = guard.take() {
            previous.close();
        }
        *guard = installation;
    }

    /// 嵌入/测试路径：用外部准备好的运行器。
    pub fn with_runner(options: ServiceOptions, runner: TurnRunner) -> Self {
        let store = Arc::new(RunBackend::memory(
            options.max_retained_runs,
            options.max_events_per_run,
        ));
        let model = options.model.model.clone();
        let workspace = options.workspace_root.clone();
        let mcp = options.mcp.clone();
        Self {
            store,
            runner: Mutex::new(Some(runner)),
            control: Mutex::new(None),
            session_id: Mutex::new(String::new()),
            prompt_append: Mutex::new(String::new()),
            model: Mutex::new(model),
            workspace: Mutex::new(workspace),
            spawner: Mutex::new(None),
            subagent_feed: SubagentFeed::new(),
            review_watch: Mutex::new(omnicrawl_host::review::ReviewConfigWatch::new()),
            closed: AtomicBool::new(false),
            mcp: Mutex::new(mcp),
            agent_temp: Mutex::new(None),
            options,
        }
    }

    /// 记录重起内核所需的信息；只有生产路径（`spawn`）会调用它。
    fn with_spawner(self, spawner: KernelSpawn) -> Self {
        if let Ok(mut guard) = self.spawner.lock() {
            *guard = Some(spawner);
        }
        self
    }

    /// 换运行状态后端：`api.workers > 1` 时切到跨进程共享存储（默认是内存）。
    pub fn with_run_backend(mut self, backend: RunBackend) -> Self {
        // 多 worker 场景：上一个 worker 崩溃会留下永远活动的运行，其内存状态已随进程消失。
        // 装配共享后端时按所有者 PID 存活情况收敛，避免客户端无限等待（对应 Python
        // `AgentAPIService.__init__` 里的 `reconcile_orphan_runs`；收敛失败不阻断启动）。
        if let Some(store) = backend.shared() {
            match store.reconcile_orphan_runs(pid_is_running) {
                Ok(0) => {}
                Ok(count) => eprintln!("[api] 收敛了 {count} 个所有进程已消失的生成任务。"),
                Err(error) => eprintln!(
                    "[api] 收敛孤儿生成任务失败：{}",
                    error.to_api_error().message
                ),
            }
        }
        self.store = Arc::new(backend);
        self
    }

    /// 已知会话 ID（内核新建会话后由 stderr 报出）。
    pub fn with_session_id(self, session_id: String) -> Self {
        if let Ok(mut guard) = self.session_id.lock() {
            *guard = session_id;
        }
        self
    }

    pub fn store(&self) -> &Arc<RunBackend> {
        &self.store
    }

    pub fn current_session_id(&self) -> String {
        self.session_id
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 修改类接口的守卫：有活动运行时拒绝（Python 的 `ensure_mutation_allowed`）。
    pub fn ensure_mutation_allowed(&self) -> Result<(), ApiError> {
        self.store
            .ensure_mutation_allowed()
            .map_err(|error| error.to_api_error())
    }

    /// 关闭：请内核退出并回收后台资源，并收尾挂载的隔离工作区。
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.finalize_isolation();
        // 临时目录清理线程随服务关闭而停：`close` 会 join 掉它，不留悬挂工作。
        if let Ok(mut guard) = self.agent_temp.lock() {
            if let Some(mut installation) = guard.take() {
                installation.close();
            }
        }
        let Ok(mut guard) = self.runner.lock() else {
            return;
        };
        if let Some(runner) = guard.as_mut() {
            runner.shutdown();
            // 等内核把 `session_closed` 落盘后再退出，确保收尾事件先落地。
            runner.wait_for_exit(SHUTDOWN_WAIT);
        }
        *guard = None;
    }

    /// 当前工作区根：`POST /projects/switch` 之后即为新根。
    pub fn workspace_root(&self) -> PathBuf {
        self.workspace
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| self.options.workspace_root.clone())
    }

    /// 进程环境：设置面按请求读写 `config.toml`。
    pub fn environment(&self) -> &ConfigEnvironment {
        &self.options.env
    }
}

mod catalog;
mod mcp;
mod monitors;
mod runs;
mod sessions;
mod settings;


/// 一次审批或提问的阻塞等待；HTTP 处理器只写运行记录，这里轮询醒来。
struct ServiceInteractor {
    service: Arc<AgentService>,
    run_id: String,
    timeout_seconds: f64,
}

impl ServiceInteractor {
    fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs_f64(self.timeout_seconds.max(0.1))
    }

    fn set_status(&self, status: RunStatus) {
        let _ = self.service.store.set_status(&self.run_id, status);
    }

    /// 消费该 Run 的跨进程决策。
    ///
    /// 所有者读的就是共享记录本身，因此决策到达时状态也已生效；这里把它们取走，
    /// 既避免在共享存储里无限堆积，也与 Python「所有者消费决策」的语义一致。
    fn drain_shared_decisions(&self) {
        if let Some(store) = self.service.store.shared() {
            let _ = store.take_decisions(&self.run_id);
        }
    }
}

impl Interactor for ServiceInteractor {
    fn decide(&mut self, tool: &str, arguments: &Map<String, Value>) -> Option<bool> {
        if self
            .service
            .store
            .cancel_requested(&self.run_id)
            .unwrap_or(true)
        {
            return None;
        }
        let confirmation_id = random_id();
        let public = public_tool_arguments(tool, arguments);
        self.service
            .store
            .register_confirmation(&self.run_id, &confirmation_id, tool, public.clone())
            .ok()?;
        self.set_status(RunStatus::WaitingConfirmation);
        let _ = self.service.store.append_event(
            &self.run_id,
            "confirmation.required",
            json!({
                "confirmation_id": confirmation_id,
                "tool": tool,
                "arguments": public,
                "timeout_seconds": self.timeout_seconds,
            }),
        );

        let deadline = self.deadline();
        loop {
            self.drain_shared_decisions();
            let Ok(run) = self.service.store.get(&self.run_id) else {
                return None;
            };
            if run.cancel_requested {
                return None;
            }
            let decided = run
                .confirmations()
                .iter()
                .find(|item| item.confirmation_id == confirmation_id)
                .and_then(|item| item.decision);
            if let Some(decision) = decided {
                self.set_status(RunStatus::Running);
                return Some(decision);
            }
            if Instant::now() >= deadline {
                // 超时与显式拒绝同为终态：置位后迟到的批准会得到 409。
                let _ =
                    self.service
                        .store
                        .resolve_confirmation(&self.run_id, &confirmation_id, false);
                self.set_status(RunStatus::Running);
                return Some(false);
            }
            thread::sleep(DECISION_POLL);
        }
    }

    fn answer(
        &mut self,
        prompt: &str,
        options: &[String],
        arguments: &Map<String, Value>,
    ) -> Option<String> {
        if self
            .service
            .store
            .cancel_requested(&self.run_id)
            .unwrap_or(true)
        {
            return None;
        }
        let question_id = random_id();
        let kind = question_kind(arguments, options);
        self.service
            .store
            .register_question(&self.run_id, &question_id, kind, prompt, options.to_vec())
            .ok()?;
        self.set_status(RunStatus::WaitingUser);
        let _ = self.service.store.append_event(
            &self.run_id,
            "ask_user.required",
            json!({
                "question_id": question_id,
                "kind": kind,
                "question": prompt,
                "options": options,
                "timeout_seconds": self.timeout_seconds,
            }),
        );

        let deadline = self.deadline();
        loop {
            self.drain_shared_decisions();
            let Ok(run) = self.service.store.get(&self.run_id) else {
                return None;
            };
            if run.cancel_requested {
                return None;
            }
            let reply = run
                .questions()
                .iter()
                .find(|item| item.question_id == question_id)
                .filter(|item| item.resolved)
                .map(|item| item.answer.clone());
            if let Some(reply) = reply {
                self.set_status(RunStatus::Running);
                return reply.filter(|text| !text.trim().is_empty());
            }
            if Instant::now() >= deadline {
                let _ = self
                    .service
                    .store
                    .expire_question(&self.run_id, &question_id);
                self.set_status(RunStatus::Running);
                let _ = self.service.store.append_event(
                    &self.run_id,
                    "ask_user.expired",
                    json!({"question_id": question_id}),
                );
                return None;
            }
            thread::sleep(DECISION_POLL);
        }
    }
}






/// 把会话 id 绑到工具表的会话级记忆上；id 不可用时保持关闭。
///
/// 与 TUI 的 `sync_session_memory` 同一口径：目录名只接受字母数字、`_` 与 `-`，
/// 不合规的 id 若照抄进去，工具调用只会拿到一个「目录不合法」的错误。
pub(crate) fn bind_session_memory(options: &mut RegistryOptions, session_id: &str) {
    let id = session_id.trim();
    let valid = !id.is_empty() && omnicrawl_controllers::memory::is_valid_session_id(id);
    options.memory.session_enabled = valid;
    options.memory.session_id = valid.then(|| id.to_string());
}












// ---- 管理面：运行态、会话、项目、提示历史、Skill、MCP、记忆 ---------------------
//
// 对应 Python `AgentAPIService.agent` 上的同名入口。差别是这里没有常驻 Agent 实例：
// 会话与项目按 `options.session_root` 现开现读（文件即事实来源），多个进程共享同一份
// 存储时不需要在内存里维护副本。内核仍持有当前会话的模型上下文，因此「新建/恢复/
// 压缩会话」这类要动内核的操作不在这里，见 crate README 的迁移表。




















#[cfg(test)]
mod tests;
