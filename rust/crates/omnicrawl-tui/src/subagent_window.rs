//! 子代理独立窗口：把 `subagent.tool.*` / `subagent.turn.*` 转成一条可跟随的
//! JSONL 流水，并在**新终端窗口**里渲染。主会话不再画进度树，只留一行受理提示。
//!
//! 分两半：
//!
//! - **主宿主**（[`SubagentWindows`]）：收到子代理事件就按 `batch_id` 记账。批次第一次出现时
//!   建流水文件并用 `CREATE_NEW_CONSOLE` 拉起一个 `omnicrawl-tui --subagent-window <文件>`
//!   子进程；之后每个事件追加一行。主会话只保留「已开窗口」这一份状态，不参与渲染。
//! - **窗口进程**（[`run_window`]）：尾随流水文件，把事件重建成 `AppState` 的会话记录，
//!   再用**主代理那套** `ui::render` 渲染整页（会话区 + 输入区方框 + 底部 HUD），
//!   并自己跑一帧一帧的动画、键盘/滚轮滚动与点击展开。批次收尾（`subagent.batch.finished`）
//!   后窗口自动关闭；用户也可以随时按 Esc 关掉，任务不受影响。
//!
//! 为什么用文件而不是管道：窗口是另一个进程、另一个控制台，宿主不能把自己的 stdout 分给它；
//! 文件同时留下现场（窗口崩溃或用户提前关掉后仍可回看），成本只是一次 `create_dir_all`。

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};

/// 窗口进程等待新行的轮询间隔：流水按行追加，20ms 足够让文字看起来是流式的。
const FOLLOW_TICK: Duration = Duration::from_millis(20);

/// 窗口进程的启动开关（`omnicrawl-tui --subagent-window <流水文件>`）。
pub const WINDOW_FLAG: &str = "--subagent-window";
/// 关掉「批次收尾后自动关闭」：按需点开的窗口要留到用户自己关。
pub const KEEP_OPEN_FLAG: &str = "--keep-open";

/// 滚轮一格滚动的行数（与主代理同值）。
const WHEEL_STEP: isize = 3;
/// 方向键与翻页键的步长（与主代理同值）。
const SCROLL_STEP: isize = 1;
const PAGE_STEP: isize = 10;

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
    Reasoning(String),
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
    /// 整个批次收尾：窗口据此自动关闭。
    BatchFinished,
}

/// 一次投递的结论：主会话据此决定要不要写那行提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// 宿主自发批次（`/review`）的窗口已自动打开（提示里带上流水路径）。
    Opened { stream: PathBuf },
    /// 事件已追加进流水。
    Appended,
    /// 这个事件不进窗口。
    Ignored,
}

/// 主宿主侧的子代理窗口管理：按批次记账、投递事件，**只在用户点击时**开窗。
#[derive(Default)]
pub struct SubagentWindows {
    batches: HashMap<String, PathBuf>,
    /// 发起批次的工具调用 id → 批次号：工具卡据此决定要不要挂「查看会话」按钮。
    calls: HashMap<String, String>,
    /// 宿主自发批次自动开的窗口：同批次不再自动开第二个。
    opened: HashSet<String>,
    /// 已经收尾的批次：窗口已自动关闭，工具卡上的「查看会话」随之撤下。
    finished: HashSet<String>,
    /// 用户点开的窗口进程：只要它还活着，再点就不开第二个；它退出（用户关窗）后
    /// 这条登记随之失效，按钮可以重新开窗。
    windows: HashMap<String, Child>,
}

