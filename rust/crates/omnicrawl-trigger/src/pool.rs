//! slot 池与一个 loop 的生命周期。
//!
//! 一个 slot 就是一套独立的 `AgentService`：自己的内核子进程、自己的工具表、自己的会话。
//! 池负责三件事：
//!
//! 1. **排队与并发**：`max_concurrency` 个 slot，超出排队；队列满由记录层拒绝（429）。
//! 2. **跑一轮**：把注入后的输入交给 `AgentService::start_run`，边读事件边转发到记录的事件流。
//! 3. **收尾**：超时与取消走同一条路径——请求取消、等运行记录进入终态、归还 slot、写终态。
//!
//! 为什么每轮都新建 `AgentService` 而不是复用一个：无人值守的两次触发之间没有任何共享状态，
//! 复用会把上一次的会话（以及它占用的内核进程）带进下一次。代价是一次内核启动，收益是两次
//! 触发之间彻底隔离——这正是「独立 loop」的含义。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use omnicrawl_api::{AgentService, ServiceOptions};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::trigger_api::TriggerApiConfig;

use crate::notify;
use crate::records::{NotifyOutcome, TriggerRecord, TriggerState, TriggerStore};

/// 事件轮询粒度：与本地 API 的 SSE 生产者同一量级。
const EVENT_POLL: Duration = Duration::from_millis(50);
/// 取消（含超时）后等运行记录进入终态的上限：单活动名额由运行记录持有，提前返回会让
/// 下一次 `start_run` 撞上 `RUN_ACTIVE`。
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 关闭服务时等活动回合收尾的上限；超时即杀内核进程树（由 `AgentService::close` 负责）。
const SHUTDOWN_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
/// 无人值守追加的固定约束：不可关闭，因为它描述的是「这个 loop 没有人在看着」这个事实。
pub const UNATTENDED_PROMPT: &str = "本次会话由外部事件触发，没有人在终端前：\
不要调用 ask_user，不要等待人工确认；拿不到信息就用现有证据给出结论，\
并在结尾用一句话写明这次做了什么。";

/// 池的运行期配置快照。
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_concurrency: usize,
    pub session_mode: String,
    pub default_workspace: String,
    pub default_timeout_seconds: i64,
    pub tools: Vec<String>,
    pub system_prompt_append: String,
    pub session_root: PathBuf,
}

impl PoolConfig {
    /// 从 `[trigger_api]` 段与会话根装配。
    pub fn from_config(api: &TriggerApiConfig, session_root: PathBuf) -> Self {
        Self {
            max_concurrency: api.max_concurrency.max(1) as usize,
            session_mode: api.session_mode.clone(),
            default_workspace: api.default_workspace.clone(),
            default_timeout_seconds: api.default_timeout_seconds,
            tools: api.tools.clone(),
            system_prompt_append: api.system_prompt_append.clone(),
            session_root,
        }
    }
}

/// 触发池：记录仓库 + 运行期配置 + 内核程序。
pub struct TriggerPool {
    store: Arc<TriggerStore>,
    config: PoolConfig,
    env: ConfigEnvironment,
    kernel_program: PathBuf,
    /// 世界是否还在转；关闭后不再取新任务，也不再受理新触发。
    alive: Arc<AtomicBool>,
    /// 正在跑的活动 slot：关闭时逐个请求取消。
    active: Arc<Mutex<Vec<Arc<AgentService>>>>,
}

impl Clone for TriggerPool {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            config: self.config.clone(),
            env: self.env.clone(),
            kernel_program: self.kernel_program.clone(),
            alive: Arc::clone(&self.alive),
            active: Arc::clone(&self.active),
        }
    }
}

impl std::fmt::Debug for TriggerPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TriggerPool")
            .field("config", &self.config)
            .finish()
    }
}

