//! `omnicrawl-tui` 二进制：终端生命周期与事件循环。
//!
//! 协议宿主、状态机与渲染都在库侧；这里只做选内核、进全屏、收事件这三件事。
//! 进全屏之前的启动准备由 [`omnicrawl_tui::ui::splash`] 的启动画面承载。

use std::io::stdout;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use omnicrawl_config::core::context::detect_project_context;
use omnicrawl_config::core::runtime::{user_config_dir, ConfigEnvironment};
use omnicrawl_config::features::agent_workspace::load_agent_workspace_config;
use omnicrawl_config::models::llm::{load_llm_config, LlmConfig};
use omnicrawl_tui::app::{is_ctrl_v_release, uses_external_channel, App};
use omnicrawl_tui::args::{parse, Options, Parsed};
use omnicrawl_tui::clipboard;
use omnicrawl_tui::kernel::{kernel_credentials_env, KernelClient};
use omnicrawl_tui::paste;
use omnicrawl_tui::ui;
use omnicrawl_tui::ui::fullscreen::terminal::console_heal::heal_console_input_mode;
use omnicrawl_tui::ui::fullscreen::terminal::timer_resolution::TimerResolutionGuard;
use omnicrawl_tui::ui::splash::{self, LogLevel, StartupLogSink};
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, prepare_isolated_workspace, start_background_isolation_sweep,
    IsolationOptions,
};

/// 退出时等内核自己收尾的时间。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// 启动准备完成后进入工作台前的停留秒数：给日志框留一段可读窗口（加载完停 1 秒）。
const SPLASH_HOLD_AFTER_DONE_SECONDS: f64 = 1.0;
/// 控制台输入模式自愈的核对间隔：锁屏/息屏恢复与工具子进程改写都在秒级被发现即可，
/// 又不至于每帧都去问一次控制台（对映 Python 侧的周期看门狗）。
const CONSOLE_HEAL_INTERVAL: Duration = Duration::from_secs(1);
/// 自愈提示的最小间隔：恢复本身照常做，只是不再每次都往屏幕上写一行。
const NOTICE_MIN_INTERVAL: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    let mut options = match parse(&args, &exe_dir)? {
        Parsed::Help(text) | Parsed::Version(text) => {
            println!("{text}");
            return Ok(ExitCode::SUCCESS);
        }
        Parsed::Run(options) => *options,
    };

    let environment = ConfigEnvironment::from_process();
    let context = detect_project_context(&environment, None);
    let workspace = context.workspace_root;
    // 默认会话根与 Python 宿主、本地 API 同址（`~/.OmniCrawl/.agent_sessions`，不绑工作区）：
    // `--session-root` 没给时也要有会话，否则 `/sessions`、
    // `/resume`、`/new`、`/compact` 一律只能报「内核未持有会话」。
    if options.session_root.is_none() {
        options.session_root = Some(user_config_dir(&environment).join(".agent_sessions"));
    }
    // 启动画面在普通屏幕上显示（左侧 Logo + 右侧圆角日志框 + 底部 XP 滚动条），
    // 准备（内核进程、配置与工具表、MCP 能力、握手）在后台线程并行完成；准备结束
    // 后再进备用屏幕交给工作台，因此握手失败的错误留在普通终端上才看得见。
    let mut app = splash::run_startup_splash(
        |sink| prepare_startup(options, &workspace, sink),
        Duration::from_secs_f64(splash::DEFAULT_DURATION),
        Duration::from_secs_f64(SPLASH_HOLD_AFTER_DONE_SECONDS),
    )?;

    let mut guard = TerminalGuard::start()?;
    // 事件循环按时间窗等待终端事件：Windows 默认计时器粒度约 15.6ms，睡 20ms 可能变成 31ms，
    // 这段白等待直接叠加在「回车 → 首字」上。提升到 1ms，随进程退出恢复。
    let timer_resolution = TimerResolutionGuard::acquire();
    let backend = CrosstermBackend::new(stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| format!("初始化终端失败：{error}"))?;
    // 首屏挂载即开播欢迎 Logo 入场动画（对映 Python `on_mount` 里的启动时机）。
    app.start_welcome_logo_animation(Instant::now());

    let result = event_loop(&mut terminal, &mut app, &mut guard);
    app.shutdown();
    app.kernel.wait_or_kill(SHUTDOWN_TIMEOUT);
    // 内核已退出（或已被强杀）：补发 `session.close.after`，与 Python
    // `close()` 里 before → 写事件 → after 的顺序对齐。
    app.finish_session_close();
    drop(timer_resolution);
    drop(guard);
    result?;
    Ok(ExitCode::SUCCESS)
}

