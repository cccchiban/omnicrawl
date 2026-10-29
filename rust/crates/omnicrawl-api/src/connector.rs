//! 连接器宿主模式：`omnicrawl-api connector <平台>` 在宿主进程内承载消息平台连接器。
//!
//! 内核自带的 `omnicrawl --connector <平台>` 只在进程内自持 `update_todos` / `ask_user` /
//! `pause_work` 三个工具，其余工具一律回 `tool_unavailable`，会话也不落盘——那条路径只能
//! 对话，无法读写项目，远程用户看到的就是「我无法访问你的项目仓库」。远程连接器的正确
//! 形态与 Python 一致：由**宿主**承载，复用宿主的真实工具表、会话存储与命令注册表。
//!
//! 本模块因此不再另写一套 Agent：它把连接器的 [`AgentDriver`] 面接到本地 API 的同一个
//! `AgentService` 上——同一个内核子进程、同一张工具表、同一份会话目录，只是对外形态从
//! HTTP/SSE 换成消息平台。审批与提问由连接器桥接（`/approve`、回复文本），走的是
//! `AgentService` 已有的待决决策入口。
//!
//! 无宿主载荷的平台（armv7 等）拿不到本二进制，`entry` 会继续把 `--connector` 交给内核，
//! 那条旧路径保留不动。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use omnicrawl_commands::agent::{AgentError, CommandAgent, GitOutput, SessionSummary, SubAgentRun};
use omnicrawl_commands::framework::{Channel, CommandType};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::core::workspace::save_workspace_root;
use omnicrawl_config::features::advisor::AdvisorConfig;
use omnicrawl_config::models::llm::{load_llm_config, LlmConfig};
use omnicrawl_extensions::skill::{SkillDiagnostic, SkillMeta};
use omnicrawl_session::{PromptHistoryEntry, SessionIndexEntry};
use serde_json::{json, Map, Value};

use omnicrawl_connectors::agent::{
    AgentDriver, AgentStatus, AskUserHandler, ConfirmHandler, ToolCall, TurnError, TurnEvent,
    TurnOutcome, WorkspaceSwitch,
};
use omnicrawl_connectors::feishu::{
    check_config, load_feishu_config, ConfigSource, FeishuApi, FeishuBot,
};
use omnicrawl_connectors::http::{HttpTransport, UreqTransport};
use omnicrawl_connectors::qqbot::{load_qqbot_config, QqApi, QqBot};
use omnicrawl_connectors::telegram::{load_telegram_config, TelegramBot};

use crate::service::{
    connector_options_from_process, resolve_kernel_program, AgentService, ServiceOptions,
};

/// 事件轮询间隔：连接器侧按秒级节流发送，这里只需不漏事件。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 回合收尾的兜底上限：超过它仍没有终态就按失败收口，避免任务线程永久挂住。
const TURN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// 取消后等运行记录进入终态的上限：单活动名额由运行记录持有，提前返回会让用户
/// 下一条消息直接撞上 `RUN_ACTIVE`。
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(120);

/// 模型流重试事件：显示层要靠它丢弃作废的半截输出，不能降级成状态提示。
pub(crate) const ROLLBACK_EVENT: &str = "assistant.rollback";

/// 启动指定平台的连接器并阻塞到它退出。
pub fn run(platform: &str) -> Result<(), String> {
    run_with_args(platform, &[])
}

/// 带附加参数启动连接器：`qqbot --bind` 走扫码绑定，其余参数忽略。
pub fn run_with_args(platform: &str, args: &[String]) -> Result<(), String> {
    if platform == "qqbot" && args.iter().any(|arg| arg == "--bind" || arg == "-b") {
        crate::qqbind::run_bind().map(|_outcome| ())?;
        return Ok(());
    }
    match platform {
        "telegram" => run_telegram(),
        "feishu" => run_feishu(),
        "qqbot" => run_qqbot(),
        // MQTT 不是聊天平台：入口是遥测，出口是指令，没有时间线可渲染。
        "mqtt" => crate::mqtt::run(),
        other => Err(format!(
            "未知连接器：{other}；当前支持 telegram、feishu、qqbot、mqtt。"
        )),
    }
}

