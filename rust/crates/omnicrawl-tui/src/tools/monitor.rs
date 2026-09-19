//! `monitor` 工具：Agent 自持的后台命令。
//!
//! 语义基准是 `omnicrawl/workspace/monitor.py` 的 `BackgroundMonitorManager`：后台进程
//! 由当前宿主持有（宿主关闭或所属回合取消时回收整棵进程树），输出进内存环形缓冲，
//! 模型按游标增量轮询，不写持久化日志。参数与文案逐条对齐 Python。

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::arguments::{limited_int, optional_text};
use super::command::{invocation, kill_process_tree_by_pid, CommandOutcome, Shell};
use super::error::ToolError;

pub const MAX_ACTIVE_MONITORS: usize = 20;
pub const MAX_BUFFERED_EVENTS: usize = 1_000;
pub const MAX_EVENT_CHARS: usize = 4_000;
pub const MAX_EVENTS_PER_POLL: usize = 200;
/// 停止任务时等待终态的时限（与 Python 一致）。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// 等读取线程收尾的时限（与 Python 的 `reader.join(timeout=5)` 一致）：被杀进程树若
/// 留下持有管道写端的孤儿，终态不能无限期挂在 running 上。
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 宿主关闭时写入任务事件的停止原因（与 Python 同文案）。
const CLOSE_REASON: &str = "Agent 已关闭，已停止后台任务。";
/// 回合取消时写入任务事件的停止原因（与 Python 同文案）。
pub const CANCEL_REASON: &str = "当前回合已取消，后台任务已强制终止。";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// 后台命令的一条可轮询事件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorEvent {
    sequence: u64,
    created_at: SystemTime,
    stream: String,
    text: String,
}

/// 后台任务的当前状态快照。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorSnapshot {
    monitor_id: String,
    command: String,
    shell: String,
    status: String,
    exit_code: Option<i32>,
    started_at: SystemTime,
    next_cursor: u64,
    dropped_events: u64,
}

/// 按游标读取到的增量事件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorPollResult {
    snapshot: MonitorSnapshot,
    events: Vec<MonitorEvent>,
    next_cursor: u64,
    first_available_cursor: u64,
}

struct Task {
    monitor_id: String,
    command: String,
    shell: String,
    pid: u32,
    status: Status,
    exit_code: Option<i32>,
    stop_requested: bool,
    started_at: SystemTime,
    scope: Option<String>,
    events: VecDeque<MonitorEvent>,
    next_sequence: u64,
    dropped_events: u64,
}

struct State {
    tasks: BTreeMap<String, Task>,
    pending_starts: usize,
    closed: bool,
    counter: u64,
    active_limit: usize,
    buffer_limit: usize,
    event_chars: usize,
    scope: Option<String>,
}

struct Inner {
    root: PathBuf,
    state: Mutex<State>,
    wake: Condvar,
}

/// 启动、轮询与终止后台命令；实例随宿主存活，进程随实例关闭被回收。
pub struct MonitorManager {
    inner: Arc<Inner>,
}

