//! 宿主侧插件运行期：进程级 [`PluginRuntime`] 的持有者 + 各 Hook 节点的编排。
//!
//! Python 侧这条链路分两半：`extensions/plugin_manager.py` 只管「怎么分发」，
//! `agent/controllers/plugins.py` 管「在哪个节点分发什么载荷、被拒时给什么文案」。
//! Rust 侧前半段已在 `omnicrawl-extensions` 里，这里补的是后半段在宿主边界上的落点：
//!
//! | Hook | 宿主节点 |
//! | --- | --- |
//! | `app.start.*` / `app.stop.*` / `workspace.switch.*` | `PluginRuntime` 自身（启停与工作区切换） |
//! | `session.start.after` / `session.resume.*` / `session.close.*` | 会话建立与关闭（[`PluginHost::session_lifecycle`]） |
//! | `turn.start` / `turn.end` / `turn.error` / `turn.cancelled` | 回合边界（[`PluginHost::turn_start`] …） |
//! | `tool.call.before` / `tool.approval.*` / `tool.execute.*` | 工具批次定调与执行（[`PluginHost::tool_call_before`] …） |
//!
//! 判定层复用 `omnicrawl-controllers` 的插件控制器（fail-closed 判定、拒绝事实提取、
//! 拒绝文案），因此这里的每个方法只负责「拼载荷 → 分发 → 把结局翻译成宿主动作」。
//!
//! 插件系统关闭或 Manager 不存在时所有方法都退化为原样放行，不引入额外分支。

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use omnicrawl_config::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use omnicrawl_config::toml::Table;
use omnicrawl_config::value::toml_to_json_object;
use omnicrawl_controllers::plugins::{
    denial_error, hook_requires_fail_closed, resolve_dispatch, session_hook_payload,
    session_lifecycle_hooks, turn_hook_action, DenialFacts, DispatchResult, HookOutcome,
    PluginHookResult, TurnHookAction,
};
use omnicrawl_controllers::AgentError;
use omnicrawl_extensions::install::{
    install_from_npm, list_plugins, parse_package_spec, register_local_dev_plugin, rollback_plugin,
    set_enabled as set_plugin_enabled, uninstall_plugin, ConfirmCallback, InstallResult,
};
use omnicrawl_extensions::manager::{DispatchRequest, PluginRuntime};
use omnicrawl_extensions::models::{hook_policy, parse_plugins_config, PluginsConfig};
use omnicrawl_extensions::protocol::{WorkerLauncher, RUNNER_DIR_ENV};
use serde_json::{Map, Value};

/// 一次 Hook 分发的结局。
#[derive(Debug, Clone, PartialEq)]
pub enum HookDecision {
    /// 放行；载荷可能已被 transform 类 Handler 改写。
    Forward(Map<String, Value>),
    /// 拒绝；携带拒绝事实供上层生成文案。
    Denied(DenialFacts),
}

impl HookDecision {
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied(_))
    }

    /// 放行时的载荷；被拒时退回传入的原始载荷。
    pub fn payload_or(self, fallback: Map<String, Value>) -> Map<String, Value> {
        match self {
            Self::Forward(payload) => payload,
            Self::Denied(_) => fallback,
        }
    }

    /// 被拒时按 Hook 名生成错误文案；放行时返回 `None`。
    pub fn denial_error(&self, hook_name: &str) -> Option<AgentError> {
        match self {
            Self::Forward(_) => None,
            Self::Denied(facts) => Some(denial_error(hook_name, Some(facts))),
        }
    }
}

/// 进程级插件运行期。
///
/// TUI 与本地 API 各自持有一个；`PluginRuntime` 需要 `&mut` 做启停与工作区切换，
/// 因此放在 `Mutex` 后面，宿主只需共享 `Arc<PluginHost>`。
pub struct PluginHost {
    runtime: Mutex<PluginRuntime>,
    workspace_root: PathBuf,
}

impl PluginHost {
    /// 按工作区与显式配置构造（未启动）。
    pub fn new(workspace_root: &Path, config: PluginsConfig) -> Self {
        let resolved = resolve_workspace(workspace_root);
        Self {
            runtime: Mutex::new(PluginRuntime::new(config, Some(&resolved))),
            workspace_root: resolved,
        }
    }