/// 启动画面期间在后台线程完成的准备：拉起内核进程、装配工具表与 MCP 能力、握手。
///
/// 各阶段的进度写进启动画面日志框；警告与错误由 `report_startup_log` 桥接进来。
fn prepare_startup(
    options: Options,
    workspace: &Path,
    sink: &StartupLogSink,
) -> Result<App, String> {
    sink.write_line("启动内核进程", LogLevel::Info);
    let environment = ConfigEnvironment::from_process();
    // 凭据注入：协议帧里只带**变量名**（`KernelModelConfig.api_key_env`），内核只从环境读密钥；
    // 而 `config.toml` 里写的字面 `api_key` 不在环境里，必须由宿主在起进程时补进去，
    // 否则每次回合都会以「读取环境变量 … 失败；模型请求无法鉴权」告终。
    // 走命令行渠道（`--base-url`）时凭据本来就来自用户自己的环境变量，子进程直接继承，
    // 这里不注入，避免把配置里的 key 送给另一个端点。
    let kernel_env = if uses_external_channel(&options) {
        Vec::new()
    } else {
        // 与 `App::new` 同样的降级口径：读不到配置就按环境变量默认值继续（App::new 会再报一次）。
        let llm = load_llm_config(&environment)
            .unwrap_or_else(|_| LlmConfig::with_environment(&environment));
        kernel_credentials_env(&environment, &llm.provider, &llm.api_key_env)
    };
    // 内核 stderr 不再直接继承到终端（运行期报错会写在光标处、盖住底部输入框）：
    // 逐行收进通道，由宿主把报错作为会话区提示显示（见 `App::drain_kernel_logs`）。
    let (kernel_log_tx, kernel_logs) = std::sync::mpsc::channel::<String>();
    let kernel = KernelClient::spawn_with_stderr_env(&options.kernel, kernel_env, move |line| {
        let _ = kernel_log_tx.send(line.to_string());
    })
    .map_err(|error| format!("启动内核失败：{error}"))?;
    // 主 Agent 隔离工作区（对映 Python `entry.py` 的启动准备）：多个进程并行时各自在独立的
    // worktree / 目录副本里读写，互不写穿；创建失败仅告警并回退主工作区，不阻断启动。
    sink.write_line("准备隔离工作区", LogLevel::Info);
    let workspace_config = load_agent_workspace_config(&environment, None).unwrap_or_default();
    let (agent_workspace, isolation) =
        prepare_isolated_workspace(IsolationOptions::new(workspace).with_config(workspace_config));
    // 启动清扫（后台）：回收上次崩溃 / 被强杀遗留的过期隔离区；历史会话多时会对
    // 每个过期会话跑多次 git 子进程，不应占用启动画面时间。
    start_background_isolation_sweep(None);
    sink.write_line("加载配置、工具表与 MCP 能力", LogLevel::Info);
    let mut app = match App::new(options, kernel, &agent_workspace) {
        Ok(app) => app,
        Err(error) => {
            // App 尚未接管隔离区：这里兜底收尾，避免改动滞留在隔离区。
            if let Some(session) = isolation.as_ref() {
                let _ = finalize_isolation_session(session, true, "auto", None, None);
            }
            return Err(error);
        }
    };
    if let Some(session) = isolation {
        app.attach_isolation_session(session);
    }
    // 插件加载诊断写在启动页日志框里（不进对话流，否则首屏欢迎 Logo 会被顶掉）。
    for line in &app.startup_plugin_lines {
        sink.write_line(&format!("插件：{line}"), LogLevel::Info);
    }
    app.attach_kernel_logs(kernel_logs);
    sink.write_line("等待内核握手", LogLevel::Info);
    app.handshake()?;
    Ok(app)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    guard: &mut TerminalGuard,
) -> Result<(), String> {
    // 终端尺寸只在「首次」与「收到 `Event::Resize`」时读：Windows 上 `terminal.size()`
    // 是一次控制台往返，空闲时每帧问一次纯属浪费，而尺寸变化一定会以 resize 事件到达。
    let mut viewport: Option<Rect> = None;
    // 粘贴识别状态：跨控制台输入批次的多行粘贴要挂起等后续批次。
    let mut paste_tracker = paste::PasteTracker::new();
    loop {
        if viewport.is_none() {
            viewport = Some(terminal_viewport(terminal)?);
            app.set_viewport(viewport.expect("刚写入视口"));
        }
        let now = Instant::now();
        app.tick_welcome_logo_animation(now);
        app.tick_subagent_trees();
        app.tick_tts_tasks();
        // 自部署（OneJev）后台任务：环境安装 / 权重下载 / 本地服务启停的结果。
        app.tick_onejev_tasks();
        // 底部单行轮播：遥测 → 工作区路径 → 留言，各 10s，切换时解密扫描。
        app.tick_carousel(now);
        // 输入框上方那行瞬时提示（拖选复制等）到时自散。
        app.state.tick_notice_line(now);
        // 慢命令（`/workspace`、`/mcp`）的后台结果：工作区切换在这里提交。
        app.tick_slow_command();
        app.tick_config_chat();
        app.tick_monitor_events(now);
        app.drain_frames();
        // 思考段逐帧铺开：一次突发的多片增量不会在同一帧里整段蹦出。
        app.state.tick_reasoning_reveal();
        // 工具卡图片预览：后台解码完的缩略图在这里接进状态（新图会让会话区重算那一张卡）。
        app.state.tick_image_previews();
        // 三块刷新（用户要求）：会话 / 输入框 / 底部。三块自上次绘制以来都没变时
        // 整帧跳过绘制——空闲的 TUI 不再以 20fps 空转，也不重算任何一块的内容。
        // 尺寸变化（resize 事件）与模态弹层都由 `needs_redraw` 一并覆盖。
        if app.needs_redraw() {
            terminal
                .draw(|frame| {
                    ui::render(
                        frame,
                        &app.state,
                        app.settings.as_ref(),
                        app.file_picker.as_ref(),
                        app.config_chat.as_ref(),
                    )
                })
                .map_err(|error| format!("渲染失败：{error}"))?;
        }
        if app.quit {
            return Ok(());
        }
        let now = Instant::now();
        if guard.heal_if_needed(now) && guard.notice_due(now) {
            // 不再直接写终端（会盖住底部输入框）：改成会话流里的提示行。
            app.state
                .notice("控制台输入模式被重置，已恢复鼠标与键盘协议。".to_string());
        }
        // 等待时长按「下一件要做的事」定：空闲（无回合、无动画、无在途结果）时久睡，
        // 有活动时按活动帧率短睡，避免固定 50ms 节拍白等。粘贴挂起期间还要缩短上限，
        // 好让「其实不是粘贴」的按键流能及时按普通输入回退。
        let budget = app.poll_budget(Instant::now());
        let wait = paste_wait(&paste_tracker, budget);
        if event::poll(wait).map_err(|error| format!("读取终端事件失败：{error}"))?
        {
            let event = event::read().map_err(|error| format!("读取终端事件失败：{error}"))?;
            if matches!(event, Event::Resize(..)) {
                let resized = terminal_viewport(terminal)?;
                viewport = Some(resized);
                app.set_viewport(resized);
            }
            handle_event(app, &mut paste_tracker, event);
        } else if paste_tracker.poll_timeout(Instant::now()) {
            // 没有新输入且挂起已超时：把这些按键当普通输入冲刷，不无限期暂存用户输入。
            flush_pending_paste(app, &mut paste_tracker);
        }
    }
}

