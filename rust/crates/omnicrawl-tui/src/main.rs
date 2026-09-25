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
use omnicrawl_tui::app::{uses_external_channel, App};
use omnicrawl_tui::args::{parse, Options, Parsed};
use omnicrawl_tui::kernel::{kernel_credentials_env, KernelClient};
use omnicrawl_tui::paste;
use omnicrawl_tui::ui;
use omnicrawl_tui::ui::fullscreen::terminal::console_heal::heal_console_input_mode;
use omnicrawl_tui::ui::splash::{self, LogLevel, StartupLogSink};
use omnicrawl_workspace::agent_isolation::{
    finalize_isolation_session, prepare_isolated_workspace, start_background_isolation_sweep,
    IsolationOptions,
};

/// 事件轮询间隔：既决定界面刷新率，也决定内核帧的处理延迟。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
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
    let env = |name: &str| std::env::var(name).ok();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));

    let mut options = match parse(&args, &env, &exe_dir)? {
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
    // `--session-root` / `OMNICRAWL_SESSION_ROOT` 都没给时也要有会话，否则 `/sessions`、
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
    loop {
        // 鼠标命中判定与渲染共用同一套布局：区域每帧在渲染前刷新一次。
        let size = terminal
            .size()
            .map_err(|error| format!("读取终端尺寸失败：{error}"))?;
        app.set_viewport(Rect::new(0, 0, size.width, size.height));
        app.tick_welcome_logo_animation(Instant::now());
        app.tick_subagent_trees();
        app.tick_tts_tasks();
        // 底部单行轮播：遥测 → 工作区路径 → 留言，各 10s，切换时解密扫描。
        app.tick_carousel(Instant::now());
        // 输入框上方那行瞬时提示（拖选复制等）到时自散。
        app.state.tick_notice_line(Instant::now());
        // 慢命令（`/workspace`、`/mcp`）的后台结果：工作区切换在这里提交。
        app.tick_slow_command();
        app.tick_config_chat();
        app.tick_monitor_events(Instant::now());
        app.drain_frames();
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
        if app.quit {
            return Ok(());
        }
        if guard.heal_if_needed(Instant::now()) && guard.notice_due(Instant::now()) {
            // 不再直接写终端（会盖住底部输入框）：改成会话流里的提示行。
            app.state
                .notice("控制台输入模式被外部重置，已恢复鼠标与键盘协议。".to_string());
        }
        if event::poll(POLL_INTERVAL).map_err(|error| format!("读取终端事件失败：{error}"))?
        {
            let event = event::read().map_err(|error| format!("读取终端事件失败：{error}"))?;
            handle_event(app, event);
        }
    }
}

/// 处理一个终端事件，并顺带识别「一串按键形式的粘贴」。
///
/// 终端支持 bracketed paste 时事件本身就是 `Event::Paste`；但 conhost 等终端不发那个序列，
/// 粘贴会退化成一串普通按键（换行成了 `Enter`）——于是第一行被提交、后面的行逐条排队。
/// 这里把**当前这一刻能读到的按键全收进来**（`poll(0)` 循环，不依赖按键间隔），整串像粘贴就
/// 当成一次粘贴交给折叠路径；不像则逐条交给正常按键路径（行为与以前一致）。非按键事件当场
/// 处理，不会排在按键后面。
fn handle_event(app: &mut App, first: Event) {
    // 防呆上限：极端情况下终端狂刷按键时别把这个循环卡住。
    const MAX_INPUT_KEYS: usize = 4096;
    let mut keys: Vec<KeyEvent> = Vec::new();
    let mut event = first;
    loop {
        match event {
            // 只留按下事件：keyup/repeat 既不该参与判定，也不该被当成输入重放。
            Event::Key(key) => {
                if key.kind == KeyEventKind::Press {
                    keys.push(key);
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
    if paste::looks_like_paste(&keys) {
        if let Some(text) = paste::burst_text(&keys) {
            app.handle_event(Event::Paste(text));
            return;
        }
    }
    for key in keys {
        app.handle_event(Event::Key(key));
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
