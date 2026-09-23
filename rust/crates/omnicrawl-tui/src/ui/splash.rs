//! 启动画面（对映 Python `omnicrawl/ui/splash.py`）：黑色背景 + 左侧黄色 Logo +
//! 右侧圆角日志框 + Windows XP 风格滚动条。
//!
//! 布局类似 fastfetch：左侧显示 Logo，右侧用圆角矩形框（╭ ╮ │ ╰ ╯）展示启动
//! 日志。启动准备（内核进程、配置与工具表、MCP 能力、内核握手）在后台线程执行，
//! 并通过 [`StartupLogSink`] 逐阶段写入日志；日志按级别着色（信息「- 」、警告
//! 「! 」、错误「× 」），超出显示框时向上滚动只保留最新几行。画面底部仍为黄色
//! 滑块滚动条，在 TUI 接管终端前显示（默认直到准备完成）。
//!
//! 渲染纯用 ANSI 转义序列，跨 Windows / macOS / Linux 均可运行。与 Python 的两处
//! 必要差异：
//! - Python 侧用 `logging.Handler` 桥接 root logger 的 WARNING/ERROR；Rust 侧没有
//!   logging 框架，改由 [`report_startup_log`] 显式上报，语义等价：有桥时进日志框
//!   并**不**落 stderr（避免打乱启动画面），无桥时回落 stderr（管道/测试下诊断不丢）。
//! - `shutil.get_terminal_size` 会先看 `COLUMNS`/`LINES` 环境变量；这里直接问终端，
//!   取不到时同样回落 80x24。

use std::any::Any;
use std::io::{self, IsTerminal, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;

// ANSI 转义序列
const CLEAR: &str = "\x1b[2J";
const HOME: &str = "\x1b[H";
const RESET: &str = "\x1b[0m";
const BG_BLACK: &str = "\x1b[40m";
const FG_YELLOW: &str = "\x1b[33m"; // 标准黄（警告）
const FG_BRIGHT_YELLOW: &str = "\x1b[93m"; // 亮黄（Logo / 日志框边框）
const FG_RED: &str = "\x1b[91m"; // 亮红（错误）
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

// Windows XP 滚动条：灰色轨道 + 黄色小方块
const TRACK_BG: &str = "\x1b[100m"; // 亮黑/灰
const SLIDER_BG: &str = "\x1b[103m"; // 亮黄

/// Logo（来自用户指定的桌面样式文件；已按 Python `_logo_render_lines()` 的同一
/// 规则去掉所有行共有的 18 列前导空白，行尾空白也一并去掉）。
pub const LOGO_LINES: [&str; 14] = [
    "     !cpmZmmn_",
    "   tdO0ZmOZwOmZOt,",
    " .wmZOZmO0pwmpmwmqqZ+",
    " qZwOmwwwmdpwdqpbkkbbbpO>.                    ..",
    "?bwmpZwwwpdkbpdbhahkhaohoaaap-;.        .I{kO0mOL'",
    "Qpmwbqpbdpdkabhbokhooooh*o*#**######MWWWaOZOOOO0Y",
    "Oqwkbqpkkbdboahoao***M*###*##MMWWWWWWMp] `I!:",
    "xpwhbwpkpoooaao***##*M####MMWMWWWWW#L'",
    ".bqakhada*oao*o**##*#WMMWWWW&&&&War.",
    " 1kkkhaoo***o#*#*M##MMWWWWWW&WWb]",
    "  )hha*oa**o*#**#M#MWMWMW&&&#Z:",
    "   :do**o*#o**#MMMWWWMWWWMb/.",
    "     Im*****###M#MWWWMMkv\"",
    "        :jpo**#*##obv_",
];

/// 0 表示不设置人为最短时长；启动页仅等待准备完成。
pub const DEFAULT_DURATION: f64 = 0.0;

/// prepare 完成后画面额外停留的秒数，便于用户查看日志框内的最新启动日志。
pub const DEFAULT_HOLD_AFTER_DONE: f64 = 2.0;

/// Logo 左侧留白列数与 Logo / 日志框之间的列间距
const LOGO_MARGIN: usize = 2;
const BOX_GAP: usize = 4;

/// 日志框内容区的最小宽度（列）
const BOX_MIN_WIDTH: usize = 30;

/// 渲染帧间隔：Python 侧 `time.sleep(0.05)` 的同一节拍。
const FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// 日志级别 → 行首标记（颜色在渲染时应用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

impl LogLevel {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Info => "- ",
            Self::Warning => "! ",
            Self::Error => "× ",
        }
    }
}