/// 读一次终端尺寸并折成渲染区域。
fn terminal_viewport(
    terminal: &Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<Rect, String> {
    let size = terminal
        .size()
        .map_err(|error| format!("读取终端尺寸失败：{error}"))?;
    Ok(Rect::new(0, 0, size.width, size.height))
}

/// 处理一个终端事件，并顺带识别「一串按键形式的粘贴」。
///
/// 终端支持 bracketed paste 时事件本身就是 `Event::Paste`；但 conhost 等终端不发那个序列，
/// 粘贴会退化成一串普通按键（换行成了 `Enter`）——于是第一行被提交、后面的行逐条排队。
/// 这里把**当前这一刻能读到的按键全收进来**（`poll(0)` 循环，不依赖按键间隔）交给
/// [`paste::PasteTracker`]：与系统剪贴板完整匹配才当一次粘贴；只是剪贴板的严格前缀时先挂起，
/// 等下一批到齐（大文本粘贴会被控制台按批次切开）；超时或对不上则逐条走正常按键路径。
/// 非按键事件当场处理，不会排在按键后面。
fn handle_event(app: &mut App, tracker: &mut paste::PasteTracker, first: Event) {
    // 防呆上限：极端情况下终端狂刷按键时别把这个循环卡住。
    const MAX_INPUT_KEYS: usize = 4096;
    let mut keys: Vec<KeyEvent> = Vec::new();
    let mut event = first;
    loop {
        match event {
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    keys.push(key);
                } else if key.kind == KeyEventKind::Release && is_ctrl_v_release(key) {
                    // Windows Terminal 把 `ctrl+v` 绑成自家粘贴动作，按下被它吃掉、只有抬起
                    // 透传；这个抬起必须放进主事件流，否则「Ctrl+V 粘贴图片」永远收不到触发。
                    // 其余抬起/重复键仍丢弃：它们既不参与粘贴判定，也不该被当成输入重放。
                    app.handle_event(Event::Key(key));
                }
            }
            other => app.handle_event(other),
        }
        if keys.len() >= MAX_INPUT_KEYS || !event::poll(Duration::ZERO).unwrap_or(false) {
            break;
        }
        match event::read() {
            Ok(next) => event = next,
            Err(_) => break,
        }
    }
    if keys.is_empty() {
        return;
    }
    deliver_keys(app, tracker, keys, Instant::now());
}

