//! `omnicrawl-tui` 二进制：终端生命周期与事件循环。
//!
//! 协议宿主、状态机与渲染都在库侧；这里只做选内核、进全屏、收事件这三件事。

use std::io::stdout;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use omnicrawl_tui::app::App;
use omnicrawl_tui::args::{parse, Parsed};
use omnicrawl_tui::kernel::KernelClient;
use omnicrawl_tui::ui;

/// 事件轮询间隔：既决定界面刷新率，也决定内核帧的处理延迟。
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// 退出时等内核自己收尾的时间。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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

    let options = match parse(&args, &env, &exe_dir)? {
        Parsed::Help(text) | Parsed::Version(text) => {
            println!("{text}");
            return Ok(ExitCode::SUCCESS);
        }
        Parsed::Run(options) => *options,
    };

    let workspace =
        std::env::current_dir().map_err(|error| format!("无法确定当前目录：{error}"))?;
    let kernel =
        KernelClient::spawn(&options.kernel).map_err(|error| format!("启动内核失败：{error}"))?;
    let mut app = App::new(options, kernel, &workspace)?;

    // 握手在进入全屏之前完成：失败信息留在普通终端上才看得见。
    app.handshake()?;

    let guard = TerminalGuard::start()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|error| format!("初始化终端失败：{error}"))?;

    let result = event_loop(&mut terminal, &mut app);
    app.shutdown();
    app.kernel.wait_or_kill(SHUTDOWN_TIMEOUT);
    drop(guard);
    result?;
    Ok(ExitCode::SUCCESS)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<(), String> {
    loop {
        app.drain_frames();
        terminal
            .draw(|frame| ui::render(frame, &app.state))
            .map_err(|error| format!("渲染失败：{error}"))?;
        if app.quit {
            return Ok(());
        }
        if event::poll(POLL_INTERVAL).map_err(|error| format!("读取终端事件失败：{error}"))?
        {
            let event = event::read().map_err(|error| format!("读取终端事件失败：{error}"))?;
            app.handle_event(event);
        }
    }
}

/// 终端状态守卫：无论正常退出还是提前返回，都把终端恢复到进入前的样子。
struct TerminalGuard;

impl TerminalGuard {
    fn start() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("进入原始模式失败：{error}"))?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableBracketedPaste)
            .map_err(|error| format!("切换备用屏幕失败：{error}"))?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}
