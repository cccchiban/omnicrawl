//! Agent 服务编排：单活动运行的宿主驱动、审批/提问的阻塞等待与事件翻译。
//!
//! 对应 `omnicrawl/api/service.py` 的 `AgentAPIService`。差别只在宿主形态：Python 侧
//! Agent 就在本进程，这里由 `omnicrawl-host` 起内核子进程、用协议 v1 驱动一个回合，
//! 并把内核通知翻成 `omnicrawl/docs/API.md` 列出的 SSE 事件。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use omnicrawl_config::core::context::detect_project_context;
use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_config::features::agent_workspace::load_agent_workspace_config;
use omnicrawl_config::features::approval::{
    load_approval_mode, APPROVAL_MODE_AUTO, APPROVAL_MODE_REVIEW,
};
use omnicrawl_config::models::llm::{load_llm_config, LlmConfig};
use omnicrawl_config::models::model_catalog::{
    build_catalog, detect_model_options, ensure_current_model_option, Catalog, CatalogPorts,
    CatalogRequest, DiscoveryCache, ModelOption, MODEL_LIST_TIMEOUT_SECONDS,
};
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_extensions::skill::SkillManager;
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::plugins::PluginHost;
use omnicrawl_host::tools::monitor::{MonitorManager, MonitorPollView, MonitorTaskView};
use omnicrawl_host::tools::RegistryOptions;
use omnicrawl_host::turn::{Interactor, RunnerOptions, TurnControl, TurnError, TurnRunner};
use omnicrawl_ipc::bridge::{
    HostEvent, KernelModelConfig, KernelSessionConfig, SessionModelSettings, SessionSettingsParams,
    SubagentEventPayload,
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
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, finalize_subagent_worktrees, prepare_isolated_workspace,
    start_background_isolation_sweep, IsolationOptions, IsolationSession,
};
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
    /// 模型发现缓存：跨请求存活，`POST /models/refresh` 清空它。
    pub discovery_cache: Arc<DiscoveryCache>,
    /// 内核可执行文件：运行期重起内核（切换会话 / 工作区）需要它；嵌入模式可以不给。
    pub kernel_program: Option<PathBuf>,
    /// 主 Agent 隔离区会话：`close()` 时 apply 变更 + 按策略清理（对映 Python `attach_isolation_session`）。
    /// 用 `Mutex` 承载是为了在 `&self` 的收尾路径里取走它，保证只收尾一次。
    pub isolation: Mutex<Option<IsolationSession>>,
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
            discovery_cache: Arc::new(DiscoveryCache::new()),
            kernel_program: None,
            isolation: Mutex::new(None),
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
    /// 服务是否已关闭（事件泵据此退出）。
    closed: AtomicBool,
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
    fn events_after(&self, session_id: &str, cursor: u64) -> Vec<RunEvent> {
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
        let client = KernelClient::spawn_with_stderr(kernel_program, move |line| {
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
        // 插件在服务装配完成后启动：Worker 起来之后才能接住第一个回合的 Hook。
        service.start_plugins(&session_id);
        Ok(service)
    }

    /// 嵌入/测试路径：用外部准备好的运行器。
    pub fn with_runner(options: ServiceOptions, runner: TurnRunner) -> Self {
        let store = Arc::new(RunBackend::memory(
            options.max_retained_runs,
            options.max_events_per_run,
        ));
        let model = options.model.model.clone();
        let workspace = options.workspace_root.clone();
        Self {
            store,
            runner: Mutex::new(Some(runner)),
            control: Mutex::new(None),
            session_id: Mutex::new(String::new()),
            model: Mutex::new(model),
            workspace: Mutex::new(workspace),
            spawner: Mutex::new(None),
            subagent_feed: SubagentFeed::new(),
            closed: AtomicBool::new(false),
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

    /// 受理一个生成任务：登记运行、起回合线程，立刻返回待办视图。
    pub fn start_run(self: &Arc<Self>, message: &str) -> Result<RunRecord, ApiError> {
        let text = message.trim();
        if text.is_empty() {
            return Err(ApiError::bad_request(
                "INVALID_MESSAGE",
                "message 不能为空。",
            ));
        }
        let record = self
            .store
            .create(&random_id(), text, &self.current_session_id())
            .map_err(|error| error.to_api_error())?;
        let run_id = record.run_id.clone();
        let service = Arc::clone(self);
        let message = text.to_string();
        thread::spawn(move || service.execute_run(&run_id, &message));
        Ok(record)
    }

    pub fn get_run(&self, run_id: &str) -> Result<RunRecord, ApiError> {
        self.store.get(run_id).map_err(|error| error.to_api_error())
    }

    pub fn summary(&self, run_id: &str) -> Result<Value, ApiError> {
        Ok(self.get_run(run_id)?.summary())
    }

    /// 读取事件流（`GET /runs/{id}/events` 的每次轮询都走这里）。
    pub fn events_after(&self, run_id: &str, cursor: u64) -> Result<Vec<RunEvent>, ApiError> {
        self.store
            .events_after(run_id, cursor)
            .map_err(|error| error.to_api_error())
    }

    /// 请求取消：置位取消标记并让待决决策得到 409。
    pub fn cancel_run(&self, run_id: &str) -> Result<Value, ApiError> {
        let run = self.get_run(run_id)?;
        if run.status.is_terminal() {
            return Ok(run.summary());
        }
        self.store
            .request_cancel(run_id)
            .map_err(|error| error.to_api_error())?;
        // 多 worker：把取消投递给持有该 Run 的所有者进程（对应 Python `request_cancel`）。
        self.push_shared_decision(run_id, DECISION_CANCEL, "", json!({}));
        if let Some(control) = self.control.lock().ok().and_then(|guard| guard.clone()) {
            control.request_cancel();
        }
        self.summary(run_id)
    }

    /// 提交审批决议；重复决议得到 409，不认识的 id 得到 404。
    pub fn decide_confirmation(
        &self,
        run_id: &str,
        confirmation_id: &str,
        approved: bool,
    ) -> Result<Value, ApiError> {
        self.get_run(run_id)?;
        let confirmation = self
            .store
            .resolve_confirmation(run_id, confirmation_id, approved)
            .map_err(|error| error.to_api_error())?;
        self.push_shared_decision(
            run_id,
            DECISION_CONFIRM,
            confirmation_id,
            json!({"approved": approved}),
        );
        Ok(json!({
            "confirmation_id": confirmation.confirmation_id,
            "approved": approved,
        }))
    }

    /// 提交提问答案；`select` 只能提交已声明的选项。
    pub fn decide_question(
        &self,
        run_id: &str,
        question_id: &str,
        answer: &str,
    ) -> Result<Value, ApiError> {
        self.get_run(run_id)?;
        let question = self
            .store
            .resolve_question(run_id, question_id, answer)
            .map_err(|error| error.to_api_error())?;
        self.push_shared_decision(
            run_id,
            DECISION_ANSWER,
            question_id,
            json!({"answer": answer}),
        );
        Ok(json!({
            "question_id": question.question_id,
            "answer": question.answer,
        }))
    }

    /// 把一条跨进程决策投进共享存储；单进程内存后端上是空操作。
    fn push_shared_decision(&self, run_id: &str, kind: &str, target_id: &str, payload: Value) {
        if let Some(store) = self.store.shared() {
            let _ = store.push_decision(run_id, kind, target_id, payload);
        }
    }

    /// 关闭：请内核退出并回收后台资源，并收尾挂载的隔离工作区。
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.finalize_isolation();
        let Ok(mut guard) = self.runner.lock() else {
            return;
        };
        if let Some(runner) = guard.as_mut() {
            runner.shutdown();
        }
        *guard = None;
    }

    /// 隔离工作区退出收尾：按 `[agent_workspace]` 的 `apply_on_exit` / `cleanup_on_exit`
    /// 处理主隔离区，再按 auto 策略收尾 SubAgent worktree（只清理无变更的）。
    ///
    /// 对映 Python `LocalToolAgent._finalize_attached_isolation`：与 TUI 共用同一收尾路径，
    /// 避免 API / 连接器入口的改动滞留在隔离区。
    fn finalize_isolation(&self) {
        let session = self
            .options
            .isolation
            .lock()
            .ok()
            .and_then(|mut guard| guard.take());
        let mut summaries: Vec<String> = Vec::new();
        if let Some(session) = session {
            let config = load_agent_workspace_config(&self.options.env, None).unwrap_or_default();
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
            eprintln!("[api] [isolation] {}", summaries.join("；"));
        }
    }

    /// 启动回合外事件泵：周期性排空内核通知并投影到会话级事件流。
    ///
    /// 回合内的事件由回合线程的 sink 直接喂；回合外内核仍会推后台任务/审批事件，没有这条泵，
    /// 订阅 `/subagents/events` 的客户端只能等到下一次回合开始才看见它们。
    pub fn start_event_pump(self: &Arc<Self>) {
        let service = Arc::clone(self);
        thread::spawn(move || loop {
            thread::sleep(EVENT_PUMP_INTERVAL);
            if service.closed.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut guard) = service.runner.try_lock() else {
                // 回合在途：事件此刻由回合线程消费，泵不争锁。
                continue;
            };
            let Some(runner) = guard.as_mut() else {
                continue;
            };
            for event in runner.drain_notifications() {
                if let HostEvent::SubagentEvent(payload) = event {
                    service.record_subagent_event(&payload);
                }
            }
        });
    }

    /// 把一条回合外的子代理通知投影到会话级事件流。
    fn record_subagent_event(&self, payload: &SubagentEventPayload) {
        let _ = self.emit_subagent_event("", payload);
    }

    /// 子代理通知 → 会话级事件流，返回公开的 `(事件名, 载荷)`。
    ///
    /// 单进程用进程内会话事件流；多 worker 以共享存储为唯一事实来源，因为产生事件的
    /// Run 可能由别的 worker 持有（对应 Python `_on_subagent_event` 的两条分支）。
    fn emit_subagent_event(&self, run_id: &str, payload: &SubagentEventPayload) -> (String, Value) {
        let (name, data) = self.subagent_event_payload(run_id, payload);
        if let Some(store) = self.store.shared() {
            self.emit_shared_subagent_event(store, &name, &data);
            return (name, data);
        }
        let session_id = self.current_session_id();
        self.subagent_feed.push(
            &session_id,
            &name,
            data.clone(),
            self.options.max_events_per_run,
        );
        (name, data)
    }

    /// 多 worker：后台任务事件写进共享存储的会话级事件流。
    ///
    /// 事件 id 由会话级计数器分配；任务来源优先查 `task_sources`，只有活动运行才能
    /// 建立新映射（与 Python `_on_shared_subagent_event` 同规则）。
    fn emit_shared_subagent_event(&self, store: &SharedRunStore, name: &str, data: &Value) {
        let task_id = data
            .get("task_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let known = if task_id.is_empty() {
            None
        } else {
            store.task_source(&task_id).ok().flatten()
        };
        let (parent_run_id, session_id) = match known {
            Some(source) => source,
            None => {
                let Some(active_id) = self.store.active_run_id() else {
                    return;
                };
                let Ok(record) = self.store.get(&active_id) else {
                    return;
                };
                if !record.status.is_active() {
                    return;
                }
                if !task_id.is_empty() {
                    let _ = store.record_task_source(&task_id, &record.run_id, &record.session_id);
                }
                (record.run_id, record.session_id)
            }
        };
        let Ok(event_id) = store.allocate_subagent_event_id(&session_id) else {
            return;
        };
        let mut payload = data.clone();
        if let Some(map) = payload.as_object_mut() {
            map.insert("session_id".to_string(), json!(session_id));
            map.insert("parent_run_id".to_string(), json!(parent_run_id));
        }
        if store
            .append_subagent_event(&session_id, event_id, name, payload)
            .is_err()
        {
            return;
        }
        if TERMINAL_SUBAGENT_EVENTS.contains(&name) && !task_id.is_empty() {
            let _ = store.drop_task_source(&task_id);
        }
    }

    /// 子代理通知 → 公开事件载荷：值级脱敏 + 补来源字段。
    ///
    /// `run_id` 为空表示这条通知发生在回合之外（后台任务），此时只带 `session_id`。
    fn subagent_event_payload(
        &self,
        run_id: &str,
        payload: &SubagentEventPayload,
    ) -> (String, Value) {
        let session_id = self.current_session_id();
        let mut data = match redact_sensitive_values(&payload.payload) {
            Value::Object(map) => Value::Object(map),
            other => json!({"payload": other}),
        };
        if let Some(map) = data.as_object_mut() {
            map.insert("session_id".to_string(), json!(session_id));
            if !run_id.is_empty() {
                map.insert("run_id".to_string(), json!(run_id));
            }
        }
        (payload.name.clone(), data)
    }

    fn execute_run(self: Arc<Self>, run_id: &str, message: &str) {
        let _ = self.store.set_status(run_id, RunStatus::Running);
        let session_id = self.current_session_id();
        let _ = self.store.append_event(
            run_id,
            "run.started",
            json!({"run_id": run_id, "session_id": session_id}),
        );

        let control = TurnControl::new();
        if let Ok(mut guard) = self.control.lock() {
            *guard = Some(control.clone());
        }
        let mut interactor = ServiceInteractor {
            service: Arc::clone(&self),
            run_id: run_id.to_string(),
            timeout_seconds: self.options.confirmation_timeout_seconds,
        };
        let outcome = {
            let mut guard = match self.runner.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.as_mut() {
                Some(runner) => {
                    let mut sink = |event: HostEvent| self.publish(run_id, event);
                    runner.submit(message, &control, &mut interactor, &mut sink)
                }
                None => Err(TurnError::KernelExited),
            }
        };
        if let Ok(mut guard) = self.control.lock() {
            *guard = None;
        }
        // 回合结束后清掉可能残留的跨进程决策（例如非等待路径上的取消）。
        if let Some(store) = self.store.shared() {
            let _ = store.take_decisions(run_id);
        }

        match outcome {
            Ok(result) => {
                let _ = self
                    .store
                    .finish(run_id, RunStatus::Completed, &result.final_text, "");
                let _ = self.store.append_event(
                    run_id,
                    "run.completed",
                    json!({"run_id": run_id, "result": result.final_text}),
                );
            }
            Err(TurnError::Cancelled) => {
                let _ = self
                    .store
                    .finish(run_id, RunStatus::Cancelled, "", RUN_CANCELLED_MESSAGE);
                let _ = self.store.append_event(
                    run_id,
                    "run.cancelled",
                    json!({"run_id": run_id, "message": RUN_CANCELLED_MESSAGE}),
                );
            }
            Err(error) => {
                // 第三方异常正文不可信：只记日志，对外统一文案。
                eprintln!("[api] run {run_id} 失败：{error}");
                let _ = self
                    .store
                    .finish(run_id, RunStatus::Failed, "", RUN_FAILED_MESSAGE);
                let _ = self.store.append_event(
                    run_id,
                    "run.failed",
                    json!({"run_id": run_id, "message": RUN_FAILED_MESSAGE}),
                );
            }
        }
    }

    /// 内核通知 → 公开 SSE 事件。
    ///
    /// 隐藏推理（`turn.reasoning_delta`）与工具输出增量不进事件流；子代理与清单事件
    /// 带来源（`run_id` / `session_id`），参数一律走安全投影。
    fn publish(&self, run_id: &str, event: HostEvent) {
        let session_id = self.current_session_id();
        let (name, data): (String, Value) = match event {
            HostEvent::Delta(payload) => (
                "assistant.delta".to_string(),
                json!({"delta": payload.text}),
            ),
            HostEvent::Status(payload) => (
                "status.changed".to_string(),
                json!({"message": payload.message}),
            ),
            HostEvent::RetryStatus(payload) => (
                "status.changed".to_string(),
                json!({"message": payload.message}),
            ),
            HostEvent::ProtocolWait => (
                "status.changed".to_string(),
                json!({"message": PROTOCOL_WAIT_STATUS}),
            ),
            HostEvent::StreamRollback => (
                "status.changed".to_string(),
                json!({"message": STREAM_ROLLBACK_STATUS}),
            ),
            HostEvent::TokenUsage(payload) => (
                "usage.updated".to_string(),
                json!({
                    "input_tokens": payload.input_tokens,
                    "output_tokens": payload.output_tokens,
                    "cached_input_tokens": payload.cached_input_tokens,
                }),
            ),
            HostEvent::ToolStarted(payload) => (
                "tool.started".to_string(),
                json!({
                    "step": payload.step,
                    "tool_call_id": payload.call.id,
                    "tool": payload.call.name,
                    "arguments": public_tool_arguments(&payload.call.name, &payload.call.arguments),
                }),
            ),
            HostEvent::ToolFinished(payload) => (
                "tool.completed".to_string(),
                json!({
                    "tool_call_id": payload.call.id,
                    "tool": payload.call.name,
                    "ok": payload.result.ok,
                    "output": payload.result.output,
                    // HTML artifact 正文只留在受会话保护的存储里；Rust 工具结果暂无 artifact。
                    "artifact": {},
                }),
            ),
            HostEvent::TodoUpdate(payload) => {
                let todos = todo_snapshots(&payload.todos);
                let _ = self.store.record_todos(run_id, todos.clone());
                (
                    "todo.updated".to_string(),
                    json!({
                        "run_id": run_id,
                        "session_id": session_id,
                        "todos": todos.iter().map(TodoSnapshot::to_value).collect::<Vec<Value>>(),
                    }),
                )
            }
            HostEvent::SubagentEvent(payload) => {
                // 会话级控制流与回合内事件流看到同一份载荷（参数已脱敏）。
                self.emit_subagent_event(run_id, &payload)
            }
            // 回合收尾由 `submit` 的返回值负责，通知本身不重复成事件。
            HostEvent::TurnFinished(_) => return,
            // 隐藏推理与工具输出增量都不进 SSE。
            HostEvent::ReasoningDelta(_) | HostEvent::ToolOutputUpdate(_) => return,
        };
        let _ = self.store.append_event(run_id, &name, data);
    }
}

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
/// `.agent_sessions`（与 Python 同一套布局）。系统提示词先用配置或兜底文本——
/// AGENTS.md、Skill 与模式提示词的完整组装还没有 Rust 版，见 crate README 的已知缺口。
pub fn options_from_process(env: &ConfigEnvironment) -> Result<ServiceOptions, String> {
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
    let workspace_config = load_agent_workspace_config(env, None).unwrap_or_default();
    let (workspace, isolation) = prepare_isolated_workspace(
        IsolationOptions::new(&main_workspace).with_config(workspace_config),
    );
    // 启动清扫（后台）：回收上次崩溃 / 被强杀遗留的过期隔离区。
    start_background_isolation_sweep(None);
    let approval = match load_approval_mode(env, None)
        .map_err(|error| error.to_string())?
        .as_str()
    {
        APPROVAL_MODE_AUTO => ApprovalMode::Auto,
        // `review`（自动审查）在 API 与 TUI 同源：审查运行期装配好就真跑模型，
        // 没装配好（未配模型/凭据）时需审查的调用按 fail-closed 拒绝。
        APPROVAL_MODE_REVIEW => ApprovalMode::Review,
        _ => ApprovalMode::Manual,
    };
    let system_prompt = if llm.system_prompt.trim().is_empty() {
        DEFAULT_SYSTEM_PROMPT.to_string()
    } else {
        llm.system_prompt.clone()
    };
    let model = KernelModelConfig {
        model: llm.model.clone(),
        provider: llm.provider.clone(),
        protocol: llm.protocol.clone(),
        base_url: llm.base_url.clone(),
        api_key_env: llm.api_key_env.clone(),
        user_agent: if llm.user_agent.trim().is_empty() {
            format!("omnicrawl-api/{}", env!("CARGO_PKG_VERSION"))
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
        prompt_cache_capable: false,
        prompt_cache_identity: Default::default(),
        request_retry_count: llm.request_retry_count.max(1) as u32,
    };
    let mut options = ServiceOptions::new(workspace.clone(), model);
    // 会话根与 Python 一致：`~/.OmniCrawl/.agent_sessions`（不绑工作区），切换工作区不必搬会话。
    options.session_root = Some(user_config_dir(env).join(".agent_sessions"));
    options.approval = approval;
    options.native_vision = llm.native_vision.unwrap_or(false);
    options.env = env.clone();
    // MCP 是增量能力：配置读不到只告警；管理器建好后同时进工具表与 `/mcp` 状态。
    let mcp = build_mcp_manager(env, &options.workspace_root);
    options.registry_options.mcp = mcp.clone();
    options.mcp = mcp;
    // 后台任务管理器同理：工具表与 `/monitors` 必须看到同一批任务。
    let monitors = MonitorManager::new(options.workspace_root.clone());
    options.registry_options.monitors = Some(monitors.clone());
    options.monitors = Some(monitors);
    // 插件运行期：即使总开关关着也建句柄，设置页才能在运行期把它打开。
    options.plugins = Some(Arc::new(PluginHost::from_environment(
        env,
        &options.workspace_root,
    )));
    // 审查运行期：与 TUI 同源（`approval.review_model` 为空时回落主模型，凭据与基地址
    // 沿用主渠道，脱敏旁路按 `[desensitization]` 配置构造）。
    options.review = build_review_options(&llm, env);
    options.isolation = Mutex::new(isolation);
    Ok(options)
}

/// 按配置与主模型视图装配审查运行期（与 TUI 的 `build_review_options` 同源）。
///
/// 模型取 `approval.review_model`，为空时回落主模型；主模型也为空时返回 `None`
/// （没有可用审查模型，需审查的调用会 fail-closed 拒绝）。
fn build_review_options(
    llm: &LlmConfig,
    env: &ConfigEnvironment,
) -> Option<omnicrawl_host::review::ReviewOptions> {
    let review_model = omnicrawl_config::features::approval::load_approval_review_model(env, None)
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
        masking: omnicrawl_host::review::masking_from_config(env).map(Arc::new),
    })
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
        session: options
            .session_root
            .as_ref()
            .map(|root| KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                // 新会话：内核建好后经 stderr 报 ID，本进程后续回合沿用同一个内核连接。
                session_id: String::new(),
                memory_root: None,
                workspace_root: Some(options.workspace_root.to_string_lossy().to_string()),
                compaction: None,
            }),
        approval: options.approval,
        command_timeout_seconds: options.command_timeout_seconds,
        tool_timeout_seconds: options.tool_timeout_seconds,
        native_vision: options.native_vision,
        client_name: "omnicrawl-api".to_string(),
        plugins: options.plugins.clone(),
        // 审查运行期与 `options_from_process` 装配的那份同源（嵌入与测试可直接填 `None`）。
        review: options.review.clone(),
    }
}