impl MonitorManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(
            root,
            MAX_ACTIVE_MONITORS,
            MAX_BUFFERED_EVENTS,
            MAX_EVENT_CHARS,
        )
    }

    fn with_limits(
        root: impl Into<PathBuf>,
        active_limit: usize,
        buffer_limit: usize,
        event_chars: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.into(),
                state: Mutex::new(State {
                    tasks: BTreeMap::new(),
                    pending_starts: 0,
                    closed: false,
                    counter: 0,
                    active_limit,
                    buffer_limit,
                    event_chars,
                    scope: None,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// 回合归属：本回合内启动的后台任务在该回合取消时被回收。
    pub fn set_scope(&self, scope: Option<String>) {
        self.state().scope = scope;
    }

    /// 执行一次 Monitor 调用：`start`、`poll`、`stop` 或 `list`。
    pub fn run(&self, arguments: &Map<String, Value>) -> Result<CommandOutcome, ToolError> {
        match optional_text(arguments, "action").to_lowercase().as_str() {
            "" | "start" => self.start(arguments),
            "status" | "log" | "logs" | "read" | "poll" => self.poll(arguments),
            "stop" => self.stop(arguments),
            "list" => Ok(self.list()),
            _ => Err(ToolError::new("action 仅支持 start、poll、stop 或 list。")),
        }
    }

    /// 终止所有仍在运行的后台进程，避免宿主退出后留下孤儿进程。
    pub fn close(&self) {
        let running: Vec<String> = {
            let mut state = self.state();
            state.closed = true;
            running_ids(&state)
        };
        for monitor_id in running {
            // 关闭路径优先尽力回收其余任务；单个进程已退出或清理失败不应阻止后续回收。
            let _ = self.stop_task(&monitor_id, CLOSE_REASON, true);
        }
    }

    /// 取消某个回合：回收该回合启动的后台任务（不等终态，取消路径不阻塞界面）。
    pub fn stop_scope(&self, scope: &str, reason: &str) -> Vec<String> {
        let ids: Vec<String> = {
            let state = self.state();
            state
                .tasks
                .values()
                .filter(|task| {
                    task.status == Status::Running && task.scope.as_deref() == Some(scope)
                })
                .map(|task| task.monitor_id.clone())
                .collect()
        };
        for monitor_id in &ids {
            let _ = self.stop_task(monitor_id, reason, false);
        }
        ids
    }

    fn start(&self, arguments: &Map<String, Value>) -> Result<CommandOutcome, ToolError> {
        let command = optional_text(arguments, "command");
        if command.is_empty() {
            return Err(ToolError::new("启动监控时 command 不能为空。"));
        }
        let requested = optional_text(arguments, "shell").to_lowercase();
        let shell_text = if requested.is_empty() {
            "powershell".to_string()
        } else {
            requested
        };
        let shell = match shell_text.as_str() {
            "bash" => Shell::Bash,
            "powershell" => Shell::PowerShell,
            _ => return Err(ToolError::new("shell 仅支持 bash 或 powershell。")),
        };

        // 先占名额再起进程：并发 start 不能同时通过容量检查。
        let scope = self.reserve_slot()?;
        let invocation = match invocation(&command, shell) {
            Ok(invocation) => invocation,
            Err(error) => {
                self.release_slot();
                return Err(error);
            }
        };
        let mut child = match spawn_background(&self.inner.root, &invocation.args) {
            Ok(child) => child,
            Err(error) => {
                self.release_slot();
                return Err(ToolError::new(format!("后台命令启动失败：{error}")));
            }
        };

        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let monitor_id = self.next_id(&command, pid);

        {
            let mut state = self.state();
            state.pending_starts -= 1;
            if state.closed {
                // 进程在 close 与 spawn 之间启动时必须立即回收，不能变成孤儿。
                drop(state);
                kill_process_tree_by_pid(pid);
                let _ = child.wait();
                return Err(ToolError::new("Agent 已关闭，后台任务已终止。"));
            }
            let buffer_limit = state.buffer_limit;
            let mut task = Task {
                monitor_id: monitor_id.clone(),
                command: command.clone(),
                shell: shell_text.clone(),
                pid,
                status: Status::Running,
                exit_code: None,
                stop_requested: false,
                started_at: SystemTime::now(),
                scope,
                events: VecDeque::new(),
                next_sequence: 1,
                dropped_events: 0,
            };
            record_event(
                &mut task,
                "system",
                &format!("已启动，shell={shell_text}。"),
                buffer_limit,
            );
            state.tasks.insert(monitor_id.clone(), task);
        }

        let mut readers = 0;
        let (done_sender, done_receiver) = mpsc::channel();
        if let Some(pipe) = stdout {
            spawn_reader(
                Arc::clone(&self.inner),
                monitor_id.clone(),
                "stdout",
                pipe,
                done_sender.clone(),
            );
            readers += 1;
        }
        if let Some(pipe) = stderr {
            spawn_reader(
                Arc::clone(&self.inner),
                monitor_id.clone(),
                "stderr",
                pipe,
                done_sender.clone(),
            );
            readers += 1;
        }
        drop(done_sender);
        self.spawn_waiter(monitor_id.clone(), child, done_receiver, readers);

        Ok(CommandOutcome {
            ok: true,
            output: format!(
                "已启动后台任务：{monitor_id}\nShell：{shell_text}\n状态：running\n下一游标：0\n\
                 使用 monitor action=poll、monitor_id={monitor_id}、cursor=0 读取增量日志；\
                 使用 action=stop 停止任务。"
            ),
        })
    }

    fn poll(&self, arguments: &Map<String, Value>) -> Result<CommandOutcome, ToolError> {
        let monitor_id = self.task_id(arguments)?;
        let cursor = limited_int(arguments, "cursor", 0, 0, 2_000_000_000);
        let max_events = limited_int(arguments, "max_events", 100, 1, MAX_EVENTS_PER_POLL as i64);
        let result = self.poll_events(&monitor_id, cursor, max_events)?;
        let snapshot = &result.snapshot;

        let mut lines = vec![
            format!("后台任务：{}", snapshot.monitor_id),
            format!("状态：{}", snapshot.status),
            format!("下一游标：{}", result.next_cursor),
        ];
        if let Some(exit_code) = snapshot.exit_code {
            lines.push(format!("退出码：{exit_code}"));
        }
        if (cursor.max(0) as u64) < result.first_available_cursor && snapshot.dropped_events > 0 {
            lines.push(format!(
                "提示：早期 {} 条事件已从内存缓冲清理。",
                snapshot.dropped_events
            ));
        }
        if result.events.is_empty() {
            lines.push("事件：暂无新增输出。".to_string());
        } else {
            lines.push("事件：".to_string());
            lines.extend(result.events.iter().map(format_event));
        }

        Ok(CommandOutcome {
            ok: snapshot.status != Status::Failed.as_str(),
            output: lines.join("\n"),
        })
    }

    fn stop(&self, arguments: &Map<String, Value>) -> Result<CommandOutcome, ToolError> {
        let monitor_id = self.task_id(arguments)?;
        self.stop_task(&monitor_id, "收到停止请求，正在终止后台任务。", true)?;
        let stop_arguments = Map::from_iter([
            ("monitor_id".to_string(), Value::from(monitor_id)),
            ("cursor".to_string(), Value::from(0)),
            ("max_events".to_string(), Value::from(20)),
        ]);
        self.poll(&stop_arguments)
    }

    fn list(&self) -> CommandOutcome {
        let rows: Vec<MonitorSnapshot> = {
            let state = self.state();
            state.tasks.values().map(snapshot_of).collect()
        };
        if rows.is_empty() {
            return CommandOutcome {
                ok: true,
                output: "当前没有后台任务。".to_string(),
            };
        }
        let mut lines = vec!["后台任务：".to_string()];
        for row in rows {
            let suffix = match row.exit_code {
                Some(exit_code) => format!("，退出码：{exit_code}"),
                None => String::new(),
            };
            lines.push(format!(
                "- {}：{}，shell={}，当前游标：{}{suffix}",
                row.monitor_id, row.status, row.shell, row.next_cursor
            ));
        }
        CommandOutcome {
            ok: true,
            output: lines.join("\n"),
        }
    }

    fn poll_events(
        &self,
        monitor_id: &str,
        cursor: i64,
        max_events: i64,
    ) -> Result<MonitorPollResult, ToolError> {
        let normalized_cursor = cursor.max(0) as u64;
        let normalized_max = max_events.max(1).min(MAX_EVENTS_PER_POLL as i64) as usize;
        let state = self.state();
        let task = state
            .tasks
            .get(monitor_id)
            .ok_or_else(|| not_found(monitor_id))?;
        let latest_sequence = task.next_sequence.saturating_sub(1);
        let effective_cursor = normalized_cursor.min(latest_sequence);
        let first_sequence = task
            .events
            .front()
            .map(|event| event.sequence)
            .unwrap_or(task.next_sequence);
        let events: Vec<MonitorEvent> = task
            .events
            .iter()
            .filter(|event| event.sequence > effective_cursor)
            .take(normalized_max)
            .cloned()
            .collect();
        let next_cursor = events
            .last()
            .map(|event| event.sequence)
            .unwrap_or(effective_cursor);
        Ok(MonitorPollResult {
            snapshot: snapshot_of(task),
            events,
            next_cursor,
            first_available_cursor: first_sequence.saturating_sub(1),
        })
    }

    fn task_id(&self, arguments: &Map<String, Value>) -> Result<String, ToolError> {
        let monitor_id = optional_text(arguments, "monitor_id");
        if monitor_id.is_empty() {
            return Err(ToolError::new("monitor_id 不能为空。"));
        }
        let state = self.state();
        if !state.tasks.contains_key(&monitor_id) {
            return Err(not_found(&monitor_id));
        }
        Ok(monitor_id)
    }

    /// 置位停止请求并回收进程树；`wait` 为真时等到任务落入终态。
    fn stop_task(&self, monitor_id: &str, reason: &str, wait: bool) -> Result<(), ToolError> {
        let pid = {
            let mut state = self.state();
            let buffer_limit = state.buffer_limit;
            let task = state
                .tasks
                .get_mut(monitor_id)
                .ok_or_else(|| not_found(monitor_id))?;
            if task.status != Status::Running {
                return Ok(());
            }
            task.stop_requested = true;
            record_event(task, "system", reason, buffer_limit);
            task.pid
        };
        kill_process_tree_by_pid(pid);
        if !wait {
            return Ok(());
        }
        self.wait_for_terminal(monitor_id)
    }

    fn wait_for_terminal(&self, monitor_id: &str) -> Result<(), ToolError> {
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut state = self.state();
        loop {
            let running = state
                .tasks
                .get(monitor_id)
                .map(|task| task.status == Status::Running)
                .unwrap_or(false);
            if !running {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ToolError::new(format!(
                    "后台任务未能在 {} 秒内停止：{monitor_id}",
                    STOP_TIMEOUT.as_secs()
                )));
            }
            let (guard, _) = self
                .inner
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    fn spawn_waiter(
        &self,
        monitor_id: String,
        child: Child,
        done: mpsc::Receiver<()>,
        readers: usize,
    ) {
        let inner = Arc::clone(&self.inner);
        thread::spawn(move || {
            let mut child = child;
            let exit_code = child
                .wait()
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            // 先等读取线程收尾，终态事件才不会插到尚未落缓冲的输出前面；孤儿进程持有
            // 管道写端时最多等 5 秒，与 Python 的 `reader.join(timeout=5)` 一致。
            let deadline = Instant::now() + READER_JOIN_TIMEOUT;
            for _ in 0..readers {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() || done.recv_timeout(remaining).is_err() {
                    break;
                }
            }
            inner.finish(&monitor_id, exit_code);
        });
    }

    fn reserve_slot(&self) -> Result<Option<String>, ToolError> {
        let mut state = self.state();
        if state.closed {
            return Err(ToolError::new("Agent 已关闭，不能启动新的后台任务。"));
        }
        let active = state
            .tasks
            .values()
            .filter(|task| task.status == Status::Running)
            .count();
        if active + state.pending_starts >= state.active_limit {
            return Err(ToolError::new(format!(
                "同时运行的后台任务最多为 {} 个，请先停止不需要的任务。",
                state.active_limit
            )));
        }
        state.pending_starts += 1;
        Ok(state.scope.clone())
    }

    fn release_slot(&self) {
        let mut state = self.state();
        state.pending_starts = state.pending_starts.saturating_sub(1);
    }

    fn next_id(&self, command: &str, pid: u32) -> String {
        let mut state = self.state();
        state.counter += 1;
        let counter = state.counter;
        drop(state);
        let mut hasher = Sha256::new();
        hasher.update(pid.to_le_bytes());
        hasher.update(counter.to_le_bytes());
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        hasher.update(nanos.to_le_bytes());
        hasher.update(command.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        format!("monitor-{}", &digest[..12])
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Inner {
    fn record(&self, monitor_id: &str, stream: &str, text: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer_limit = state.buffer_limit;
        let event_chars = state.event_chars;
        let Some(task) = state.tasks.get_mut(monitor_id) else {
            return;
        };
        for chunk in split_event(text, event_chars) {
            record_event(task, stream, &chunk, buffer_limit);
        }
        drop(state);
        self.wake.notify_all();
    }

    fn finish(&self, monitor_id: &str, exit_code: i32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer_limit = state.buffer_limit;
        let Some(task) = state.tasks.get_mut(monitor_id) else {
            return;
        };
        task.exit_code = Some(exit_code);
        let (status, message) = if task.stop_requested {
            (
                Status::Stopped,
                format!("任务已停止，退出码：{exit_code}。"),
            )
        } else if exit_code == 0 {
            (Status::Completed, "任务已完成，退出码：0。".to_string())
        } else {
            (Status::Failed, format!("任务失败，退出码：{exit_code}。"))
        };
        task.status = status;
        record_event(task, "system", &message, buffer_limit);
        drop(state);
        self.wake.notify_all();
    }
}

fn spawn_background(root: &Path, args: &[String]) -> std::io::Result<Child> {
    let mut command = Command::new(&args[0]);
    command
        .args(&args[1..])
        .current_dir(root)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn()
}

fn spawn_reader<R: Read + Send + 'static>(
    inner: Arc<Inner>,
    monitor_id: String,
    stream: &'static str,
    pipe: R,
    done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut line = String::from_utf8_lossy(&buffer).to_string();
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    inner.record(&monitor_id, stream, &line);
                }
            }
        }
        let _ = done.send(());
    })
}