fn run_telegram() -> Result<(), String> {
    let env = ConfigEnvironment::from_process();
    let data = crate::service::connector_config_data(&env);
    let config = load_telegram_config(data.get("telegram"))?;
    let driver = Arc::new(HostConnectorDriver::start(&env)?);
    let transport: Arc<dyn HttpTransport> = Arc::new(UreqTransport::new());
    let bot = Arc::new(TelegramBot::new(&config, transport, Arc::clone(&driver))?);
    eprintln!("[connector] telegram 连接器已在宿主进程内启动（真实工具表与会话）。");
    let outcome = bot.run().map_err(|error| error.to_string());
    driver.shutdown();
    outcome
}

fn run_feishu() -> Result<(), String> {
    let env = ConfigEnvironment::from_process();
    let data = crate::service::connector_config_data(&env);
    let config = load_feishu_config(ConfigSource { data: &data })?;
    if check_config(&config).get("ready").and_then(Value::as_bool) != Some(true) {
        return Err(
            "飞书连接器还没有就绪：请在 config.toml 的 [feishu] 段填写 app_id 与 app_secret\
             （可选 allowed_user_ids）。"
                .to_string(),
        );
    }
    let driver = Arc::new(HostConnectorDriver::start(&env)?);
    let api = Arc::new(FeishuApi::with_ureq(&config.app_id, &config.app_secret));
    let bot = Arc::new(FeishuBot::new(
        api,
        Arc::clone(&driver),
        config.allowed_user_ids.clone(),
        config.confirmation_timeout_seconds,
    ));
    eprintln!("[connector] feishu 连接器已在宿主进程内启动（真实工具表与会话）。");
    let outcome = bot.run().map_err(|error| error.to_string());
    driver.shutdown();
    outcome
}

fn run_qqbot() -> Result<(), String> {
    let env = ConfigEnvironment::from_process();
    let data = crate::service::connector_config_data(&env);
    let config = load_qqbot_config(data.get("qqbot"))?;
    if omnicrawl_connectors::qqbot::check_config(&config)
        .get("ready")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(concat!(
            "QQ 机器人连接器还没有就绪：请在 config.toml 的 [qqbot] 段填写 app_id 与 app_secret",
            "（可先用 `omnicrawl-host connector qqbot --bind` 扫码获取）。"
        )
        .to_string());
    }
    let driver = Arc::new(HostConnectorDriver::start(&env)?);
    let api = Arc::new(QqApi::with_ureq(&config));
    let bot = Arc::new(QqBot::new(&config, api, Arc::clone(&driver))?);
    eprintln!(
        "[connector] qqbot 连接器已在宿主进程内启动（真实工具表与会话，平台 {}）。",
        if config.sandbox { "沙箱" } else { "正式" }
    );
    let outcome = bot.run();
    driver.shutdown();
    outcome
}

/// 宿主侧连接器驱动：把连接器的 `AgentDriver` 面接到 [`AgentService`]。
///
/// 一个连接器进程持有一个内核子进程与一张工具表：连接器自身保证同一时刻只有一个活动
/// 任务，因此这里不需要额外的运行并发控制，`AgentService` 的单活动语义刚好吻合。
pub struct HostConnectorDriver {
    service: Arc<AgentService>,
    confirm: Mutex<Option<Arc<dyn ConfirmHandler>>>,
    ask_user: Mutex<Option<Arc<dyn AskUserHandler>>>,
    /// 当前回合的 Run ID：取消与审批决议都要它。
    current_run: Mutex<String>,
    cancelled: AtomicBool,
}

impl HostConnectorDriver {
    /// 起内核、握手、建工具表；装配口径与本地 API 完全同源。
    pub fn start(env: &ConfigEnvironment) -> Result<HostConnectorDriver, String> {
        let options: ServiceOptions = connector_options_from_process(env)?;
        let program = resolve_kernel_program(env, None);
        Self::start_with(options, &program)
    }

    /// 带自定义选项启动：无人值守回路要改审批模式与工具白名单，
    /// 不能再把配置原样读一遍（配置里的 `auto` 与全量工具表都不适合无人值守）。
    pub fn start_with(
        options: ServiceOptions,
        program: &std::path::Path,
    ) -> Result<HostConnectorDriver, String> {
        let service = AgentService::spawn(options, program)
            .map_err(|error| format!("连接器宿主启动失败：{error}"))?;
        let service = Arc::new(service);
        // 回合外事件泵：后台子任务事件不必等到下一次回合才进会话级事件流。
        service.start_event_pump();
        Ok(HostConnectorDriver {
            service,
            confirm: Mutex::new(None),
            ask_user: Mutex::new(None),
            current_run: Mutex::new(String::new()),
            cancelled: AtomicBool::new(false),
        })
    }

