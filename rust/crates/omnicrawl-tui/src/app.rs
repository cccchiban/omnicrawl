//! 接线层：内核帧、用户按键、工具执行层与状态机之间的翻译。
//!
//! 工具执行放在独立线程上（一个调用一个线程，同批并发），主线程只跑事件循环：
//! 执行结果经通道回到主线程，再回填批次并按模型顺序回内核。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::subagents::load_subagent_config;
use omnicrawl_controllers::subagents::definitions::AgentDefinitionRegistry;
use omnicrawl_core::{ToolCall, ToolResult};
use omnicrawl_ipc::{
    bridge::{
        Command, HostEvent, InitializeParams, KernelModelConfig, KernelSessionConfig, ToolBatch,
    },
    error_code, Frame, Id, PROTOCOL_VERSION,
};

use crate::args::Options;
use crate::host::{self, BatchStep, Waiting};
use crate::kernel::KernelClient;
use crate::state::{AppState, Record};
use crate::tools::{AdvisorOptions, ImageGenOptions, RegistryOptions, ToolRegistry};

/// 握手响应最多等这么久；内核启动即刻回帧，卡住说明进程有问题。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 方向键滚动步长与翻页步长（行）。
const SCROLL_STEP: isize = 1;
const PAGE_STEP: isize = 10;

/// 一次工具执行的完成通知。
struct ToolCompletion {
    index: usize,
    call: ToolCall,
    result: ToolResult,
    vision: Option<host::VisionPayload>,
}

