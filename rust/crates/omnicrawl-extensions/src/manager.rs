//! `omnicrawl/extensions/plugin_manager.py` 的 Rust 移植：
//! Worker 生命周期、执行计划、dispatch 与熔断。
//!
//! 与 Python 的两处结构性差异：
//! 1. 子任务的只读投递上下文用线程局部存储承载（Python 用 `ContextVar`），
//!    因此对外形状是 [`activate_plugin_dispatch_context`] 这个闭包包装；
//! 2. Worker 运行态（失败计数、熔断标记、活跃标记）放在 `Mutex` 后面，
//!    不可变部分（manifest / record / client）保持只读，避免每次分发都抢锁。

use crate::error::{PluginError, PluginProtocolError};
use crate::install::{verify_content_tree_hash, verify_lockfile_hash, verify_store_integrity};
use crate::models::{
    apply_json_patch, decode_utf8_sig, hook_policy, new_event_id, parse_plugin_manifest,
    stable_hash, utc_now_iso, validate_json_patch, validate_payload_against_schema,
    DispatchOutcome, HookEvent, HookPolicy, HookResult, PluginManifest, PluginRecord,
    PluginsConfig, ResolvedHandler, CORE_HOOKS, HANDLER_MODE_GUARD, HANDLER_MODE_NOTIFY,
    HANDLER_MODE_OBSERVE, HANDLER_MODE_TRANSFORM, HOOK_API_VERSION, OMNICRAWL_VERSION,
};
use crate::path::{expand_user, resolve_path};
use crate::protocol::{PluginWorkerClient, WorkerConfig, WorkerLauncher};
use crate::registry::{
    build_execution_plan, load_registry_document, merge_registry_documents, project_registry_path,
    user_registry_path, user_store_root, ManifestEntry,
};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

pub type AuditSink = Arc<dyn Fn(&AuditRecord) + Send + Sync>;

/// 子任务专用的只读 Plugin Hook 投递上下文。
///
/// 在父线程创建子任务前从当前计划冻结。子任务只能用 `handlers` 做 dispatch，
/// 不得调用 `begin_turn` / `end_turn`；`handlers` 为空表示「本任务不投递任何
/// 插件 handler」，而不是回退到父 live plan。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginDispatchContext {
    pub handlers: Vec<ResolvedHandler>,
    /// `parent-turn` | `parent-plan` | `none`
    pub source: String,
}

thread_local! {
    static ACTIVE_DISPATCH_CONTEXT: RefCell<Option<PluginDispatchContext>> = const { RefCell::new(None) };
}

/// 在子任务 worker 内绑定只读 dispatch context，避免父子共享 live plan。
pub fn activate_plugin_dispatch_context<F, R>(
    context: Option<PluginDispatchContext>,
    action: F,
) -> R
where
    F: FnOnce() -> R,
{
    let previous = ACTIVE_DISPATCH_CONTEXT.with(|slot| slot.replace(context));
    let result = action();
    ACTIVE_DISPATCH_CONTEXT.with(|slot| {
        slot.replace(previous);
    });
    result
}

/// 读取当前线程绑定的子任务 Plugin dispatch context。
pub fn get_active_plugin_dispatch_context() -> Option<PluginDispatchContext> {
    ACTIVE_DISPATCH_CONTEXT.with(|slot| slot.borrow().clone())
}

/// 单个启用插件的运行态。
pub struct WorkerHandle {
    pub name: String,
    pub manifest: PluginManifest,
    pub scope: String,
    pub record: PluginRecord,
    pub root: PathBuf,
    pub client: Option<PluginWorkerClient>,
    runtime: Mutex<WorkerRuntime>,
}

#[derive(Debug, Clone, Default)]
struct WorkerRuntime {
    failures: i64,
    circuit_open: bool,
    last_error: String,
    active: bool,
}

impl WorkerHandle {
    pub fn active(&self) -> bool {
        lock(&self.runtime).active
    }

    pub fn circuit_open(&self) -> bool {
        lock(&self.runtime).circuit_open
    }

    pub fn failures(&self) -> i64 {
        lock(&self.runtime).failures
    }

    pub fn last_error(&self) -> String {
        lock(&self.runtime).last_error.clone()
    }

    fn snapshot(&self) -> WorkerRuntime {
        lock(&self.runtime).clone()
    }

    fn trip(&self) {
        let mut runtime = lock(&self.runtime);
        runtime.active = false;
        runtime.circuit_open = true;
    }
}

/// 一条 Hook 审计记录。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditRecord {
    pub event_id: String,
    pub hook: String,
    pub handler_key: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub mode: String,
    pub status: String,
    pub elapsed_ms: f64,
    pub patch_paths: Vec<String>,
    pub before_hash: String,
    pub after_hash: String,
    pub error: String,
}

struct DispatcherPlan {
    plan: Vec<ResolvedHandler>,
    turn_plan: Option<Vec<ResolvedHandler>>,
}

/// Core Hook 分发器；无外部插件时也可用，保持 Host 语义节点统一。
pub struct HookDispatcher {
    config: PluginsConfig,
    audit_sink: Option<AuditSink>,
    sequence: AtomicI64,
    state: Mutex<DispatcherPlan>,
    workers: Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>,
}

impl HookDispatcher {
    pub fn new(
        config: PluginsConfig,
        audit_sink: Option<AuditSink>,
        workers: Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>,
    ) -> Self {
        Self {
            config,
            audit_sink,
            sequence: AtomicI64::new(0),
            state: Mutex::new(DispatcherPlan {
                plan: Vec::new(),
                turn_plan: None,
            }),
            workers,
        }
    }

    pub fn set_execution_plan(
        &self,
        handlers: Vec<ResolvedHandler>,
        workers: Option<HashMap<String, Arc<WorkerHandle>>>,
    ) {
        {
            let mut state = lock(&self.state);
            state.plan = handlers;
            // 若当前 turn 已冻结计划，则保留快照，使启停从下一轮生效。
        }
        if let Some(workers) = workers {
            *lock(&self.workers) = workers;
        }
    }

    /// turn 开始时冻结当前执行计划。
    pub fn begin_turn(&self) {
        let mut state = lock(&self.state);
        state.turn_plan = Some(state.plan.clone());
    }

    pub fn end_turn(&self) {
        let mut state = lock(&self.state);
        state.turn_plan = None;
    }

    /// 冻结当前可投递计划，供子任务独立使用。
    pub fn freeze_dispatch_context(&self) -> PluginDispatchContext {
        let state = lock(&self.state);
        if let Some(turn_plan) = &state.turn_plan {
            return PluginDispatchContext {
                handlers: turn_plan.clone(),
                source: "parent-turn".to_string(),
            };
        }
        if !state.plan.is_empty() {
            return PluginDispatchContext {
                handlers: state.plan.clone(),
                source: "parent-plan".to_string(),
            };
        }
        PluginDispatchContext {
            handlers: Vec::new(),
            source: "none".to_string(),
        }
    }

    pub fn current_plan(&self) -> Vec<ResolvedHandler> {
        let state = lock(&self.state);
        match &state.turn_plan {
            Some(turn_plan) => turn_plan.clone(),
            None => state.plan.clone(),
        }
    }

