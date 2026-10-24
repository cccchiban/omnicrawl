//! 子代理独立窗口：把 `subagent.tool.*` / `subagent.turn.text` 转成一条可跟随的
//! JSONL 流水，并在**新终端窗口**里渲染。主会话不再画进度树，只留一行受理提示。
//!
//! 分两半：
//!
//! - **主宿主**（[`SubagentWindows`]）：收到子代理事件就按 `batch_id` 记账。批次第一次出现时
//!   建流水文件并用 `CREATE_NEW_CONSOLE` 拉起一个 `omnicrawl-tui --subagent-window <文件>`
//!   子进程；之后每个事件追加一行。主会话只保留「已开窗口」这一份状态，不参与渲染。
//! - **窗口进程**（[`run_window`]）：尾随流水文件，把事件重建成 `AppState` 的会话记录，
//!   再用**主代理那套** `ui::render` 渲染整页（会话区 + 输入区方框 + 底部 HUD）。
//!   窗口关闭即进程结束，宿主侧不受影响。
//!
//! 为什么用文件而不是管道：窗口是另一个进程、另一个控制台，宿主不能把自己的 stdout 分给它；
//! 文件同时留下现场（窗口崩溃或用户提前关掉后仍可回看），成本只是一次 `create_dir_all`。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// 窗口进程等待新行的轮询间隔：流水按行追加，20ms 足够让文字看起来是流式的。
const FOLLOW_TICK: Duration = Duration::from_millis(20);

/// 窗口进程的启动开关（`omnicrawl-tui --subagent-window <流水文件>`）。
pub const WINDOW_FLAG: &str = "--subagent-window";

/// 一次子任务在窗口里的进度。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskLine {
    task_id: String,
    agent_type: String,
    description: String,
    status: String,
}

impl TaskLine {
    fn is_terminal(&self) -> bool {
        crate::state::subagent_status_is_terminal(&self.status)
    }
}

/// 一条已经落进窗口正文的流水事件（窗口进程按到达顺序渲染）。
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Text(String),
    ToolStarted {
        call_id: String,
        tool: String,
        arguments: serde_json::Value,
    },
    ToolCompleted {
        call_id: String,
        ok: bool,
        output: String,
        seconds: Option<f64>,
    },
    TaskStatus {
        task_id: String,
        agent_type: String,
        description: String,
        status: String,
    },
}

/// 一次投递的结论：主会话据此决定要不要写那行提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// 该批次第一次开窗口（提示里带上流水路径）。
    Opened { stream: PathBuf },
    /// 已开过窗口，事件已追加。
    Appended,
    /// 这个事件不进窗口。
    Ignored,
}

/// 主宿主侧的子代理窗口管理：按批次建窗口、记账、投递事件。
#[derive(Default)]
pub struct SubagentWindows {
    batches: HashMap<String, PathBuf>,
}

impl SubagentWindows {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一条子代理事件交给对应批次的窗口。
    ///
    /// `root` 是流水落盘目录；`exe` 是要拉起的窗口可执行文件。两者都由调用方给：测试指到
    /// 临时目录与假可执行文件，生产走用户配置目录与 `current_exe()`。
    pub fn deliver(
        &mut self,
        root: &Path,
        exe: Option<&Path>,
        name: &str,
        payload: &serde_json::Value,
    ) -> Delivery {
        let Some(entry) = entry_from_event(name, payload) else {
            return Delivery::Ignored;
        };
        let batch_id = payload_text(payload, "batch_id").unwrap_or_else(|| "batch".to_string());
        let opened = !self.batches.contains_key(&batch_id);
        if opened {
            let stream = stream_path(root, &batch_id);
            // 同一批次号在重启后复用（概率极低）：旧文件截断，免得窗口一开始就显示旧记录。
            let _ = std::fs::create_dir_all(root);
            match File::create(&stream) {
                Ok(file) => drop(file),
                Err(_) => return Delivery::Ignored,
            }
            self.batches.insert(batch_id.clone(), stream.clone());
            spawn_window(exe, &stream);
        }
        let Some(stream) = self.batches.get(&batch_id) else {
            return Delivery::Ignored;
        };
        if append_entry(stream, &entry).is_err() {
            return Delivery::Ignored;
        }
        if opened {
            Delivery::Opened {
                stream: stream.clone(),
            }
        } else {
            Delivery::Appended
        }
    }

    /// 该批次是否开过窗口。
    pub fn has_window(&self, batch_id: &str) -> bool {
        self.batches.contains_key(batch_id)
    }
}