/// 线程安全的启动日志收集器；渲染线程按帧读取并展示。
///
/// `prepare` 在后台线程写入，splash 渲染循环在主线程读取，通过锁保证可见性；
/// 快照按写入顺序返回，渲染层只取尾部窗口。
#[derive(Debug, Default)]
pub struct StartupLogSink {
    entries: Mutex<Vec<(LogLevel, String)>>,
}

impl StartupLogSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一行启动日志；空行被忽略。
    pub fn write_line(&self, text: &str, level: LogLevel) {
        // 单行渲染：日志内的换行统一折叠为空格。
        let message = fold_to_single_line(text);
        if message.is_empty() {
            return;
        }
        // 日志锁中毒不能影响启动主流程：直接取回内部数据继续写入。
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.push((level, message));
    }

    /// 返回当前全部日志（按写入顺序）。
    pub fn snapshot(&self) -> Vec<(LogLevel, String)> {
        self.entries
            .lock()
            .map(|entries| entries.clone())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries
            .lock()
            .map(|entries| entries.is_empty())
            .unwrap_or(true)
    }
}

/// 对映 `" ".join(text.splitlines())`：折叠 Python `str.splitlines` 认作换行的字符。
fn fold_to_single_line(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split(|ch: char| {
            matches!(
                ch,
                '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}'
                    ..='\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 启动日志桥：splash 显示期间把启动期诊断接进日志框。
static BRIDGE: OnceLock<Mutex<Option<Arc<StartupLogSink>>>> = OnceLock::new();

/// 桥接句柄；Drop 时摘除（对映 Python `attach_startup_log_handler` 返回的卸载函数）。
#[must_use]
pub struct StartupLogBridge;

impl Drop for StartupLogBridge {
    fn drop(&mut self) {
        if let Ok(mut slot) = BRIDGE.get_or_init(|| Mutex::new(None)).lock() {
            *slot = None;
        }
    }
}

/// 把启动期诊断桥接到启动日志框，返回 Drop 时自动摘除的句柄。
pub fn attach_startup_log_handler(sink: Arc<StartupLogSink>) -> StartupLogBridge {
    if let Ok(mut slot) = BRIDGE.get_or_init(|| Mutex::new(None)).lock() {
        *slot = Some(sink);
    }
    StartupLogBridge
}

/// 上报一条启动期诊断：有桥进日志框，无桥落 stderr。
///
/// 启动画面期间不能直接写 stderr——转义序列与日志行互相插入会把画面打乱，
/// 因此有桥时只写日志框；非交互环境（没有桥）下仍保持与原来相同的 stderr 输出，
/// 诊断不会被吞掉。
pub fn report_startup_log(level: LogLevel, message: &str) {
    let attached = BRIDGE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|slot| slot.clone());
    match attached {
        Some(sink) => sink.write_line(message, level),
        None => eprintln!("[tui] {message}"),
    }
}

/// 后台准备的产物：准备失败与线程 panic 都要被渲染循环看见，画面才不会卡住。
///
/// 对映 Python 侧 worker 的 `except BaseException`：异常存进 `error_box`，渲染循环
/// 读 `has_error()` 后立即收画面；Rust 侧 `catch_unwind` 保住 panic 载荷，收完画面
/// 再原样抛出。
type PrepareOutcome<T, E> = Result<Result<T, E>, Box<dyn Any + Send + 'static>>;

/// 显示启动画面，同时后台执行 `prepare`（加载启动依赖）。
///
/// - `prepare`：启动准备回调，在 splash 显示期间于后台线程执行；接收日志收集器
///   可随时写入启动日志行，返回值（含错误）透传给调用方。
/// - `duration`：可选的最短显示时长；为零时不设置人为等待，默认直到 `prepare`
///   完成就结束；若 `prepare` 耗时更长，则持续显示滚动条。
/// - `hold_after_done`：`prepare` 完成后画面额外停留的时长（此时日志框内的启动
///   日志保持可见，滚动条继续动画）；为零时完成即退出。`prepare` 失败时不额外
///   停留，立即结束画面让错误尽快呈现。
///
/// 非交互输出流（测试、管道、重定向）时直接同步执行 `prepare` 并返回，不显示动画。
/// splash 显示期间会把启动期诊断桥接进日志框，`prepare` 返回或失败后自动摘除。
pub fn run_startup_splash<T, E, F>(
    prepare: F,
    duration: Duration,
    hold_after_done: Duration,
) -> Result<T, E>
where
    F: FnOnce(&StartupLogSink) -> Result<T, E> + Send,
    T: Send,
    E: Send,
{
    let sink = Arc::new(StartupLogSink::new());
    // 桥接必须早于后台线程启动，避免准备线程早期诊断在安装前漏到 stderr。
    let bridge = attach_startup_log_handler(Arc::clone(&sink));

    if !io::stdout().is_terminal() {
        let result = prepare(sink.as_ref());
        drop(bridge);
        return result;
    }

    let (sender, receiver) = mpsc::channel();
    let worker_sink = Arc::clone(&sink);
    let result = thread::scope(|scope| {
        scope.spawn(move || {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| prepare(worker_sink.as_ref())));
            // 结果只可能被渲染线程取走；发送失败说明画面已先退出（准备失败路径），
            // 此时结果已由渲染线程持有，无需再处理。
            let _ = sender.send(outcome);
        });
        let mut out = io::stdout().lock();
        render_splash(
            &mut out,
            sink.as_ref(),
            &receiver,
            duration,
            hold_after_done,
        )
    });
    drop(bridge);
    result
}

/// 绘制启动画面：左侧 Logo + 右侧圆角日志框 + 底部 XP 滚动条，并在准备结束后收尾。
///
/// 仅当准备完成且满足显示条件（最短时长 `duration` 已到、完成后停留
/// `hold_after_done` 已到）时结束；始终等待准备完成，准备失败立即结束以便错误
/// 尽快呈现。日志框只保留最新的 `box_inner_rows` 行，内容变化时整窗重绘，
/// 避免重叠残留。
fn render_splash<T, E>(
    out: &mut impl Write,
    sink: &StartupLogSink,
    results: &Receiver<PrepareOutcome<T, E>>,
    duration: Duration,
    hold_after_done: Duration,
) -> Result<T, E> {
    let (width, height) = terminal_size();
    let logo_lines = logo_render_lines();
    let logo_width = logo_lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let layout = Layout::new(width, height, logo_width, logo_lines.len());

    // 写失败（管道已关闭等）不阻断启动：画面只是显示不出来，后续终端初始化会给出
    // 更明确的错误。
    let _ = write!(out, "{BG_BLACK}{CLEAR}{HOME}{HIDE_CURSOR}");
    let _ = out.flush();

    // 左侧 logo（亮黄色）
    for (index, line) in logo_lines.iter().enumerate() {
        let row = layout.top + index + 1;
        let _ = write!(
            out,
            "\x1b[{row};{}H{FG_BRIGHT_YELLOW}{line}{RESET}",
            layout.left + 1
        );
    }
    draw_box_border(out, &layout);
    draw_box_content(out, &layout, &sink.snapshot());
    let _ = out.flush();

    let start = Instant::now();
    let mut done_at: Option<f64> = None;
    let mut frame: u64 = 0;
    let mut last_entries: Option<Vec<(LogLevel, String)>> = None;
    let mut finished: Option<PrepareOutcome<T, E>> = None;
    loop {
        if finished.is_none() {
            finished = results.try_recv().ok();
        }
        let elapsed = start.elapsed().as_secs_f64();
        match &finished {
            // 准备失败或准备线程崩溃：立即结束画面，让调用方快速看到错误。
            Some(Ok(Err(_))) | Some(Err(_)) => break,
            Some(Ok(Ok(_))) => {
                // 记录完成时刻（相对画面起点的秒数），进入「完成后停留」阶段：
                // 日志框保持可见、滚动条继续动画。退出需同时满足最短展示时长
                // duration（默认 0 恒真）与完成后额外停留 hold_after_done。
                let done = *done_at.get_or_insert(elapsed);
                if elapsed >= duration.as_secs_f64()
                    && elapsed - done >= hold_after_done.as_secs_f64()
                {
                    break;
                }
            }
            None => {}
        }
        let entries = sink.snapshot();
        if last_entries.as_ref() != Some(&entries) {
            draw_box_content(out, &layout, &entries);
            last_entries = Some(entries);
        }
        draw_progress_bar(out, &layout, frame);
        frame += 1;
        thread::sleep(FRAME_INTERVAL);
    }

    // 复位：显示光标并清屏，避免残留
    let _ = write!(out, "{SHOW_CURSOR}{RESET}{CLEAR}{HOME}");
    let _ = out.flush();

    // 循环只在拿到准备结果后退出；准备线程 panic 时先复位终端，再把 panic 原样抛出。
    match finished {
        Some(Ok(Ok(value))) => Ok(value),
        Some(Ok(Err(error))) => Err(error),
        Some(Err(payload)) => panic::resume_unwind(payload),
        None => unreachable!("splash 循环只在收到准备结果后退出"),
    }
}

/// 终端尺寸（列, 行）；取不到时回落 Python 侧的 80x24。
fn terminal_size() -> (usize, usize) {
    match crossterm::terminal::size() {
        Ok((columns, rows)) if columns > 0 && rows > 0 => (columns as usize, rows as usize),
        _ => (80, 24),
    }
}

/// 返回实际渲染的 Logo 行：去掉所有行共有的前导空格。
///
/// 原 Logo 文本自带大量前导空白，会把图形推到第 25 列附近，挤占右侧日志框；
/// 统一去掉公共前导空格后，图形从 [`LOGO_MARGIN`] 处开始。字面量是纯 ASCII，
/// 按字节切片即按列切片。
fn logo_render_lines() -> Vec<String> {
    let indent = LOGO_LINES
        .iter()
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    LOGO_LINES
        .iter()
        .map(|line| line[indent..].to_string())
        .collect()
}

/// 启动画面的全部落点（列/行都是 0 起的屏幕坐标，写入时再转 1 起）。
struct Layout {
    /// Logo 起始列
    left: usize,
    /// 顶部留白行数
    top: usize,
    /// 日志框左边界列与顶边界行
    box_left: usize,
    box_top: usize,
    /// 日志框内容区宽度与高度（不含边框）
    inner_width: usize,
    box_inner_rows: usize,
    /// 滚动条落点与尺寸
    bar_row: usize,
    bar_left: usize,
    bar_width: usize,
    slider_width: usize,
}

impl Layout {
    /// 按终端尺寸与 Logo 尺寸算落点。
    ///
    /// 窄终端保护：日志框至少保留 [`BOX_MIN_WIDTH`] 列，放不下时整体左移，但下限
    /// 必须保证 Logo 与日志框之间至少 1 列间隙，绝不能覆盖 Logo；若终端仍不够宽，
    /// 允许日志框超出右边界（由终端换行），优先保证 Logo 完整可见。日志框高度跟随
    /// Logo：上下边框 + 内容区，垂直与 Logo 主体对齐。
    fn new(width: usize, height: usize, logo_width: usize, logo_rows: usize) -> Self {
        let left = LOGO_MARGIN;
        let mut box_left = left + logo_width + BOX_GAP;
        if box_left + BOX_MIN_WIDTH > width {
            box_left = (left + logo_width + 1).max(width.saturating_sub(BOX_MIN_WIDTH));
        }
        let inner_width = width.saturating_sub(box_left + 2).max(6);
        let box_inner_rows = logo_rows.min(height.saturating_sub(8)).max(4);
        let box_outer_rows = box_inner_rows + 2;
        let body_rows = logo_rows.max(box_outer_rows);
        let total_rows = body_rows + 4;
        let top = height.saturating_sub(total_rows) / 2;
        let box_top = top + body_rows.saturating_sub(box_outer_rows) / 2;
        let bar_row = top + body_rows + 2;
        let bar_width = (width.saturating_sub(4) / 2).clamp(10, 30);
        let slider_width = (bar_width / 5).max(4);
        let bar_left = width.saturating_sub(bar_width) / 2;
        Self {
            left,
            top,
            box_left,
            box_top,
            inner_width,
            box_inner_rows,
            bar_row,
            bar_left,
            bar_width,
            slider_width,
        }
    }
}

/// 绘制圆角矩形日志框边框（╭ ╮ │ ╰ ╯），亮黄色以匹配 Logo。
fn draw_box_border(out: &mut impl Write, layout: &Layout) {
    let right = layout.box_left + layout.inner_width + 1;
    let _ = write!(
        out,
        "\x1b[{};{}H{FG_BRIGHT_YELLOW}╭{}╮{RESET}",
        layout.box_top + 1,
        layout.box_left + 1,
        "─".repeat(layout.inner_width)
    );
    for row in 1..=layout.box_inner_rows {
        let _ = write!(
            out,
            "\x1b[{};{}H{FG_BRIGHT_YELLOW}│{RESET}\x1b[{};{}H{FG_BRIGHT_YELLOW}│{RESET}",
            layout.box_top + row + 1,
            layout.box_left + 1,
            layout.box_top + row + 1,
            right + 1
        );
    }
    let _ = write!(
        out,
        "\x1b[{};{}H{FG_BRIGHT_YELLOW}╰{}╯{RESET}",
        layout.box_top + layout.box_inner_rows + 1,
        layout.box_left + 1,
        "─".repeat(layout.inner_width)
    );
}

/// 把日志尾窗口绘制进日志框；内容超出时只保留最新行（向上滚动）。
///
/// 信息「- 」默认前景色，警告「! 」黄色，错误「× 」红色；每行按框内宽度折行并
/// 补齐空白，重绘时不残留旧文本。
fn draw_box_content(out: &mut impl Write, layout: &Layout, entries: &[(LogLevel, String)]) {
    let mut display: Vec<String> = Vec::new();
    for (level, text) in entries {
        let prefix = level.marker();
        let prefix_width = prefix.chars().count();
        for (index, chunk) in
            wrap_text(text, layout.inner_width.saturating_sub(prefix_width).max(1))
                .iter()
                .enumerate()
        {
            if index == 0 {
                display.push(format!("{prefix}{chunk}"));
            } else {
                display.push(format!("{}{chunk}", " ".repeat(prefix_width)));
            }
        }
    }

    let visible = &display[display.len().saturating_sub(layout.box_inner_rows)..];
    for (row, line) in visible.iter().enumerate() {
        let style = if line.starts_with("! ") {
            FG_YELLOW
        } else if line.starts_with("× ") {
            FG_RED
        } else {
            ""
        };
        let _ = write!(
            out,
            "\x1b[{};{}H{style}{}{RESET}",
            layout.box_top + 1 + row,
            layout.box_left + 2,
            pad_to_width(line, layout.inner_width)
        );
    }
}

/// 在指定行绘制一帧滚动条（灰色轨道 + 黄色滑块循环滑动）。
///
/// 动画流程（每周期 `滑入 + 横穿 + 滑出 + 空档` 帧，约 2 秒）：
/// 1. 滑入：滑块从轨道左侧外右移进入，可见部分逐帧变宽，直到完整出现在左端；
/// 2. 横穿：滑块保持全宽匀速滑过整个轨道，到达右端；
/// 3. 滑出：滑块整体继续右移、滑出右边界，可见部分逐帧变窄直至完全消失；
/// 4. 空档：轨道上无滑块，停顿片刻后重新从左侧滑入，形成单向循环。
fn draw_progress_bar(out: &mut impl Write, layout: &Layout, frame: u64) {
    let width = layout.bar_width;
    let slider_width = layout.slider_width;
    let fade = slider_width; // 滑入/滑出各占 slider_width 帧
    let travel = width.saturating_sub(slider_width).max(1); // 全宽横穿帧数
    let gap = (slider_width / 2).max(2); // 完全消失后的空档帧数
    let cycle = fade + travel + fade + gap;
    let position = frame % cycle as u64;

    let offset = if position < fade as u64 {
        // 阶段 1：滑入 —— 滑块左端从 -slider_width 推进到 0，逐渐出现在左端
        position as isize - slider_width as isize
    } else if position < (fade + travel) as u64 {
        // 阶段 2：横穿 —— 滑块左端从 0 推进到 width - slider_width
        position as isize - fade as isize
    } else if position < (fade + travel + fade) as u64 {
        // 阶段 3：滑出 —— 滑块左端从 width - slider_width 推进到 width，逐渐消失
        (position as isize - (fade + travel) as isize) + (width - slider_width) as isize
    } else {
        // 阶段 4：空档 —— 滑块完全移出轨道，不绘制
        width as isize
    };

    // 统一裁剪：只绘制滑块与轨道 [0, width) 相交的可见部分
    let start = offset.max(0) as usize;
    let end = (offset + slider_width as isize).clamp(0, width as isize) as usize;

    let mut line = format!("\x1b[{};{}H{TRACK_BG}", layout.bar_row, layout.bar_left + 1);
    line.push_str(&" ".repeat(start));
    if end > start {
        line.push_str(SLIDER_BG);
        line.push_str(&" ".repeat(end - start));
    }
    // 滑块右侧的轨道必须重新声明灰色背景，否则终端会沿用滑块黄色背景，
    // 导致滑块右侧整段都被染黄（此前误渲染成「进度条填充」观感）。
    line.push_str(TRACK_BG);
    line.push_str(&" ".repeat(width - end));
    line.push_str(RESET);
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// 返回字符的显示宽度：CJK 等全角字符计 2 列，其余计 1 列。
fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// 按显示宽度折行：CJK/全角字符计 2 列，超宽单词按字符拆分。
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let cells = char_width(ch);
        if !current.is_empty() && current_width + cells > width {
            lines.push(std::mem::take(&mut current));
            current.push(ch);
            current_width = cells;
        } else {
            current.push(ch);
            current_width += cells;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 把一行补齐/截断到指定显示宽度（全角字符计 2 列）。
///
/// 折行已保证内容不超过 `width`，这里只做兜底截断并按剩余宽度补空格，避免重绘时
/// 行尾残留旧文本，也不会因 CJK 双宽字符把内容推出日志框。
fn pad_to_width(line: &str, width: usize) -> String {
    let mut chars: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in line.chars() {
        let cells = char_width(ch);
        if used + cells > width {
            break;
        }
        chars.push(ch);
        used += cells;
    }
    chars.into_iter().collect::<String>() + &" ".repeat(width.saturating_sub(used))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_folds_newlines_and_ignores_blank_lines() {
        let sink = StartupLogSink::new();
        sink.write_line("  ", LogLevel::Info);
        sink.write_line("", LogLevel::Error);
        sink.write_line("  第一行\n第二行  ", LogLevel::Warning);
        sink.write_line("错误", LogLevel::Error);
        assert_eq!(
            sink.snapshot(),
            vec![
                (LogLevel::Warning, "第一行 第二行".to_string()),
                (LogLevel::Error, "错误".to_string()),
            ]
        );
    }

    #[test]
    fn markers_match_python_levels() {
        assert_eq!(LogLevel::Info.marker(), "- ");
        assert_eq!(LogLevel::Warning.marker(), "! ");
        assert_eq!(LogLevel::Error.marker(), "× ");
    }

    #[test]
    fn logo_lines_drop_common_indent_only() {
        let lines = logo_render_lines();
        assert_eq!(lines.len(), LOGO_LINES.len());
        assert_eq!(lines[0], "     !cpmZmmn_", "第 5 列起画图形");
        assert_eq!(lines[9], " 1kkkhaoo***o#*#*M##MMWWWWWW&WWb]");
        assert_eq!(
            lines.iter().map(|line| line.chars().count()).max(),
            Some(50),
            "公共缩进 18 列后最宽 50 列，与 Python 一致"
        );
        for line in &lines {
            assert_eq!(line, &line.trim_end(), "行尾不得残留填充空白");
        }
    }

    #[test]
    fn layout_keeps_box_clear_of_logo_on_narrow_terminal() {
        let wide = Layout::new(120, 40, 50, 14);
        assert_eq!(wide.left, 2);
        assert_eq!(wide.box_left, 2 + 50 + 4);
        assert_eq!(wide.box_inner_rows, 14, "日志框高度跟随 Logo");
        assert_eq!(
            wide.bar_row,
            wide.top + 14 + 2 + 2,
            "滚动条在主体下方留一行余量"
        );

        let narrow = Layout::new(60, 24, 50, 14);
        assert_eq!(narrow.box_left, 53, "放不下时左移到 Logo 右侧 1 列间隙");
        assert!(narrow.box_left > narrow.left + 50);
        assert_eq!(narrow.inner_width, 6, "越界宽度按最小内容区兜底");

        let snug = Layout::new(80, 24, 50, 14);
        assert_eq!(
            snug.box_left, 53,
            "max(Logo 间隙优先, 终端宽 - 30)：不能为凑宽度压掉 Logo 间隙"
        );
        assert_eq!(snug.inner_width, 80 - 53 - 2);
    }

    #[test]
    fn layout_clamps_to_minimum_rows_on_short_terminal() {
        let layout = Layout::new(80, 10, 50, 14);
        assert_eq!(layout.box_inner_rows, 4, "矮终端下内容区至少 4 行");
        assert_eq!(layout.top, 0);
    }

    #[test]
    fn wrap_text_counts_display_columns() {
        assert_eq!(wrap_text("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(wrap_text("中文测试", 4), vec!["中文", "测试"]);
        assert_eq!(wrap_text("", 5), vec![""]);
        assert_eq!(wrap_text("abc", 0), vec!["abc"]);
    }

    #[test]
    fn pad_to_width_fills_and_truncates() {
        assert_eq!(pad_to_width("ab", 5), "ab   ");
        assert_eq!(pad_to_width("中文", 4), "中文");
        assert_eq!(pad_to_width("中文", 3), "中 ", "双宽字符不会被截成半格残留");
        assert_eq!(pad_to_width("abcdef", 3), "abc");
    }

    #[test]
    fn box_border_and_content_stay_inside_the_frame() {
        let layout = Layout::new(100, 30, 50, 14);
        let mut out: Vec<u8> = Vec::new();
        draw_box_border(&mut out, &layout);
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.contains("╭"), "{text:?}");
        assert!(text.contains("╯"), "{text:?}");
        assert_eq!(
            text.matches('│').count(),
            layout.box_inner_rows * 2,
            "左右边框逐行各一条"
        );

        let mut content: Vec<u8> = Vec::new();
        draw_box_content(
            &mut content,
            &layout,
            &[
                (LogLevel::Info, "加载配置".to_string()),
                (LogLevel::Warning, "插件初始化失败".to_string()),
            ],
        );
        let rendered = String::from_utf8(content).unwrap();
        assert!(rendered.contains("- 加载配置"), "{rendered:?}");
        assert!(
            rendered.contains(&format!("{FG_YELLOW}! 插件初始化失败")),
            "{rendered:?}"
        );
    }

    #[test]
    fn box_content_keeps_only_latest_rows() {
        let layout = Layout::new(100, 30, 50, 14);
        let entries: Vec<(LogLevel, String)> = (0..layout.box_inner_rows + 5)
            .map(|index| (LogLevel::Info, format!("日志{index}")))
            .collect();
        let mut out: Vec<u8> = Vec::new();
        draw_box_content(&mut out, &layout, &entries);
        let rendered = String::from_utf8(out).unwrap();
        assert!(!rendered.contains("日志0"), "旧行应被滚出日志框");
        assert!(rendered.contains(&format!("日志{}", entries.len() - 1)));
    }

    #[test]
    fn progress_bar_cycles_slide_travel_and_gap() {
        let layout = Layout::new(80, 24, 50, 14);
        let width = layout.bar_width;
        let slider = layout.slider_width;
        let cycle = slider * 2 + width.saturating_sub(slider).max(1) + (slider / 2).max(2);
        assert!(cycle > slider * 2);

        let render = |frame: u64| {
            let mut out: Vec<u8> = Vec::new();
            draw_progress_bar(&mut out, &layout, frame);
            String::from_utf8(out).unwrap()
        };
        // 滑入首帧：滑块仍完全在轨道左侧外，只有轨道可见。
        let entering = render(0);
        assert!(
            !entering.contains(SLIDER_BG),
            "滑块尚未进入轨道：{entering:?}"
        );
        assert!(entering.contains(TRACK_BG));
        // 滑入后段：滑块部分可见，轨道左侧已有灰色。
        let sliding_in = render((slider - 1) as u64);
        assert!(sliding_in.contains(SLIDER_BG), "{sliding_in:?}");
        assert!(sliding_in.contains(TRACK_BG));
        // 横穿中段：滑块全宽可见，轨道左右都还有灰色。
        let travel = render((slider + 1) as u64);
        assert!(travel.contains(SLIDER_BG) && travel.contains(TRACK_BG));
        // 空档：完全没有滑块。
        let gap = render((cycle - 1) as u64);
        assert!(!gap.contains(SLIDER_BG), "空档帧不应绘制滑块：{gap:?}");
        assert!(gap.contains(TRACK_BG));
    }

    #[test]
    fn non_tty_startup_runs_prepare_synchronously_with_log_bridge() {
        // 测试进程的 stdout 不是终端：splash 走同步路径，不渲染动画。
        let result: Result<usize, String> = run_startup_splash(
            |sink| {
                report_startup_log(LogLevel::Warning, "准备阶段警告");
                sink.write_line("阶段一", LogLevel::Info);
                assert!(!sink.is_empty(), "日志框在 prepare 期间即可读");
                Ok(3)
            },
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(result, Ok(3));
        // 桥已在返回前摘除：后续诊断回落 stderr，不会残留在已丢弃的 sink 上。
        let error: Result<usize, String> = run_startup_splash(
            |_| Err("准备失败".to_string()),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(error, Err("准备失败".to_string()));
    }
}