/// 把一批按键交给粘贴识别状态机，并按结果分派。
fn deliver_keys(
    app: &mut App,
    tracker: &mut paste::PasteTracker,
    keys: Vec<KeyEvent>,
    now: Instant,
) {
    // 读一次剪贴板：读不到（非 Windows / 被占用 / 不是文本）时退回形状判定。
    let clipboard = clipboard::read_text();
    match tracker.feed(&keys, clipboard.as_deref(), now) {
        paste::PasteOutcome::Paste(text) => app.handle_event(Event::Paste(text)),
        // 等后续批次：不处理任何按键，由事件循环按 `paste_timeout` 收紧等待。
        paste::PasteOutcome::Pending => {}
        paste::PasteOutcome::Bypass(keys) => {
            for key in keys {
                app.handle_event(Event::Key(key));
            }
        }
    }
}

/// 挂起超时后把候选前缀冲刷成普通输入（对映 Python 空闲循环里的 `flush_keys(force=True)`）。
fn flush_pending_paste(app: &mut App, tracker: &mut paste::PasteTracker) {
    for key in tracker.flush() {
        app.handle_event(Event::Key(key));
    }
}

/// 等待终端事件的时长：挂起期间按短窗口醒来，好让超时能及时回退。
fn paste_wait(tracker: &paste::PasteTracker, budget: Duration) -> Duration {
    if tracker.is_pending() {
        budget.min(paste::PASTE_SINGLE_LINE_PREFIX_TIMEOUT)
    } else {
        budget
    }
}

/// 终端状态守卫：无论正常退出还是提前返回，都把终端恢复到进入前的样子。
///
/// 除了原始模式与备用屏幕，还负责鼠标/焦点报告与**控制台输入模式自愈**：
/// Windows 上锁屏、息屏或工具子进程都可能把控制台输入模式重置回普通模式，
/// 之后 crossterm 再也收不到鼠标与功能键事件。守卫按 [`CONSOLE_HEAL_INTERVAL`]
/// 核对一次，被改写时改回去并重发终端协议序列（对映 Python 侧的
/// `_recover_stale_mouse_interaction` 周期看门狗）。
struct TerminalGuard {
    last_heal: Instant,
    /// 上一次向用户提示自愈的时间；一分钟内的重复恢复只恢复、不再刷屏。
    last_notice: Option<Instant>,
}

impl TerminalGuard {
    /// 是否该向用户提示这次自愈（同一分钟内的重复恢复只提示一次）。
    fn notice_due(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last_notice {
            if now.saturating_duration_since(last) < NOTICE_MIN_INTERVAL {
                return false;
            }
        }
        self.last_notice = Some(now);
        true
    }

    fn start() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("进入原始模式失败：{error}"))?;
        let mut out = stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            EnableFocusChange
        )
        .map_err(|error| format!("切换备用屏幕失败：{error}"))?;
        Ok(Self {
            last_heal: Instant::now(),
            last_notice: None,
        })
    }

    /// 周期核对控制台输入模式；返回是否真的恢复过（调用方据此提示用户）。
    fn heal_if_needed(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_heal) < CONSOLE_HEAL_INTERVAL {
            return false;
        }
        self.last_heal = now;
        if !heal_console_input_mode().is_restored() {
            return false;
        }
        // 控制台模式被系统重置后不会再产生可识别的 AppFocus，必须主动重发
        // 鼠标与焦点报告序列，否则「模式恢复了但事件依旧不来」。
        // （重发鼠标捕获会把模式整值设回 crossterm 的目标值，与自愈目标一致，
        // 不再互相覆盖——两者若不一致就会一秒一次地互相判定为「被重置」。）
        let mut out = stdout();
        let _ = execute!(out, EnableMouseCapture, EnableFocusChange);
        true
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(
            out,
            DisableFocusChange,
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}