impl SubagentWindows {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一条子代理事件交给对应批次的流水。
    ///
    /// `root` 是流水落盘目录；`exe` 是要拉起的窗口可执行文件。两者都由调用方给：测试指到
    /// 临时目录与假可执行文件，生产走用户配置目录与 `current_exe()`。
    ///
    /// 带 `call_id` 的批次（模型自己调 `subagent` / `computer_use` 起的）**不**开窗，
    /// 只在工具卡后面留一个「查看会话」入口；不带 `call_id` 的是宿主自发入口（`/review`），
    /// 用户主动下了这条命令，照旧直接开窗。
    pub fn deliver(
        &mut self,
        root: &Path,
        exe: Option<&Path>,
        name: &str,
        payload: &serde_json::Value,
    ) -> Delivery {
        let batch_id = payload_text(payload, "batch_id").unwrap_or_else(|| "batch".to_string());
        let entry = entry_from_event(name, payload);
        // 收尾事件只往**已经登记**的流水里写：批次没有窗口（全部任务在准入阶段就被拒）
        // 时不该为了关窗专门建一个文件。
        if matches!(entry, Some(Entry::BatchFinished)) {
            let Some(stream) = self.batches.get(&batch_id) else {
                return Delivery::Ignored;
            };
            self.finished.insert(batch_id.clone());
            return match append_entry(stream, &Entry::BatchFinished) {
                Ok(()) => Delivery::Appended,
                Err(_) => Delivery::Ignored,
            };
        }
        let mut auto_opened = None;
        if !self.batches.contains_key(&batch_id) {
            let call_id = payload_text(payload, "origin_call_id");
            // 既不是可渲染事件、又不是工具调用发起的批次：不建流水（免得点出来一个空窗口）。
            if entry.is_none() && call_id.is_none() {
                return Delivery::Ignored;
            }
            let stream = stream_path(root, &batch_id);
            // 同一批次号在重启后复用（概率极低）：旧文件截断，免得窗口一开始就显示旧记录。
            let _ = std::fs::create_dir_all(root);
            if File::create(&stream).is_err() {
                return Delivery::Ignored;
            }
            self.batches.insert(batch_id.clone(), stream.clone());
            match call_id {
                Some(call_id) => {
                    self.calls.insert(call_id, batch_id.clone());
                }
                None => {
                    self.opened.insert(batch_id.clone());
                    // 宿主自发窗口不挂按钮（没有工具卡），句柄只用来回收进程。
                    let _ = spawn_window(exe, &stream, false);
                    auto_opened = Some(stream);
                }
            }
        }
        let Some(stream) = self.batches.get(&batch_id) else {
            return Delivery::Ignored;
        };
        if let Some(entry) = entry {
            if append_entry(stream, &entry).is_err() {
                return Delivery::Ignored;
            }
        }
        match auto_opened {
            Some(stream) => Delivery::Opened { stream },
            None => Delivery::Appended,
        }
    }

    /// 宿主是否已经为该批次自动开过窗口（`/review` 那类入口，同批次只开一次）。
    pub fn is_opened(&self, batch_id: &str) -> bool {
        self.opened.contains(batch_id)
    }

    /// 该批次的点开窗口是否还在显示：窗口被用户关掉后即可重新打开。
    fn window_running(&mut self, batch_id: &str) -> bool {
        let exited = match self.windows.get_mut(batch_id) {
            // 进程还在跑：窗口开着。
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => return false,
        };
        // 进程已经退出（用户关窗、或窗口自己崩溃）：登记随之失效。
        if exited {
            self.windows.remove(batch_id);
        }
        !exited
    }

    /// 某次工具调用派发的批次号：工具卡据此决定要不要挂「查看会话」。
    ///
    /// 批次收尾后返回 `None`：窗口进程收到收尾事件会自己关掉，按钮留着只会点出一个
    /// 立刻退出的空窗口。
    pub fn view_batch(&self, call_id: &str) -> Option<&str> {
        let batch = self.calls.get(call_id)?;
        (!self.finished.contains(batch)).then_some(batch.as_str())
    }

    /// 某个批次由哪次工具调用发起（收尾时要让那张卡重算，把「查看会话」撤下）。
    pub fn call_of(&self, batch_id: &str) -> Option<&str> {
        self.calls
            .iter()
            .find(|(_, batch)| batch.as_str() == batch_id)
            .map(|(call, _)| call.as_str())
    }