/// 单条事件的超长文本按字符切块（Python 按字符串下标切片，等价于按码点）。
fn split_event(text: &str, event_chars: usize) -> Vec<String> {
    if text.chars().count() <= event_chars {
        return vec![text.to_string()];
    }
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(event_chars)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn record_event(task: &mut Task, stream: &str, text: &str, buffer_limit: usize) {
    task.events.push_back(MonitorEvent {
        sequence: task.next_sequence,
        created_at: SystemTime::now(),
        stream: stream.to_string(),
        text: text.to_string(),
    });
    task.next_sequence += 1;
    if task.events.len() > buffer_limit {
        task.events.pop_front();
        task.dropped_events += 1;
    }
}

fn running_ids(state: &State) -> Vec<String> {
    state
        .tasks
        .values()
        .filter(|task| task.status == Status::Running)
        .map(|task| task.monitor_id.clone())
        .collect()
}

fn snapshot_of(task: &Task) -> MonitorSnapshot {
    MonitorSnapshot {
        monitor_id: task.monitor_id.clone(),
        command: task.command.clone(),
        shell: task.shell.clone(),
        status: task.status.as_str().to_string(),
        exit_code: task.exit_code,
        started_at: task.started_at,
        next_cursor: task.next_sequence.saturating_sub(1),
        dropped_events: task.dropped_events,
    }
}

fn format_event(event: &MonitorEvent) -> String {
    let stamp: DateTime<Local> = event.created_at.into();
    let text = if event.text.is_empty() {
        "(空行)"
    } else {
        event.text.as_str()
    };
    format!(
        "{} [{}] {}: {}",
        event.sequence,
        stamp.format("%H:%M:%S"),
        event.stream,
        text
    )
}

fn not_found(monitor_id: &str) -> ToolError {
    ToolError::new(format!("未找到后台任务：{monitor_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-monitor-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        root
    }

    /// 本机可用的解释器与「打印一行」命令。
    fn shell() -> (&'static str, String, String) {
        if bash_available() {
            (
                "bash",
                "echo monitor-ok".to_string(),
                "sleep 30".to_string(),
            )
        } else {
            (
                "powershell",
                "Write-Output 'monitor-ok'".to_string(),
                "Start-Sleep -Seconds 30".to_string(),
            )
        }
    }

    /// 长命令优先用原生解释器：MSYS bash 起进程树是多层的，杀树与建树并发时容易留下孤儿
    /// （见 README 的已知限制），本用例只关心监控状态机，不必把这个平台噪声带进来。
    fn sleeping_shell() -> Option<(&'static str, String)> {
        if powershell_available() {
            Some(("powershell", "Start-Sleep -Seconds 30".to_string()))
        } else if bash_available() {
            Some(("bash", "sleep 30".to_string()))
        } else {
            None
        }
    }

    fn bash_available() -> bool {
        super::super::command::find_bash_executable(&|name| std::env::var(name).ok()).is_some()
    }

    fn powershell_available() -> bool {
        super::super::command::find_powershell_executable(&|name| std::env::var(name).ok())
            .is_some()
    }

    fn start(manager: &MonitorManager, shell: &str, command: &str) -> String {
        let result = manager
            .run(&args(
                json!({"action": "start", "command": command, "shell": shell}),
            ))
            .expect("启动后台任务");
        assert!(result.ok, "{}", result.output);
        result
            .output
            .lines()
            .next()
            .expect("启动输出第一行")
            .trim_start_matches("已启动后台任务：")
            .to_string()
    }

    /// 反复 poll 直到条件成立，返回最后一次输出。
    fn poll_until(
        manager: &MonitorManager,
        monitor_id: &str,
        deadline: Duration,
        ready: impl Fn(&str) -> bool,
    ) -> String {
        let limit = Instant::now() + deadline;
        loop {
            let output = manager
                .run(&args(json!({"action": "poll", "monitor_id": monitor_id})))
                .expect("轮询后台任务")
                .output;
            if ready(&output) || Instant::now() >= limit {
                return output;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn start_poll_and_list_walk_a_task_to_completion() {
        let (shell, quick, _) = shell();
        let manager = MonitorManager::new(root("lifecycle"));
        let monitor_id = start(&manager, shell, &quick);
        assert_eq!(monitor_id.len(), "monitor-".len() + 12, "{monitor_id}");

        let started = manager.run(&args(
            json!({"action": "start", "command": quick, "shell": shell}),
        ));
        let output = started.expect("启动").output;
        assert!(output.contains(&format!("Shell：{shell}")), "{output}");
        assert!(output.contains("状态：running"), "{output}");
        assert!(output.contains("下一游标：0"), "{output}");
        assert!(output.contains("使用 action=stop 停止任务。"), "{output}");

        let listed = manager.run(&args(json!({"action": "list"}))).expect("列表");
        assert!(
            listed.output.starts_with("后台任务：\n"),
            "{}",
            listed.output
        );
        assert!(
            listed.output.contains(&format!(
                "- {monitor_id}：running，shell={shell}，当前游标："
            )),
            "{}",
            listed.output
        );

        let events = poll_until(&manager, &monitor_id, Duration::from_secs(20), |output| {
            output.contains("任务已完成，退出码：0。")
        });
        assert!(events.contains("状态：completed"), "{events}");
        assert!(events.contains("退出码：0"), "{events}");
        assert!(events.contains("stdout: monitor-ok"), "{events}");
        assert!(events.contains("system: 已启动，shell="), "{events}");

        // 空轮询仍推进游标，且不会重复吐已消费的事件。
        let cursor = events
            .lines()
            .find_map(|line| line.strip_prefix("下一游标："))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .expect("输出里有下一游标");
        let again = manager
            .run(&args(
                json!({"action": "poll", "monitor_id": monitor_id, "cursor": cursor}),
            ))
            .expect("增量轮询");
        assert!(
            again.output.contains("事件：暂无新增输出。"),
            "{}",
            again.output
        );
        assert!(
            again.output.contains(&format!("下一游标：{cursor}")),
            "{}",
            again.output
        );
    }

    #[test]
    fn action_aliases_and_unknown_actions_follow_python() {
        let (shell, quick, _) = shell();
        let manager = MonitorManager::new(root("actions"));
        let monitor_id = start(&manager, shell, &quick);
        for alias in ["poll", "status", "log", "logs", "read"] {
            let result = manager
                .run(&args(json!({"action": alias, "monitor_id": monitor_id})))
                .expect("别名动作");
            assert!(
                result.output.starts_with("后台任务："),
                "{alias}: {}",
                result.output
            );
        }
        let unknown = manager
            .run(&args(json!({"action": "attach"})))
            .expect_err("未知动作要报错");
        assert_eq!(unknown.message, "action 仅支持 start、poll、stop 或 list。");
        let _ = manager.run(&args(json!({"action": "stop", "monitor_id": monitor_id})));
    }

    #[test]
    fn argument_errors_match_python_texts() {
        let (shell, quick, _) = shell();
        let manager = MonitorManager::new(root("errors"));
        let empty = manager
            .run(&args(json!({"action": "start", "command": "  "})))
            .expect_err("空命令要报错");
        assert_eq!(empty.message, "启动监控时 command 不能为空。");
        let bad_shell = manager
            .run(&args(
                json!({"action": "start", "command": quick, "shell": "cmd"}),
            ))
            .expect_err("非法 Shell 要报错");
        assert_eq!(bad_shell.message, "shell 仅支持 bash 或 powershell。");
        let missing = manager
            .run(&args(json!({"action": "poll"})))
            .expect_err("缺 monitor_id 要报错");
        assert_eq!(missing.message, "monitor_id 不能为空。");
        let unknown = manager
            .run(&args(
                json!({"action": "poll", "monitor_id": "monitor-000000000000"}),
            ))
            .expect_err("未知任务要报错");
        assert_eq!(unknown.message, "未找到后台任务：monitor-000000000000");
        let stopped_missing = manager
            .run(&args(
                json!({"action": "stop", "monitor_id": "monitor-000000000000"}),
            ))
            .expect_err("停止未知任务要报错");
        assert_eq!(
            stopped_missing.message,
            "未找到后台任务：monitor-000000000000"
        );
        let shell_used = manager
            .run(&args(
                json!({"action": "start", "command": quick, "shell": shell}),
            ))
            .expect("正常启动");
        assert!(shell_used.ok);
    }

    #[test]
    fn running_task_is_reported_and_stopped() {
        let Some((shell, sleeping)) = sleeping_shell() else {
            eprintln!("跳过：本机既没有 Git Bash 也没有 PowerShell");
            return;
        };
        let manager = MonitorManager::new(root("stop"));
        let monitor_id = start(&manager, shell, &sleeping);
        let output = manager
            .run(&args(json!({"action": "stop", "monitor_id": monitor_id})))
            .expect("停止后台任务");
        assert!(output.ok, "{}", output.output);
        assert!(output.output.contains("状态：stopped"), "{}", output.output);
        assert!(
            output
                .output
                .contains("system: 收到停止请求，正在终止后台任务。"),
            "{}",
            output.output
        );
        assert!(
            output.output.contains("任务已停止，退出码："),
            "{}",
            output.output
        );
        // 失败任务才让 poll 返回 ok=false：停止过的任务是 stopped，不是 failed。
        let after = manager
            .run(&args(json!({"action": "poll", "monitor_id": monitor_id})))
            .expect("复读");
        assert!(after.ok, "{}", after.output);

        let empty = manager
            .run(&args(json!({"action": "stop", "monitor_id": monitor_id})))
            .expect("重复停止是幂等的");
        assert!(empty.ok, "{}", empty.output);
    }

    #[test]
    fn long_lines_are_chunked_by_event_limit() {
        let (shell, _, _) = shell();
        // 纯解释器内建：不起额外进程，避免把进程启动时间吃进事件的断言里。
        let command = if shell == "bash" {
            "printf 'x%.0s' {1..5000}; echo"
        } else {
            "'x' * 5000"
        };
        let manager = MonitorManager::with_limits(root("chunks"), 4, 100, 4_000);
        let monitor_id = start(&manager, shell, command);
        let output = poll_until(&manager, &monitor_id, Duration::from_secs(30), |output| {
            output.contains("任务已完成")
        });
        let chunks: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("stdout: "))
            .collect();
        assert_eq!(chunks.len(), 2, "{output}");
        assert!(
            chunks[0].ends_with(&"x".repeat(4_000)),
            "首块应满 4000 字符"
        );
        assert!(
            chunks[1].ends_with(&"x".repeat(1_000)),
            "尾块应是余下的 1000 字符"
        );
    }

    #[test]
    fn ring_buffer_drops_oldest_events_and_reports_them() {
        let (shell, _, _) = shell();
        let command = if shell == "bash" {
            "for i in 1 2 3 4 5 6 7 8; do echo line-$i; done".to_string()
        } else {
            "1..8 | ForEach-Object { Write-Output \"line-$_\" }".to_string()
        };
        let manager = MonitorManager::with_limits(root("ring"), 4, 5, 4_000);
        let monitor_id = start(&manager, shell, &command);
        let output = poll_until(&manager, &monitor_id, Duration::from_secs(30), |output| {
            output.contains("任务已完成")
        });
        assert!(output.contains("提示：早期 "), "{output}");
        let dropped = output
            .lines()
            .find_map(|line| line.strip_prefix("提示：早期 "))
            .and_then(|text| text.split(' ').next())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("提示里带条数");
        assert!(dropped >= 3, "{output}");
    }

    #[test]
    fn active_limit_rejects_one_task_too_many() {
        let Some((shell, sleeping)) = sleeping_shell() else {
            eprintln!("跳过：本机既没有 Git Bash 也没有 PowerShell");
            return;
        };
        let manager = MonitorManager::with_limits(root("full"), 1, 100, 4_000);
        let first = start(&manager, shell, &sleeping);
        let rejected = manager
            .run(&args(
                json!({"action": "start", "command": sleeping, "shell": shell}),
            ))
            .expect_err("超出并发上限要报错");
        assert_eq!(
            rejected.message,
            "同时运行的后台任务最多为 1 个，请先停止不需要的任务。"
        );
        assert!(
            manager
                .run(&args(json!({"action": "stop", "monitor_id": first})))
                .expect("停止首个任务")
                .ok
        );
    }

    #[test]
    fn close_stops_tasks_and_refuses_new_starts() {
        let Some((shell, sleeping)) = sleeping_shell() else {
            eprintln!("跳过：本机既没有 Git Bash 也没有 PowerShell");
            return;
        };
        let manager = MonitorManager::new(root("close"));
        let monitor_id = start(&manager, shell, &sleeping);
        manager.close();
        let refused = manager
            .run(&args(
                json!({"action": "start", "command": sleeping, "shell": shell}),
            ))
            .expect_err("关闭后不能再启动");
        assert_eq!(refused.message, "Agent 已关闭，不能启动新的后台任务。");
        // 关闭会等待终态；这里的轮询再兜一层，避免机器繁忙时的回收延迟把断言变成竞态。
        let state = poll_until(&manager, &monitor_id, Duration::from_secs(20), |output| {
            output.contains("状态：stopped")
        });
        assert!(state.contains("状态：stopped"), "{state}");
        assert!(
            state.contains("system: Agent 已关闭，已停止后台任务。"),
            "{state}"
        );
    }

    #[test]
    fn canceling_a_turn_stops_only_its_own_tasks() {
        let Some((shell, sleeping)) = sleeping_shell() else {
            eprintln!("跳过：本机既没有 Git Bash 也没有 PowerShell");
            return;
        };
        let manager = MonitorManager::new(root("scope"));
        manager.set_scope(Some("turn-1".to_string()));
        let first = start(&manager, shell, &sleeping);
        manager.set_scope(Some("turn-2".to_string()));
        let second = start(&manager, shell, &sleeping);

        let stopped = manager.stop_scope("turn-1", CANCEL_REASON);
        assert_eq!(stopped, vec![first.clone()]);
        // 取消路径不等终态（不阻塞界面），这里轮询到回收完成再断言。
        let first_state = poll_until(&manager, &first, Duration::from_secs(20), |output| {
            output.contains("状态：stopped")
        });
        assert!(first_state.contains("状态：stopped"), "{first_state}");
        assert!(
            first_state.contains(&format!("system: {CANCEL_REASON}")),
            "{first_state}"
        );
        let second_state = manager
            .run(&args(json!({"action": "list"})))
            .expect("列表")
            .output;
        assert!(
            second_state.contains(&format!("- {second}：running")),
            "{second_state}"
        );
        assert!(
            manager
                .run(&args(json!({"action": "stop", "monitor_id": second})))
                .expect("停止第二个任务")
                .ok
        );
    }
}