fn payload_text(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 事件 → 窗口正文条目；子代理对话之外的批次事件与未知事件不进窗口。
fn entry_from_event(name: &str, payload: &serde_json::Value) -> Option<Entry> {
    let text = |key: &str| payload_text(payload, key).unwrap_or_default();
    match name {
        "subagent.turn.text" => {
            let body = text("text");
            (!body.trim().is_empty()).then_some(Entry::Text(body))
        }
        "subagent.tool.started" => Some(Entry::ToolStarted {
            call_id: text("call_id"),
            tool: text("tool"),
            arguments: payload
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        "subagent.tool.completed" => Some(Entry::ToolCompleted {
            call_id: text("call_id"),
            ok: payload
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: text("output"),
            seconds: payload
                .get("duration_seconds")
                .and_then(serde_json::Value::as_f64),
        }),
        _ => {
            let status = status_for_event(name)?;
            let task_id = text("task_id");
            Some(Entry::TaskStatus {
                task_id: if task_id.is_empty() {
                    "task".to_string()
                } else {
                    task_id
                },
                agent_type: text("agent_type"),
                description: text("description"),
                status: status.to_string(),
            })
        }
    }
}

/// 事件名 → 状态词。
fn status_for_event(name: &str) -> Option<&'static str> {
    match name {
        "subagent.task.queued" => Some("queued"),
        "subagent.task.started" | "subagent.task.running" => Some("running"),
        "subagent.task.waiting_approval" => Some("waiting_approval"),
        "subagent.task.completed" => Some("completed"),
        "subagent.task.failed" => Some("failed"),
        "subagent.task.cancelled" | "subagent.task.approval_cancelled" => Some("cancelled"),
        _ => None,
    }
}

/// 流水文件路径：`<root>/<batch_id>.jsonl`。
fn stream_path(root: &Path, batch_id: &str) -> PathBuf {
    root.join(format!("{}.jsonl", safe_file_stem(batch_id)))
}

/// 批次号只用来命名文件：把路径分隔符与保留字符换掉，避免写到目录外。
fn safe_file_stem(batch_id: &str) -> String {
    let cleaned: String = batch_id
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other if other.is_control() => '_',
            other => other,
        })
        .take(96)
        .collect();
    if cleaned.trim().is_empty() {
        "batch".to_string()
    } else {
        cleaned
    }
}

/// 追加一行流水。
fn append_entry(path: &Path, entry: &Entry) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(entry_to_json(entry).to_string().as_bytes())?;
    file.write_all(b"\n")
}

/// 条目 → 一行 JSON（窗口进程按 `kind` 分支）。
fn entry_to_json(entry: &Entry) -> serde_json::Value {
    match entry {
        Entry::Text(text) => serde_json::json!({ "kind": "text", "text": text }),
        Entry::ToolStarted {
            call_id,
            tool,
            arguments,
        } => serde_json::json!({
            "kind": "tool.started",
            "call_id": call_id,
            "tool": tool,
            "arguments": arguments,
        }),
        Entry::ToolCompleted {
            call_id,
            ok,
            output,
            seconds,
        } => serde_json::json!({
            "kind": "tool.completed",
            "call_id": call_id,
            "ok": ok,
            "output": output,
            "duration_seconds": seconds,
        }),
        Entry::TaskStatus {
            task_id,
            agent_type,
            description,
            status,
        } => serde_json::json!({
            "kind": "task",
            "task_id": task_id,
            "agent_type": agent_type,
            "description": description,
            "status": status,
        }),
    }
}