    pub fn next_sequence(&self) -> i64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 统一分发入口。
    pub fn dispatch(
        &self,
        hook_name: &str,
        request: DispatchRequest,
    ) -> Result<DispatchOutcome, PluginError> {
        if !CORE_HOOKS.contains(&hook_name) && !hook_name.starts_with("plugin.") {
            return Err(PluginError::new(format!("未知 Hook：{hook_name}")));
        }
        let policy = request
            .policy
            .unwrap_or_else(|| hook_policy(hook_name).unwrap_or_default());
        let mut working_payload = request.payload.clone();
        let mut outcome = DispatchOutcome::new(hook_name, working_payload.clone());

        let override_handlers = match request.handlers_override {
            Some(handlers) => Some(handlers),
            None => get_active_plugin_dispatch_context().map(|context| context.handlers),
        };
        let handlers: Vec<ResolvedHandler> = match override_handlers {
            Some(handlers) => handlers
                .into_iter()
                .filter(|item| item.hook == hook_name)
                .collect(),
            None => self
                .current_plan()
                .into_iter()
                .filter(|item| item.hook == hook_name)
                .collect(),
        };
        if handlers.is_empty() || !self.config.enabled {
            return Ok(outcome);
        }

        let mut event = self.build_event(
            hook_name,
            &working_payload,
            request.workspace.clone(),
            request.session_id.clone(),
            request.turn_id.clone(),
            request.parent_event_id.clone(),
            request.depth,
        );

        for mode in [
            HANDLER_MODE_GUARD,
            HANDLER_MODE_TRANSFORM,
            HANDLER_MODE_OBSERVE,
            HANDLER_MODE_NOTIFY,
        ] {
            let mode_handlers: Vec<ResolvedHandler> = handlers
                .iter()
                .filter(|item| item.mode == mode)
                .cloned()
                .collect();
            if mode_handlers.is_empty() {
                continue;
            }
            if mode == HANDLER_MODE_OBSERVE || mode == HANDLER_MODE_NOTIFY {
                self.run_parallel_readonly(
                    &mode_handlers,
                    &event,
                    &working_payload,
                    &policy,
                    &mut outcome,
                );
            } else {
                for handler in &mode_handlers {
                    let result = self.invoke_handler(handler, &event, &working_payload);
                    if let Some(applied) = self.apply_handler_result(
                        handler,
                        &result,
                        &working_payload,
                        &policy,
                        &mut outcome,
                        &event,
                    ) {
                        working_payload = applied;
                        event = rebuild_event_payload(&event, &working_payload);
                    }
                    if outcome.denied {
                        outcome.payload = working_payload;
                        return Ok(outcome);
                    }
                }
            }
        }

        outcome.payload = working_payload;
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_event(
        &self,
        hook_name: &str,
        payload: &Map<String, Value>,
        workspace: Option<Map<String, Value>>,
        session_id: Option<String>,
        turn_id: Option<String>,
        parent_event_id: Option<String>,
        depth: i64,
    ) -> HookEvent {
        let mut trace = Map::new();
        trace.insert(
            "parentEventId".to_string(),
            match &parent_event_id {
                Some(value) => Value::from(value.clone()),
                None => Value::Null,
            },
        );
        trace.insert("depth".to_string(), Value::from(depth));
        HookEvent {
            api_version: HOOK_API_VERSION.to_string(),
            event_id: new_event_id(),
            hook: hook_name.to_string(),
            timestamp: utc_now_iso(),
            sequence: self.next_sequence(),
            workspace: workspace.unwrap_or_else(default_workspace),
            payload: payload.clone(),
            deadline_ms: self.config.default_timeout_ms,
            session_id,
            turn_id,
            trace,
        }
    }

    /// 同插件串行、不同插件可并行。
    fn run_parallel_readonly(
        &self,
        handlers: &[ResolvedHandler],
        event: &HookEvent,
        payload: &Map<String, Value>,
        policy: &HookPolicy,
        outcome: &mut DispatchOutcome,
    ) {
        if handlers.is_empty() {
            return;
        }
        let mut groups: Vec<(String, Vec<ResolvedHandler>)> = Vec::new();
        for handler in handlers {
            match groups
                .iter_mut()
                .find(|(name, _)| name == &handler.plugin_name)
            {
                Some((_, group)) => group.push(handler.clone()),
                None => groups.push((handler.plugin_name.clone(), vec![handler.clone()])),
            }
        }

        let results: Vec<Vec<(ResolvedHandler, HookResult)>> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (_, group) in &groups {
                let group = group.clone();
                handles.push(scope.spawn(move || {
                    let mut pairs: Vec<(ResolvedHandler, HookResult)> = Vec::new();
                    for handler in &group {
                        pairs.push((
                            handler.clone(),
                            self.invoke_handler(handler, event, payload),
                        ));
                    }
                    pairs
                }));
            }
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect()
        });