pub struct App {
    pub state: AppState,
    pub options: Options,
    pub kernel: KernelClient,
    pub quit: bool,
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
    next_turn: u64,
    /// 顾问可见的工作分支（工具批次开始前由对话记录刷新）。
    advisor_context: Arc<Mutex<Vec<serde_json::Value>>>,
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
        let registry_options = RegistryOptions {
            session_held_by_kernel: options.session_root.is_some(),
            native_vision: options.native_vision,
            subagent_types: subagent_role_names(),
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
        let (completion_sender, completions) = mpsc::channel();
        Ok(Self {
            state,
            options,
            kernel,
            quit: false,
            registry,
            registry_options,
            batch_registry: None,
            completions,
            completion_sender,
            tool_deadline: None,
            next_turn: 1,
            advisor_context,
        })
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
    pub fn handshake(&mut self) -> Result<(), String> {
        let model = KernelModelConfig {
            model: self.options.model.clone(),
            provider: String::new(),
            protocol: String::new(),
            base_url: self.options.base_url.clone(),
            api_key_env: self.options.api_key_env.clone(),
            user_agent: format!("omnicrawl-tui/{}", env!("CARGO_PKG_VERSION")),
            system_prompt: self.options.system_prompt.clone(),
            tools: self.registry.declarations(),
            options: json!({}),
            request_timeout_seconds: None,
            context_window_tokens: 0,
            prompt_cache_capable: false,
            prompt_cache_identity: Default::default(),
            request_retry_count: 1,
        };
        let session = self.options.session_root.as_ref().map(|root| {
            Box::new(KernelSessionConfig {
                root: root.to_string_lossy().to_string(),
                session_id: String::new(),
                memory_root: None,
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
                let declared = self.registry.declarations().len();
                eprintln!("[tui] 已向内核声明 {declared} 个工具。");
                return Ok(());
            }
            self.handle_frame(frame);
        }
        Err("等待内核握手响应超时。".to_string())
    }

    /// 收干内核帧与工具执行结果；内核退出时收尾退出。
    pub fn drain_frames(&mut self) {
        while let Some(frame) = self.kernel.try_recv() {
            self.handle_frame(frame);
        }
        self.drain_completions();
        self.enforce_tool_deadline();
        if self.kernel.is_closed() && !self.quit {
            self.state.fail_turn("内核进程已退出。".to_string());
            self.quit = true;
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
        let native_vision = self.options.native_vision;
        if let Some(observations) = self.state.take_observations(native_vision) {
            self.respond_batch(&request_id, observations);
        }
    }

    fn handle_frame(&mut self, frame: Frame) {
        if frame.is_notification() {
            match HostEvent::from_frame(&frame) {
                Ok(event) => self.state.apply(&event, Instant::now()),
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
            Some(method) => {
                let _ = self.kernel.respond_unsupported(&id, method);
            }
            // 响应帧（例如 `turn.submit` 的确认）不需要界面处理：回合并行由通知驱动。
            None => {}
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
        match step {
            BatchStep::Awaiting => {}
            BatchStep::Execute(jobs) => {
                self.tool_deadline = Some(
                    Instant::now()
                        + Duration::from_secs(self.options.tool_timeout_seconds.max(1) as u64),
                );
                // 本批的工具属于当前回合：回合取消时只回收它自己的后台任务。
                self.refresh_advisor_context();
                self.batch().set_monitor_scope(self.state.turn.turn_id());
                for (index, call) in jobs {
                    self.state.begin_tool_run(&call, Instant::now());
                    self.spawn_tool_job(index, call);
                }
            }
            BatchStep::Complete => {
                self.tool_deadline = None;
                self.batch_registry = None;
                let native_vision = self.options.native_vision;
                if let Some(observations) = self.state.take_observations(native_vision) {
                    self.respond_batch(&request_id, observations);
                }
            }
        }
    }

    /// 一个调用一个线程：同批工具并发执行（与 Python 宿主一致）。
    ///
    /// 执行体的 panic 必须转成一条失败观察：线程静默消失会让整批永远凑不齐，
    /// 内核就会一直等这个 `tool.batch` 的响应。
    fn spawn_tool_job(&self, index: usize, call: ToolCall) {
        let registry = Arc::clone(self.batch());
        let sender = self.completion_sender.clone();
        thread::spawn(move || {
            let tool_name = call.name.clone();
            let executed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.execute_with_vision(&call)
            }));
            let (result, vision) = match executed {
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
                Err(_) => (host::panicked_result(&tool_name), None),
            };
            let _ = sender.send(ToolCompletion {
                index,
                call,
                result,
                vision,
            });
        });
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
        eprintln!(
            "[tui] 工具执行超过 {seconds} 秒仍未完成，已按超时回收等待（后台线程的结果会被丢弃）。"
        );
        self.tool_deadline = None;
        if ready {
            self.flush_ready_batch();
        }
    }

    fn respond_batch(&mut self, id: &Id, observations: Vec<omnicrawl_core::AgentLoopObservation>) {
        let result = omnicrawl_ipc::ToolBatchResult { observations }.to_result();
        if let Err(error) = self.kernel.respond(id, result) {
            eprintln!("[tui] 回工具批次失败：{error}");
        }
    }

    /// 处理一个终端事件。
    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => self.handle_key(key),
            Event::Paste(text) => self.state.composer.insert(&text),
            Event::Resize(..) => {}
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.state.composer.clear();
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

        match key.code {
            KeyCode::Enter => {
                if !self.state.turn.is_running() {
                    self.submit_composer();
                }
            }
            KeyCode::Char('j') if ctrl => self.state.composer.newline(),
            KeyCode::Char('l') if ctrl => self.state.records.clear(),
            KeyCode::Esc => {
                if self.state.turn.is_running() {
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
                        if let Some(step) = self.state.answer_question(answer) {
                            self.handle_batch_step(batch_id, step);
                        }
                        true
                    }
                    KeyCode::Enter => {
                        if !self.state.composer.is_empty() {
                            let answer = self.state.composer.take();
                            if let Some(step) = self.state.answer_question(answer) {
                                self.handle_batch_step(batch_id, step);
                            }
                        }
                        true
                    }
                    KeyCode::Esc => {
                        if let Some(step) = self.state.answer_question(String::new()) {
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

    fn submit_composer(&mut self) {
        let Some(text) = self.state.submit() else {
            return;
        };
        let turn_id = format!("turn-{}", self.next_turn);
        self.next_turn += 1;
        self.state.begin_turn(turn_id.clone(), text.clone());
        self.send(Command::TurnSubmit(omnicrawl_ipc::TurnSubmitParams {
            turn_id,
            user_text: text,
        }));
    }

    fn cancel_turn(&mut self) {
        let Some(turn_id) = self.state.turn.turn_id().map(|value| value.to_string()) else {
            return;
        };
        self.send(Command::TurnCancel(omnicrawl_ipc::TurnCancelParams {
            turn_id: turn_id.clone(),
        }));
        // 先回收正在跑的进程树与这个回合的后台任务，再卸下批次：迟到的结果会被忽略。
        self.registry.cancel_token().cancel();
        self.registry
            .stop_monitors_in_scope(&turn_id, crate::tools::monitor::CANCEL_REASON);
        self.tool_deadline = None;
        self.state.cancel_batch();
        self.state.fail_turn("已取消当前回合。".to_string());
    }

    /// 退出收尾：回收后台进程后请内核退出。
    pub fn shutdown(&mut self) {
        self.registry.close_monitors();
        self.request_shutdown();
    }

    fn start_shutdown(&mut self) {
        self.request_shutdown();
        self.quit = true;
    }

    /// 请内核退出；连接已断时什么都不做。
    pub fn request_shutdown(&mut self) {
        if self.kernel.is_closed() {
            return;
        }
        self.send(Command::Shutdown);
    }

    fn send(&mut self, command: Command) {
        let id = self.kernel.next_id();
        let frame = command.to_frame(id);
        if let Err(error) = self.kernel.send_frame(&frame) {
            eprintln!("[tui] 发送内核命令失败：{error}");
        }
    }
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
