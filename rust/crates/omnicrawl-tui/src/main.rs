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
    EnableFocusChange, EnableMouseCapture,
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
use omnicrawl_tui::app::App;
use omnicrawl_tui::args::{parse, Options, Parsed};
use omnicrawl_tui::kernel::KernelClient;
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
/// 启动准备完成后进入工作台前的停留秒数：只保留日志框的短暂可读窗口，启动速度
/// 优先（与 Python 入口的 `SPLASH_HOLD_AFTER_DONE_SECONDS` 同值）。
const SPLASH_HOLD_AFTER_DONE_SECONDS: f64 = 0.5;
/// 控制台输入模式自愈的核对间隔：锁屏/息屏恢复与工具子进程改写都在秒级被发现即可，
/// 又不至于每帧都去问一次控制台（对映 Python 侧的周期看门狗）。
const CONSOLE_HEAL_INTERVAL: Duration = Duration::from_secs(1);

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
    let kernel =
        KernelClient::spawn(&options.kernel).map_err(|error| format!("启动内核失败：{error}"))?;
    // 主 Agent 隔离工作区（对映 Python `entry.py` 的启动准备）：多个进程并行时各自在独立的
    // worktree / 目录副本里读写，互不写穿；创建失败仅告警并回退主工作区，不阻断启动。
    sink.write_line("准备隔离工作区", LogLevel::Info);
    let environment = ConfigEnvironment::from_process();
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
        if guard.heal_if_needed(Instant::now()) {
            eprintln!("[tui] 控制台输入模式被外部重置，已恢复鼠标与键盘协议。");
        }
        if event::poll(POLL_INTERVAL).map_err(|error| format!("读取终端事件失败：{error}"))?
        {
            let event = event::read().map_err(|error| format!("读取终端事件失败：{error}"))?;
            app.handle_event(event);
        }
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
}

impl TerminalGuard {
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
