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
use omnicrawl_controllers::memory::user_data_root;
use omnicrawl_controllers::settings::{
    context_compaction_trigger_tokens, kernel_compaction_settings,
};
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_controllers::vision_proxy::vision_proxy_configured;
use omnicrawl_extensions::skill::SkillManager;
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::plugins::PluginHost;
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
    SessionStoreError,
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
    /// 插件运行期：进程级 `PluginRuntime` 的持有者；`None` 表示本进程不装插件。
    pub plugins: Option<Arc<PluginHost>>,
    /// 审查运行期（`approval.mode = review` 时用）；`None` 表示没有审查运行期，
    /// 此时需审查的调用按 fail-closed 拒绝。
    pub review: Option<omnicrawl_host::review::ReviewOptions>,
    /// 提问托管运行期（`[decision_models.features] ask_user_custody`）；`None` 表示不托管，
    /// 有选项的提问照旧等 HTTP 提交的答案。
    pub custody: Option<omnicrawl_host::tools::CustodyOptions>,
    /// 工具调用淘汰运行期（`[decision_models.features] tool_call_prune`）；`None` 表示不淘汰，
    /// 「刚变老」的那一批调用照旧全部留在上下文里。
    pub prune: Option<omnicrawl_host::tools::PruneOptions>,
    /// 工具建议运行期（`[decision_models.features] tool_suggestion`）；`None` 表示不建议，
    /// user 原文照发。
    pub suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    /// 子代理建议运行期（`[decision_models.features] subagent_suggestion`）；与工具建议互相独立，
    /// 两者同时开启时并发发起，总耗时取较大者而不是相加。
    pub subagent_suggest: Option<omnicrawl_host::tools::SuggestOptions>,
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
            plugins: None,
            review: None,
            custody: None,
            prune: None,
            suggest: None,
            subagent_suggest: None,
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
        let spawner = KernelSpawn {
            program: kernel_program.to_path_buf(),
            runner: runner_options(&options),
            registry_options: options.registry_options.clone(),
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
        // 插件在服务装配完成后启动：Worker 起来之后才能接住第一个回合的 Hook。
        service.start_plugins(&session_id);
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
        // `session.close.before`：内核据此在补写 `session_closed` 与丢弃空占位前
        // 得到一次观察机会。会话 id 未知（内核未回填）时跳过。
        let session_id = self.current_session_id();
        let plugins = self.options.plugins.clone();
        let notify_close = |before: bool| {
            if session_id.is_empty() {
                return;
            }
            if let Some(plugins) = plugins.as_ref() {
                plugins.session_close(&session_id, before);
            }
        };
        notify_close(true);
        let Ok(mut guard) = self.runner.lock() else {
            return;
        };
        if let Some(runner) = guard.as_mut() {
            runner.shutdown();
            // 等内核把 `session_closed` 落盘后进程退出，再发 after——否则通知会早于事件。
            runner.wait_for_exit(SHUTDOWN_WAIT);
        }
        *guard = None;
        notify_close(false);
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
mod plugins;
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

/// 提问种类：工具参数里的 `kind` 优先，缺省按有没有选项判。
fn question_kind(arguments: &Map<String, Value>, options: &[String]) -> &'static str {
    match arguments.get("kind").and_then(Value::as_str) {
        Some("select") => "select",
        Some("confirm") => "confirm",
        Some("question") => "question",
        _ => {
            if options.is_empty() {
                "question"
            } else {
                "select"
            }
        }
    }
}