/// 一行 JSON → 条目（形状不符返回 `None`，窗口跳过该行）。
fn entry_from_json(line: &str) -> Option<Entry> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match text("kind").as_str() {
        "text" => Some(Entry::Text(text("text"))),
        "tool.started" => Some(Entry::ToolStarted {
            call_id: text("call_id"),
            tool: text("tool"),
            arguments: value
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        "tool.completed" => Some(Entry::ToolCompleted {
            call_id: text("call_id"),
            ok: value
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            output: text("output"),
            seconds: value
                .get("duration_seconds")
                .and_then(serde_json::Value::as_f64),
        }),
        "task" => Some(Entry::TaskStatus {
            task_id: text("task_id"),
            agent_type: text("agent_type"),
            description: text("description"),
            status: text("status"),
        }),
        _ => None,
    }
}

/// 在新终端窗口里拉起窗口进程；拿不到可执行文件或平台不支持时静默跳过（流水仍在）。
///
/// 放到后台线程：`CreateProcess` 在冷启动时要几十到上百毫秒，而这条路径挂在界面线程的
/// 事件处理里，阻塞它会直接让开窗那一刻掉帧。窗口进程本身会等流水文件出现，所以先后无关。
fn spawn_window(exe: Option<&Path>, stream: &Path) {
    let Some(exe) = exe.map(Path::to_path_buf) else {
        return;
    };
    let stream = stream.to_path_buf();
    std::thread::spawn(move || {
        let mut command = Command::new(&exe);
        command.arg(WINDOW_FLAG).arg(&stream);
        #[cfg(windows)]
        {
            // 三个标准句柄一律**不重定向**：`CREATE_NEW_CONSOLE` 给子进程分配的新控制台
            // 就挂在它的 stdout/stderr 上，一旦显式塞进 `Stdio::null()`，窗口进程会把整屏
            // 内容画进 NUL 设备——窗口开出来了，却永远是一块空白。
            command.stdin(Stdio::null());
            // 新控制台起不来（受限桌面会话）时不回退到「无窗口运行」：那会得到一个收了
            // 输出却不显示的进程。流水文件仍在，用户可自己 `--subagent-window` 打开。
            let _ = platform::spawn_in_new_console(&mut command);
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 没有「新建控制台」语义：直接后台跑，用户在支持多窗口的终端里
            // 自己 `--subagent-window <文件>` 打开（流水与渲染是同一套）。
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = command.spawn();
        }
    });
}

#[cfg(windows)]
mod platform {
    use std::process::Command;

    /// `CREATE_NEW_CONSOLE`：让子进程独占一个新控制台窗口。
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    /// 从宿主所在的 Job 中脱离：窗口进程不随宿主退出被回收。
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// 在新控制台窗口里拉起；父 Job 不允许 breakaway 时退回不带它的一次尝试。
    pub fn spawn_in_new_console(command: &mut Command) -> bool {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_CONSOLE | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_ok() {
            return true;
        }
        command.creation_flags(CREATE_NEW_CONSOLE);
        command.spawn().is_ok()
    }
}

/// 窗口进程主体：尾随流水文件并按主代理的版式渲染，直到窗口关闭（写不出去即结束）。
pub fn run_window(stream: PathBuf) -> Result<(), String> {
    let mut terminal = WindowTerminal::start()?;
    let mut rendered = Rendered::default();
    let mut state = crate::state::AppState::new(
        "子代理".to_string(),
        String::new(),
        crate::args::ApprovalMode::Auto,
    );
    write_update(&mut terminal, &mut state, &stream, &rendered)?;
    let mut file = wait_for_stream(&stream)?;
    let mut offset: u64 = 0;
    let mut pending = String::new();
    loop {
        // 流水被重建（同一批次号复用）时文件会变短：读位置超出长度就从头重来，
        // 否则窗口会永远停在旧内容上。
        if file.metadata().map(|meta| meta.len()).unwrap_or(0) < offset {
            offset = 0;
            pending.clear();
            rendered = Rendered::default();
            state = crate::state::AppState::new(
                "子代理".to_string(),
                String::new(),
                crate::args::ApprovalMode::Auto,
            );
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return Ok(());
        }
        let mut reader = BufReader::new(&mut file);
        let mut read_any = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(count) => {
                    offset += count as u64;
                    read_any = true;
                    // 写入方可能把一行拆成多次写：凑齐换行才当作完整事件。
                    if !line.ends_with('\n') {
                        pending.push_str(&line);
                        continue;
                    }
                    let completed = if pending.is_empty() {
                        line
                    } else {
                        let mut joined = std::mem::take(&mut pending);
                        joined.push_str(&line);
                        joined
                    };
                    consume(&mut rendered, &mut state, &completed);
                }
                Err(_) => return Ok(()),
            }
        }
        if read_any {
            write_update(&mut terminal, &mut state, &stream, &rendered)?;
            continue;
        }
        // 读到末尾：等下一批追加（宿主仍在跑任务，窗口保持跟随）。
        std::thread::sleep(FOLLOW_TICK);
    }
}

/// 等流水文件出现；读取权限一直拿不到时报错退出（窗口不该无限空转）。
fn wait_for_stream(stream: &Path) -> Result<File, String> {
    for _ in 0..1500 {
        match File::open(stream) {
            Ok(file) => return Ok(file),
            Err(_) => std::thread::sleep(FOLLOW_TICK),
        }
    }
    Err(format!("打不开子代理流水：{}", stream.display()))
}