        let readonly_payload = payload.clone();
        for pairs in results {
            for (handler, result) in pairs {
                self.apply_handler_result(
                    &handler,
                    &result,
                    &readonly_payload,
                    policy,
                    outcome,
                    event,
                );
            }
        }
    }

    fn invoke_handler(
        &self,
        handler: &ResolvedHandler,
        event: &HookEvent,
        payload: &Map<String, Value>,
    ) -> HookResult {
        let Some(worker) = self.worker_for(&handler.plugin_name) else {
            return HookResult::continue_result(&handler.key, Map::new(), 0.0, "skip");
        };
        let runtime = worker.snapshot();
        if runtime.circuit_open {
            return HookResult::continue_result(&handler.key, Map::new(), 0.0, "circuit-open");
        }
        let Some(client) = worker.client.as_ref() else {
            return HookResult::continue_result(&handler.key, Map::new(), 0.0, "skip");
        };
        if !runtime.active {
            return HookResult::continue_result(&handler.key, Map::new(), 0.0, "skip");
        }

        let started = Instant::now();
        let mut live_event = rebuild_event_payload(event, payload);
        if !worker
            .manifest
            .permissions
            .iter()
            .any(|item| item == "workspace:metadata")
            && live_event.workspace.contains_key("root")
        {
            let mut workspace = Map::new();
            workspace.insert(
                "id".to_string(),
                live_event
                    .workspace
                    .get("id")
                    .cloned()
                    .unwrap_or_else(|| Value::from("default")),
            );
            live_event.workspace = workspace;
        }

        let event_payload = Value::Object(live_event.to_dict());
        let event_map = match event_payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        match client.invoke_handler(&handler.handler_id, &event_map, handler.timeout_ms) {
            Ok(raw) => {
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                match crate::models::parse_hook_result(
                    Some(&Value::Object(raw)),
                    &handler.key,
                    elapsed,
                ) {
                    Ok(result) => {
                        lock(&worker.runtime).failures = 0;
                        result
                    }
                    Err(error) => {
                        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                        self.register_failure(&worker, &error.to_string());
                        HookResult {
                            action: "continue".to_string(),
                            handler_key: handler.key.clone(),
                            elapsed_ms: elapsed,
                            status: "handler-error".to_string(),
                            reason: error.to_string(),
                            ..HookResult::default()
                        }
                    }
                }
            }
            Err(error) => {
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                let text = error.to_string();
                self.register_failure(&worker, &text);
                let status = if text.contains("超时") {
                    "timeout"
                } else {
                    "protocol-error"
                };
                HookResult {
                    action: "continue".to_string(),
                    handler_key: handler.key.clone(),
                    elapsed_ms: elapsed,
                    status: status.to_string(),
                    reason: text,
                    ..HookResult::default()
                }
            }
        }
    }

    fn worker_for(&self, plugin_name: &str) -> Option<Arc<WorkerHandle>> {
        lock(&self.workers).get(plugin_name).cloned()
    }

    fn register_failure(&self, worker: &Arc<WorkerHandle>, error: &str) {
        let mut runtime = lock(&worker.runtime);
        runtime.failures += 1;
        runtime.last_error = error.to_string();
        if runtime.failures >= self.config.failure_threshold {
            runtime.circuit_open = true;
        }
    }

    fn apply_handler_result(
        &self,
        handler: &ResolvedHandler,
        result: &HookResult,
        payload: &Map<String, Value>,
        policy: &HookPolicy,
        outcome: &mut DispatchOutcome,
        event: &HookEvent,
    ) -> Option<Map<String, Value>> {
        outcome.results.push(result.clone());
        if !result.annotations.is_empty() {
            outcome.annotations.insert(
                handler.key.clone(),
                Value::Object(result.annotations.clone()),
            );
        }

        let status = result.status.as_str();
        if matches!(
            status,
            "timeout" | "protocol-error" | "handler-error" | "circuit-open" | "skip"
        ) {
            let decision = match status {
                "timeout" => policy.on_timeout,
                "protocol-error" => policy.on_protocol_error,
                "handler-error" => policy.on_handler_error,
                _ => "skip-handler",
            };
            self.audit(
                event,
                handler,
                status,
                result.elapsed_ms,
                &[],
                "",
                "",
                &result.reason,
            );
            if decision == "reject-operation" {
                outcome.denied = true;
                outcome.deny_reason = if result.reason.is_empty() {
                    format!("Hook {} Handler 失败：{status}", handler.hook)
                } else {
                    result.reason.clone()
                };
                outcome.deny_code = status.to_string();
            }
            if policy.disable_plugin_on_error {
                if let Some(worker) = self.worker_for(&handler.plugin_name) {
                    worker.trip();
                }
            }
            return None;
        }

        if result.action == "deny" {
            // 只有 guard 可 deny；approve 在 parse 阶段已拒绝，这里再双保险。
            if handler.mode != HANDLER_MODE_GUARD {
                self.audit(
                    event,
                    handler,
                    "invalid-deny",
                    result.elapsed_ms,
                    &[],
                    "",
                    "",
                    "非 guard Handler 不能 deny",
                );
                return None;
            }
            self.audit(
                event,
                handler,
                "deny",
                result.elapsed_ms,
                &[],
                "",
                "",
                &result.reason,
            );
            if policy.on_deny == "reject-operation" {
                outcome.denied = true;
                outcome.deny_reason = if result.reason.is_empty() {
                    format!("插件拒绝：{}", handler.key)
                } else {
                    result.reason.clone()
                };
                outcome.deny_code = if result.code.is_empty() {
                    "deny".to_string()
                } else {
                    result.code.clone()
                };
            }
            return None;
        }

        if result.action == "patch" {
            if handler.mode != HANDLER_MODE_TRANSFORM {
                self.audit(
                    event,
                    handler,
                    "invalid-patch",
                    result.elapsed_ms,
                    &[],
                    "",
                    "",
                    "仅 transform Handler 可返回 patch",
                );
                return None;
            }
            let before_hash = stable_hash(&Value::Object(payload.clone()));
            match self.apply_patch(handler, result, payload) {
                Ok((new_payload, paths)) => {
                    let after_hash = stable_hash(&Value::Object(new_payload.clone()));
                    let path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                    self.audit(
                        event,
                        handler,
                        "patch",
                        result.elapsed_ms,
                        &path_refs,
                        &before_hash,
                        &after_hash,
                        "",
                    );
                    Some(new_payload)
                }
                Err(error) => {
                    self.audit(
                        event,
                        handler,
                        "invalid-patch",
                        result.elapsed_ms,
                        &[],
                        &before_hash,
                        "",
                        &error,
                    );
                    if policy.on_handler_error == "reject-operation" {
                        outcome.denied = true;
                        outcome.deny_reason = format!("非法 Patch：{error}");
                        outcome.deny_code = "invalid-patch".to_string();
                    }
                    None
                }
            }
        } else {
            self.audit(
                event,
                handler,
                "continue",
                result.elapsed_ms,
                &[],
                "",
                "",
                "",
            );
            None
        }
    }

    /// 校验并应用 transform Patch：白名单路径以 `/payload/...` 为根，文档包装为 `{"payload": ...}`。
    fn apply_patch(
        &self,
        handler: &ResolvedHandler,
        result: &HookResult,
        payload: &Map<String, Value>,
    ) -> Result<(Map<String, Value>, Vec<String>), String> {
        let normalized = validate_json_patch(
            &Value::Array(
                result
                    .patch
                    .iter()
                    .map(|item| Value::Object(item.clone()))
                    .collect(),
            ),
            &handler.hook,
        )
        .map_err(|error| error.to_string())?;

        let mut document = Map::new();
        document.insert("payload".to_string(), Value::Object(payload.clone()));
        let updated =
            apply_json_patch(&document, &normalized).map_err(|error| error.to_string())?;
        let new_payload = updated
            .get("payload")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| "transform 后 payload 必须是对象".to_string())?;
        let paths: Vec<String> = normalized
            .iter()
            .map(|item| text_of(item.get("path")))
            .collect();
        Ok((new_payload, paths))
    }

    #[allow(clippy::too_many_arguments)]
    fn audit(
        &self,
        event: &HookEvent,
        handler: &ResolvedHandler,
        status: &str,
        elapsed_ms: f64,
        patch_paths: &[&str],
        before_hash: &str,
        after_hash: &str,
        error: &str,
    ) {
        if !self.config.audit_log_enabled {
            return;
        }
        let record = AuditRecord {
            event_id: event.event_id.clone(),
            hook: event.hook.clone(),
            handler_key: handler.key.clone(),
            plugin_name: handler.plugin_name.clone(),
            plugin_version: handler.plugin_version.clone(),
            mode: handler.mode.clone(),
            status: status.to_string(),
            elapsed_ms,
            patch_paths: patch_paths.iter().map(|item| (*item).to_string()).collect(),
            before_hash: before_hash.to_string(),
            after_hash: after_hash.to_string(),
            error: error.to_string(),
        };
        if let Some(sink) = &self.audit_sink {
            sink(&record);
        }
    }
}