    /// 从配置环境读取 `plugins` 段构造（读取失败时按默认配置，即关闭）。
    ///
    /// 与 Python 侧的差别：配置读取失败不抛异常。Python 在入口处读不到配置会直接
    /// 崩溃；Rust 侧沿用「配置层读取失败降级为默认值 + 诊断」的既有约定。
    pub fn from_environment(env: &ConfigEnvironment, workspace_root: &Path) -> Self {
        let section = load_config_data(env, None)
            .ok()
            .and_then(|data: Table| get_section(&data, "plugins").ok())
            .map(|section| toml_to_json_object(&section));
        let config = parse_plugins_config(section.as_ref()).unwrap_or_default();
        Self::new(workspace_root, config)
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    fn lock(&self) -> MutexGuard<'_, PluginRuntime> {
        self.runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 配置里的总开关；启用不代表 Manager 一定装配成功（见 [`Self::diagnostics`]）。
    pub fn configured(&self) -> bool {
        self.lock().config().enabled
    }

    /// Manager 是否已装配（有它才会有 Worker 与执行计划）。
    pub fn active(&self) -> bool {
        self.lock().manager().is_some()
    }

    /// 对外生效的启用状态：与 Python `PluginManager.enabled` 同义。
    pub fn enabled(&self) -> bool {
        let runtime = self.lock();
        runtime.config().enabled && runtime.manager().is_some()
    }

    /// 启动：装配 Manager、拉起 Worker、发 `app.start.before`。返回诊断行。
    pub fn start(&self) -> Vec<String> {
        match self.lock().start() {
            Ok(diagnostics) => diagnostics,
            Err(error) => vec![format!("插件启动失败：{error}")],
        }
    }

    /// 启动后补发 `app.start.after`（与 Python 入口的 ready 通知同一时机）。
    pub fn notify_app_started(&self) {
        self.lock().notify_app_started();
    }

    pub fn close(&self) {
        self.lock().close();
    }

    pub fn diagnostics(&self) -> Vec<String> {
        self.lock().diagnostics().to_vec()
    }

    /// 事务式切换总开关；返回新装配后的诊断行。
    pub fn set_enabled(&self, enabled: bool) -> Result<Vec<String>, String> {
        let mut runtime = self.lock();
        runtime
            .set_enabled(enabled)
            .map_err(|error| error.to_string())?;
        Ok(runtime.diagnostics().to_vec())
    }

    /// 重新装配（安装 / 启停 / 卸载后的热更新）。
    pub fn reload(&self) -> Result<Vec<String>, String> {
        self.lock().reload().map_err(|error| error.to_string())
    }

    /// 切换工作区：事务式重建 Manager，旧 Worker 会被回收。
    pub fn switch_workspace(&self, new_root: &Path) -> Result<Vec<String>, String> {
        let resolved = resolve_workspace(new_root);
        let mut runtime = self.lock();
        let diagnostics = runtime
            .switch_workspace(&resolved, true)
            .map_err(|error| error.to_string())?;
        Ok(diagnostics)
    }

    /// 已激活插件的运行态表（`/plugins` 与设置页的展示来源）。
    pub fn status_rows(&self) -> Vec<Map<String, Value>> {
        match self.lock().manager() {
            Some(manager) => manager.list_status(),
            None => Vec::new(),
        }
    }

    /// 当前执行计划的 Handler 数（诊断用）。
    pub fn handler_count(&self) -> usize {
        match self.lock().manager() {
            Some(manager) => manager.current_plan_len(),
            None => 0,
        }
    }

    // ---------- 安装器（CLI / API / TUI 共用，落盘后热更新） ----------

    /// 提交安装动作（npm 包或本地开发目录）并按需热更新。
    ///
    /// `package_spec` 以 `.` / `/` / 盘符开头或 `dev=true` 时走本地开发注册，
    /// 与 Python `cli.py` 的判定保持一致。
    #[allow(clippy::too_many_arguments)]
    pub fn install(
        &self,
        package_spec: &str,
        scope: &str,
        enable: bool,
        yes: bool,
        dev: bool,
        confirm: Option<ConfirmCallback>,
        allow_network_install: bool,
    ) -> Result<InstallResult, String> {
        let launcher = self.launcher()?;
        let is_dev = dev || looks_like_local_path(package_spec);
        let result = if is_dev {
            register_local_dev_plugin(
                Path::new(package_spec),
                &launcher.runner_path,
                scope,
                Some(&self.workspace_root),
                enable,
            )
        } else {
            parse_package_spec(package_spec).map_err(|error| error.message().to_string())?;
            install_from_npm(
                package_spec,
                &launcher.runner_path,
                scope,
                Some(&self.workspace_root),
                enable,
                yes,
                confirm,
                allow_network_install,
            )
        }
        .map_err(|error| error.message().to_string())?;
        self.refresh_after_install(&result);
        Ok(result)
    }

    /// 启停单个插件并热更新。
    pub fn set_plugin_enabled(&self, name: &str, enabled: bool, scope: &str) -> Result<(), String> {
        set_plugin_enabled(name, enabled, scope, Some(&self.workspace_root))
            .map_err(|error| error.message().to_string())?;
        let _ = self.reload();
        Ok(())
    }

    /// 卸载并热更新。
    pub fn uninstall(&self, name: &str, scope: &str, purge: bool) -> Result<(), String> {
        uninstall_plugin(name, scope, Some(&self.workspace_root), purge)
            .map_err(|error| error.message().to_string())?;
        let _ = self.reload();
        Ok(())
    }

    /// 回滚到上一版本并热更新。
    pub fn rollback(&self, name: &str, scope: &str) -> Result<String, String> {
        let launcher = self.launcher()?;
        let reference = rollback_plugin(
            name,
            &launcher.runner_path,
            scope,
            Some(&self.workspace_root),
        )
        .map_err(|error| error.message().to_string())?;
        let _ = self.reload();
        Ok(reference.version)
    }

    /// 安装产物列表（CLI 与 API 的展示来源）。
    pub fn list(&self, scope: &str) -> Vec<Map<String, Value>> {
        list_plugins(scope, Some(&self.workspace_root))
    }

    fn launcher(&self) -> Result<WorkerLauncher, String> {
        WorkerLauncher::resolve().map_err(|error| error.to_string())
    }

    fn refresh_after_install(&self, result: &InstallResult) {
        // 安装只是把记录与 store 写盘；运行期是否生效还要看 enabled 与总开关。
        if result.enabled && self.configured() {
            let _ = self.reload();
        }
    }

    // ---------- Hook 编排 ----------

    /// 统一分发：拼好 `DispatchRequest`，交给控制器判定层折算成 [`HookDecision`]。
    pub fn dispatch(
        &self,
        hook_name: &str,
        payload: Map<String, Value>,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> HookDecision {
        let runtime = self.lock();
        let (manager_present, dispatch_callable) =
            (runtime.manager().is_some(), runtime.manager().is_some());
        if !manager_present || !dispatch_callable {
            return HookDecision::Forward(payload);
        }
        let outcome = runtime.dispatch(
            hook_name,
            DispatchRequest {
                payload: payload.clone(),
                session_id: session_id.map(str::to_string),
                turn_id: turn_id.map(str::to_string),
                ..DispatchRequest::default()
            },
        );
        let dispatched: Result<DispatchResult, String> = outcome
            .map(|outcome| DispatchResult {
                denied: outcome.denied,
                deny_code: outcome.deny_code,
                deny_reason: outcome.deny_reason,
                results: outcome
                    .results
                    .iter()
                    .map(|result| PluginHookResult {
                        status: result.status.clone(),
                        handler_key: result.handler_key.clone(),
                        elapsed_ms: Some(result.elapsed_ms),
                    })
                    .collect(),
                payload: Some(Value::Object(outcome.payload)),
            })
            .map_err(|error| error.to_string());
        let fail_closed = hook_policy(hook_name)
            .map(|policy| {
                hook_requires_fail_closed(
                    policy.on_deny,
                    policy.on_timeout,
                    policy.on_protocol_error,
                    policy.on_handler_error,
                )
            })
            .unwrap_or(false);
        match resolve_dispatch(
            hook_name,
            &Value::Object(payload),
            manager_present,
            dispatch_callable,
            fail_closed,
            dispatched,
        ) {
            HookOutcome::Forward(value) => {
                HookDecision::Forward(value.as_object().cloned().unwrap_or_default())
            }
            HookOutcome::Denied(facts) => HookDecision::Denied(facts),
        }
    }

    /// turn 开始：冻结本轮执行计划，并分发 `turn.start`（可改写 `userText`）。
    pub fn turn_start(
        &self,
        user_text: &str,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Result<String, AgentError> {
        if let TurnHookAction::Call = turn_hook_action(self.active(), true) {
            if let Some(manager) = self.lock().manager() {
                manager.begin_turn();
            }
        }
        let mut payload = Map::new();
        payload.insert("userText".to_string(), Value::from(user_text));
        if let Some(session_id) = session_id {
            payload.insert("sessionId".to_string(), Value::from(session_id));
        }
        if let Some(turn_id) = turn_id {
            payload.insert("turnId".to_string(), Value::from(turn_id));
        }
        let original = payload.clone();
        let decision = self.dispatch("turn.start", payload, session_id, turn_id);
        if let Some(error) = decision.denial_error("turn.start") {
            return Err(error);
        }
        let resolved = decision.payload_or(original);
        Ok(match resolved.get("userText") {
            Some(Value::String(text)) => text.clone(),
            _ => user_text.to_string(),
        })
    }

    /// turn 结束：解除计划冻结并分发 `turn.end`。
    pub fn turn_end(&self, session_id: Option<&str>, turn_id: Option<&str>) {
        let mut payload = Map::new();
        payload.insert(
            "turnId".to_string(),
            Value::from(turn_id.unwrap_or_default()),
        );
        let _ = self.dispatch("turn.end", payload, session_id, turn_id);
        if let Some(manager) = self.lock().manager() {
            manager.end_turn();
        }
    }

    /// turn 失败：分发 `turn.error`（通知类，故障不阻断收尾）。
    pub fn turn_error(&self, error: &str, session_id: Option<&str>, turn_id: Option<&str>) {
        let mut payload = Map::new();
        payload.insert("error".to_string(), Value::from(error));
        let _ = self.dispatch("turn.error", payload, session_id, turn_id);
        if let Some(manager) = self.lock().manager() {
            manager.end_turn();
        }
    }

    /// turn 取消：分发 `turn.cancelled`。
    pub fn turn_cancelled(&self, session_id: Option<&str>, turn_id: Option<&str>) {
        let mut payload = Map::new();
        payload.insert(
            "turnId".to_string(),
            Value::from(turn_id.unwrap_or_default()),
        );
        let _ = self.dispatch("turn.cancelled", payload, session_id, turn_id);
        if let Some(manager) = self.lock().manager() {
            manager.end_turn();
        }
    }

    /// 会话建立/恢复后的生命周期 Hook：`session.start.after` 或
    /// `session.resume.before`（守卫）+ `session.resume.after`。
    pub fn session_lifecycle(
        &self,
        resume_session_id: &str,
        session_id: &str,
    ) -> Result<(), AgentError> {
        let (before, after) = session_lifecycle_hooks(resume_session_id);
        if !before.is_empty() {
            let payload = session_hook_payload(session_id);
            let decision = self.dispatch(
                before,
                payload
                    .as_object()
                    .cloned()
                    .expect("session_hook_payload 恒为对象"),
                Some(session_id),
                None,
            );
            if let Some(error) = decision.denial_error(before) {
                return Err(error);
            }
        }
        let payload = session_hook_payload(session_id);
        let _ = self.dispatch(
            after,
            payload
                .as_object()
                .cloned()
                .expect("session_hook_payload 恒为对象"),
            Some(session_id),
            None,
        );
        Ok(())
    }

    /// 会话关闭前后的 Hook（`before` 为观察类，`after` 为通知类）。
    pub fn session_close(&self, session_id: &str, before: bool) {
        let hook = if before {
            "session.close.before"
        } else {
            "session.close.after"
        };
        let mut payload = Map::new();
        payload.insert("sessionId".to_string(), Value::from(session_id));
        let _ = self.dispatch(hook, payload, Some(session_id), None);
    }

    /// `tool.call.before`：允许改写调用参数，或拒绝整个调用。
    ///
    /// 返回 `Err` 时调用方应直接产出拒绝结果（对应 Python 的
    /// `插件拒绝工具调用：{tool}。`），不再进入 schema 校验与审批。
    pub fn tool_call_before(
        &self,
        tool: &str,
        arguments: &mut Map<String, Value>,
    ) -> Result<(), AgentError> {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("arguments".to_string(), Value::Object(arguments.clone()));
        let original = payload.clone();
        let decision = self.dispatch("tool.call.before", payload, None, None);
        if let Some(error) = decision.denial_error("tool.call.before") {
            return Err(error);
        }
        let resolved = decision.payload_or(original);
        // 插件只能改 `arguments`；其他字段原样忽略。
        if let Some(Value::Object(rewritten)) = resolved.get("arguments") {
            arguments.clear();
            arguments.extend(rewritten.clone());
        }
        Ok(())
    }

    /// `tool.approval.before`：只能拒绝，不能代表用户批准。
    pub fn tool_approval_before(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
        requires_confirmation: bool,
        mode: &str,
    ) -> Result<(), AgentError> {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("arguments".to_string(), Value::Object(arguments.clone()));
        payload.insert(
            "requiresConfirmation".to_string(),
            Value::from(requires_confirmation),
        );
        payload.insert("mode".to_string(), Value::from(mode));
        let decision = self.dispatch("tool.approval.before", payload, None, None);
        match decision.denial_error("tool.approval.before") {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// `tool.approval.after`：审批结论的通知（含被拒时的原因）。失败不阻断。
    pub fn tool_approval_after(&self, tool: &str, approved: bool, reason: &str, mode: &str) {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("approved".to_string(), Value::from(approved));
        if !approved && !reason.is_empty() {
            payload.insert("reason".to_string(), Value::from(reason));
        }
        payload.insert("mode".to_string(), Value::from(mode));
        let _ = self.dispatch("tool.approval.after", payload, None, None);
    }

    /// `tool.execute.before`：执行前守卫。
    pub fn tool_execute_before(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Result<(), AgentError> {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("arguments".to_string(), Value::Object(arguments.clone()));
        let decision = self.dispatch("tool.execute.before", payload, None, None);
        match decision.denial_error("tool.execute.before") {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// `tool.execute.error`：执行体抛错时的通知。
    pub fn tool_execute_error(&self, tool: &str, error: &str) {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("error".to_string(), Value::from(error));
        let _ = self.dispatch("tool.execute.error", payload, None, None);
    }

    /// `tool.execute.after`：观察/改写执行结果，返回最终展示文本。
    pub fn tool_execute_after(&self, tool: &str, ok: bool, display_text: &str) -> String {
        let mut payload = Map::new();
        payload.insert("tool".to_string(), Value::from(tool));
        payload.insert("ok".to_string(), Value::from(ok));
        payload.insert("displayText".to_string(), Value::from(display_text));
        payload.insert("annotations".to_string(), Value::Object(Map::new()));
        let original = payload.clone();
        let decision = self.dispatch("tool.execute.after", payload, None, None);
        let resolved = decision.payload_or(original);
        match resolved.get("displayText") {
            Some(Value::String(text)) => text.clone(),
            _ => display_text.to_string(),
        }
    }

    /// `context.build.before`：允许插件附加上下文（transform），返回 `additionalContext`。
    ///
    /// 该 Hook 是 fail-open（`ignore_deny`）：被拒、超时或分发异常时退回空附加上下文，
    /// 与 Python `_context_messages` 里 `self._dispatch_plugin_hook(...) or {"additionalContext": []}`
    /// 完全同义——插件不能阻断上下文装配。
    pub fn context_build_before(
        &self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) -> Option<Value> {
        let mut payload = Map::new();
        payload.insert("additionalContext".to_string(), Value::Array(Vec::new()));
        let original = payload.clone();
        let decision = self.dispatch("context.build.before", payload, session_id, turn_id);
        // 被拒时 `payload_or` 退回原始载荷，`additionalContext` 为空数组 → 不注入。
        decision
            .payload_or(original)
            .get("additionalContext")
            .cloned()
    }

    /// `context.build.after`：只报最终上下文消息条数（观察类，失败不阻断）。
    pub fn context_build_after(
        &self,
        message_count: usize,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) {
        let mut payload = Map::new();
        payload.insert("messageCount".to_string(), Value::from(message_count));
        let _ = self.dispatch("context.build.after", payload, session_id, turn_id);
    }

    /// `context.compaction.after_turn`：回合结束边界触发压缩后的通知（notify 类，失败不阻断）。
    ///
    /// 载荷与 Python `_trigger_context_compaction_after_turn` 一致：压缩后的实际上下文 Token、
    /// 触发阈值 Token 与本回合 id。调用方需先判定 `trigger_reached`，未触发不调用。
    pub fn compaction_after_turn(
        &self,
        post_turn_context_tokens: i64,
        trigger_context_tokens: i64,
        session_id: Option<&str>,
        turn_id: Option<&str>,
    ) {
        let mut payload = Map::new();
        payload.insert(
            "postTurnContextTokens".to_string(),
            Value::from(post_turn_context_tokens),
        );
        payload.insert(
            "triggerContextTokens".to_string(),
            Value::from(trigger_context_tokens),
        );
        payload.insert(
            "turnId".to_string(),
            Value::from(turn_id.unwrap_or_default()),
        );
        let _ = self.dispatch(
            "context.compaction.after_turn",
            payload,
            session_id,
            turn_id,
        );
    }

    /// `model.request.before`：允许插件改写消息（transform）或拒绝本轮（guard）。
    ///
    /// 载荷与 Python 同形 `{messages, model}`；拒绝时返回带插件文案的 `AgentError`，
    /// 调用方据此中止本轮模型请求。放行时用返回值里的 `messages` 覆盖入参（仅在
    /// 确为数组时，与 Python 的 `isinstance(..., list)` 判定一致）。
    pub fn model_request_before(
        &self,
        messages: &mut Vec<Value>,
        model: &str,
        session_id: Option<&str>,
    ) -> Result<(), AgentError> {
        let mut payload = Map::new();
        payload.insert("messages".to_string(), Value::Array(messages.clone()));
        payload.insert("model".to_string(), Value::from(model));
        let original = payload.clone();
        let decision = self.dispatch("model.request.before", payload, session_id, None);
        if let Some(error) = decision.denial_error("model.request.before") {
            return Err(error);
        }
        let resolved = decision.payload_or(original);
        if let Some(Value::Array(rewritten)) = resolved.get("messages") {
            *messages = rewritten.clone();
        }
        Ok(())
    }

    /// `model.response.after`：模型请求成功返回后的观察通知（失败不阻断）。
    pub fn model_response_after(
        &self,
        model: &str,
        content: &str,
        tool_call_count: usize,
        session_id: Option<&str>,
    ) {
        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::from(model));
        payload.insert("content".to_string(), Value::from(content));
        payload.insert("toolCallCount".to_string(), Value::from(tool_call_count));
        let _ = self.dispatch("model.response.after", payload, session_id, None);
    }

    /// `model.request.error`：模型请求以协议错误终结时的通知（失败不阻断）。
    pub fn model_request_error(&self, error: &str, model: &str, session_id: Option<&str>) {
        let mut payload = Map::new();
        payload.insert("error".to_string(), Value::from(error));
        payload.insert("model".to_string(), Value::from(model));
        let _ = self.dispatch("model.request.error", payload, session_id, None);
    }
}

/// 工作区根：空路径回落进程工作目录（与 Python `Path.cwd()` 一致）。
fn resolve_workspace(workspace_root: &Path) -> PathBuf {
    if workspace_root.as_os_str().is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    workspace_root.to_path_buf()
}

/// `cli.py` 的本地路径判定：`.` / `/` 开头或含 Windows 盘符。
fn looks_like_local_path(package_spec: &str) -> bool {
    if package_spec.starts_with('.') || package_spec.starts_with('/') || package_spec.contains('\\')
    {
        return true;
    }
    let bytes = package_spec.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':'
}

/// 环境变量名透出给调用方（启动器与打包脚本共用同一份约定）。
pub const RUNNER_DIR_VARIABLE: &str = RUNNER_DIR_ENV;

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> PluginHost {
        PluginHost::new(Path::new("."), PluginsConfig::default())
    }

    #[test]
    fn context_build_before_falls_back_to_empty_when_disabled() {
        // fail-open：没有 Manager 时原样返回初始载荷，即空附加上下文。
        let plugins = host();
        assert_eq!(
            plugins.context_build_before(None, None),
            Some(Value::Array(Vec::new()))
        );
        assert_eq!(
            plugins.context_build_before(Some("session-1"), Some("turn-1")),
            Some(Value::Array(Vec::new()))
        );
    }

    #[test]
    fn notify_context_hooks_are_noops_when_disabled() {
        let plugins = host();
        plugins.context_build_after(3, None, None);
        plugins.compaction_after_turn(120_000, 100_000, Some("session-1"), Some("turn-1"));
    }

    #[test]
    fn model_hooks_fall_back_when_disabled() {
        let plugins = host();
        let mut messages = vec![serde_json::json!({"role": "user", "content": "你好"})];
        // 未启用（无 Manager）时每个模型 Hook 都退化为原样放行。
        plugins
            .model_request_before(&mut messages, "gpt-4o", None)
            .expect("模型请求 Hook 应原样放行");
        assert_eq!(messages.len(), 1);
        plugins.model_response_after("gpt-4o", "完成", 0, None);
        plugins.model_request_error("上游超时", "gpt-4o", None);
    }
}