    /// 收尾：请内核退出并回收资源。
    pub fn shutdown(&self) {
        self.service.close();
    }

    fn confirm_handler(&self) -> Option<Arc<dyn ConfirmHandler>> {
        self.confirm.lock().ok().and_then(|slot| slot.clone())
    }

    fn ask_user_handler(&self) -> Option<Arc<dyn AskUserHandler>> {
        self.ask_user.lock().ok().and_then(|slot| slot.clone())
    }

    fn set_current_run(&self, run_id: &str) {
        if let Ok(mut slot) = self.current_run.lock() {
            *slot = run_id.to_string();
        }
    }

    fn current_run(&self) -> String {
        self.current_run
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default()
    }

    /// 一个回合的主体：提交给 `AgentService`，再把公开事件翻成连接器的回合事件。
    ///
    /// 审批与提问在事件流里出现时**就地**问连接器再回填决议：连接器的 `confirm` /
    /// `ask` 都是阻塞接口（等到用户 `/approve` 或回复文本），因此这段等待发生在任务线程里，
    /// 与 TUI 侧把面板交给用户是同一个语义。
    fn drive_turn(
        &self,
        text: &str,
        events: &mut dyn FnMut(TurnEvent),
    ) -> Result<String, TurnError> {
        // 先复位取消标志再受理：`/cancel` 在上一回合结束后到达时不该牵连本回合，
        // 而在受理之后到达的取消必须被本轮看见并落到本轮 run 上。
        self.cancelled.store(false, Ordering::SeqCst);
        let record = self
            .service
            .start_run(text)
            .map_err(|error| TurnError::Failed(error.message.clone()))?;
        let run_id = record.run_id.clone();
        self.set_current_run(&run_id);

        let mut cursor = 0u64;
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            if self.cancelled.load(Ordering::SeqCst) {
                let _ = self.service.cancel_run(&run_id);
                // 运行记录的终态由回合线程写，单活动名额也由它让出。取消请求只是置位，
                // 这里必须等到状态落到终态才能返回——否则连接器以为回合已结束，
                // 用户的下一句话会立刻收到「当前已有生成任务运行」。
                self.await_settle(&run_id);
                return Err(TurnError::Cancelled);
            }
            let items = match self.service.events_after(&run_id, cursor) {
                Ok(items) => items,
                // 运行事件按条数保留，长流输出会把早期的增量挤出去（`EVENT_CURSOR_EXPIRED`）。
                // 这不是失败：终态事件永远是最新一条，跳到保留窗口的起点继续读即可，
                // 否则一次长回答就会因为「没跟上增量」而整轮报错。
                Err(error) if error.code == "EVENT_CURSOR_EXPIRED" => {
                    let earliest = error
                        .details
                        .as_ref()
                        .and_then(|details| details.get("earliest_event_id"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    if earliest == 0 {
                        return Err(TurnError::Failed(error.message.clone()));
                    }
                    cursor = earliest.saturating_sub(1);
                    continue;
                }
                Err(error) => {
                    return Err(TurnError::Failed(error.message.clone()));
                }
            };
            for item in items {
                cursor = item.id;
                if let Some(outcome) = self.dispatch_event(&run_id, &item.event, &item.data, events)
                {
                    return outcome;
                }
            }
            if Instant::now() >= deadline {
                let _ = self.service.cancel_run(&run_id);
                self.await_settle(&run_id);
                return Err(TurnError::Failed("回合超时未收尾。".to_string()));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// 取消收尾：等到运行记录进入终态，让出单活动名额后再返回。
    ///
    /// 取消是建议性的（内核可能已经在收尾），因此这里只等终态事实，不再向显示层转发
    /// 任何增量：用户已经要求取消，迟到的正文段不该再追加到被中断的消息里。
    /// 等待超时也按取消返回——但那样下一回合会收到 `RUN_ACTIVE`，所以尽量等到。
    fn await_settle(&self, run_id: &str) {
        let deadline = Instant::now() + CANCEL_SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            match self.service.get_run(run_id) {
                Ok(run) if run.status.is_terminal() => return,
                Ok(_) => {}
                // 记录已不在（保留窗口淘汰）：没有活动名额可让，直接收尾。
                Err(_) => return,
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// 处理一条运行事件；返回 `Some` 表示回合已到终态。
    fn dispatch_event(
        &self,
        run_id: &str,
        name: &str,
        data: &Value,
        events: &mut dyn FnMut(TurnEvent),
    ) -> Option<Result<String, TurnError>> {
        if let Some(outcome) = terminal_outcome(name, data) {
            return Some(outcome);
        }
        match name {
            "assistant.delta" => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    events(TurnEvent::Delta(delta.to_string()));
                }
            }
            "status.changed" => {
                if let Some(message) = data.get("message").and_then(Value::as_str) {
                    events(TurnEvent::Status(message.to_string()));
                }
            }
            // 模型流重试：作废的半截输出必须让显示层丢弃，否则重试前的片段会和
            // 重试后的新正文累积在同一条消息里。
            ROLLBACK_EVENT => events(TurnEvent::StreamRollback),
            "tool.started" => {
                let call = tool_call_from(data);
                let step = data.get("step").and_then(Value::as_u64).unwrap_or(1);
                events(TurnEvent::ToolStarted { step, call });
            }
            "tool.completed" => {
                let call = tool_call_from(data);
                let result = omnicrawl_connectors::agent::ToolResult {
                    ok: data.get("ok").and_then(Value::as_bool).unwrap_or(true),
                    output: data
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    error_code: None,
                    retryable: false,
                };
                events(TurnEvent::ToolFinished { call, result });
            }
            "todo.updated" => {
                let todos = data.get("todos").cloned().unwrap_or(Value::Null);
                events(TurnEvent::TodoUpdate { todos });
            }
            "confirmation.required" => self.settle_confirmation(run_id, data),
            "ask_user.required" => self.settle_question(run_id, data),
            _ => {}
        }
        None
    }

    /// `confirmation.required`：问连接器拿到批准与否，再回填决议。
    fn settle_confirmation(&self, run_id: &str, data: &Value) {
        let Some(confirmation_id) = data.get("confirmation_id").and_then(Value::as_str) else {
            return;
        };
        let tool = data.get("tool").and_then(Value::as_str).unwrap_or_default();
        let arguments = data.get("arguments").cloned().unwrap_or(Value::Null);
        let approved = match self.confirm_handler() {
            Some(handler) => handler.confirm(tool, &arguments),
            // 没有桥接时按拒绝：审批边界 fail-closed，不能默认放行。
            None => false,
        };
        if let Err(error) = self
            .service
            .decide_confirmation(run_id, confirmation_id, approved)
        {
            eprintln!("[connector] 回填审批决议失败：{}", error.message);
        }
    }

    /// `ask_user.required`：问连接器拿到答案，再回填提问。
    fn settle_question(&self, run_id: &str, data: &Value) {
        let Some(question_id) = data.get("question_id").and_then(Value::as_str) else {
            return;
        };
        let answer = match self.ask_user_handler() {
            Some(handler) => handler.ask(data),
            None => None,
        };
        let Some(answer) = answer.filter(|text| !text.trim().is_empty()) else {
            // 未作答：让提问按超时收口，不编造答案。
            return;
        };
        if let Err(error) = self.service.decide_question(run_id, question_id, &answer) {
            eprintln!("[connector] 回填提问答案失败：{}", error.message);
        }
    }
}

impl AgentDriver for HostConnectorDriver {
    fn status(&self) -> AgentStatus {
        AgentStatus {
            workspace_root: self.service.workspace_root().display().to_string(),
            session_id: self.service.current_session_id(),
        }
    }

    fn reset_conversation(&self) -> Result<(), String> {
        self.service
            .switch_session("", None)
            .map(|_| ())
            .map_err(|error| error.message.clone())
    }

    fn switch_workspace(&self, path: &str) -> Result<WorkspaceSwitch, String> {
        let candidate = PathBuf::from(path);
        let root = candidate
            .canonicalize()
            .map_err(|error| format!("工作区不存在或不是目录：{path}（{error}）"))?;
        let session_id = self.service.current_session_id();
        self.service
            .switch_session(&session_id, Some(&root))
            .map_err(|error| error.message.clone())?;
        // 跨端同步：把新工作区写回 config.toml，本地 TUI 下次读取即跟随；
        // 写失败不阻断切换（与命令层 `persist_workspace_root` 同口径）。
        let note = match save_workspace_root(&ConfigEnvironment::from_process(), &root, None) {
            Ok(_) => "已切换工作区，会话历史已清空。".to_string(),
            Err(error) => format!("已切换工作区；写入 config.toml 失败：{}", error.message()),
        };
        Ok(WorkspaceSwitch {
            workspace_root: root.display().to_string(),
            note,
        })
    }

    fn handle_command(&self, text: &str, channel: &str) -> Result<Option<String>, String> {
        let agent = ServiceCommandAgent {
            service: Arc::clone(&self.service),
        };
        let result = omnicrawl_commands::slash::registry().dispatch(
            text,
            &agent,
            Channel::parse(channel),
            None,
        );
        if !result.handled {
            return Ok(None);
        }
        // 延迟执行体（网络/进程/文件 I/O 或模型循环）在这里同步跑完：连接器没有工作线程池，
        // 而命令处理器本身已经跑在任务线程上。
        let result = result.resolve(&agent);
        if let Some(error) = result.error {
            return Err(error);
        }
        Ok(result.message)
    }

    fn active_mode(&self) -> String {
        self.service.active_mode()
    }

    fn activate_mode(&self, mode: &str) -> Result<String, String> {
        self.service
            .activate_mode(mode)
            .map_err(|error| error.message)
    }

    fn deactivate_mode(&self) -> Result<String, String> {
        self.service
            .deactivate_mode()
            .map_err(|error| error.message)
    }

    /// 发布到 Telegram 菜单的命令表来自全局命令注册表。
    ///
    /// 只取注册表里“不是纯界面动作”的命令：`/quit`、`/settings` 这类在远程被明确拒绝，
    /// 列进菜单只会误导。Skill 是运行期发现的，不在静态注册表里，因此不涉及。
    fn command_menu(&self) -> Vec<(String, String)> {
        omnicrawl_commands::slash::registry()
            .commands(false)
            .into_iter()
            .filter(|command| command.command_type != CommandType::Ui)
            .map(|command| (command.name.clone(), command.description.clone()))
            .collect()
    }

    fn run_turn(&self, text: &str, events: &mut dyn FnMut(TurnEvent)) -> Result<String, TurnError> {
        let outcome = self.drive_turn(text, events)?;
        events(TurnEvent::Finished(TurnOutcome {
            final_text: outcome.clone(),
            reasoning: String::new(),
            model_turns: 0,
            tool_calls: 0,
            paused: false,
        }));
        Ok(outcome)
    }

    fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let run_id = self.current_run();
        if !run_id.is_empty() {
            let _ = self.service.cancel_run(&run_id);
        }
    }

    fn temp_root(&self) -> PathBuf {
        self.service
            .workspace_root()
            .join(".omnicrawl")
            .join(".agent_tmp")
    }

    fn shutdown(&self) -> Result<(), String> {
        self.service.close();
        Ok(())
    }

    fn set_confirm_handler(&self, handler: Arc<dyn ConfirmHandler>) {
        if let Ok(mut slot) = self.confirm.lock() {
            *slot = Some(handler);
        }
    }

    fn set_ask_user_handler(&self, handler: Arc<dyn AskUserHandler>) {
        if let Ok(mut slot) = self.ask_user.lock() {
            *slot = Some(handler);
        }
    }
}

/// 事件载荷里的工具调用（`tool.started` / `tool.completed` 的公开投影）。
fn tool_call_from(data: &Value) -> ToolCall {
    ToolCall {
        name: data
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        arguments: data.get("arguments").cloned().unwrap_or(Value::Null),
        id: data
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// 终态事件 → 回合结果；非终态返回 `None`。
///
/// 三条终态与 `RunStatus` 的终态一一对应：完成给最终文本，取消与失败各有自己的错误形态。
fn terminal_outcome(name: &str, data: &Value) -> Option<Result<String, TurnError>> {
    match name {
        "run.completed" => Some(Ok(data
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())),
        "run.cancelled" => Some(Err(TurnError::Cancelled)),
        "run.failed" => Some(Err(TurnError::Failed(
            data.get("message")
                .and_then(Value::as_str)
                .unwrap_or("生成任务失败。")
                .to_string(),
        ))),
        _ => None,
    }
}

/// 命令层看到的宿主能力面：全部转发到本地 API 的同一个 `AgentService`。
///
/// 实现口径与 TUI 的 `TuiHostAgent` 一致：宿主真有的能力如实实现，内核侧有、但连接器
/// 这条同步路径拿不到的（`/review` 的子代理派发、需要保持事件循环可响应的）显式报错，
/// 不让这些输入悄悄变成一次模型对话。
struct ServiceCommandAgent {
    service: Arc<AgentService>,
}

/// 宿主缺这项能力时的统一说法：点明缺什么，不让调用方以为命令成功了。
fn blocked<T>(what: &str, why: &str) -> Result<T, AgentError> {
    Err(AgentError::new(format!("{what}暂不可用：{why}")))
}

impl CommandAgent for ServiceCommandAgent {
    fn config_environment(&self) -> ConfigEnvironment {
        ConfigEnvironment::from_process()
    }

    fn current_session_id(&self) -> String {
        self.service.current_session_id()
    }

    fn workspace_root(&self) -> PathBuf {
        self.service.workspace_root()
    }

    fn reset_conversation(&self) -> Result<(), AgentError> {
        self.service
            .switch_session("", None)
            .map(|_| ())
            .map_err(|error| AgentError::new(error.message))
    }

    fn rename_current_session(&self, title: &str) -> Result<SessionSummary, AgentError> {
        let entry = self
            .service
            .rename_current_session(title)
            .map_err(|error| AgentError::new(error.message))?;
        Ok(SessionSummary {
            session_id: entry.session_id,
            title: entry.title,
            message_count: entry.message_count as usize,
        })
    }

    fn resume_session(&self, session_id: &str) -> Result<SessionSummary, AgentError> {
        self.service
            .session_state_payload(session_id)
            .map_err(|error| AgentError::new(error.message))?;
        self.service
            .switch_session(session_id, None)
            .map_err(|error| AgentError::new(error.message))?;
        Ok(SessionSummary {
            session_id: session_id.to_string(),
            title: String::new(),
            message_count: 0,
        })
    }

    fn archive_current_session(&self) -> Result<SessionSummary, AgentError> {
        let archived = self
            .service
            .archive_current_session()
            .map_err(|error| AgentError::new(error.message))?;
        // 归档后立刻开一条新会话：内核侧同一套语义，连接器这边跟上。
        self.service
            .switch_session("", None)
            .map_err(|error| AgentError::new(error.message))?;
        Ok(SessionSummary {
            session_id: archived.session_id,
            title: archived.title,
            message_count: archived.message_count as usize,
        })
    }

    fn undo_last_turn(&self) -> Result<(), AgentError> {
        self.service
            .undo_current_session()
            .map(|_| ())
            .map_err(|error| AgentError::new(error.message))
    }

    fn compact_conversation_model(&self) -> Result<(), AgentError> {
        self.service
            .compact_current_session()
            .map(|_| ())
            .map_err(|error| AgentError::new(error.message))
    }

    fn compaction_notice(&self) -> String {
        String::new()
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError> {
        self.service
            .list_sessions(limit, false)
            .map_err(|error| AgentError::new(error.message))
    }

    fn list_archived_sessions(&self, limit: usize) -> Result<Vec<SessionIndexEntry>, AgentError> {
        self.service
            .list_sessions(limit, true)
            .map_err(|error| AgentError::new(error.message))
    }

    fn search_prompt_history(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<PromptHistoryEntry>, AgentError> {
        self.service
            .search_prompt_history(query, limit as i64, true)
            .map_err(|error| AgentError::new(error.message))
    }

    fn activate_mode(&self, mode: &str) -> Result<String, AgentError> {
        self.service
            .activate_mode(mode)
            .map_err(|error| AgentError::new(error.message))
    }

    fn active_mode(&self) -> String {
        self.service.active_mode()
    }

    fn deactivate_mode(&self) -> Result<String, AgentError> {
        self.service
            .deactivate_mode()
            .map_err(|error| AgentError::new(error.message))
    }

    fn approval_mode(&self) -> String {
        self.service.approval_mode()
    }

    fn set_approval_mode(&self, mode: &str) -> Result<(), AgentError> {
        let parsed = omnicrawl_host::approval::ApprovalMode::parse(mode)
            .map_err(|error| AgentError::new(error))?;
        self.service
            .set_runtime_approval_mode(parsed)
            .map_err(|error| AgentError::new(error.message))
    }

    fn reasoning_effort(&self) -> Option<String> {
        Some(self.service.reasoning_effort())
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<String, AgentError> {
        let normalized = omnicrawl_config::models::llm::normalize_reasoning_effort(effort)
            .map_err(|error| AgentError::new(error.message()))?;
        self.service
            .apply_reasoning_effort(normalized)
            .map_err(|error| AgentError::new(error.message))?;
        Ok(normalized.to_string())
    }

    fn current_model(&self) -> String {
        self.service.current_model()
    }

    fn llm_config(&self) -> LlmConfig {
        load_llm_config(&self.service.environment())
            .unwrap_or_else(|_| LlmConfig::with_environment(&ConfigEnvironment::from_process()))
    }

    fn set_model(
        &self,
        selection: &str,
        persist: &mut dyn FnMut() -> Result<(), AgentError>,
    ) -> Result<(), AgentError> {
        let environment = ConfigEnvironment::from_process();
        let current = self.llm_config();
        // 解析失败即报错：绝不把无效 key 静默降级成裸 model_id（与 Python 同口径）。
        let resolved = omnicrawl_config::models::llm_multi::apply_model_selection(
            &environment,
            &current,
            selection,
        )
        .map_err(|error| AgentError::new(error.message()))?;
        persist()?;
        self.service
            .apply_model_switch(&resolved)
            .map_err(|error| AgentError::new(error.message))
    }

    fn apply_advisor_config(&self, config: &AdvisorConfig) -> Result<(), AgentError> {
        self.service
            .apply_advisor_config(config)
            .map_err(|error| AgentError::new(error.message))
    }

    fn switch_workspace(&self, path: &str) -> Result<PathBuf, AgentError> {
        let root = PathBuf::from(path).canonicalize().map_err(|error| {
            AgentError::new(format!("工作区不存在或不是目录：{path}（{error}）"))
        })?;
        let session_id = self.service.current_session_id();
        self.service
            .switch_session(&session_id, Some(&root))
            .map_err(|error| AgentError::new(error.message))?;
        Ok(root)
    }

    fn skill_metas(&self) -> Option<Vec<SkillMeta>> {
        // 命令层要的是元数据：`AgentService::skills` 给的就是装配期那份投影。
        let values = self.service.skills();
        Some(
            values
                .iter()
                .map(|value| SkillMeta {
                    name: map_text(value, "name"),
                    description: map_text(value, "description"),
                    source_path: PathBuf::from(map_text(value, "source_path")),
                    base_dir: PathBuf::from(map_text(value, "base_dir")),
                    scope: map_text(value, "scope"),
                    disable_model_invocation: value
                        .get("disable_model_invocation")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
                .collect(),
        )
    }

    fn skill_diagnostics(&self) -> Option<Vec<SkillDiagnostic>> {
        None
    }

    fn clean_memory(&self) -> Result<Vec<String>, AgentError> {
        self.service
            .clean_memory()
            .map_err(|error| AgentError::new(error.message))
    }

    fn mcp_status(&self) -> String {
        self.service.mcp_status()
    }

    fn plugins_status(&self) -> Option<String> {
        let status = self.service.plugins_status();
        let rows: Vec<omnicrawl_controllers::control::PluginWorkerRow> = status
            .get("plugins")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| omnicrawl_controllers::control::PluginWorkerRow {
                        name: text_field(item, "name"),
                        version: text_field(item, "version"),
                        scope: text_field(item, "scope"),
                        active: item.get("active").and_then(Value::as_bool).unwrap_or(false),
                        circuit_open: item
                            .get("circuitOpen")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        dev_mode: item
                            .get("devMode")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        handlers: item
                            .get("handlers")
                            .and_then(Value::as_array)
                            .map(|names| {
                                names
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        last_error: text_field(item, "lastError"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let enabled = status
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let configured = status
            .get("configured")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(if configured {
            omnicrawl_controllers::control::plugins_status_text(Some(&(enabled, rows)))
        } else {
            omnicrawl_controllers::control::plugins_status_text(None)
        })
    }

    fn list_subagent_tasks(&self) -> Result<Vec<Value>, AgentError> {
        self.service
            .list_subagent_tasks()
            .map_err(|error| AgentError::new(error.message))
    }

    fn get_subagent_task(&self, task_id: &str) -> Result<Option<Value>, AgentError> {
        self.service
            .get_subagent_task(task_id)
            .map_err(|error| AgentError::new(error.message))
    }

    fn cancel_subagent_task(&self, task_id: &str) -> Result<Value, AgentError> {
        let result = self
            .service
            .cancel_subagent_task(task_id)
            .map_err(|error| AgentError::new(error.message))?;
        Ok(result.unwrap_or_else(|| json!({"ok": false})))
    }

    fn run_subagent_task(&self, _request: SubAgentRun) -> Result<String, AgentError> {
        blocked(
            "/review",
            "它要派生子 Agent 并让工具批次回到宿主执行，连接器这条同步路径会在等待中\
             阻塞事件循环。",
        )
    }

    fn remember_review_report(&self, report: &str) -> Result<(), AgentError> {
        self.service
            .append_assistant_text(report)
            .map(|_| ())
            .map_err(|error| AgentError::new(error.message))
    }

    fn run_git(&self, workspace_root: &std::path::Path, args: &[&str]) -> Option<GitOutput> {
        run_git(workspace_root, args)
    }
}

/// 在工作区根下跑一次只读 git：与 TUI 侧同一口径（`git -c color.ui=never --no-pager`、
/// 捕获 stdout、30 秒超时），git 缺失或超时返回 `None`。
fn run_git(workspace_root: &std::path::Path, args: &[&str]) -> Option<GitOutput> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-c")
        .arg("color.ui=never")
        .arg("--no-pager")
        .args(args)
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 不弹控制台窗口：连接器本身没有终端。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    Some(GitOutput {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
    })
}

fn text_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `Map` 形状的字段读取（`skills()` 给的是 JSON 对象映射）。
fn map_text(value: &Map<String, Value>, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_events_map_to_turn_outcomes() {
        // 完成：最终文本取自 `run.completed.result`。
        match terminal_outcome("run.completed", &json!({"result": "好了"})) {
            Some(Ok(text)) => assert_eq!(text, "好了"),
            other => panic!("run.completed 应是成功终态：{other:?}"),
        }
        // 取消：连接器侧按 Cancelled 收口，不当作失败。
        assert!(matches!(
            terminal_outcome("run.cancelled", &json!({})),
            Some(Err(TurnError::Cancelled))
        ));
        // 失败：缺 message 时给通用文案，不把空串露给用户。
        match terminal_outcome("run.failed", &json!({})) {
            Some(Err(TurnError::Failed(message))) => assert_eq!(message, "生成任务失败。"),
            other => panic!("run.failed 应是失败终态：{other:?}"),
        }
        // 非终态事件不结束回合，否则一次增量就会把回合提前收掉。
        assert!(terminal_outcome("assistant.delta", &json!({"delta": "你"})).is_none());
    }

    #[test]
    fn tool_call_projection_keeps_name_and_id() {
        let call = tool_call_from(&json!({
            "tool": "read",
            "tool_call_id": "call-1",
            "arguments": {"path": "README.md"}
        }));
        assert_eq!(call.name, "read");
        assert_eq!(call.id.as_deref(), Some("call-1"));
        assert_eq!(
            call.arguments.get("path").and_then(Value::as_str),
            Some("README.md")
        );
        // 缺字段时给安全默认值，不让投影 panic。
        let missing = tool_call_from(&json!({}));
        assert_eq!(missing.name, "?");
        assert!(missing.id.is_none());
    }

    /// 模型流重试的回滚事件必须落到 `StreamRollback`：降级成状态提示时显示层收不到，
    /// 作废的半截输出会和重试后的新正文累积在同一条消息里。
    #[test]
    fn stream_rollback_event_name_is_its_own_event() {
        assert_eq!(ROLLBACK_EVENT, "assistant.rollback");
        assert_ne!(ROLLBACK_EVENT, "status.changed");
    }
}