// ---- 管理面：运行态、会话、项目、提示历史、Skill、MCP、记忆 ---------------------
//
// 对应 Python `AgentAPIService.agent` 上的同名入口。差别是这里没有常驻 Agent 实例：
// 会话与项目按 `options.session_root` 现开现读（文件即事实来源），多个进程共享同一份
// 存储时不需要在内存里维护副本。内核仍持有当前会话的模型上下文，因此「新建/恢复/
// 压缩会话」这类要动内核的操作不在这里，见 crate README 的迁移表。

impl AgentService {
    /// 当前工作区根：`POST /projects/switch` 之后即为新根。
    pub fn workspace_root(&self) -> PathBuf {
        self.workspace
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| self.options.workspace_root.clone())
    }

    /// 当前模型：启动值，或最近一次运行期切换后的值。
    pub fn current_model(&self) -> String {
        self.model
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 运行期切换成功后同步当前模型。
    fn set_current_model(&self, model: &str) {
        if let Ok(mut guard) = self.model.lock() {
            *guard = model.to_string();
        }
    }

    /// 进程环境：设置面按请求读写 `config.toml`。
    pub fn environment(&self) -> &ConfigEnvironment {
        &self.options.env
    }

    /// MCP 子系统是否启用（管理器缺失按未启用处理）。
    pub fn mcp_enabled(&self) -> bool {
        self.options
            .mcp
            .as_ref()
            .map(|manager| manager.enabled())
            .unwrap_or(false)
    }

    /// 审批模式取 `config.toml` 当前值；读不到时退到启动时解析的结果。
    pub fn approval_mode(&self) -> String {
        match load_approval_mode(&self.options.env, None) {
            Ok(mode) => mode,
            Err(_) => match self.options.approval {
                ApprovalMode::Auto => "auto".to_string(),
                ApprovalMode::Manual | ApprovalMode::Review => "manual".to_string(),
            },
        }
    }

    /// 推理强度取 `config.toml` 当前值；空值与缺失统一报 `none`。
    pub fn reasoning_effort(&self) -> String {
        let effort = load_llm_config(&self.options.env)
            .map(|config| config.reasoning_effort)
            .unwrap_or_default();
        let effort = effort.trim();
        if effort.is_empty() {
            "none".to_string()
        } else {
            effort.to_string()
        }
    }

    /// `GET /runtime` 的响应体。
    pub fn runtime_snapshot(&self) -> Value {
        json!({
            "workspace_root": self.workspace_root().to_string_lossy(),
            "session_id": self.current_session_id(),
            "model": self.current_model(),
            "reasoning_effort": self.reasoning_effort(),
            "approval_mode": self.approval_mode(),
            "active_run_id": self.store.active_run_id(),
        })
    }

    /// 会话存储根；未启用会话系统时报 503。
    pub fn session_root(&self) -> Result<&Path, ApiError> {
        self.options
            .session_root
            .as_deref()
            .ok_or_else(session_disabled)
    }

    fn session_store(&self) -> Result<SessionStore, ApiError> {
        let store = SessionStore::open(self.session_root()?.to_path_buf());
        store.ensure().map_err(session_store_error)?;
        Ok(store)
    }

    fn project_store(&self) -> Result<ProjectStore, ApiError> {
        let store = ProjectStore::open(self.session_root()?);
        store.ensure().map_err(session_store_error)?;
        Ok(store)
    }

    /// `GET /sessions`：`archived` 为真时只看归档，否则只看活跃（归档默认隐藏）。
    pub fn list_sessions(
        &self,
        limit: usize,
        archived: bool,
    ) -> Result<Vec<SessionIndexEntry>, ApiError> {
        let query = SessionListQuery {
            workspace_root: None,
            project_path: None,
            limit,
            include_archived: false,
            archived_only: archived,
        };
        self.session_store()?
            .list_sessions_filtered(&query)
            .map_err(session_store_error)
    }

    /// `GET /sessions/{id}/events`：转录里的有效事件（坏行记诊断后跳过）。
    pub fn session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, ApiError> {
        self.session_store()?
            .read_session_events_with_diagnostics(session_id)
            .map_err(session_store_error)
            .map(|result| result.events)
    }

    /// 会话与提示历史的损坏诊断；`session_id` 为空时只报提示历史总览。
    pub fn session_diagnostics(&self, session_id: Option<&str>) -> Result<Value, ApiError> {
        let store = self.session_store()?;
        let (_entries, prompt_diagnostics) = store
            .prompt_history()
            .read_entries_with_diagnostics()
            .map_err(session_store_error)?;
        let prompt_payload: Vec<Value> = prompt_diagnostics
            .iter()
            .map(SessionRecordDiagnostic::to_dict)
            .collect();
        let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(json!({
                "session_id": Value::Null,
                "event_diagnostics": [],
                "prompt_history_diagnostics": prompt_payload,
            }));
        };
        let result = store
            .read_session_events_with_diagnostics(session_id)
            .map_err(session_store_error)?;
        Ok(json!({
            "session_id": session_id,
            "event_count": result.events.len(),
            "event_diagnostics": result.diagnostics.iter().map(SessionRecordDiagnostic::to_dict).collect::<Vec<Value>>(),
            "prompt_history_diagnostics": prompt_payload,
            "has_errors": result.has_errors(),
        }))
    }

    /// `PATCH /sessions/current`：重命名当前会话。
    pub fn rename_current_session(&self, title: &str) -> Result<SessionIndexEntry, ApiError> {
        let store = self.session_store()?;
        store
            .rename_session(&self.current_session_id(), title, utc_now())
            .map_err(session_store_error)
    }

    /// `DELETE /sessions/{id}`：当前活跃会话不允许删除。
    pub fn delete_session(&self, session_id: &str) -> Result<(), ApiError> {
        if session_id.trim() == self.current_session_id().trim() {
            return Err(ApiError::new(
                "SESSION_ACTIVE",
                "不能删除当前活跃会话，请先切换到其他会话。",
                StatusCode::CONFLICT,
                None,
            ));
        }
        self.session_store()?
            .delete_session(session_id)
            .map_err(session_store_error)
    }

    /// `POST /sessions/current/export`：把客户端渲染好的 Markdown 落进 `exports/`。
    pub fn export_current_session_markdown(&self, markdown: &str) -> Result<PathBuf, ApiError> {
        let store = self.session_store()?;
        store
            .export_session_markdown(&self.current_session_id(), markdown, utc_now())
            .map_err(session_store_error)
    }

    pub fn read_session_artifact_text(
        &self,
        session_id: &str,
        artifact_path: &str,
    ) -> Result<String, ApiError> {
        self.session_store()?
            .read_artifact_text(session_id, artifact_path)
            .map_err(session_store_error)
    }

    /// 重起内核并切到指定会话；空 id 表示开新会话，返回内核报出的会话 id。
    ///
    /// 协议 v1 没有「换会话」命令，而内核的 `KernelSession::open` 已经支持按 id 续跑，
    /// 因此宿主侧的等价做法是重起内核并把目标会话写进 `initialize`。会话目录不随工作区
    /// 变化（与 Python 一致），所以工作区切换保持同一会话。
    pub fn switch_session(
        &self,
        session_id: &str,
        workspace_root: Option<&Path>,
    ) -> Result<String, ApiError> {
        let mut runner_guard = self
            .runner
            .try_lock()
            .map_err(|_| kernel_busy("生成任务进行中，暂不能切换会话。"))?;
        let mut spawner_guard = self
            .spawner
            .lock()
            .map_err(|_| kernel_restart_unavailable())?;
        let Some(spawner) = spawner_guard.as_mut() else {
            return Err(kernel_restart_unavailable());
        };

        // 旧内核先收干净：进程、工具表与后台任务都要在新内核起来之前让出资源。
        if let Some(runner) = runner_guard.as_mut() {
            runner.shutdown();
        }
        *runner_guard = None;

        let workspace = workspace_root
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace_root());
        let session_root = spawner
            .runner
            .session
            .as_ref()
            .map(|session| session.root.clone())
            .or_else(|| {
                self.options
                    .session_root
                    .as_ref()
                    .map(|root| root.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        spawner.runner.workspace_root = workspace.clone();
        spawner.runner.session = Some(KernelSessionConfig {
            root: session_root,
            session_id: session_id.to_string(),
            memory_root: None,
            workspace_root: Some(workspace.to_string_lossy().to_string()),
            compaction: None,
        });

        let ready = Arc::new(Mutex::new(String::new()));
        let slot = Arc::clone(&ready);
        let client = KernelClient::spawn_with_stderr(&spawner.program, move |line| {
            if let Some(found) = parse_session_ready(line) {
                if let Ok(mut guard) = slot.lock() {
                    *guard = found;
                }
            }
        })
        .map_err(|error| kernel_restart_failed(format!("启动内核进程失败：{error}")))?;
        let mut runner = TurnRunner::new(client, spawner.runner.clone(), &spawner.registry_options)
            .map_err(kernel_restart_failed)?;
        runner
            .handshake(&mut |_| {})
            .map_err(|error| kernel_restart_failed(format!("内核握手失败：{error}")))?;
        let opened = wait_for_session_id(&ready);
        *runner_guard = Some(runner);
        drop(spawner_guard);
        drop(runner_guard);

        // 新会话 ID 由内核经 stderr 报出；等不到就沿用请求值（ID 未知不影响回合）。
        let resolved = if opened.is_empty() {
            session_id.to_string()
        } else {
            opened
        };
        if let Ok(mut guard) = self.session_id.lock() {
            *guard = resolved.clone();
        }
        if workspace_root.is_some() {
            if let Ok(mut guard) = self.workspace.lock() {
                *guard = workspace;
            }
        }
        Ok(resolved)
    }

    /// 会话状态视图：索引元数据 + 转录投影 + 运行守护残留（对应 Python `SessionState`）。
    ///
    /// 恢复类端点（`POST /sessions/{id}/resume`）用它回 `pending_user_text` 与 `todo_items`，
    /// 让客户端能在暂停后继续任务。
    pub fn session_state_payload(&self, session_id: &str) -> Result<Value, ApiError> {
        let store = self.session_store()?;
        let entry = store
            .list_sessions()
            .map_err(session_store_error)?
            .into_iter()
            .find(|item| item.session_id == session_id.trim())
            .ok_or_else(|| {
                ApiError::new(
                    "SESSION_NOT_FOUND",
                    format!("未找到会话：{}", session_id.trim()),
                    StatusCode::NOT_FOUND,
                    None,
                )
            })?;
        let events = store
            .read_events(&entry.session_id)
            .map_err(session_store_error)?;
        let (pending_user_text, todo_items) = recover_run_guard_state(&events);
        let messages: Vec<Value> = project_session_history(&events)
            .into_iter()
            .map(|(_role, message)| message)
            .collect();
        Ok(json!({
            "session_id": entry.session_id,
            "title": entry.title,
            "workspace_root": entry.workspace_root,
            "path": entry.path,
            "created_at": format_datetime(entry.created_at),
            "updated_at": format_datetime(entry.updated_at),
            "messages": messages,
            "last_event_type": entry.last_event_type,
            "event_count": entry.event_count,
            "archived_at": entry.archived_at.map(format_datetime),
            "pending_user_text": pending_user_text,
            "todo_items": todo_items,
        }))
    }

    /// `POST /sessions/current/archive`：把当前会话移进归档，返回归档后的索引条目。
    pub fn archive_current_session(&self) -> Result<SessionIndexEntry, ApiError> {
        let store = self.session_store()?;
        store
            .archive_session(&self.current_session_id(), utc_now())
            .map_err(session_store_error)
    }

    /// `POST /sessions/current/compact`：显式压缩当前会话，返回内核回执。
    pub fn compact_current_session(&self) -> Result<Value, ApiError> {
        let mut guard = self
            .runner
            .try_lock()
            .map_err(|_| kernel_busy("生成任务进行中，暂不能压缩会话。"))?;
        match guard.as_mut() {
            Some(runner) => runner.compact_session().map_err(kernel_command_failed),
            None => Err(ApiError::service_unavailable()),
        }
    }

    /// 冻结当前 API 可见的 Session，用于后台事件流（对应 Python `current_subagent_session_id`）。
    ///
    /// 多 worker 下不能取本进程内核的会话：产生事件的 Run 可能由别的 worker 持有，
    /// 取本地会话会让 SSE 订到错误的 Session。改取最近一次 Run 的 Session；尚未跑过
    /// Run 的 worker 退回本进程会话。
    pub fn current_subagent_session_id(&self) -> String {
        if let Some(store) = self.store.shared() {
            if let Ok(Some(record)) = store.latest_run() {
                return record.session_id;
            }
        }
        self.current_session_id()
    }

    /// 指定会话的后台任务事件（游标之后）；`/subagents/events` 的每次轮询都走这里。
    ///
    /// 共享存储是唯一事实来源：所有者与旁观 worker 必须读到同一份事件。
    pub fn subagent_events_after(&self, session_id: &str, cursor: u64) -> Vec<RunEvent> {
        if let Some(store) = self.store.shared() {
            return store
                .subagent_events_after(session_id, cursor)
                .unwrap_or_default();
        }
        self.subagent_feed.events_after(session_id, cursor)
    }

    /// 等到指定会话的事件流有新事件或超时。
    ///
    /// 跨进程没有可用的条件变量（所有者与旁观 worker 不在同一进程），改用短轮询；
    /// 本地单次存在性查询在微秒级，对单机 API 足够。
    pub fn wait_for_subagent_events(&self, session_id: &str, cursor: u64, timeout: Duration) {
        if let Some(store) = self.store.shared() {
            let deadline = Instant::now() + timeout;
            loop {
                if store.has_subagent_events_after(session_id, cursor) {
                    return;
                }
                let now = Instant::now();
                if now >= deadline {
                    return;
                }
                thread::sleep(SHARED_WAIT_POLL.min(deadline - now));
            }
        }
        self.subagent_feed
            .wait_for_events(session_id, cursor, timeout);
    }

    /// `GET /subagents`：当前会话可见的后台任务。
    pub fn list_subagent_tasks(&self) -> Result<Vec<Value>, ApiError> {
        let value = self.subagent_query("list", "")?;
        if subagent_unavailable_in(&value) {
            return Err(subagent_unavailable());
        }
        Ok(value
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// `GET /subagents/{task_id}`：任务不可见时返回 `None`（路由按 404 回答）。
    pub fn get_subagent_task(&self, task_id: &str) -> Result<Option<Value>, ApiError> {
        let value = self.subagent_query("get", task_id)?;
        if subagent_unavailable_in(&value) {
            return Err(subagent_unavailable());
        }
        Ok(value.get("task").cloned().filter(|task| !task.is_null()))
    }

    /// `POST /subagents/{task_id}/cancel`：取消未被受理时返回 `None`（路由按 404 回答）。
    pub fn cancel_subagent_task(&self, task_id: &str) -> Result<Option<Value>, ApiError> {
        let value = self.subagent_query("cancel", task_id)?;
        if subagent_unavailable_in(&value) {
            return Err(subagent_unavailable());
        }
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        Ok(ok.then_some(result))
    }

    /// 一次内核侧后台任务查询；回合在途时快速失败（内核主循环被回合占用）。
    fn subagent_query(&self, action: &str, task_id: &str) -> Result<Value, ApiError> {
        let mut guard = self
            .runner
            .try_lock()
            .map_err(|_| kernel_busy("生成任务进行中，暂不能查询后台任务。"))?;
        match guard.as_mut() {
            Some(runner) => runner
                .manage_subagents(action, task_id)
                .map_err(kernel_command_failed),
            None => Err(ApiError::service_unavailable()),
        }
    }

    /// `GET /monitors`：当前受管的后台任务快照。
    pub fn list_monitors(&self) -> Result<Vec<Value>, ApiError> {
        Ok(self
            .monitors()?
            .tasks()
            .iter()
            .map(MonitorTaskView::to_value)
            .collect())
    }

    /// `GET /monitors/{id}`：未知任务回 404。
    pub fn monitor_task(&self, monitor_id: &str) -> Result<Value, ApiError> {
        self.monitors()?
            .task(monitor_id)
            .map(|task| task.to_value())
            .ok_or_else(monitor_not_found)
    }

    /// 后台任务日志的一轮轮询（SSE 生产者每次循环调用一次）。
    pub fn poll_monitor_events(
        &self,
        monitor_id: &str,
        cursor: u64,
        max_events: usize,
    ) -> Result<MonitorPollView, ApiError> {
        self.monitors()?
            .poll_view(monitor_id, cursor, max_events)
            .ok_or_else(monitor_not_found)
    }

    /// 等到有新日志或任务结束；任务消失时立即返回（SSE 侧据此收尾）。
    pub fn wait_for_monitor_events(&self, monitor_id: &str, cursor: u64, timeout: Duration) {
        if let Ok(monitors) = self.monitors() {
            monitors.wait_for_events(monitor_id, cursor, timeout);
        }
    }

    fn monitors(&self) -> Result<&MonitorManager, ApiError> {
        self.options
            .monitors
            .as_ref()
            .ok_or_else(monitor_unavailable)
    }

    /// `GET /models`：当前 Provider 的模型列表，保证当前模型一定在列表里。
    ///
    /// 端点、请求头与超时由 `omnicrawl-config` 构造，这里只提供发请求的端口。
    pub fn model_options(&self) -> Result<Vec<ModelOption>, ApiError> {
        let llm = load_llm_config(self.environment()).map_err(model_list_failed)?;
        let options = detect_model_options(
            &llm,
            MODEL_LIST_TIMEOUT_SECONDS,
            &model_discovery::fetch_model_list,
        )
        .map_err(model_list_failed)?;
        Ok(ensure_current_model_option(&options, &self.current_model()))
    }

    /// 双列目录；`refresh=true` 时先清空发现缓存再重建（`POST /models/refresh`）。
    pub fn model_catalog(&self, refresh: bool) -> Result<Catalog, ApiError> {
        let env = self.environment();
        let llm = load_llm_config(env).map_err(model_catalog_failed)?;
        if refresh {
            self.options.discovery_cache.clear();
        }
        let request = CatalogRequest {
            config: Some(&llm),
            refresh,
            ..CatalogRequest::default()
        };
        let ports = CatalogPorts {
            discover: &model_discovery::discover_profile,
            cache: &self.options.discovery_cache,
        };
        build_catalog(env, &request, &ports).map_err(model_catalog_failed)
    }

    /// 运行期下发一次 `session.settings`。
    ///
    /// 内核主循环在跑回合时被占用，`session.settings` 要到回合结束才会被处理，因此这里用
    /// `try_lock` 快速失败而不是排队等待——否则 HTTP 请求会挂住整段生成时间。
    pub fn apply_session_settings(&self, params: SessionSettingsParams) -> Result<Value, ApiError> {
        let mut guard = self
            .runner
            .try_lock()
            .map_err(|_| kernel_busy("生成任务进行中，暂不能下发运行期设置。"))?;
        match guard.as_mut() {
            Some(runner) => runner
                .apply_session_settings(params, &mut |_| {})
                .map_err(|message| {
                    ApiError::new(
                        "KERNEL_SETTINGS_FAILED",
                        message,
                        StatusCode::BAD_GATEWAY,
                        None,
                    )
                }),
            None => Err(ApiError::service_unavailable()),
        }
    }

    /// 切换模型：把新模型视图整理成 `session.settings` 下发，成功后同步当前模型。
    ///
    /// 工具声明不下发（切换模型不动工具表）；空字符串按「不改」处理，与协议语义一致。
    pub fn apply_model_switch(&self, llm: &LlmConfig) -> Result<(), ApiError> {
        let settings = SessionModelSettings {
            model: Some(llm.model.clone()),
            options: Some(generation_options(llm)),
            reasoning_effort: non_empty(&llm.reasoning_effort),
            system_prompt: None,
            context_messages: None,
            tools: None,
            context_window_tokens: Some(llm.context_window_tokens),
            provider: non_empty(&llm.provider),
            protocol: non_empty(&llm.protocol),
            base_url: non_empty(&llm.base_url),
            api_key_env: non_empty(&llm.api_key_env),
        };
        self.apply_session_settings(SessionSettingsParams {
            model: Some(Box::new(settings)),
            compaction: None,
        })?;
        self.set_current_model(&llm.model);
        Ok(())
    }

    /// 运行期切换推理强度：只写内核生成选项里的这一个键。
    pub fn apply_reasoning_effort(&self, effort: &str) -> Result<(), ApiError> {
        let settings = SessionModelSettings {
            reasoning_effort: Some(effort.to_string()),
            ..SessionModelSettings::default()
        };
        self.apply_session_settings(SessionSettingsParams {
            model: Some(Box::new(settings)),
            compaction: None,
        })?;
        Ok(())
    }

    /// 运行期切换审批模式（下一次工具批次生效；审批由宿主定调）。
    pub fn set_runtime_approval_mode(&self, mode: ApprovalMode) -> Result<(), ApiError> {
        let mut guard = self
            .runner
            .try_lock()
            .map_err(|_| kernel_busy("生成任务进行中，暂不能切换审批模式。"))?;
        match guard.as_mut() {
            Some(runner) => {
                runner.set_approval_mode(mode);
                Ok(())
            }
            None => Err(ApiError::service_unavailable()),
        }
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectEntry>, ApiError> {
        self.project_store()?
            .list_projects()
            .map_err(session_store_error)
    }

    /// `GET /projects/overview`：显式项目 + 会话索引稳定目录的只读聚合。
    pub fn project_overview(&self) -> Result<Vec<ProjectOverview>, ApiError> {
        let sessions = self
            .session_store()?
            .list_sessions_filtered(&SessionListQuery {
                limit: 100,
                include_archived: true,
                ..SessionListQuery::default()
            })
            .map_err(session_store_error)?;
        let entries: Vec<OverviewSessionEntry> = sessions
            .iter()
            .map(|entry| OverviewSessionEntry {
                workspace_root: Some(entry.workspace_root.clone()),
                updated_at: Some(entry.updated_at),
                session_id: entry.session_id.clone(),
                title: entry.title.clone(),
            })
            .collect();
        self.project_store()?
            .project_overview(&entries, None, 0)
            .map_err(session_store_error)
    }

    /// 创建项目：`path` 为空时落在工作区下按展示名派生的目录（与 Python 同规则）。
    pub fn create_project(&self, name: &str, path: &str) -> Result<ProjectEntry, ApiError> {
        let raw = path.trim();
        let resolved = if raw.is_empty() {
            self.workspace_root()
                .join(project_directory_name(name))
                .to_string_lossy()
                .to_string()
        } else {
            raw.to_string()
        };
        self.project_store()?
            .create_project(name, &resolved, Some(utc_now()))
            .map_err(session_store_error)
    }

    pub fn import_project(&self, name: &str, path: &str) -> Result<ProjectEntry, ApiError> {
        self.project_store()?
            .import_project(name, path, Some(utc_now()))
            .map_err(session_store_error)
    }

    pub fn rename_project(&self, path: &str, name: &str) -> Result<ProjectEntry, ApiError> {
        self.project_store()?
            .rename_project(path, name, Some(utc_now()))
            .map_err(session_store_error)
    }

    pub fn pin_project(&self, path: &str, pinned: bool) -> Result<ProjectEntry, ApiError> {
        self.project_store()?
            .pin_project(path, pinned, Some(utc_now()))
            .map_err(session_store_error)
    }

    pub fn remove_project(&self, path: &str) -> Result<(), ApiError> {
        self.project_store()?
            .remove_project(path)
            .map_err(session_store_error)
    }

    /// `GET /history`：提示历史按时间倒序去重；`current_session_only` 只看当前会话。
    pub fn search_prompt_history(
        &self,
        query: &str,
        limit: i64,
        current_session_only: bool,
    ) -> Result<Vec<PromptHistoryEntry>, ApiError> {
        let session_id = if current_session_only {
            Some(self.current_session_id())
        } else {
            None
        };
        self.session_store()?
            .prompt_history()
            .search(None, session_id.as_deref(), query, limit)
            .map_err(session_store_error)
    }

    /// `GET /skills`：按工作区重新发现 Skill 索引（与 TUI 同源，不缓存到进程）。
    pub fn skills(&self) -> Vec<Map<String, Value>> {
        let mut manager = SkillManager::new();
        let root = self.workspace_root();
        manager.discover(Some(root.as_path()), &[]);
        manager
            .list_all()
            .iter()
            .map(|meta| meta.to_dict())
            .collect()
    }

    /// 插件运行期句柄（未装配插件时为 `None`）。
    pub fn plugins(&self) -> Option<Arc<PluginHost>> {
        self.options.plugins.clone()
    }

    /// 插件是否在运行期生效（对应 Python `agent._plugin_manager.enabled`）。
    pub fn plugins_enabled(&self) -> bool {
        self.options
            .plugins
            .as_ref()
            .map(|plugins| plugins.enabled())
            .unwrap_or(false)
    }

    /// 启动插件运行期并发布会话生命周期 Hook；服务装配完成后调用一次。
    pub fn start_plugins(&self, session_id: &str) {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return;
        };
        for line in plugins.start() {
            eprintln!("[api] 插件：{line}");
        }
        if plugins.active() {
            plugins.notify_app_started();
        }
        self.notify_plugins_session(session_id);
    }

    /// `session.start.after` / `session.resume.*`：会话标识未知（内核尚未回填）时跳过。
    fn notify_plugins_session(&self, session_id: &str) {
        if session_id.trim().is_empty() || self.options.plugins.is_none() {
            return;
        }
        let guard = match self.runner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(runner) = guard.as_ref() else {
            return;
        };
        if let Err(error) = runner.notify_session_lifecycle(session_id) {
            eprintln!("[api] 会话生命周期 Hook 失败：{error}");
        }
    }

    /// 插件总开关热更新（`PUT /settings/features` 的 `plugins` 键）。
    ///
    /// 写盘在路由层完成，这里只切运行期：关闭时回收 Worker 与执行计划，打开时按当前
    /// 注册表重新装配（与 Python 设置面板的事务式重建同义）。
    pub fn set_plugins_enabled(&self, enabled: bool) -> Result<Vec<String>, ApiError> {
        let Some(plugins) = self.options.plugins.as_ref() else {
            return Err(ApiError::new(
                "PLUGIN_RUNTIME_UNAVAILABLE",
                "当前进程没有装配插件运行期，插件开关需要重启服务生效。".to_string(),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
            ));
        };
        plugins.set_enabled(enabled).map_err(|error| {
            ApiError::new(
                "PLUGIN_RUNTIME_FAILED",
                format!("插件运行期切换失败：{error}"),
                StatusCode::BAD_GATEWAY,
                None,
            )
        })
    }

    /// 插件运行态摘要：设置页与诊断用的展示来源（无运行期时全为默认值）。
    pub fn plugins_status(&self) -> Value {
        match self.options.plugins.as_ref() {
            Some(plugins) => json!({
                "enabled": plugins.enabled(),
                "configured": plugins.configured(),
                "handlers": plugins.handler_count(),
                "diagnostics": plugins.diagnostics(),
                "plugins": plugins.status_rows(),
            }),
            None => json!({
                "enabled": false,
                "configured": false,
                "handlers": 0,
                "diagnostics": Vec::<String>::new(),
                "plugins": Vec::<Value>::new(),
            }),
        }
    }

    /// `GET /mcp`：管理器缺失时报「未接入」，否则用管理器自己的状态摘要。
    pub fn mcp_status(&self) -> String {
        match self.options.mcp.as_ref() {
            Some(manager) => manager.format_status(),
            None => "MCP 未接入：配置读取失败已跳过。".to_string(),
        }
    }

    /// `POST /memory/clean`：清理项目级、当前会话级与用户级的过期记忆。
    pub fn clean_memory(&self) -> Result<Vec<String>, ApiError> {
        let user_root = user_config_dir(&self.options.env)
            .parent()
            .map(|home| home.join(".omnicrawl"))
            .unwrap_or_else(|| PathBuf::from(".omnicrawl"));
        let stores = [
            (
                "project",
                self.workspace_root().join(".omnicrawl/.oclmemory"),
            ),
            (
                "session",
                user_root
                    .join("Session_memory")
                    .join(self.current_session_id()),
            ),
            ("user", user_root.join("User_memory")),
        ];
        let mut deleted: Vec<String> = Vec::new();
        for (scope, root) in stores {
            let store = MemoryStore::open(root);
            let removed = store
                .clean_expired_memories()
                .map_err(session_store_error)?;
            deleted.extend(removed.into_iter().map(|path| format!("{scope}:{path}")));
        }
        Ok(deleted)
    }
}

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
    manager.discover();
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