/// `update_todos` 的清单投影（最多 20 项，空步骤跳过）。
fn todo_snapshots(raw: &Value) -> Vec<TodoSnapshot> {
    let Some(items) = raw.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .take(20)
        .filter_map(|item| {
            let object = item.as_object()?;
            let step = object.get("step").and_then(Value::as_str)?.trim();
            if step.is_empty() {
                return None;
            }
            Some(TodoSnapshot {
                id: object
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                step: step.to_string(),
                completed: object
                    .get("completed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// 配置值 → 宿主运行期的审批模式（启动与热应用共用同一套映射）。
///
/// `review`（自动审查）在这里保持 `Review`：宿主执行层的审查闸按它判定哪些调用要交给审查模型。
fn runtime_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        APPROVAL_MODE_AUTO => Some(ApprovalMode::Auto),
        APPROVAL_MODE_REVIEW => Some(ApprovalMode::Review),
        APPROVAL_MODE_MANUAL => Some(ApprovalMode::Manual),
        _ => None,
    }
}

/// 从内核 stderr 抓「会话已就绪」行里的会话 ID。
pub fn parse_session_ready(line: &str) -> Option<String> {
    let marker = "会话已就绪：";
    let index = line.find(marker)?;
    let rest = line[index + marker.len()..].trim();
    let session_id: String = rest.chars().take_while(|ch| !ch.is_whitespace()).collect();
    if session_id.is_empty() {
        return None;
    }
    Some(session_id)
}

/// 内核可执行文件：显式路径 > `$OMNICRAWL_BINARY` > 与当前二进制同目录的 `omnicrawl` > PATH。
pub fn resolve_kernel_program(env: &ConfigEnvironment, explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let from_env = env.get_trimmed("OMNICRAWL_BINARY");
    if !from_env.is_empty() {
        return PathBuf::from(from_env);
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(directory) = current.parent() {
            let sibling = directory.join(if cfg!(windows) {
                "omnicrawl.exe"
            } else {
                "omnicrawl"
            });
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("omnicrawl")
}

/// 从本地配置与进程环境组装服务选项（对应 Python 的 `create_default_agent`）。
///
/// 模型与审批模式读 `config.toml`（`llm` / `approval` 段），会话根取工作区下的
/// `.agent_sessions`（与 Python 同一套布局）。系统提示词与上下文消息走 `PromptRuntime`
/// 装配（模板 / AGENTS.md / Skill / 运行环境），装不出来才回落到配置文本。
pub fn options_from_process(env: &ConfigEnvironment) -> Result<ServiceOptions, String> {
    options_from_process_profile(env, true)
}

/// 连接器宿主的服务选项：与 API 同源，但**不挂隔离工作区**。
///
/// 远程连接器改的是用户当前那个工作区，会话也在同一份目录里；再套一层 worktree 会让
/// 「远程改的文件」落在隔离区、退出才 apply，与本地工作区看到的不是同一份事实。
pub fn connector_options_from_process(env: &ConfigEnvironment) -> Result<ServiceOptions, String> {
    options_from_process_profile(env, false)
}

fn options_from_process_profile(
    env: &ConfigEnvironment,
    isolate: bool,
) -> Result<ServiceOptions, String> {
    let llm = load_llm_config(env).map_err(|error| error.to_string())?;
    if llm.model.trim().is_empty() {
        return Err(
            "未配置模型：请在 ~/.OmniCrawl/config.toml 的 llm 段或 models.toml 里选择模型。"
                .to_string(),
        );
    }
    let context = detect_project_context(env, None);
    let main_workspace = context.workspace_root;
    // 主 Agent 隔离工作区（对映 Python `api/app.py::create_default_agent`）：多个进程并行时各自在
    // 独立 worktree / 目录副本里读写，互不写穿；创建失败仅告警并回退主工作区。
    let (workspace, isolation) = if isolate {
        let workspace_config = load_agent_workspace_config(env, None).unwrap_or_default();
        let prepared = prepare_isolated_workspace(
            IsolationOptions::new(&main_workspace).with_config(workspace_config),
        );
        // 启动清扫（后台）：回收上次崩溃 / 被强杀遗留的过期隔离区。
        start_background_isolation_sweep(None);
        prepared
    } else {
        (main_workspace.clone(), None)
    };
    let approval = load_approval_mode(env, None)
        .map_err(|error| error.to_string())
        .and_then(|mode| {
            runtime_approval_mode(&mode).ok_or_else(|| format!("approval.mode 取值非法：{mode}"))
        })?;
    let system_prompt = if llm.system_prompt.trim().is_empty() {
        DEFAULT_SYSTEM_PROMPT.to_string()
    } else {
        llm.system_prompt.clone()
    };
    // 提示词装配：与 TUI 同源（模板 + AGENTS.md + Skill 索引 + 运行环境 + 模式切换）。
    // 装配成功时它以 `RunnerOptions.prompt` 覆盖 system prompt 与上下文消息；下面那份
    // 文本只作为装不出来时的兜底，避免 API 与 TUI 两头各有一套提示词口径。
    //
    // **不**把 `llm.system_prompt` 当显式覆盖：那份文案是语音客户端的默认值
    // （`config/models/llm.py` 的 `LLMConfig.system_prompt`），Python 的 Agent 从来不读它，
    // 提示词一律来自包内模板。
    let prompt = build_prompt_runtime(env, &workspace, &llm.model);
    let system_prompt = match prompt.as_ref() {
        Some(runtime) => runtime.system_prompt(),
        None => system_prompt,
    };
    let model = KernelModelConfig {
        model: llm.model.clone(),
        provider: llm.provider.clone(),
        protocol: llm.protocol.clone(),
        base_url: llm.base_url.clone(),
        api_key: llm.api_key.clone(),
        user_agent: if llm.user_agent.trim().is_empty() {
            USER_AGENT.to_string()
        } else {
            llm.user_agent.clone()
        },
        system_prompt,
        // system 之外的上下文消息由运行器按提示词装配结果覆盖。
        context_messages: Vec::new(),
        // 工具声明由运行器用工具表覆盖。
        tools: Vec::new(),
        options: generation_options(&llm),
        request_timeout_seconds: (llm.request_timeout_seconds > 0)
            .then_some(llm.request_timeout_seconds as f64),
        context_window_tokens: llm.context_window_tokens,
        // 自定义模型条目声明的 prompt 缓存能力；未声明时只对 GPT 系列回退尝试。
        prompt_cache_capable: llm.prompt_cache.unwrap_or(false),
        prompt_cache_identity: Default::default(),
        // 原生视觉的优先级由内核判定（`route_image_result` 同顺序）：为真时不走代理。
        // 传三态而不是布尔：未配置时留给内核按模型能力回落，压成 `false` 会误走代理。
        native_vision: llm.native_vision,
        vision_capability: llm.vision_capability,
        request_retry_count: llm.request_retry_count.max(1) as u32,
    };
    let mut options = ServiceOptions::new(workspace.clone(), model);
    // 会话根与 Python 一致：`~/.OmniCrawl/.agent_sessions`（不绑工作区），切换工作区不必搬会话。
    options.session_root = Some(user_config_dir(env).join(".agent_sessions"));
    options.approval = approval;
    options.native_vision = llm.native_vision.unwrap_or(false);
    options.prompt = prompt;
    options.env = env.clone();
    // MCP 是增量能力：配置读不到只告警；管理器建好后同时进工具表与 `/mcp` 状态。
    let mcp = build_mcp_manager(env, &options.workspace_root);
    options.registry_options.mcp = mcp.clone();
    options.mcp = mcp;
    // 后台任务管理器同理：工具表与 `/monitors` 必须看到同一批任务。
    let monitors = MonitorManager::new(options.workspace_root.clone());
    options.registry_options.monitors = Some(monitors.clone());
    // 记忆工具整组进出：与 Python `entry.py` 的 `load_feature_enabled("memory", default=True)`
    // 同源，缺省开启（本处早先没有设置，`RegistryOptions::default()` 让记忆整组永远不进表，
    // 而 `/settings` 的 `features.memory` 又报 true——两侧会自相矛盾）。
    // 作用域对齐 Python 的 `_create_memory_stores`：项目级与用户级常开；会话级不开——
    // 这里的会话每个 run 现开现关，`RegistryOptions` 是进程级共享的，塞不进某个 run 的 session id。
    options.registry_options.memory_enabled =
        load_feature_enabled(env, "memory", true, None, None).unwrap_or(true);
    options.registry_options.memory = MemoryOptions {
        project_enabled: true,
        user_enabled: true,
        session_enabled: false,
        rerank: Arc::new(omnicrawl_host::tools::rerank_options_from_config(
            env,
            DECISION_SWITCH_MEMORY_SEARCH,
        )),
        embedding: Arc::new(
            omnicrawl_host::embedding_client::embedding_options_from_config(
                env,
                load_feature_enabled(env, "memory_embedding", false, None, None).unwrap_or(false),
            ),
        ),
        ..MemoryOptions::default()
    };
    // 知识库检索重排：与 TUI 同源，按 `[decision_models.features] kb_search_rerank` 装配。
    options.registry_options.knowledge_rerank = Arc::new(
        omnicrawl_host::tools::rerank_options_from_config(env, DECISION_SWITCH_KB_SEARCH),
    );
    // Skill 检索重排：同一装配口径（`[decision_models.features] skill_search_rerank`）。
    options.registry_options.skill_rerank = Arc::new(
        omnicrawl_host::tools::rerank_options_from_config(env, DECISION_SWITCH_SKILL_SEARCH),
    );
    // 工具开关（`[tools]` 段）同样在启动期装载：若这里不装，`config.toml` 里关掉的
    // 内置工具（如 `powershell`）启动后仍会照旧进表，模型可见即可调用，开关形同虚设。
    options.registry_options.disabled_tools = load_disabled_tools(env, None).unwrap_or_default();
    // 联网搜索同样在启动期装载：`RegistryOptions::default()` 的搜索选项是「默认引擎 duckduckgo +
    // 无凭据」，这里不装的话 `[web_search]` 里选的 tavily/exa/firecrawl/anysearch/tinyfish 与各自的 API Key 全部
    // 失效，工具会一直报「来源：duckduckgo」（与 TUI 同源，见 `web_search_options_from_config`）。
    // 该装配函数同时按 `[decision_models.features] web_search_rerank` 装上排序运行期。
    options.registry_options.web_search =
        omnicrawl_host::tools::web_search_options_from_config(env);
    // 图像生成同理：默认选项是「未启用 + 无凭据」，不装的话 `[image_gen]` 里配好的服务商、
    // 接口地址、模型与明文 api_key 全部失效（与 TUI 同源，见 `image_gen_options_from_config`）。
    options.registry_options.image_gen = omnicrawl_host::tools::image_gen_options_from_config(env);
    // TLS 会话恢复同理：`[tls_resumption] enabled` 为真时才建缓存，否则 `fetcher`
    // 永远拿不到 `Some`，配置段写了也没人读（会话恢复默认关闭，改的是密钥调度）。
    options.registry_options.fetcher.tls_session_cache =
        omnicrawl_host::tools::tls_session_cache_from_config(env);
    options.monitors = Some(monitors);
    // 插件运行期：即使总开关关着也建句柄，设置页才能在运行期把它打开。
    options.plugins = Some(Arc::new(PluginHost::from_environment(
        env,
        &options.workspace_root,
    )));
    // 审查运行期：与 TUI 同源（`approval.review_model` 为空时回落主模型，凭据与基地址
    // 沿用主渠道，脱敏旁路按 `[desensitization]` 配置构造）。
    options.review = build_review_options(&llm, env);
    // 提问托管：与 TUI 同源（`[decision_models.features] ask_user_custody`）。
    options.custody = Some(omnicrawl_host::tools::custody_options_from_config(env));
    // 工具调用淘汰：与 TUI 同源（`[decision_models.features] tool_call_prune`）。
    options.prune = Some(omnicrawl_host::tools::prune_options_from_config(env));
    // 建议：与 TUI 同源（`[decision_models.features] tool_suggestion` 与 `subagent_suggestion`）。
    options.suggest = Some(omnicrawl_host::tools::suggest_options_from_config(env));
    options.subagent_suggest = Some(omnicrawl_host::tools::subagent_suggest_options_from_config(
        env,
    ));
    options.isolation = Mutex::new(isolation);
    Ok(options)
}

/// 按配置与主模型视图装配审查运行期（与 TUI 同源：`omnicrawl_host::review`）。
///
/// 审查通道按 `[decision_models.features] tool_call_review` 在对话模型与结构化决策模型之间选；
/// 没有可用审查模型时返回 `None`（需审查的调用会 fail-closed 拒绝）。
fn build_review_options(
    llm: &LlmConfig,
    env: &ConfigEnvironment,
) -> Option<omnicrawl_host::review::ReviewOptions> {
    omnicrawl_host::review::review_options_from_config(llm, env)
}

/// `GenerationOptions` 的 JSON 形状：只带明确配置过的项。
fn generation_options(llm: &LlmConfig) -> Value {
    let mut map = Map::new();
    if let Some(temperature) = llm.temperature {
        map.insert("temperature".to_string(), json!(temperature));
    }
    if !llm.reasoning_effort.trim().is_empty() {
        map.insert("reasoning_effort".to_string(), json!(llm.reasoning_effort));
    }
    if llm.max_output_tokens > 0 {
        map.insert(
            "max_output_tokens".to_string(),
            json!(llm.max_output_tokens),
        );
    }
    if !llm.provider_options.is_empty() {
        map.insert("provider_options".to_string(), json!(llm.provider_options));
    }
    Value::Object(map)
}

fn runner_options(options: &ServiceOptions) -> RunnerOptions {
    RunnerOptions {
        workspace_root: options.workspace_root.clone(),
        model: options.model.clone(),
        session: options.session_root.as_ref().map(|root| {
            let (memory_root, compaction) = session_config_defaults(options);
            KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                // 新会话：内核建好后经 stderr 报 ID，本进程后续回合沿用同一个内核连接。
                session_id: String::new(),
                memory_root,
                workspace_root: Some(options.workspace_root.to_string_lossy().to_string()),
                compaction,
            }
        }),
        approval: options.approval,
        command_timeout_seconds: options.command_timeout_seconds,
        tool_timeout_seconds: options.tool_timeout_seconds,
        // 交不交图与「内核走不走代理」是两件事：原生视觉或 `[vision]` 代理任一可用都要把
        // 图片交出去，内核再按优先级决定直送主模型还是交给代理。
        attach_vision_images: options.native_vision || vision_proxy_configured(&options.env),
        client_name: "omnicrawl-api".to_string(),
        plugins: options.plugins.clone(),
        // 审查运行期与 `options_from_process` 装配的那份同源（嵌入与测试可直接填 `None`）。
        review: options.review.clone(),
        // 提问托管同源：与审查一样走决策渠道，失败一律退回人工提问。
        custody: options.custody.clone(),
        // 工具调用淘汰同源：与审查一样走决策渠道，失败一律保留原文。
        prune: options.prune.clone(),
        // 建议同源：提交前问一次决策模型该用哪些工具／哪些子代理，失败一律原文照发。
        suggest: options.suggest.clone(),
        subagent_suggest: options.subagent_suggest.clone(),
        // 提示词装配结果：无头宿主与 TUI 共用同一套模板 / AGENTS.md / Skill 装配。
        prompt: options.prompt.clone(),
    }
}

/// 内核会话块里与配置有关的两项：记忆根与压缩策略。
///
/// 两者都必须由宿主下发：内核读不到用户配置，缺省就是 `CompactionConfig::default()`
/// （摘要预算 2000 token、记忆回写关闭），于是 `[context_compaction]` / `[memory]`
/// 就成了「配置写了却不生效」。启动与重起内核必须用同一份口径，否则切换会话或工作区后
/// 策略会悄悄退回默认值。
fn session_config_defaults(
    options: &ServiceOptions,
) -> (Option<String>, Option<KernelCompactionConfig>) {
    let env = &options.env;
    // 开关与工具表同源（`[memory] enabled`，缺省开启）：关掉记忆就不该再写记忆目录。
    let memory_root = load_feature_enabled(env, "memory", true, None, None)
        .unwrap_or(true)
        .then(|| user_data_root(env.home()).to_string_lossy().to_string());
    let compaction = match load_context_compaction_config(env, None) {
        Ok(config) => Some(kernel_compaction_settings(
            options.model.context_window_tokens,
            &config,
        )),
        Err(error) => {
            // 无头宿主没有界面可提示，只能落到 stderr；降级为内核默认而不是让启动失败。
            eprintln!(
                "[api] 上下文压缩配置读取失败：{}（本会话按内核默认压缩策略运行）",
                error.message()
            );
            None
        }
    };
    (memory_root, compaction)
}

/// 窗口变化后要下发的压缩阈值：只带这两个字段，其余字段由内核保留现值。
fn window_compaction_settings(
    options: &ServiceOptions,
    window: i64,
) -> Option<KernelCompactionConfig> {
    let config = load_context_compaction_config(&options.env, None).ok()?;
    let percent = config.trigger_context_percent?;
    Some(KernelCompactionConfig {
        trigger_context_tokens: Some(context_compaction_trigger_tokens(window, percent)),
        context_window_tokens: Some(window),
        ..KernelCompactionConfig::default()
    })
}

/// 提示词里的临时目录展示路径：优先用 `[agent_temp] directory` 的实际落点。
fn agent_temp_dir_display(env: &ConfigEnvironment, workspace: &Path) -> String {
    let config =
        omnicrawl_workspace::load_agent_temp_workspace_config(env, None).unwrap_or_default();
    if !config.enabled {
        return omnicrawl_host::prompt::DEFAULT_AGENT_TEMP_DIR.to_string();
    }
    let resolved = omnicrawl_workspace::resolve_agent_temp_dir(workspace, &config.directory)
        .unwrap_or_else(|_| workspace.join(&config.directory));
    match resolved.strip_prefix(&omnicrawl_workspace::resolve_path(workspace)) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => resolved.to_string_lossy().to_string(),
    }
}

/// 按工作区装配提示词运行时（与 TUI 的 `load_prompt_runtime` 同一套输入）。
///
/// `system_prompt_override` 恒为 `None`：API 没有「命令行提示词」这个概念，`config.toml`
/// 的 `llm.system_prompt` 也不参与 Agent 提示词（Python 侧只给语音客户端用）。
/// 装配失败时返回 `None` 并告警，由调用方沿用配置文本——无头服务不该因为提示词
/// 装不出来就起不来。
fn build_prompt_runtime(
    env: &ConfigEnvironment,
    workspace: &Path,
    model: &str,
) -> Option<Arc<PromptRuntime>> {
    let mut options = PromptOptions::new(workspace.to_path_buf());
    // 临时目录取 `[agent_temp] directory` 的实际落点：提示词里给模型的路径必须与
    // 宿主/工具真正落盘的位置一致，否则模型按提示词去找文件会找不到。
    options.agent_temp_dir = agent_temp_dir_display(env, workspace);
    options.workspace_detection_summary = detect_project_context(env, None).detection_summary();
    options.system_prompt_override = None;
    // 顾问准则只在顾问真正可用（已启用且未命中黑名单）时追加，与 Python 同判据。
    let advisor =
        omnicrawl_config::features::advisor::load_advisor_config(env, None).unwrap_or_default();
    options.advisor_active = advisor.enabled;
    options.advisor_blacklisted = advisor.disabled_for_models.iter().any(|name| name == model);
    match PromptRuntime::load(env, options) {
        Ok(runtime) => Some(Arc::new(runtime)),
        Err(error) => {
            eprintln!("[api] 提示词装配失败，改用配置里的系统提示词：{error}");
            None
        }
    }
}

// ---- 管理面：运行态、会话、项目、提示历史、Skill、MCP、记忆 ---------------------
//
// 对应 Python `AgentAPIService.agent` 上的同名入口。差别是这里没有常驻 Agent 实例：
// 会话与项目按 `options.session_root` 现开现读（文件即事实来源），多个进程共享同一份
// 存储时不需要在内存里维护副本。内核仍持有当前会话的模型上下文，因此「新建/恢复/
// 压缩会话」这类要动内核的操作不在这里，见 crate README 的迁移表。


/// 按展示名派生项目目录名（与 Python `project_directory_name` 同规则）。
fn project_directory_name(name: &str) -> String {
    let mut cleaned = String::new();
    for ch in name.trim().chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            cleaned.push('-');
        } else {
            cleaned.push(ch);
        }
    }
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<&str>>().join("-");
    let trimmed = collapsed.trim_matches(|ch: char| ch == ' ' || ch == '.' || ch == '-');
    if trimmed.is_empty() {
        "new-project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// MCP 管理器：配置读不到或未启用时按空表处理，工具表与状态面共用同一个句柄。
fn build_mcp_manager(env: &ConfigEnvironment, workspace: &Path) -> Option<Arc<McpClientManager>> {
    let config = match omnicrawl_mcp::load_mcp_config(env, None) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[api] MCP 配置读取失败，已跳过 MCP：{error}");
            return None;
        }
    };
    let manager = Arc::new(McpClientManager::new(config, workspace));
    // 与 TUI 同口径：有缓存清单就直接声明，首次调用某个 Server 的工具时再连接。
    if !manager.has_cached_capabilities() {
        manager.discover();
    }
    for diagnostic in manager.diagnostics() {
        let server = diagnostic.server_name.clone().unwrap_or_default();
        eprintln!(
            "[api] MCP [{}] {}: {}",
            diagnostic.severity, server, diagnostic.message
        );
    }
    Some(manager)
}

/// 会话系统未启用。
fn session_disabled() -> ApiError {
    ApiError::new(
        "SESSION_DISABLED",
        "会话系统未启用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}

/// 后台任务管理器缺位。
fn monitor_unavailable() -> ApiError {
    ApiError::new(
        "MONITOR_UNAVAILABLE",
        "后台监控不可用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}

/// 未知后台任务。
fn monitor_not_found() -> ApiError {
    ApiError::new(
        "MONITOR_NOT_FOUND",
        "后台任务不存在。",
        StatusCode::NOT_FOUND,
        None,
    )
}

/// 内核正忙（回合在途，运行期设置要等回合结束才能被内核处理）。
fn kernel_busy(message: &str) -> ApiError {
    ApiError::new("RUN_ACTIVE", message, StatusCode::CONFLICT, None)
}

/// 内核重起失败（切换会话 / 工作区 / 压缩）。
fn kernel_restart_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "KERNEL_RESTART_FAILED",
        message.into(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}

/// 当前服务不持有内核启动信息（嵌入模式不支持运行期切换会话）。
fn kernel_restart_unavailable() -> ApiError {
    ApiError::new(
        "KERNEL_RESTART_UNAVAILABLE",
        "当前服务未持有内核启动信息，不能在运行期切换会话或工作区。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}

/// 内核运行期命令失败（压缩等）。
fn kernel_command_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "KERNEL_COMMAND_FAILED",
        message.into(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}

/// 提示词装配失败（模式切换要求重新装配 system prompt）。
fn prompt_failed(message: impl Into<String>) -> ApiError {
    ApiError::new(
        "PROMPT_DISABLED",
        format!("提示词装配失败：{}", message.into()),
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}

/// 连接器进程读到的 `config.toml`：读不到就按空对象处理。
///
/// 与内核连接器模式同口径（`omnicrawl-cli` 的 `connector_config_data`）：`[telegram]` /
/// `[feishu]` 段由连接器自己解析，配置读取失败只意味着「未配置」，不该让进程起不来。
pub fn connector_config_data(env: &ConfigEnvironment) -> Value {
    omnicrawl_config::core::runtime::load_config_data(env, None)
        .map(|table| omnicrawl_config::value::toml_to_json_object(&table))
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
}

/// SubAgent 后台任务不可用（功能未启用）。
fn subagent_unavailable() -> ApiError {
    ApiError::new(
        "SUBAGENT_UNAVAILABLE",
        "SubAgent 后台任务不可用。",
        StatusCode::SERVICE_UNAVAILABLE,
        None,
    )
}

/// 内核回执里的 `unavailable` 标记。
fn subagent_unavailable_in(value: &Value) -> bool {
    value
        .get("unavailable")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 等内核经 stderr 报出的会话 ID；等不到按空值返回（ID 未知不影响回合）。
fn wait_for_session_id(ready: &Arc<Mutex<String>>) -> String {
    for _ in 0..40 {
        if let Ok(guard) = ready.lock() {
            if !guard.is_empty() {
                return guard.clone();
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    String::new()
}

/// 模型列表探测失败（Python `MODEL_LIST_FAILED`）。
fn model_list_failed(error: omnicrawl_config::ConfigError) -> ApiError {
    ApiError::new(
        "MODEL_LIST_FAILED",
        error.message(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}

/// 双列目录构建失败（Python `MODEL_CATALOG_FAILED`）。
fn model_catalog_failed(error: omnicrawl_config::ConfigError) -> ApiError {
    ApiError::new(
        "MODEL_CATALOG_FAILED",
        error.message(),
        StatusCode::BAD_GATEWAY,
        None,
    )
}

/// 非空字符串转 `Option`；空串按「不改」处理（协议语义与内核一致）。
fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 会话与项目存储错误 → API 错误。
///
/// Python 侧这些异常会穿过 `APIServiceError` 处理器落到通用 500；这里按「资源缺失 → 404、
/// 其余 → 400」收紧，错误文案保持原样（已知差异见 crate README）。
fn session_store_error(error: SessionStoreError) -> ApiError {
    let message = error.message().to_string();
    let missing = message.starts_with("未找到会话")
        || message.starts_with("project 不存在")
        || message.starts_with("artifact 不存在")
        || message.starts_with("会话不存在");
    if missing {
        ApiError::new("NOT_FOUND", message, StatusCode::NOT_FOUND, None)
    } else {
        ApiError::new("INVALID_REQUEST", message, StatusCode::BAD_REQUEST, None)
    }
}

#[cfg(test)]
mod tests;