/// 一次 dispatch 的入参。
#[derive(Debug, Clone, Default)]
pub struct DispatchRequest {
    pub payload: Map<String, Value>,
    pub policy: Option<HookPolicy>,
    pub workspace: Option<Map<String, Value>>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub parent_event_id: Option<String>,
    pub depth: i64,
    pub handlers_override: Option<Vec<ResolvedHandler>>,
}

fn default_workspace() -> Map<String, Value> {
    let mut workspace = Map::new();
    workspace.insert("id".to_string(), Value::from("default"));
    workspace
}

fn rebuild_event_payload(event: &HookEvent, payload: &Map<String, Value>) -> HookEvent {
    HookEvent {
        payload: payload.clone(),
        ..event.clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 管理器共享核心：Worker 表与分发器要在 Worker 回调里被引用，因此单独持有。
pub struct ManagerShared {
    pub config: PluginsConfig,
    pub workspace_root: PathBuf,
    pub workers: Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>>,
    pub dispatcher: HookDispatcher,
}

/// 工作区级插件管理器：加载 registry、启动 Worker、提供 dispatch。
pub struct PluginManager {
    shared: Arc<ManagerShared>,
    user_registry: PathBuf,
    project_registry: PathBuf,
    store_root: PathBuf,
    /// Worker 启动路径；`None` 表示 bootstrap 时按 [`WorkerLauncher::resolve`] 自动解析。
    launcher: Option<WorkerLauncher>,
    closed: AtomicBool,
}

impl PluginManager {
    pub fn new(
        workspace_root: &Path,
        config: PluginsConfig,
        user_registry: Option<PathBuf>,
        project_registry: Option<PathBuf>,
        store_root: Option<PathBuf>,
    ) -> Self {
        let workspace_root = resolve_path(workspace_root);
        let workers: Arc<Mutex<HashMap<String, Arc<WorkerHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let dispatcher = HookDispatcher::new(config.clone(), None, Arc::clone(&workers));
        let store_root = store_root.unwrap_or_else(user_store_root);
        Self {
            user_registry: user_registry.unwrap_or_else(user_registry_path),
            project_registry: project_registry
                .unwrap_or_else(|| project_registry_path(&workspace_root)),
            store_root: resolve_path(&expand_user(&store_root.to_string_lossy())),
            shared: Arc::new(ManagerShared {
                config,
                workspace_root,
                workers,
                dispatcher,
            }),
            launcher: None,
            closed: AtomicBool::new(false),
        }
    }

    /// 显式指定 Worker 启动路径；不调用时由 bootstrap 自动解析。
    pub fn with_launcher(mut self, launcher: Option<WorkerLauncher>) -> Self {
        self.launcher = launcher;
        self
    }

    /// 当前生效（已解析或已注入）的 Worker 启动路径。
    pub fn worker_launcher(&self) -> Option<&WorkerLauncher> {
        self.launcher.as_ref()
    }

    pub fn enabled(&self) -> bool {
        self.shared.config.enabled
    }

    pub fn workspace_root(&self) -> &Path {
        &self.shared.workspace_root
    }

    /// 读取注册表、启动启用插件、构建执行计划。返回诊断信息。
    pub fn bootstrap(&self) -> Vec<String> {
        let mut diagnostics: Vec<String> = Vec::new();
        if !self.shared.config.enabled {
            self.shared
                .dispatcher
                .set_execution_plan(Vec::new(), Some(HashMap::new()));
            diagnostics.push("plugins.enabled=false，以无插件模式运行。".to_string());
            return diagnostics;
        }

        // Worker 启动路径必须在启动任何插件之前确定：缺 runner 或 Node 时逐个插件报「握手
        // 失败」只会掩盖真实原因，这里一次性失败并降级。
        let launcher = match self.launcher.clone() {
            Some(launcher) => Some(launcher),
            None => match WorkerLauncher::resolve() {
                Ok(launcher) => Some(launcher),
                Err(error) => {
                    diagnostics.push(format!(
                        "插件 Worker 启动路径不可用，已按无插件模式运行：{error}"
                    ));
                    None
                }
            },
        };
        let Some(launcher) = launcher else {
            self.shared
                .dispatcher
                .set_execution_plan(Vec::new(), Some(HashMap::new()));
            return diagnostics;
        };

        let user_doc = load_registry_document(&self.user_registry);
        let project_doc = load_registry_document(&self.project_registry);
        let (user_doc, project_doc) = match (user_doc, project_doc) {
            (Ok(user_doc), Ok(project_doc)) => (user_doc, project_doc),
            (Err(error), _) => {
                diagnostics.push(format!("注册表读取失败，降级无插件：{error}"));
                self.shared
                    .dispatcher
                    .set_execution_plan(Vec::new(), Some(HashMap::new()));
                return diagnostics;
            }
            (_, Err(error)) => {
                diagnostics.push(format!("注册表读取失败，降级无插件：{error}"));
                self.shared
                    .dispatcher
                    .set_execution_plan(Vec::new(), Some(HashMap::new()));
                return diagnostics;
            }
        };

        let merged = merge_registry_documents(&user_doc, &project_doc);
        let mut entries: HashMap<String, Arc<WorkerHandle>> = HashMap::new();
        let mut manifests: Vec<ManifestEntry> = Vec::new();

        for record in &merged.plugins {
            if !record.enabled {
                continue;
            }
            // project 覆盖后 scope 以 project_doc 是否含该插件为准。
            let scope = if project_doc.get(&record.name).is_some() {
                "project"
            } else {
                "user"
            };
            match self.prepare_worker(record, scope, &launcher, &mut diagnostics) {
                Ok(Some(handle)) => {
                    manifests.push(ManifestEntry {
                        manifest: handle.manifest.clone(),
                        scope: handle.scope.clone(),
                        record: handle.record.clone(),
                    });
                    entries.insert(record.name.clone(), handle);
                }
                Ok(None) => {}
                Err(error) => {
                    diagnostics.push(format!("加载插件失败 {}：{error}", record.name));
                }
            }
        }

        let plan = build_execution_plan(
            &manifests,
            &merged.disabled_handlers,
            self.shared.config.max_timeout_ms,
        );
        let count = entries.len();
        let handler_count = plan.len();
        *lock(&self.shared.workers) = entries;
        self.shared.dispatcher.set_execution_plan(plan, None);
        diagnostics.push(format!(
            "已加载 {count} 个插件，{handler_count} 个 Handler。"
        ));
        diagnostics
    }

    /// 单个插件的加载与握手；`Ok(None)` 表示包名不一致而跳过。
    fn prepare_worker(
        &self,
        record: &PluginRecord,
        scope: &str,
        launcher: &WorkerLauncher,
        diagnostics: &mut Vec<String>,
    ) -> Result<Option<Arc<WorkerHandle>>, PluginError> {
        let root = self.resolve_plugin_root(record)?;
        let manifest = self.load_manifest(&root)?;
        if manifest.name != record.name {
            diagnostics.push(format!(
                "插件目录包名 {} 与注册表 {} 不一致，已跳过。",
                manifest.name, record.name
            ));
            return Ok(None);
        }
        if !record.dev_mode {
            let Some(active) = record.active.as_ref() else {
                return Err(PluginError::new(format!(
                    "插件 {} 缺少 active 版本指针。",
                    record.name
                )));
            };
            let store_root = &self.store_root;
            if root == *store_root || !root.starts_with(store_root) {
                return Err(PluginError::new(format!(
                    "插件 {} store 路径不在受信任目录：{}",
                    record.name,
                    root.display()
                )));
            }
            if active.integrity.is_empty() || active.content_hash.is_empty() {
                return Err(PluginError::new(format!(
                    "插件 {} 缺少完整性记录，拒绝启动。",
                    record.name
                )));
            }
            if manifest.version != active.version {
                return Err(PluginError::new(format!(
                    "插件 {} 版本不匹配：registry={}，manifest={}。",
                    record.name, active.version, manifest.version
                )));
            }
            verify_store_integrity(&root, &active.integrity)
                .map_err(|error| PluginError::new(error.to_string()))?;
            verify_lockfile_hash(&root, &active.lockfile_hash)
                .map_err(|error| PluginError::new(error.to_string()))?;
            verify_content_tree_hash(&root, &active.content_hash)
                .map_err(|error| PluginError::new(error.to_string()))?;
        }
        let mut unapproved: Vec<String> = manifest
            .permissions
            .iter()
            .filter(|item| !record.approved_permissions.contains(item))
            .cloned()
            .collect();
        if !unapproved.is_empty() {
            unapproved.sort();
            return Err(PluginError::new(format!(
                "插件 {} 声明了未批准权限：{}",
                record.name,
                unapproved.join(", ")
            )));
        }
        let entry = resolve_path(&root.join(&manifest.entry));
        if !entry.is_file() || !entry.starts_with(&root) {
            return Err(PluginError::new(format!(
                "插件 {} entry 越界或不存在：{}",
                record.name, manifest.entry
            )));
        }

        let handle = match self.start_worker(&manifest, record, scope, &root, launcher, diagnostics)
        {
            Some(handle) => handle,
            None => {
                diagnostics.push(format!("插件握手失败，已禁用：{}", record.name));
                return Ok(None);
            }
        };
        Ok(Some(handle))
    }

    fn resolve_plugin_root(&self, record: &PluginRecord) -> Result<PathBuf, PluginError> {
        if record.dev_mode && !record.local_path.is_empty() {
            let root = resolve_path(&expand_user(&record.local_path));
            if !root.is_dir() {
                return Err(PluginError::new(format!(
                    "开发模式插件路径不存在：{}",
                    root.display()
                )));
            }
            return Ok(root);
        }
        if let Some(active) = record
            .active
            .as_ref()
            .filter(|item| !item.store_path.is_empty())
        {
            let root = resolve_path(&expand_user(&active.store_path));
            if !root.is_dir() {
                return Err(PluginError::new(format!(
                    "插件 store 路径不存在：{}",
                    root.display()
                )));
            }
            return Ok(root);
        }
        if !record.local_path.is_empty() {
            let root = resolve_path(&expand_user(&record.local_path));
            if root.is_dir() {
                return Ok(root);
            }
        }
        Err(PluginError::new(format!(
            "插件 {} 缺少可用安装路径（localPath/storePath）。",
            record.name
        )))
    }

    fn load_manifest(&self, root: &Path) -> Result<PluginManifest, PluginError> {
        let package_path = root.join("package.json");
        let bytes = std::fs::read(&package_path)
            .map_err(|error| PluginError::new(format!("{}，{error}", package_path.display())))?;
        let data: Value = serde_json::from_str(&decode_utf8_sig(&bytes))
            .map_err(|error| PluginError::new(error.to_string()))?;
        parse_plugin_manifest(&data, &package_path.to_string_lossy())
            .map_err(|error| PluginError::new(error.to_string()))
    }

    /// 启动 Worker 并完成握手；失败时给出诊断并回收客户端。
    fn start_worker(
        &self,
        manifest: &PluginManifest,
        record: &PluginRecord,
        scope: &str,
        root: &Path,
        launcher: &WorkerLauncher,
        diagnostics: &mut Vec<String>,
    ) -> Option<Arc<WorkerHandle>> {
        let shared = Arc::clone(&self.shared);
        let source_name = manifest.name.clone();
        let on_host_request: crate::protocol::HostRequestHandler =
            Arc::new(move |method: &str, params: &Map<String, Value>| {
                shared.handle_worker_request(&source_name, method, params)
            });

        let (node_executable, runner_path) = launcher.worker_config_fields();
        let client = match PluginWorkerClient::new(WorkerConfig {
            plugin_root: root.to_path_buf(),
            plugin_name: manifest.name.clone(),
            timeout_ms: Some(self.shared.config.default_timeout_ms),
            max_message_bytes: Some(self.shared.config.max_message_bytes),
            node_executable,
            runner_path,
            env: None,
            on_stderr: None,
            on_host_request: Some(on_host_request),
        }) {
            Ok(client) => client,
            Err(error) => {
                diagnostics.push(format!("{} initialize 失败：{error}", manifest.name));
                return None;
            }
        };

        let handshake = (|| -> Result<(), PluginProtocolError> {
            client.start()?;
            let mut params = Map::new();
            params.insert("apiVersion".to_string(), Value::from(HOOK_API_VERSION));
            params.insert(
                "omnicrawlVersion".to_string(),
                Value::from(OMNICRAWL_VERSION),
            );
            let mut permissions: Vec<String> = record
                .approved_permissions
                .iter()
                .filter(|item| manifest.permissions.contains(item))
                .cloned()
                .collect();
            permissions.sort();
            permissions.dedup();
            params.insert("permissions".to_string(), Value::from(permissions));
            let mut manifest_block = Map::new();
            manifest_block.insert("name".to_string(), Value::from(manifest.name.clone()));
            manifest_block.insert("version".to_string(), Value::from(manifest.version.clone()));
            let hooks: Vec<Value> = manifest
                .hooks
                .iter()
                .map(|item| {
                    let mut hook = Map::new();
                    hook.insert("id".to_string(), Value::from(item.id.clone()));
                    hook.insert("hook".to_string(), Value::from(item.hook.clone()));
                    hook.insert("mode".to_string(), Value::from(item.mode.clone()));
                    Value::Object(hook)
                })
                .collect();
            manifest_block.insert("hooks".to_string(), Value::Array(hooks));
            params.insert("manifest".to_string(), Value::Object(manifest_block));

            let result =
                client.initialize(&params, Some(self.shared.config.max_timeout_ms.min(5000)))?;
            let actual_handlers = match result.get("handlers") {
                Some(Value::Array(items)) => items.clone(),
                None | Some(Value::Null) => Vec::new(),
                Some(_) => {
                    return Err(PluginProtocolError::new("initialized.handlers 必须是数组"));
                }
            };
            let mut declared: Vec<(String, String, String)> = manifest
                .hooks
                .iter()
                .map(|item| (item.id.clone(), item.hook.clone(), item.mode.clone()))
                .collect();
            let mut actual_ids: Vec<String> = Vec::new();
            for item in &actual_handlers {
                let Some(map) = item.as_object() else {
                    continue;
                };
                let handler_id = text_of(map.get("id"));
                let actual_hook = text_of(map.get("hook"));
                let actual_mode = text_of(map.get("mode"));
                let Some((_, hook, mode)) = declared.iter().find(|(id, _, _)| *id == handler_id)
                else {
                    return Err(PluginProtocolError::new(format!(
                        "运行期注册了 manifest 外 Handler：{handler_id}"
                    )));
                };
                actual_ids.push(handler_id.clone());
                if *hook != actual_hook || *mode != actual_mode {
                    return Err(PluginProtocolError::new(format!(
                        "运行期 Handler 与 manifest 不一致：{handler_id} ({actual_hook}, {actual_mode}) != ({hook}, {mode})"
                    )));
                }
            }
            let mut missing: Vec<String> = declared
                .iter()
                .map(|(id, _, _)| id.clone())
                .filter(|id| !actual_ids.contains(id))
                .collect();
            if !missing.is_empty() {
                missing.sort();
                return Err(PluginProtocolError::new(format!(
                    "运行期缺少 manifest Handler：{}",
                    missing.join(", ")
                )));
            }
            declared.clear();
            Ok(())
        })();

        if let Err(error) = handshake {
            diagnostics.push(format!("{} initialize 失败：{error}", manifest.name));
            client.close();
            return None;
        }

        Some(Arc::new(WorkerHandle {
            name: manifest.name.clone(),
            manifest: manifest.clone(),
            scope: scope.to_string(),
            record: record.clone(),
            root: root.to_path_buf(),
            client: Some(client),
            runtime: Mutex::new(WorkerRuntime {
                failures: 0,
                circuit_open: false,
                last_error: String::new(),
                active: true,
            }),
        }))
    }

    /// 当前生效执行计划的 Handler 数（诊断与状态展示用）。
    pub fn current_plan_len(&self) -> usize {
        self.shared.dispatcher.current_plan().len()
    }

    pub fn begin_turn(&self) {
        self.shared.dispatcher.begin_turn();
    }

    pub fn end_turn(&self) {
        self.shared.dispatcher.end_turn();
    }

    /// 为子任务冻结只读 Plugin dispatch context。
    pub fn freeze_dispatch_context(&self) -> PluginDispatchContext {
        self.shared.dispatcher.freeze_dispatch_context()
    }

    pub fn dispatch(
        &self,
        hook_name: &str,
        request: DispatchRequest,
    ) -> Result<DispatchOutcome, PluginError> {
        let workspace = match request.workspace.clone() {
            Some(workspace) => workspace,
            None => self.default_workspace(),
        };
        self.shared.dispatcher.dispatch(
            hook_name,
            DispatchRequest {
                workspace: Some(workspace),
                ..request
            },
        )
    }

    fn default_workspace(&self) -> Map<String, Value> {
        let mut workspace = Map::new();
        workspace.insert(
            "id".to_string(),
            Value::from(stable_hash(&Value::from(
                self.shared.workspace_root.to_string_lossy().to_string(),
            ))),
        );
        // 默认不暴露真实路径，除非插件获准 workspace:metadata。
        let any_metadata = lock(&self.shared.workers).values().any(|worker| {
            worker
                .manifest
                .permissions
                .iter()
                .any(|item| item == "workspace:metadata")
        });
        if any_metadata {
            workspace.insert(
                "root".to_string(),
                Value::from(self.shared.workspace_root.to_string_lossy().to_string()),
            );
        }
        workspace
    }

    /// 返回已激活且已批准插件显式声明的 Agent Markdown 路径。
    pub fn agent_definition_paths(&self) -> Vec<(String, PathBuf)> {
        let mut workers: Vec<(String, Arc<WorkerHandle>)> = lock(&self.shared.workers)
            .iter()
            .map(|(name, worker)| (name.clone(), Arc::clone(worker)))
            .collect();
        workers.sort_by(|left, right| left.0.cmp(&right.0));

        let mut definitions: Vec<(String, PathBuf)> = Vec::new();
        for (name, worker) in workers {
            if !worker.active() {
                continue;
            }
            if !worker
                .manifest
                .permissions
                .iter()
                .any(|item| item == "agent:definitions")
            {
                continue;
            }
            if !worker
                .record
                .approved_permissions
                .iter()
                .any(|item| item == "agent:definitions")
            {
                continue;
            }
            let root = resolve_path(&worker.root);
            for relative_path in &worker.manifest.agents {
                let candidate = resolve_path(&root.join(relative_path));
                if candidate.starts_with(&root) {
                    definitions.push((name.clone(), candidate));
                }
            }
        }
        definitions
    }

    pub fn list_status(&self) -> Vec<Map<String, Value>> {
        let mut workers: Vec<(String, Arc<WorkerHandle>)> = lock(&self.shared.workers)
            .iter()
            .map(|(name, worker)| (name.clone(), Arc::clone(worker)))
            .collect();
        workers.sort_by(|left, right| left.0.cmp(&right.0));

        let mut rows: Vec<Map<String, Value>> = Vec::new();
        for (name, worker) in workers {
            let mut row = Map::new();
            row.insert("name".to_string(), Value::from(name));
            row.insert(
                "version".to_string(),
                Value::from(worker.manifest.version.clone()),
            );
            row.insert("scope".to_string(), Value::from(worker.scope.clone()));
            row.insert("active".to_string(), Value::from(worker.active()));
            row.insert(
                "circuitOpen".to_string(),
                Value::from(worker.circuit_open()),
            );
            row.insert("failures".to_string(), Value::from(worker.failures()));
            row.insert("lastError".to_string(), Value::from(worker.last_error()));
            row.insert(
                "handlers".to_string(),
                Value::from(
                    worker
                        .manifest
                        .hooks
                        .iter()
                        .map(|item| item.id.clone())
                        .collect::<Vec<String>>(),
                ),
            );
            row.insert("devMode".to_string(), Value::from(worker.record.dev_mode));
            row.insert(
                "root".to_string(),
                Value::from(worker.root.to_string_lossy().to_string()),
            );
            rows.push(row);
        }
        rows
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let workers: Vec<Arc<WorkerHandle>> =
            lock(&self.shared.workers).values().cloned().collect();
        for worker in workers {
            lock(&worker.runtime).active = false;
            if let Some(client) = worker.client.as_ref() {
                client.shutdown(2000);
            }
        }
        lock(&self.shared.workers).clear();
        self.shared
            .dispatcher
            .set_execution_plan(Vec::new(), Some(HashMap::new()));
    }
}

impl ManagerShared {
    /// 处理 Worker → Host 请求。V1 支持 `custom.emit`。
    pub fn handle_worker_request(
        &self,
        source_plugin: &str,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Map<String, Value>, String> {
        if method != "custom.emit" {
            return Err(format!("不支持的 Host method：{method}"));
        }
        let event_name = text_of(params.get("event")).trim().to_string();
        let version = match params.get("version") {
            None | Some(Value::Null) => 1,
            Some(value) => match value {
                Value::Number(number) => number.as_i64().unwrap_or(0),
                Value::String(text) => text.parse::<i64>().unwrap_or(0),
                _ => 0,
            },
        };
        let version = if version == 0 { 1 } else { version };
        let payload = match params.get("payload") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err("custom.emit payload 必须是对象。".to_string()),
        };
        let parent_event_id = {
            let text = text_of(params.get("parentEventId"));
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        };
        let depth = int_value(params.get("depth"));
        let local_delivered = int_value(params.get("localDelivered"));
        self.emit_custom_event(
            source_plugin,
            &event_name,
            version,
            &payload,
            parent_event_id.as_deref(),
            depth,
            None,
            None,
            local_delivered,
        )
        .map_err(|error| error.to_string())
    }

    /// 校验并分发自定义事件到已订阅 Handler。
    #[allow(clippy::too_many_arguments)]
    pub fn emit_custom_event(
        &self,
        source_plugin: &str,
        event_name: &str,
        version: i64,
        payload: &Map<String, Value>,
        parent_event_id: Option<&str>,
        depth: i64,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        local_delivered: i64,
    ) -> Result<Map<String, Value>, PluginError> {
        if !self.config.enabled {
            return Err(PluginError::new("插件系统未启用，无法发布自定义事件。"));
        }
        if depth >= self.config.custom_event_max_depth {
            return Err(PluginError::new(format!(
                "自定义事件递归深度超限（{depth} >= {}）。",
                self.config.custom_event_max_depth
            )));
        }
        let source = lock(&self.workers).get(source_plugin).cloned();
        let Some(source) = source else {
            return Err(PluginError::new(format!("来源插件未激活：{source_plugin}")));
        };
        if !source.active() {
            return Err(PluginError::new(format!("来源插件未激活：{source_plugin}")));
        }
        if !source
            .manifest
            .permissions
            .iter()
            .any(|item| item == "hook:custom-emit")
        {
            return Err(PluginError::new(format!(
                "插件 {source_plugin} 缺少 hook:custom-emit 权限。"
            )));
        }

        let declaration = source
            .manifest
            .custom_events
            .iter()
            .find(|item| item.name == event_name && item.version == version)
            .or_else(|| {
                source
                    .manifest
                    .custom_events
                    .iter()
                    .find(|item| item.name == event_name)
            });
        let Some(declaration) = declaration else {
            return Err(PluginError::new(format!(
                "事件 {event_name}@v{version} 未在 {source_plugin} 的 customEvents 中声明。"
            )));
        };
        validate_payload_against_schema(&Value::Object(payload.clone()), Some(&declaration.schema))
            .map_err(|error| PluginError::new(error.to_string()))?;

        // 跨插件投递：排除 source 自身，避免对同一 Worker 重入。
        let mut subscribers: Vec<ResolvedHandler> = self
            .dispatcher
            .current_plan()
            .into_iter()
            .filter(|item| item.hook == event_name && item.plugin_name != source_plugin)
            .collect();
        if declaration.visibility == "private" {
            subscribers.clear();
        } else {
            let workers = lock(&self.workers);
            subscribers.retain(|item| {
                let Some(worker) = workers.get(&item.plugin_name) else {
                    return false;
                };
                let required = format!("hook:{}", item.hook);
                worker.manifest.permissions.iter().any(|permission| {
                    permission == "hook:custom-subscribe" || *permission == required
                })
            });
        }

        let mut cross_plugin = 0usize;
        let mut denied = false;
        if !subscribers.is_empty() {
            let mut workspace = Map::new();
            workspace.insert(
                "id".to_string(),
                Value::from(stable_hash(&Value::from(
                    self.workspace_root.to_string_lossy().to_string(),
                ))),
            );
            let outcome = self.dispatcher.dispatch(
                event_name,
                DispatchRequest {
                    payload: payload.clone(),
                    policy: Some(HookPolicy {
                        on_deny: "ignore",
                        on_timeout: "skip-handler",
                        on_protocol_error: "skip-handler",
                        on_handler_error: "skip-handler",
                        disable_plugin_on_error: false,
                    }),
                    workspace: Some(workspace),
                    session_id: session_id.map(str::to_string),
                    turn_id: turn_id.map(str::to_string),
                    parent_event_id: parent_event_id.map(str::to_string),
                    depth: depth + 1,
                    handlers_override: Some(subscribers),
                },
            )?;
            cross_plugin = outcome.results.len();
            denied = outcome.denied;
        }

        let mut result = Map::new();
        result.insert("ok".to_string(), Value::from(true));
        result.insert("delivered".to_string(), Value::from(cross_plugin as i64));
        result.insert("crossPlugin".to_string(), Value::from(cross_plugin as i64));
        result.insert("localDelivered".to_string(), Value::from(local_delivered));
        result.insert("event".to_string(), Value::from(event_name));
        result.insert("version".to_string(), Value::from(declaration.version));
        result.insert("denied".to_string(), Value::from(denied));
        Ok(result)
    }
}

fn int_value(value: Option<&Value>) -> i64 {
    match value {
        None | Some(Value::Null) => 0,
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) => text.parse::<i64>().unwrap_or(0),
        Some(Value::Bool(flag)) => i64::from(*flag),
        _ => 0,
    }
}

/// 进程级运行时：持有配置与当前工作区 PluginManager。
pub struct PluginRuntime {
    config: PluginsConfig,
    workspace_root: PathBuf,
    manager: Option<PluginManager>,
    diagnostics: Vec<String>,
    started: bool,
    /// Worker 启动路径；`None` 时由每个 PluginManager 的 bootstrap 自行解析。
    launcher: Option<WorkerLauncher>,
}

impl PluginRuntime {
    pub fn new(config: PluginsConfig, workspace_root: Option<&Path>) -> Self {
        let workspace_root = match workspace_root {
            Some(path) => resolve_path(path),
            None => match std::env::current_dir() {
                Ok(current) => resolve_path(&current),
                Err(_) => PathBuf::from("."),
            },
        };
        Self {
            config,
            workspace_root,
            manager: None,
            diagnostics: Vec::new(),
            started: false,
            launcher: None,
        }
    }

    /// 显式注入 Worker 启动路径（CLI 与测试用）；不注入时自动解析。
    pub fn set_worker_launcher(&mut self, launcher: Option<WorkerLauncher>) {
        self.launcher = launcher;
    }

    pub fn worker_launcher(&self) -> Option<&WorkerLauncher> {
        self.launcher.as_ref()
    }

    /// 从整份配置数据里取 `plugins` 段构造运行时。
    pub fn from_config_data(
        data: Option<&Value>,
        workspace_root: Option<&Path>,
    ) -> Result<Self, PluginError> {
        let section = data
            .and_then(Value::as_object)
            .and_then(|map| map.get("plugins"))
            .cloned();
        let config = crate::models::parse_plugins_config(section.as_ref())?;
        Ok(Self::new(config, workspace_root))
    }

    pub fn config(&self) -> &PluginsConfig {
        &self.config
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn manager(&self) -> Option<&PluginManager> {
        self.manager.as_ref()
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn start(&mut self) -> Result<Vec<String>, PluginError> {
        if self.started && self.manager.is_some() {
            return Ok(self.diagnostics.clone());
        }

        let candidate =
            PluginManager::new(&self.workspace_root, self.config.clone(), None, None, None)
                .with_launcher(self.launcher.clone());
        let mut diagnostics = candidate.bootstrap();
        if self.config.enabled {
            match candidate.dispatch(
                "app.start.before",
                DispatchRequest {
                    payload: {
                        let mut payload = Map::new();
                        payload.insert("phase".to_string(), Value::from("bootstrap"));
                        payload
                    },
                    ..DispatchRequest::default()
                },
            ) {
                Ok(outcome) => {
                    if outcome.denied {
                        // 显式 deny 阻断启动；插件故障已在 policy 中 skip。
                        candidate.close();
                        return Err(PluginError::new(if outcome.deny_reason.is_empty() {
                            "app.start.before 被插件拒绝".to_string()
                        } else {
                            outcome.deny_reason
                        }));
                    }
                }
                Err(error) => {
                    if error.to_string().contains("被插件拒绝") {
                        candidate.close();
                        return Err(error);
                    }
                    diagnostics.push(format!("app.start.before 分发异常：{error}"));
                }
            }
        }

        self.manager = Some(candidate);
        self.diagnostics = diagnostics;
        self.started = true;
        Ok(self.diagnostics.clone())
    }

    pub fn notify_app_started(&mut self) {
        if self.manager.is_none() || !self.config.enabled {
            return;
        }
        let mut payload = Map::new();
        payload.insert("phase".to_string(), Value::from("ready"));
        let result = self.manager.as_ref().map(|manager| {
            manager.dispatch(
                "app.start.after",
                DispatchRequest {
                    payload,
                    ..DispatchRequest::default()
                },
            )
        });
        if let Some(Err(error)) = result {
            self.diagnostics
                .push(format!("app.start.after 忽略故障：{error}"));
        }
    }

    /// 事务式重建 PluginManager；候选启动失败时保留旧 Manager。
    pub fn switch_workspace(
        &mut self,
        new_root: &Path,
        emit_hooks: bool,
    ) -> Result<Vec<String>, PluginError> {
        let new_root = resolve_path(new_root);
        if emit_hooks && self.manager.is_some() && self.config.enabled {
            let mut payload = Map::new();
            payload.insert(
                "from".to_string(),
                Value::from(self.workspace_root.to_string_lossy().to_string()),
            );
            payload.insert(
                "to".to_string(),
                Value::from(new_root.to_string_lossy().to_string()),
            );
            let outcome = self.manager.as_ref().map(|manager| {
                manager.dispatch(
                    "workspace.switch.before",
                    DispatchRequest {
                        payload,
                        ..DispatchRequest::default()
                    },
                )
            });
            if let Some(Ok(outcome)) = outcome {
                if outcome.denied {
                    return Err(PluginError::new(if outcome.deny_reason.is_empty() {
                        "workspace.switch.before 拒绝切换".to_string()
                    } else {
                        outcome.deny_reason
                    }));
                }
            }
        }

        let candidate = PluginManager::new(&new_root, self.config.clone(), None, None, None)
            .with_launcher(self.launcher.clone());
        let diagnostics = candidate.bootstrap();
        if let Some(old_manager) = self.manager.take() {
            old_manager.close();
        }
        self.workspace_root = new_root;
        self.manager = Some(candidate);
        self.diagnostics = diagnostics.clone();
        if emit_hooks && self.config.enabled {
            let mut payload = Map::new();
            payload.insert(
                "workspace".to_string(),
                Value::from(self.workspace_root.to_string_lossy().to_string()),
            );
            let result = self.manager.as_ref().map(|manager| {
                manager.dispatch(
                    "workspace.switch.after",
                    DispatchRequest {
                        payload,
                        ..DispatchRequest::default()
                    },
                )
            });
            if let Some(Err(error)) = result {
                self.diagnostics
                    .push(format!("workspace.switch.after 忽略故障：{error}"));
            }
        }
        Ok(diagnostics)
    }

    /// 事务式切换总开关。
    ///
    /// 与 Python 的差异：Python 返回当前 Manager 供调用方更新引用，
    /// Rust 侧 Manager 由运行时自己持有，因此只返回成功与否。
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), PluginError> {
        if self.config.enabled == enabled && (!enabled || self.manager.is_some()) {
            return Ok(());
        }
        let previous = self.config.clone();
        self.config.enabled = enabled;
        let result = (|| -> Result<(), PluginError> {
            if enabled {
                if self.manager.is_none() {
                    self.start()?;
                } else {
                    let root = self.workspace_root.clone();
                    self.switch_workspace(&root, false)?;
                }
                // 应用启动路径会在 start() 后显式发送 app.start.after；运行中
                // 由设置面板重新启用时也要补齐同一生命周期通知。
                self.notify_app_started();
            } else {
                self.close_manager_only();
            }
            Ok(())
        })();
        if result.is_err() {
            self.config = previous;
        }
        result
    }

    /// 重新装配当前工作区的 PluginManager（安装 / 启停 / 卸载后的热更新）。
    ///
    /// 走完整的关停与启动生命周期（`app.stop.*` → 重建 → `app.start.before`），
    /// 因此旧 Worker 会被回收、旧执行计划会失效，插件的启停从下一次分发生效。
    /// 重建失败时没有可回退的旧 Manager（旧 Worker 已回收），调用方需要把错误
    /// 报告给用户；此时运行时保持无插件状态（[`PluginRuntime::manager`] 为 `None`）。
    pub fn reload(&mut self) -> Result<Vec<String>, PluginError> {
        self.close();
        let result = self.start();
        if result.is_ok() {
            self.notify_app_started();
        }
        result
    }

    pub fn close_manager_only(&mut self) {
        if let Some(manager) = self.manager.take() {
            manager.close();
        }
    }

    pub fn close(&mut self) {
        if self.manager.is_some() && self.config.enabled {
            let mut payload = Map::new();
            payload.insert("phase".to_string(), Value::from("stopping"));
            if let Some(manager) = self.manager.as_ref() {
                let _ = manager.dispatch(
                    "app.stop.before",
                    DispatchRequest {
                        payload,
                        ..DispatchRequest::default()
                    },
                );
            }
        }
        // Agent/会话资源由调用方先关闭，再调用这里。
        if self.manager.is_some() && self.config.enabled {
            let mut payload = Map::new();
            payload.insert("phase".to_string(), Value::from("stopped"));
            if let Some(manager) = self.manager.as_ref() {
                let _ = manager.dispatch(
                    "app.stop.after",
                    DispatchRequest {
                        payload,
                        ..DispatchRequest::default()
                    },
                );
            }
        }
        self.close_manager_only();
        self.started = false;
    }

    pub fn dispatch(
        &self,
        hook_name: &str,
        request: DispatchRequest,
    ) -> Result<DispatchOutcome, PluginError> {
        match self.manager.as_ref() {
            Some(manager) => manager.dispatch(hook_name, request),
            None => Ok(DispatchOutcome::new(hook_name, request.payload)),
        }
    }
}