impl TriggerPool {
    pub fn new(
        store: Arc<TriggerStore>,
        config: PoolConfig,
        env: ConfigEnvironment,
        kernel_program: PathBuf,
    ) -> Self {
        Self {
            store,
            config,
            env,
            kernel_program,
            alive: Arc::new(AtomicBool::new(true)),
            active: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn store(&self) -> &Arc<TriggerStore> {
        &self.store
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// 起工作线程：每个线程循环「等任务 → 跑一轮」，数量等于并发上限。
    pub fn start(self: &Arc<Self>) {
        for index in 0..self.config.max_concurrency {
            let pool = Arc::clone(self);
            thread::Builder::new()
                .name(format!("trigger-slot-{index}"))
                .spawn(move || pool.slot_loop())
                .unwrap_or_else(|error| {
                    eprintln!("[trigger] 启动工作线程失败：{error}");
                    std::process::exit(1)
                });
        }
    }

    /// 一个 slot 的主循环。
    fn slot_loop(self: Arc<Self>) {
        while self.is_alive() {
            // 先判活再等：关闭信号会把 `alive` 置假并唤醒，这里下一轮就退出。
            self.store.wait_for_work(Duration::from_millis(500));
            if !self.is_alive() {
                return;
            }
            let Some(record) = self.store.take_next(self.config.max_concurrency) else {
                continue;
            };
            self.run_job(record);
        }
    }

    /// 跑一个触发：装配服务、跑回合、收尾。任何失败都落到 `failed` 而不是悄悄消失。
    fn run_job(&self, record: TriggerRecord) {
        let trigger_id = record.trigger_id.clone();
        let _ = self.store.append_event(
            &trigger_id,
            "run.started",
            json!({"trigger_id": trigger_id, "title": record.title}),
        );

        let service = match self.spawn_service(&record) {
            Ok(service) => service,
            Err(error) => {
                eprintln!("[trigger] {trigger_id} 启动失败：{error}");
                self.finish(&record, TriggerState::Failed, "", &error);
                return;
            }
        };
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active.push(Arc::clone(&service));
        }
        // 会话 ID 一旦报出就记下：转录端点与排障都要它。
        let session_id = service.current_session_id();
        if !session_id.is_empty() {
            let _ = self.store.attach_session(&trigger_id, &session_id);
        }

        let outcome = self.drive_turn(&record, &service);
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active.retain(|item| !Arc::ptr_eq(item, &service));
        }
        service.close();

        match outcome {
            TurnOutcome::Done {
                text,
                model_turns,
                tool_calls,
            } => {
                let _ = self
                    .store
                    .record_usage(&trigger_id, model_turns, tool_calls);
                self.finish(&record, TriggerState::Completed, &text, "");
            }
            TurnOutcome::Cancelled {
                model_turns,
                tool_calls,
            } => {
                let _ = self
                    .store
                    .record_usage(&trigger_id, model_turns, tool_calls);
                self.finish(&record, TriggerState::Cancelled, "", "取消：回合被中断。");
            }
            TurnOutcome::Failed {
                message,
                model_turns,
                tool_calls,
            } => {
                let _ = self
                    .store
                    .record_usage(&trigger_id, model_turns, tool_calls);
                self.finish(&record, TriggerState::Failed, "", &message);
            }
        }
    }

    /// 起一套独立的 AgentService：审批固定自动审查、工具表收窄到白名单、会话根独立。
    fn spawn_service(&self, record: &TriggerRecord) -> Result<Arc<AgentService>, String> {
        let mut options: ServiceOptions =
            omnicrawl_api::service::connector_options_from_process(&self.env)?;
        // 无人值守：没有人在终端前点确认，审批只能靠自动审查（审查闸仍会拦下危险调用）；
        // 没有审查运行期时 `ApprovalMode::Review` 是 fail-closed 的。
        options.approval = omnicrawl_host::approval::ApprovalMode::Review;
        options.registry_options.allowed_tools = Some(self.config.tools.clone());
        // 等人工确认的上限压到几秒：无人值守没人会点「批准」，等满默认的几分钟只会把回合
        // 拖到超时。等不到就按拒绝处理。
        options.confirmation_timeout_seconds = options.confirmation_timeout_seconds.min(5.0);
        // 托管提问不启用：无人值守没有答主，宁可让模型自己给结论，也不让决策模型替他编答案。
        options.custody = None;
        // 会话根独立：无人值守噪音不进用户的对话历史，也不出现在 TUI 的 `/sessions` 里。
        options.session_root = Some(self.config.session_root.clone());
        options.workspace_root = self.workspace_for(record)?;
        // 提示词与 TUI 同源（模板 / AGENTS.md / Skill 索引），只在其后追加无人值守约束与
        // 部署专属追加段。追加走 `PromptRuntime` 的入口而不是改写 `model.system_prompt`：
        // 后者会被 `runner_options` 用装配结果覆盖掉。
        if let Some(runtime) = options.prompt.as_ref() {
            runtime.set_system_prompt_append(unattended_prompt_append(
                &self.config.system_prompt_append,
            ));
        } else {
            // 没有装配结果（配置缺失等）时才退回直接改写兜底提示词。
            options.model.system_prompt = unattended_system_prompt(
                &options.model.system_prompt,
                &self.config.system_prompt_append,
            );
        }
        AgentService::spawn(options, &self.kernel_program)
            .map(Arc::new)
            .map_err(|error| format!("启动内核失败：{error}"))
    }

    /// 工作区：请求给了就用请求的（必须存在），否则依次回落配置缺省与宿主当前工作区。
    fn workspace_for(&self, record: &TriggerRecord) -> Result<PathBuf, String> {
        let candidate = if !record.workspace.trim().is_empty() {
            record.workspace.trim().to_string()
        } else if !self.config.default_workspace.trim().is_empty() {
            self.config.default_workspace.trim().to_string()
        } else {
            String::new()
        };
        if candidate.is_empty() {
            // 宿主当前工作区：`connector_options_from_process` 已经按配置解析过一遍。
            let options = omnicrawl_api::service::connector_options_from_process(&self.env)?;
            return Ok(options.workspace_root);
        }
        let path = PathBuf::from(&candidate);
        if !path.is_dir() {
            return Err(format!("工作区不存在或不是目录：{candidate}"));
        }
        Ok(path)
    }

    /// 跑一轮并翻译事件；超时与取消都收敛到同一个收尾路径。
    fn drive_turn(&self, record: &TriggerRecord, service: &Arc<AgentService>) -> TurnOutcome {
        let trigger_id = record.trigger_id.clone();
        let timeout = Duration::from_secs(record.timeout_seconds.max(1) as u64);
        let run = match service.start_run(&record.prompt) {
            Ok(run) => run,
            Err(error) => {
                return TurnOutcome::Failed {
                    message: format!("受理回合失败：{}", error.message),
                    model_turns: 0,
                    tool_calls: 0,
                };
            }
        };
        let run_id = run.run_id.clone();
        let deadline = Instant::now() + timeout;
        let mut cursor = 0u64;
        let mut tool_calls = 0u64;
        let mut model_turns = 0u64;

        loop {
            // 取消与超时走同一条收尾：请求取消、等终态、按取消返回。
            let timed_out = Instant::now() >= deadline;
            let cancelled = self.store.cancel_requested(&trigger_id).unwrap_or(false);
            if timed_out || cancelled {
                let _ = service.cancel_run(&run_id);
                self.await_settle(service, &run_id);
                if timed_out {
                    return TurnOutcome::Failed {
                        message: format!("回合超时（{} 秒）已取消。", record.timeout_seconds),
                        model_turns,
                        tool_calls,
                    };
                }
                return TurnOutcome::Cancelled {
                    model_turns,
                    tool_calls,
                };
            }

            let items = match service.events_after(&run_id, cursor) {
                Ok(items) => items,
                // 长流输出会把早期增量挤出保留窗口；终态事件永远是最新一条，
                // 跳到保留窗口起点继续读即可，不因此判整轮失败。
                Err(error) if error.code == "EVENT_CURSOR_EXPIRED" => {
                    let earliest = error
                        .details
                        .as_ref()
                        .and_then(|details| details.get("earliest_event_id"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    cursor = earliest.saturating_sub(1);
                    continue;
                }
                Err(error) => {
                    return TurnOutcome::Failed {
                        message: format!("读取回合事件失败：{}", error.message),
                        model_turns,
                        tool_calls,
                    };
                }
            };

            for item in items {
                cursor = item.id;
                if item.event == "tool.started" {
                    tool_calls += 1;
                }
                if item.event == "run.started" {
                    model_turns += 1;
                }
                // 事件原样转发：客户端复用 `/runs/{id}/events` 的同一套解析。
                let _ = self
                    .store
                    .append_event(&trigger_id, &item.event, item.data.clone());
                match item.event.as_str() {
                    "run.completed" => {
                        let text = item
                            .data
                            .get("result")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        return TurnOutcome::Done {
                            text,
                            model_turns,
                            tool_calls,
                        };
                    }
                    "run.cancelled" => {
                        return TurnOutcome::Cancelled {
                            model_turns,
                            tool_calls,
                        };
                    }
                    "run.failed" => {
                        let message = item
                            .data
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("回合失败。")
                            .to_string();
                        return TurnOutcome::Failed {
                            message,
                            model_turns,
                            tool_calls,
                        };
                    }
                    _ => {}
                }
            }
            thread::sleep(EVENT_POLL);
        }
    }

    /// 等运行记录进入终态：单活动名额由它持有，提前返回会让下一次 `start_run` 撞上 `RUN_ACTIVE`。
    fn await_settle(&self, service: &AgentService, run_id: &str) {
        let deadline = Instant::now() + CANCEL_SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            match service.get_run(run_id) {
                Ok(run) if run.status.is_terminal() => return,
                Ok(_) => {}
                // 记录已被保留窗口淘汰：没有活动名额可让。
                Err(_) => return,
            }
            thread::sleep(EVENT_POLL);
        }
    }

    /// 终态收尾：写记录、追加终态事件、按 `notify` 投递。
    fn finish(&self, record: &TriggerRecord, state: TriggerState, result: &str, error: &str) {
        let trigger_id = record.trigger_id.clone();
        if let Err(failure) = self.store.finish(&trigger_id, state, result, error) {
            eprintln!("[trigger] {trigger_id} 写终态失败：{failure:?}");
            return;
        }
        let event = match state {
            TriggerState::Completed => "run.completed",
            TriggerState::Cancelled => "run.cancelled",
            _ => "run.failed",
        };
        let _ = self.store.append_event(
            &trigger_id,
            event,
            json!({"trigger_id": trigger_id, "result": result, "message": error}),
        );
        self.deliver(record, state, result, error);
    }

    /// 终态投递：失败只写 `notify.error`，不改触发状态。
    fn deliver(&self, record: &TriggerRecord, state: TriggerState, result: &str, error: &str) {
        let Some(spec) = record.notify.as_ref() else {
            return;
        };
        if !spec.matches(state) {
            return;
        }
        let outcome = match notify::deliver(&self.env, spec, &record.title, state, result, error) {
            Ok(()) => NotifyOutcome::delivered(),
            Err(reason) => {
                eprintln!("[trigger] {} 投递失败：{reason}", record.trigger_id);
                NotifyOutcome::failed(reason)
            }
        };
        let _ = self.store.record_notify(&record.trigger_id, outcome);
    }

    /// 关闭：停止取新任务、取消排队中的、请求取消运行中的，然后等它们收尾。
    pub fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        let queued = self.store.cancel_queued();
        if queued > 0 {
            eprintln!("[trigger] 已取消排队中的 {queued} 条触发。");
        }
        // 运行中的逐个请求取消：真正的收尾在各自的 slot 线程里（它持有运行记录与内核进程）。
        for trigger_id in self.store.running_ids() {
            let _ = self.store.request_cancel(&trigger_id);
        }
        let deadline = Instant::now() + SHUTDOWN_SETTLE_TIMEOUT;
        while Instant::now() < deadline && !self.store.running_ids().is_empty() {
            thread::sleep(Duration::from_millis(100));
        }
        // 等超时仍未收尾的：直接关服务（`AgentService::close` 会请求内核退出并回收资源）。
        let active: Vec<Arc<AgentService>> = {
            let guard = self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.clone()
        };
        for service in active {
            service.close();
        }
    }
}

/// 一轮的结果。
enum TurnOutcome {
    Done {
        text: String,
        model_turns: u64,
        tool_calls: u64,
    },
    Cancelled {
        model_turns: u64,
        tool_calls: u64,
    },
    Failed {
        message: String,
        model_turns: u64,
        tool_calls: u64,
    },
}

/// 系统提示词 = 原有装配 + 固定的无人值守约束 + 部署专属追加段。
pub fn unattended_system_prompt(base: &str, append: &str) -> String {
    let mut out = base.trim_end().to_string();
    out.push_str("\n\n");
    out.push_str(UNATTENDED_PROMPT);
    let append = append.trim();
    if !append.is_empty() {
        out.push_str("\n\n");
        out.push_str(append);
    }
    out
}

/// 只算追加段（供 `PromptRuntime::set_system_prompt_append` 使用）。
pub fn unattended_prompt_append(append: &str) -> String {
    let mut out = UNATTENDED_PROMPT.to_string();
    let append = append.trim();
    if !append.is_empty() {
        out.push_str("\n\n");
        out.push_str(append);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattended_prompt_keeps_base_and_appends_constraints() {
        let prompt = unattended_system_prompt("基础提示词", "回复不超过 300 字");
        assert!(prompt.starts_with("基础提示词"));
        assert!(prompt.contains("不要调用 ask_user"));
        assert!(prompt.ends_with("回复不超过 300 字"));
    }

    #[test]
    fn unattended_prompt_skips_empty_append() {
        let prompt = unattended_system_prompt("基础", "   ");
        assert!(prompt.contains("不要等待人工确认"));
        assert!(!prompt.ends_with("\n\n"));
    }

    #[test]
    fn pool_config_maps_api_section() {
        let api = omnicrawl_config::features::trigger_api::TriggerApiConfig {
            max_concurrency: 3,
            session_mode: "keyed".to_string(),
            default_workspace: "D:/Proj".to_string(),
            default_timeout_seconds: 120,
            tools: vec!["read".to_string()],
            system_prompt_append: "简洁".to_string(),
            ..Default::default()
        };
        let config = PoolConfig::from_config(&api, PathBuf::from("C:/sessions"));
        assert_eq!(config.max_concurrency, 3);
        assert_eq!(config.session_mode, "keyed");
        assert_eq!(config.default_timeout_seconds, 120);
        assert_eq!(config.tools, vec!["read".to_string()]);
        assert_eq!(config.session_root, PathBuf::from("C:/sessions"));
    }
}