    /// 用户点了「查看会话」：该批次没有在显示的窗口就开一个。
    ///
    /// 返回 `false` 表示没有该批次的流水、拿不到可执行文件，或这个批次的窗口还开着。
    /// 窗口被用户关掉后登记自动失效，按钮可以重新开窗——不然一次关窗就把入口彻底作废。
    ///
    /// 点开的窗口**不随批次结束自动关闭**：用户是特地来读这段过程的，读着读着窗口
    /// 自己消失只会让人以为出错；关窗交给 Esc / Ctrl+Q。
    pub fn open(&mut self, exe: Option<&Path>, batch_id: &str) -> bool {
        let Some(exe) = exe else {
            return false;
        };
        if !self.can_open(batch_id) {
            return false;
        }
        let Some(stream) = self.batches.get(batch_id).cloned() else {
            return false;
        };
        match spawn_window(Some(exe), &stream, true) {
            Some(child) => {
                self.windows.insert(batch_id.to_string(), child);
                true
            }
            None => false,
        }
    }

    /// 此刻能不能给该批次开窗：流水已登记，且它的窗口没有开着。
    fn can_open(&mut self, batch_id: &str) -> bool {
        self.batches.contains_key(batch_id) && !self.window_running(batch_id)
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
        "subagent.turn.reasoning" => {
            let body = text("text");
            (!body.trim().is_empty()).then_some(Entry::Reasoning(body))
        }
        "subagent.batch.finished" => Some(Entry::BatchFinished),
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
        Entry::Reasoning(text) => serde_json::json!({ "kind": "reasoning", "text": text }),
        Entry::BatchFinished => serde_json::json!({ "kind": "batch.finished" }),
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
        "reasoning" => Some(Entry::Reasoning(text("text"))),
        "batch.finished" => Some(Entry::BatchFinished),
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
/// 同步 spawn 是为了拿到进程句柄：窗口被用户关掉后要能看出来，按钮才不会一次点击就
/// 永久作废。代价是一次 `CreateProcess`（冷启动几十到上百毫秒），只在开窗那一刻付一次。
fn spawn_window(exe: Option<&Path>, stream: &Path, keep_open: bool) -> Option<Child> {
    let exe = exe?;
    let mut command = Command::new(exe);
    command.arg(WINDOW_FLAG).arg(stream);
    if keep_open {
        command.arg(KEEP_OPEN_FLAG);
    }
    #[cfg(windows)]
    {
        // stdout/stderr 一律**不重定向**：`CREATE_NEW_CONSOLE` 给子进程分配的新控制台
        // 就挂在它的 stdout/stderr 上，一旦显式塞进 `Stdio::null()`，窗口进程会把整屏
        // 内容画进 NUL 设备——窗口开出来了，却永远是一块空白。
        //
        // stdin 置空不影响键盘：crossterm 在 Windows 上不走 stdin，而是自己
        // `CreateFileW("CONIN$")` 打开**当前控制台**的输入句柄（事件轮询与原始模式
        // 都走它），新控制台的输入队列因此照旧可读。
        command.stdin(Stdio::null());
        // 新控制台起不来（受限桌面会话）时不回退到「无窗口运行」：那会得到一个收了
        // 输出却不显示的进程。流水文件仍在，用户可自己 `--subagent-window` 打开。
        platform::spawn_in_new_console(&mut command)
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 没有「新建控制台」语义：直接后台跑，用户在支持多窗口的终端里
        // 自己 `--subagent-window <文件>` 打开（流水与渲染是同一套）。
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().ok()
    }
}

#[cfg(windows)]
mod platform {
    use std::process::Command;

    /// `CREATE_NEW_CONSOLE`：让子进程独占一个新控制台窗口。
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    /// 从宿主所在的 Job 中脱离：窗口进程不随宿主退出被回收。
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// 在新控制台窗口里拉起；父 Job 不允许 breakaway 时退回不带它的一次尝试。
    pub fn spawn_in_new_console(command: &mut Command) -> Option<std::process::Child> {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NEW_CONSOLE | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = command.spawn() {
            return Some(child);
        }
        command.creation_flags(CREATE_NEW_CONSOLE);
        command.spawn().ok()
    }
}

/// 窗口进程主体：尾随流水文件并按主代理的版式渲染。
///
/// 窗口自己跑事件循环：流水有新行就重画，空闲时按 [`FOLLOW_TICK`] 醒来同时推进动画
/// （spinner、思考段铺开、图片解码、底部轮播）。Esc / Ctrl+Q 随时关窗；批次收尾
/// （[`Entry::BatchFinished`]）后画完最后一帧自动退出——除非是用户点开的窗口
/// （`keep_open`），那种留到用户自己关。
pub fn run_window(stream: PathBuf, keep_open: bool) -> Result<(), String> {
    let mut terminal = WindowTerminal::start()?;
    // 窗口按 20ms 轮询：Windows 默认计时器粒度约 15.6ms，不提升精度会让「睡 20ms」
    // 变成 31ms，动画与流式文字都会一顿一顿。随进程退出恢复。
    let _timer = crate::ui::fullscreen::terminal::timer_resolution::TimerResolutionGuard::acquire();
    let mut rendered = Rendered::default();
    let mut state = window_state();
    draw(&mut terminal, &mut state, &stream, &rendered, keep_open)?;
    // `draw` 里写过状态行（会置脏输入块）：版本号在首帧之后取，才对得上屏幕上的内容。
    let mut drawn_versions = state.refresh_versions();
    let mut dirty = false;
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
            state = window_state();
            dirty = true;
        }
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return Ok(());
        }
        let mut reader = BufReader::new(&mut file);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(count) => {
                    offset += count as u64;
                    dirty = true;
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

        let now = Instant::now();
        state.tick_activity();
        let _ = state.tick_reasoning_reveal();
        let _ = state.tick_image_previews();
        state.refresh_carousel(now, "");
        state.tick_notice_line(now);

        let versions = state.refresh_versions();
        if dirty || versions != drawn_versions {
            draw(&mut terminal, &mut state, &stream, &rendered, keep_open)?;
            drawn_versions = versions;
            dirty = false;
        }
        // 批次收尾：最后一帧已经画完，窗口自动关闭（流水留在磁盘上可回看）。
        // 用户点开的窗口例外：它是来读过程的，不能读着读着自己消失。
        if rendered.finished && !keep_open {
            return Ok(());
        }

        if !event::poll(FOLLOW_TICK).map_err(|error| format!("读取终端事件失败：{error}"))? {
            continue;
        }
        let event = event::read().map_err(|error| format!("读取终端事件失败：{error}"))?;
        if handle_event(&mut state, &mut terminal, event)? {
            return Ok(());
        }
        dirty = true;
    }
}