/// 渲染状态：已收下的任务进度。
#[derive(Default)]
struct Rendered {
    tasks: Vec<TaskLine>,
    /// 已经写过终态提示的任务，避免同一终态被重复上报时刷屏。
    settled: std::collections::HashSet<String>,
}

/// 一行流水 → 渲染增量：同时推进窗口的任务表与 `AppState` 的会话记录。
fn consume(rendered: &mut Rendered, state: &mut crate::state::AppState, line: &str) {
    let Some(entry) = entry_from_json(line.trim()) else {
        return;
    };
    match entry {
        Entry::TaskStatus {
            task_id,
            agent_type,
            description,
            status,
        } => {
            let task_key = if description.is_empty() {
                task_id.clone()
            } else {
                description.clone()
            };
            let line = TaskLine {
                agent_type: if agent_type.is_empty() {
                    "subagent".to_string()
                } else {
                    agent_type
                },
                description: task_key,
                task_id: task_id.clone(),
                status,
            };
            match rendered
                .tasks
                .iter_mut()
                .find(|task| task.task_id == line.task_id)
            {
                // 终态不被迟到的活动事件回退（与旧进度树同一条规则）。
                Some(existing) if !existing.is_terminal() => *existing = line,
                Some(_) => {}
                None => rendered.tasks.push(line),
            }
            // 任务条目与主代理的消息流提示同一措辞（`· ` 灰行）。只在任务落定时补一条：
            // 运行中的每一步都补会让窗口被状态噪声刷满，进度已经写在状态行里。
            let settled = rendered
                .tasks
                .iter()
                .find(|task| task.task_id == task_id && task.is_terminal())
                .map(|task| {
                    (
                        task.task_id.clone(),
                        task.agent_type.clone(),
                        task.status.clone(),
                    )
                });
            if let Some((task_id, agent_type, status)) = settled {
                if rendered.settled.insert(task_id.clone()) {
                    state.notice(format!(
                        "子任务 {task_id} · {agent_type} · {}",
                        status_label(&status)
                    ));
                }
            }
            state.set_subagent_running(!rendered.tasks.iter().all(TaskLine::is_terminal));
        }
        Entry::Text(text) => {
            state.subagent_text(text);
        }
        Entry::ToolStarted {
            call_id,
            tool,
            arguments,
        } => {
            state.subagent_tool_started(call_id, tool, arguments);
        }
        Entry::ToolCompleted {
            call_id,
            ok,
            output,
            seconds,
        } => {
            state.subagent_tool_completed(&call_id, ok, &output, seconds);
        }
    }
}

/// 整屏重画：交给主代理那套渲染（会话区 + 输入区 + 底部 HUD）。
fn write_update(
    terminal: &mut WindowTerminal,
    state: &mut crate::state::AppState,
    stream: &Path,
    rendered: &Rendered,
) -> Result<(), String> {
    // 顶部标题行之外的语义全在主页面里：窗口只把「这是哪个批次」补进状态行，
    // 其余版式与会话区构成与主代理逐块一致。文本没变时不重写，免得每帧把输入块置脏。
    let title = window_title(stream, rendered);
    if state.status.as_deref() != Some(title.as_str()) {
        state.set_status(Some(title));
    }
    terminal
        .draw(|frame| crate::ui::render(frame, state, None))
        .map_err(|error| format!("写窗口内容失败：{error}"))
}

/// 状态行文本：批次标题 + 任务进度。
fn window_title(stream: &Path, rendered: &Rendered) -> String {
    let name = stream
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "batch".to_string());
    let total = rendered.tasks.len();
    let done = rendered
        .tasks
        .iter()
        .filter(|task| task.is_terminal())
        .count();
    if total == 0 {
        return format!("子代理 · {name}");
    }
    format!("子代理 · {name} · {done}/{total}")
}

/// 状态 → 中文状态词（窗口里的任务提示行用它）。
fn status_label(status: &str) -> &'static str {
    match status {
        "queued" => "等待中",
        "running" => "运行中",
        "waiting_approval" => "等待审批",
        "completed" => "完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        _ => "进行中",
    }
}