/// 窗口进程的空界面状态：只放会话记录，不连内核。
fn window_state() -> crate::state::AppState {
    let mut state = crate::state::AppState::new(
        "子代理".to_string(),
        String::new(),
        crate::args::ApprovalMode::Auto,
    );
    // 首帧先占一条记录：空记录会画主代理的欢迎 Logo（窗口里那是错的内容）。
    state.notice("等待子代理事件".to_string());
    state
}

/// 处理一个终端事件；返回 `true` 表示用户要关窗。
fn handle_event(
    state: &mut crate::state::AppState,
    terminal: &mut WindowTerminal,
    event: Event,
) -> Result<bool, String> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => return Ok(true),
                KeyCode::Char('q') | KeyCode::Char('Q') if ctrl => return Ok(true),
                KeyCode::Up => state.scroll_by(-SCROLL_STEP),
                KeyCode::Down => state.scroll_by(SCROLL_STEP),
                KeyCode::PageUp => state.scroll_by(-PAGE_STEP),
                KeyCode::PageDown => state.scroll_by(PAGE_STEP),
                KeyCode::Home => scroll_to_top(state, terminal)?,
                KeyCode::End => state.scroll_to_bottom(),
                // 其余按键一律吞掉：窗口只读，不接受输入，不该把按键变成草稿文本。
                _ => {}
            }
        }
        Event::Mouse(mouse) => {
            // 右缘滚动条拖动优先：按住后上下移动就是滚会话，与主代理同一条路径。
            if drag_scrollbar(state, terminal, &mouse)? {
                return Ok(false);
            }
            match mouse.kind {
                MouseEventKind::ScrollUp => state.scroll_by(-WHEEL_STEP),
                MouseEventKind::ScrollDown => state.scroll_by(WHEEL_STEP),
                MouseEventKind::Down(MouseButton::Left) => {
                    let viewport = terminal.viewport()?;
                    click(state, viewport, mouse.column, mouse.row);
                }
                _ => {}
            }
        }
        // 尺寸变化由下一帧的 `frame.area()` 自然接手，这里只需重画。
        Event::Resize(..) => {}
        _ => {}
    }
    Ok(false)
}

/// 滚动条拖动：命中返回 `true`（这一下不再当点选处理）。
///
/// 拖动状态存在 [`WindowTerminal`] 上：窗口只渲染一块会话区，没有别的交互需要它与
/// 主代理的 `scrollbar_drag` 一样的生命周期。
fn drag_scrollbar(
    state: &mut crate::state::AppState,
    terminal: &mut WindowTerminal,
    mouse: &crossterm::event::MouseEvent,
) -> Result<bool, String> {
    let dragging = terminal.scrollbar_drag;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let viewport = terminal.viewport()?;
            let area = crate::ui::layout(viewport, state).conversation;
            if crate::ui::conversation::scrollbar_column(area) != Some(mouse.column) {
                return Ok(false);
            }
            terminal.scrollbar_drag = true;
            drag_to(state, area, mouse.row);
            Ok(true)
        }
        MouseEventKind::Drag(MouseButton::Left) if dragging => {
            let viewport = terminal.viewport()?;
            let area = crate::ui::layout(viewport, state).conversation;
            drag_to(state, area, mouse.row);
            Ok(true)
        }
        MouseEventKind::Up(MouseButton::Left) if dragging => {
            terminal.scrollbar_drag = false;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// 把会话区滚动到鼠标所在行对应的偏移。
fn drag_to(state: &mut crate::state::AppState, area: ratatui::layout::Rect, row: u16) {
    let Some(offset) = crate::ui::conversation::scroll_offset_for_row(state, area, row) else {
        return;
    };
    // 走 `scroll_by` 的差值而不是直接写字段：会话块要置脏，否则拖动后画面不跟着动。
    let delta = offset as isize - state.scroll_from_bottom as isize;
    state.scroll_by(delta);
}

/// 跳到最顶：`scroll_from_bottom` 的上限是「总行数 − 可见行数」。
///
/// 会话区的显示行数只有这里需要（渲染与命中测试各自按需算），因此按当前宽度数一次。
fn scroll_to_top(
    state: &mut crate::state::AppState,
    terminal: &WindowTerminal,
) -> Result<(), String> {
    let viewport = terminal.viewport()?;
    let area = crate::ui::layout(viewport, state).conversation;
    let text = crate::ui::conversation::text_area(area);
    let total = crate::ui::conversation::display_line_count(state, text.width);
    state.scroll_by(-(total as isize));
    Ok(())
}

/// 左键点击会话区：与主代理同一套命中测试，展开/收起思考段与工具卡。
fn click(state: &mut crate::state::AppState, viewport: ratatui::layout::Rect, column: u16, row: u16) {
    let areas = crate::ui::layout(viewport, state);
    if !areas.conversation.contains(ratatui::layout::Position::new(column, row)) {
        return;
    }
    let relative = (row - areas.conversation.y) as usize;
    match crate::ui::conversation::hit_test(
        state,
        areas.conversation,
        state.scroll_from_bottom,
        relative,
    ) {
        Some(crate::ui::conversation::LineHit::ToolHint { call_id }) => state.expand_tool(&call_id),
        Some(crate::ui::conversation::LineHit::ToolCard { call_id }) => {
            state.collapse_tool(&call_id);
        }
        Some(crate::ui::conversation::LineHit::Reasoning { index }) => {
            state.toggle_reasoning_expanded(index);
        }
        // 窗口里没有「重试」与「立即结束」：那两条都要求宿主侧的内核连接。
        Some(_) | None => {}
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
    /// 批次已经收尾：窗口画完最后一帧后自动关闭。
    finished: bool,
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
        Entry::Reasoning(text) => {
            state.subagent_reasoning(text);
        }
        Entry::BatchFinished => {
            rendered.finished = true;
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
fn draw(
    terminal: &mut WindowTerminal,
    state: &mut crate::state::AppState,
    stream: &Path,
    rendered: &Rendered,
    keep_open: bool,
) -> Result<(), String> {
    // 顶部标题行之外的语义全在主页面里：窗口只把「这是哪个批次」补进状态行，
    // 其余版式与会话区构成与主代理逐块一致。文本没变时不重写，免得每帧把输入块置脏。
    let title = window_title(stream, rendered, keep_open);
    if state.status.as_deref() != Some(title.as_str()) {
        state.set_status(Some(title));
    }
    terminal
        .draw(|frame| crate::ui::render(frame, state, None))
        .map_err(|error| format!("写窗口内容失败：{error}"))?;
    // 窗口不接输入，但页面上有个空输入框：把隐藏的终端光标挪到它的插入点，
    // 免得停在会话区末尾（与主窗口同一处理，见 `main::settle_terminal_cursor`）。
    if let Some(caret) = state.composer_caret() {
        terminal
            .terminal
            .set_cursor_position(caret)
            .map_err(|error| format!("定位终端光标失败：{error}"))?;
    }
    Ok(())
}

/// 状态行文本：批次标题 + 任务进度。
fn window_title(stream: &Path, rendered: &Rendered, keep_open: bool) -> String {
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
    if keep_open && rendered.finished {
        return format!("子代理 · {name} · {done}/{total} · 已结束（Esc 关闭）");
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
/// 与 `main.rs` 里的 `TerminalGuard` 同一职责，但窗口进程没有内核、也没有需要自愈的
/// 工具子进程，因此只补上鼠标**拖动/移动**追踪序列（Windows 上 crossterm 的
/// `EnableMouseCapture` 只设 winapi 模式，不发 `?1003h`，滚轮之外的鼠标事件收不到）。
struct WindowTerminal {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    /// 右缘滚动条是否正被按住拖动。
    scrollbar_drag: bool,
}

impl WindowTerminal {
    /// 鼠标移动/拖动追踪的开与关（与 `main.rs` 的 `MOUSE_MOVE_ON/OFF` 同一组序列）。
    const MOUSE_MOVE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
    const MOUSE_MOVE_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

    fn start() -> Result<Self, String> {
        use crossterm::terminal::{
            disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
        };
        enable_raw_mode().map_err(|error| format!("进入原始模式失败：{error}"))?;
        let mut out = std::io::stdout();
        if let Err(error) = crossterm::execute!(out, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(format!("切换备用屏幕失败：{error}"));
        }
        let _ = write!(std::io::stdout(), "{}", Self::MOUSE_MOVE_ON);
        let _ = std::io::stdout().flush();
        let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
        match ratatui::Terminal::new(backend) {
            Ok(terminal) => Ok(Self {
                terminal,
                scrollbar_drag: false,
            }),
            Err(error) => {
                let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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

    /// 当前视口（鼠标命中测试用）。尺寸只在这里读：终端尺寸是一次控制台往返。
    fn viewport(&self) -> Result<ratatui::layout::Rect, String> {
        let size = self
            .terminal
            .size()
            .map_err(|error| format!("读取终端尺寸失败：{error}"))?;
        Ok(ratatui::layout::Rect::new(0, 0, size.width, size.height))
    }
}

impl Drop for WindowTerminal {
    fn drop(&mut self) {
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        let mut out = std::io::stdout();
        let _ = write!(out, "{}", Self::MOUSE_MOVE_OFF);
        let _ = crossterm::execute!(out, DisableMouseCapture, LeaveAlternateScreen);
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
        assert!(!windows.is_opened("batch-x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 模型自己调的 `subagent` 起的批次：只落流水、绑定工具卡，不开窗。
    #[test]
    fn a_tool_call_batch_waits_for_the_click() {
        let root = temp_root("on-demand");
        let mut windows = SubagentWindows::new();
        let created = payload(&[
            ("batch_id", serde_json::json!("batch-7")),
            ("origin_call_id", serde_json::json!("call_7")),
        ]);
        assert_eq!(
            windows.deliver(&root, None, "subagent.batch.created", &created),
            Delivery::Appended
        );
        assert!(!windows.is_opened("batch-7"), "点击之前不该开窗");
        assert_eq!(windows.view_batch("call_7"), Some("batch-7"));

        let started = payload(&[
            ("batch_id", serde_json::json!("batch-7")),
            ("task_id", serde_json::json!("t1")),
        ]);
        assert_eq!(
            windows.deliver(&root, None, "subagent.task.started", &started),
            Delivery::Appended
        );
        let text = std::fs::read_to_string(root.join("batch-7.jsonl")).expect("流水已建立");
        assert_eq!(text.lines().count(), 1, "只有可渲染事件落盘：{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 窗口被关掉（进程退出）后登记失效，按钮可以重新开窗。
    #[cfg(windows)]
    #[test]
    fn a_closed_window_lets_the_button_open_again() {
        let root = temp_root("click-reopen");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[
            ("batch_id", serde_json::json!("batch-8")),
            ("origin_call_id", serde_json::json!("call_8")),
        ]);
        windows.deliver(&root, None, "subagent.batch.created", &batch);

        // 一个还在跑的子进程代表「用户没关的窗口」。
        let live = Command::new("cmd")
            .args(["/C", "ping -n 60 127.0.0.1"])
            .stdout(Stdio::null())
            .spawn()
            .expect("起假窗口进程");
        windows.windows.insert("batch-8".to_string(), live);
        assert!(windows.window_running("batch-8"), "进程还在跑：窗口算开着");
        assert!(!windows.can_open("batch-8"), "窗口开着就不再开第二个");

        // 用户关掉窗口：进程退出后登记失效，按钮重新可用。
        // 注意这里保留登记（与真实场景一致：用户关窗时宿主并不知道，只能靠进程状态发现）。
        if let Some(live) = windows.windows.get_mut("batch-8") {
            let _ = live.kill();
            let _ = live.wait();
        }
        assert!(
            !windows.window_running("batch-8"),
            "退出后该批次不再算「已有窗口」"
        );
        assert!(windows.can_open("batch-8"), "关窗之后要能再开一次");

        // 没有登记流水的批次不开。
        assert!(!windows.can_open("batch-unknown"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拿不到可执行文件时什么都不做（测试与无自身路径的场景）。
    #[test]
    fn open_without_an_executable_does_nothing() {
        let root = temp_root("no-exe");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[
            ("batch_id", serde_json::json!("batch-8")),
            ("origin_call_id", serde_json::json!("call_8")),
        ]);
        windows.deliver(&root, None, "subagent.batch.created", &batch);
        assert!(!windows.open(None, "batch-8"));
        assert!(!windows.window_running("batch-8"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 批次收尾后撤下入口：窗口进程会自己关，留着按钮只能点出一个马上退出的空窗。
    #[test]
    fn a_finished_batch_drops_the_button() {
        let root = temp_root("finished-button");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[
            ("batch_id", serde_json::json!("batch-9")),
            ("origin_call_id", serde_json::json!("call_9")),
        ]);
        windows.deliver(&root, None, "subagent.batch.created", &batch);
        assert_eq!(windows.view_batch("call_9"), Some("batch-9"));
        assert_eq!(
            windows.deliver(&root, None, "subagent.batch.finished", &batch),
            Delivery::Appended
        );
        assert_eq!(windows.view_batch("call_9"), None);
        assert_eq!(windows.call_of("batch-9"), Some("call_9"));
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
        // 第 0 条是 `window_state` 的「等待」占位提示，这里只看它之后的两条。
        assert!(matches!(state.records[1], Record::Assistant(ref text) if text == "先看一眼"));
        let Record::Tool(ref card) = state.records[2] else {
            panic!("第二条应是工具卡：{:?}", state.records[2]);
        };
        assert_eq!(card.name, "read");
        assert_eq!(card.status, ToolStatus::Ok);
        assert_eq!(card.body, "ok");
        // 还有任务在跑：状态行会显示运行态（主代理据此画 spinner 与 `[ ESC ]`）。
        assert!(state.turn.is_running());
    }

    #[test]
    fn streamed_reasoning_and_text_join_one_record_each() {
        let mut rendered = Rendered::default();
        let mut state = window_state();
        for line in [
            serde_json::json!({"kind": "reasoning", "text": "先看"}),
            serde_json::json!({"kind": "reasoning", "text": "一眼"}),
            serde_json::json!({"kind": "text", "text": "接着"}),
            serde_json::json!({"kind": "text", "text": "读文件"}),
        ] {
            consume(&mut rendered, &mut state, &line.to_string());
        }

        // 增量逐片到达：同一段思考/正文各自只有一条记录，不会碎成四条消息。
        // 第 0 条是 `window_state` 的占位提示。
        assert_eq!(state.records.len(), 3, "{:?}", state.records);
        assert!(matches!(state.records[1], Record::Reasoning(ref text) if text == "先看一眼"));
        assert!(matches!(state.records[2], Record::Assistant(ref text) if text == "接着读文件"));
    }

    #[test]
    fn batch_finished_closes_the_window_after_the_last_event() {
        let root = temp_root("finished");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[
            ("batch_id", serde_json::json!("batch-1")),
            ("task_id", serde_json::json!("t1")),
            ("agent_type", serde_json::json!("explore")),
            ("description", serde_json::json!("查仓库")),
        ]);
        let Delivery::Opened { stream } =
            windows.deliver(&root, None, "subagent.task.started", &batch)
        else {
            panic!("首个事件应当开窗口");
        };
        assert_eq!(
            windows.deliver(&root, None, "subagent.batch.finished", &batch),
            Delivery::Appended
        );

        let mut rendered = Rendered::default();
        let mut state = window_state();
        for line in std::fs::read_to_string(&stream).expect("流水可读").lines() {
            consume(&mut rendered, &mut state, line);
        }
        assert!(rendered.finished, "收尾事件应让窗口准备退出");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn batch_finished_never_opens_a_window() {
        let root = temp_root("finished-none");
        let mut windows = SubagentWindows::new();
        let batch = payload(&[("batch_id", serde_json::json!("batch-x"))]);
        assert_eq!(
            windows.deliver(&root, None, "subagent.batch.finished", &batch),
            Delivery::Ignored
        );
        assert!(!windows.is_opened("batch-x"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