/// 窗口终端：进入备用屏幕与原始模式，退出时无条件恢复原状。
///
/// 与 `main.rs` 里的 `TerminalGuard` 同一职责，但窗口进程只做渲染、不接输入，
/// 因此不需要鼠标/焦点报告与自愈核对。
struct WindowTerminal {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl WindowTerminal {
    fn start() -> Result<Self, String> {
        use crossterm::terminal::{
            disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        };
        enable_raw_mode().map_err(|error| format!("进入原始模式失败：{error}"))?;
        let mut out = std::io::stdout();
        if let Err(error) = crossterm::execute!(out, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(format!("切换备用屏幕失败：{error}"));
        }
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        match ratatui::Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(format!("初始化终端失败：{error}"))
            }
        }
    }

    fn draw<F>(&mut self, render: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal.draw(render).map(|_| ())
    }
}

impl Drop for WindowTerminal {
    fn drop(&mut self) {
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Record, ToolStatus};

    fn payload(pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        serde_json::Value::Object(map)
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("oc-subwin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn each_event_appends_one_jsonl_line() {
        let root = temp_root("lines");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[
            ("batch_id", serde_json::json!("batch-1")),
            ("task_id", serde_json::json!("t1")),
            ("agent_type", serde_json::json!("explore")),
            ("description", serde_json::json!("查仓库")),
        ]);

        let first = windows.deliver(&root, None, "subagent.task.started", &batch);
        let Delivery::Opened { stream } = first else {
            panic!("首个事件应当开窗口：{first:?}");
        };
        assert_eq!(
            windows.deliver(&root, None, "subagent.task.running", &batch),
            Delivery::Appended
        );

        let text = std::fs::read_to_string(&stream).expect("流水文件可读");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(
            entry_from_json(lines[0]),
            Some(Entry::TaskStatus {
                task_id: "t1".to_string(),
                agent_type: "explore".to_string(),
                description: "查仓库".to_string(),
                status: "running".to_string(),
            })
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_events_do_not_open_a_window() {
        let root = temp_root("none");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[("batch_id", serde_json::json!("batch-x"))]);
        assert_eq!(
            windows.deliver(&root, None, "subagent.batch.created", &batch),
            Delivery::Ignored
        );
        assert!(!windows.has_window("batch-x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_id_never_escapes_the_root() {
        assert_eq!(safe_file_stem("../a/b"), ".._a_b");
        assert_eq!(safe_file_stem("   "), "batch");
    }

    #[test]
    fn terminal_status_wins_over_late_activity() {
        let mut rendered = Rendered::default();
        let mut state = window_state();
        for status in ["completed", "running"] {
            consume(
                &mut rendered,
                &mut state,
                &serde_json::json!({
                    "kind": "task", "task_id": "t1", "agent_type": "explore",
                    "description": "查仓库", "status": status,
                })
                .to_string(),
            );
        }
        assert_eq!(rendered.tasks[0].status, "completed");
    }

    /// 窗口进程用的空白界面状态：只放会话记录，不连内核。
    fn window_state() -> crate::state::AppState {
        crate::state::AppState::new(
            "子代理".to_string(),
            String::new(),
            crate::args::ApprovalMode::Auto,
        )
    }

    #[test]
    fn events_build_the_same_records_as_the_main_conversation() {
        let mut rendered = Rendered::default();
        let mut state = window_state();
        for line in [
            serde_json::json!({"kind": "task", "task_id": "t1", "agent_type": "explore",
                               "description": "查仓库", "status": "running"}),
            serde_json::json!({"kind": "text", "text": "先看一眼"}),
            serde_json::json!({"kind": "tool.started", "call_id": "c1", "tool": "read",
                               "arguments": {"path": "a.rs"}}),
            serde_json::json!({"kind": "tool.completed", "call_id": "c1", "ok": true,
                               "output": "ok", "duration_seconds": 0.5}),
        ] {
            consume(&mut rendered, &mut state, &line.to_string());
        }

        // 过程文本 → assistant 记录；工具 → 主代理同款的工具卡（含收口后的状态）。
        assert!(matches!(state.records[0], Record::Assistant(ref text) if text == "先看一眼"));
        let Record::Tool(ref card) = state.records[1] else {
            panic!("第二条应是工具卡：{:?}", state.records[1]);
        };
        assert_eq!(card.name, "read");
        assert_eq!(card.status, ToolStatus::Ok);
        assert_eq!(card.body, vec!["ok".to_string()]);
        // 还有任务在跑：状态行会显示运行态（主代理据此画 spinner 与 `[ ESC ]`）。
        assert!(state.turn.is_running());
    }
}
