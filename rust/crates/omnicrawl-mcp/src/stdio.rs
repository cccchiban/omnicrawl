//! MCP stdio 传输（对应 `omnicrawl/mcp/client.py` 的 `_StdioMCPConnection`）。
//!
//! MCP stdio 使用 `Content-Length` 帧承载 JSON-RPC。连接维持两条常驻线程：stdout 线程
//! 按请求 id 把响应分发给等待方，stderr 线程持续排空子进程日志。用常驻线程而不是
//! 「每请求新建一个线程」，是为了让 stderr 始终有人读取——Server 写满 stderr 管道后
//! 会阻塞在 write 上，不排空会让工具调用只能干等到超时。
//!
//! 请求超时或子进程退出后，下一次调用会重启子进程并重新握手（`initialize` +
//! `notifications/initialized`），单个 Server 不会被一次超时在会话里永久拖死。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::client::{DiscoveredCapabilities, McpConnection};
use crate::config::McpServerConfig;
use crate::jsonrpc::{
    capability_declared, encode_frame, json_rpc_failure_message, list_capability_pages, read_frame,
    FrameKind, McpCallError, McpClientError,
};

/// 与 Python 客户端一致的协议版本。
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// stdio Server 的 stderr 只保留尾部若干行做诊断，内容不进入模型上下文。
const MAX_STDERR_TAIL_LINES: usize = 64;
const MAX_STDERR_PREVIEW_CHARS: usize = 1000;
const CLIENT_NAME: &str = "ai-voice-agent";
const CLIENT_VERSION: &str = "0.1";

/// 在途请求的响应通道。
type PendingSender = Sender<Result<Map<String, Value>, McpClientError>>;

/// 读线程与写请求共享的状态（读线程只碰这几项，因此可以自带 Arc 而不引用整个连接）。
struct Shared {
    server_name: String,
    pending: Mutex<Vec<(u64, PendingSender)>>,
    stderr_lines: Mutex<VecDeque<String>>,
}

impl Shared {
    fn put_pending(&self, request_id: u64, sender: PendingSender) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.push((request_id, sender));
        }
    }

    fn take_pending(&self, request_id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.retain(|(id, _)| *id != request_id);
        }
    }

    fn dispatch(&self, request_id: u64, message: Map<String, Value>) {
        let sender = self.pending.lock().ok().and_then(|pending| {
            pending
                .iter()
                .find(|(id, _)| *id == request_id)
                .map(|(_, sender)| sender.clone())
        });
        if let Some(sender) = sender {
            let _ = sender.send(Ok(message));
        }
    }

    /// 把失败传给所有在途请求，避免等待方一直等到超时。
    fn fail_pending(&self, error: McpClientError) {
        let waiters = match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => Vec::new(),
        };
        for (_, sender) in waiters {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn push_stderr(&self, line: &str) {
        if let Ok(mut lines) = self.stderr_lines.lock() {
            if lines.len() == MAX_STDERR_TAIL_LINES {
                lines.pop_front();
            }
            lines.push_back(line.to_string());
        }
    }

    /// 返回已排空的 stderr 尾部（取末尾 1000 字符内的内容），用于失败诊断。
    fn stderr_preview(&self) -> String {
        let lines: Vec<String> = match self.stderr_lines.lock() {
            Ok(lines) => lines.iter().cloned().collect(),
            Err(_) => Vec::new(),
        };
        let joined = lines.join(" | ");
        let chars: Vec<char> = joined.chars().collect();
        let start = chars.len().saturating_sub(MAX_STDERR_PREVIEW_CHARS);
        chars[start..].iter().collect()
    }
}

struct ProcessState {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    readers: Vec<JoinHandle<()>>,
}

pub struct StdioMcpConnection {
    server: McpServerConfig,
    workspace_root: PathBuf,
    shared: Arc<Shared>,
    process: Mutex<ProcessState>,
    /// 串行化「发送 + 等待响应」（stdio 单管道）。
    request_lock: Mutex<()>,
    /// 串行化启动/握手/关闭；不与 `request_lock` 嵌套获取，避免请求等待与生命周期互锁。
    lifecycle_lock: Mutex<()>,
    init_result: Mutex<Option<Map<String, Value>>>,
    initialized: AtomicBool,
    closed: AtomicBool,
    next_request_id: AtomicU64,
}

impl StdioMcpConnection {
    pub fn new(server: &McpServerConfig, workspace_root: &Path) -> Self {
        Self {
            server: server.clone(),
            workspace_root: workspace_root.to_path_buf(),
            shared: Arc::new(Shared {
                server_name: server.name.clone(),
                pending: Mutex::new(Vec::new()),
                stderr_lines: Mutex::new(VecDeque::new()),
            }),
            process: Mutex::new(ProcessState {
                child: None,
                stdin: None,
                readers: Vec::new(),
            }),
            request_lock: Mutex::new(()),
            lifecycle_lock: Mutex::new(()),
            init_result: Mutex::new(None),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn server(&self) -> &McpServerConfig {
        &self.server
    }

    fn discover_capabilities(&self) -> Result<DiscoveredCapabilities, McpCallError> {
        let init_result = self.ensure_initialized()?;
        let discover = |method: &str, result_key: &str, capability: &str| {
            if !capability_declared(&init_result, capability) {
                // 服务器未在 initialize 响应中声明该能力：跳过请求（少一次进程内往返）。
                return Vec::new();
            }
            list_capability_pages(
                |method, params| match self.request(method, params, false) {
                    Ok(Value::Object(map)) => Ok(map),
                    Ok(_) => Err(McpClientError::new(format!(
                        "MCP 请求 {method} 返回结果必须是 JSON 对象。"
                    ))),
                    Err(error) => Err(McpClientError::new(error.message().to_string())),
                },
                method,
                result_key,
            )
        };
        Ok(DiscoveredCapabilities {
            tools: discover("tools/list", "tools", "tools"),
            resources: discover("resources/list", "resources", "resources"),
            prompts: discover("prompts/list", "prompts", "prompts"),
        })
    }

    /// 确保子进程存活且已完成握手，返回 initialize 结果。
    ///
    /// MCP 要求每个新进程先 initialize 再接受请求：进程退出或上次调用超时被回收后，
    /// 这里重启子进程并重发 initialize / notifications/initialized，而不是让这条连接
    /// 在会话内永久失效。
    fn ensure_initialized(&self) -> Result<Map<String, Value>, McpCallError> {
        if self.is_running() {
            return Ok(self.init_result_value());
        }
        let _guard = self.lock(&self.lifecycle_lock);
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpClientError::new("MCP 连接已关闭。").into());
        }
        if self.is_running() {
            return Ok(self.init_result_value());
        }
        // 回收可能残留的失活进程（超时路径已回收过时是空操作）。
        self.invalidate("MCP Server 正在重启，旧请求已作废。");
        self.start()?;
        let result = self.request(
            "initialize",
            &params(&[
                (
                    "protocolVersion",
                    Value::String(MCP_PROTOCOL_VERSION.to_string()),
                ),
                ("capabilities", Value::Object(Map::new())),
                (
                    "clientInfo",
                    serde_json::json!({"name": CLIENT_NAME, "version": CLIENT_VERSION}),
                ),
            ]),
            true,
        )?;
        self.notify("notifications/initialized", &Map::new())?;
        self.initialized.store(true, Ordering::SeqCst);
        // 非对象结果按空表处理（Python：`result if isinstance(result, dict) else {}`）。
        let result = match result {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        if let Ok(mut slot) = self.init_result.lock() {
            *slot = Some(result.clone());
        }
        Ok(result)
    }

    fn init_result_value(&self) -> Map<String, Value> {
        self.init_result
            .lock()
            .map(|slot| slot.clone().unwrap_or_default())
            .unwrap_or_default()
    }

    /// 子进程存活且握手完成时返回 `true`。
    fn is_running(&self) -> bool {
        if !self.initialized.load(Ordering::SeqCst) {
            return false;
        }
        let mut state = self.lock(&self.process);
        match state.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn start(&self) -> Result<(), McpCallError> {
        {
            let mut state = self.lock(&self.process);
            if let Some(child) = state.child.as_mut() {
                if matches!(child.try_wait(), Ok(None)) {
                    return Ok(());
                }
            }
        }

        let Some(command_text) = self.server.command.as_deref() else {
            return Err(McpClientError::new("stdio MCP Server 缺少 command。").into());
        };
        let program = resolve_command(command_text)?;

        let mut command = Command::new(program);
        command
            .args(&self.server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.server.env {
            command.env(name, value);
        }
        if !self.server.env.contains_key("MCP_WORKSPACE_ROOT") {
            command.env("MCP_WORKSPACE_ROOT", &self.workspace_root);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // 与 Python 的 CREATE_NO_WINDOW 一致：不弹控制台窗口。
            command.creation_flags(0x0800_0000);
        }

        let mut child = command
            .spawn()
            .map_err(|error| McpClientError::new(format!("启动 MCP Server 失败：{error}")))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // 新进程不继承旧的在途请求与旧日志。
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.clear();
        }
        if let Ok(mut lines) = self.shared.stderr_lines.lock() {
            lines.clear();
        }

        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            let shared = Arc::clone(&self.shared);
            readers.push(std::thread::spawn(move || read_stdout_loop(shared, stdout)));
        }
        if let Some(stderr) = stderr {
            let shared = Arc::clone(&self.shared);
            readers.push(std::thread::spawn(move || read_stderr_loop(shared, stderr)));
        }

        let mut state = self.lock(&self.process);
        state.child = Some(child);
        state.stdin = stdin;
        state.readers.extend(readers);
        Ok(())
    }

    /// 发送一个 JSON-RPC 请求并等待常驻读线程分发响应。
    ///
    /// 非握手请求先做一次存活检查：子进程退出或上次调用超时被回收后，这里会自动重启
    /// 并重新握手，单个 Server 不会因一次超时在会话内永久失效。返回原始 `result`
    /// 负载：是不是对象由各调用方按自己的文案判定（与 Python 同）。
    fn request(
        &self,
        method: &str,
        params: &Map<String, Value>,
        handshake: bool,
    ) -> Result<Value, McpCallError> {
        if !handshake {
            self.ensure_initialized()?;
        }
        let _guard = self.lock(&self.request_lock);
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        self.shared.put_pending(request_id, sender);
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        message.insert("id".to_string(), Value::from(request_id));
        message.insert("method".to_string(), Value::String(method.to_string()));
        message.insert("params".to_string(), Value::Object(params.clone()));

        let send_result = self.send_message(&message);
        if let Err(error) = send_result {
            self.shared.take_pending(request_id);
            return Err(error.into());
        }
        let response = self.await_response(&receiver);
        self.shared.take_pending(request_id);
        let response = response?;

        if response.contains_key("error") {
            return Err(McpClientError::new(json_rpc_failure_message(&response, method)).into());
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&self, method: &str, params: &Map<String, Value>) -> Result<(), McpCallError> {
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        message.insert("method".to_string(), Value::String(method.to_string()));
        message.insert("params".to_string(), Value::Object(params.clone()));
        self.send_message(&message).map_err(McpCallError::from)
    }

    fn send_message(&self, message: &Map<String, Value>) -> Result<(), McpClientError> {
        let frame = encode_frame(&Value::Object(message.clone()));
        let mut state = self.lock(&self.process);
        if state.child.is_none() {
            return Err(McpClientError::new("MCP Server 尚未启动。"));
        }
        let Some(stdin) = state.stdin.as_mut() else {
            return Err(McpClientError::new("MCP Server stdin 不可用。"));
        };
        stdin
            .write_all(&frame)
            .and_then(|_| stdin.flush())
            .map_err(|error| McpClientError::new(format!("写入 MCP Server 失败：{error}")))
    }

    /// 等待本次响应；超时则回收进程，并允许下一次调用重启。
    fn await_response(
        &self,
        receiver: &Receiver<Result<Map<String, Value>, McpClientError>>,
    ) -> Result<Map<String, Value>, McpCallError> {
        let timeout = Duration::from_secs(self.server.timeout_seconds.max(1) as u64);
        match receiver.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => Err(error.into()),
            Err(RecvTimeoutError::Timeout) => {
                // 超时说明进程状态不可信：结束它，后续调用会重启并重新握手。
                self.invalidate(&format!(
                    "MCP Server 响应超时，连接已回收：{}",
                    self.server.name
                ));
                Err(McpClientError::new(format!(
                    "MCP 请求超过 {} 秒：{}",
                    self.server.timeout_seconds, self.server.name
                ))
                .into())
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(McpClientError::new("MCP Server 响应通道已关闭。").into())
            }
        }
    }

    /// 让当前子进程失效：结束进程并唤醒所有在途请求。
    ///
    /// 只清理进程与在途状态，不改变「已关闭」标记：超时路径调用后可自动重启，
    /// `close()` 路径已先置位关闭标记，之后不再允许重连。
    fn invalidate(&self, reason: &str) {
        self.initialized.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = self.init_result.lock() {
            *slot = None;
        }
        let (child, stdin, readers) = {
            let mut state = self.lock(&self.process);
            (
                state.child.take(),
                state.stdin.take(),
                std::mem::take(&mut state.readers),
            )
        };
        self.shared
            .fail_pending(McpClientError::new(reason.to_string()));
        drop(stdin);
        drop(readers);
        terminate_process(child);
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl McpConnection for StdioMcpConnection {
    fn discover(&self) -> Result<DiscoveredCapabilities, McpCallError> {
        self.discover_capabilities()
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, McpCallError> {
        let payload = self.request(
            "tools/call",
            &params(&[
                ("name", Value::String(name.to_string())),
                ("arguments", Value::Object(arguments.clone())),
            ]),
            false,
        )?;
        object_payload(payload, "MCP Tool 返回结果必须是 JSON 对象。")
    }

    fn read_resource(&self, uri: &str) -> Result<Map<String, Value>, McpCallError> {
        let payload = self.request(
            "resources/read",
            &params(&[("uri", Value::String(uri.to_string()))]),
            false,
        )?;
        object_payload(payload, "MCP Resource 返回结果必须是 JSON 对象。")
    }

    fn get_prompt(
        &self,
        name: &str,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Map<String, Value>, McpCallError> {
        let payload = self.request(
            "prompts/get",
            &params(&[
                ("name", Value::String(name.to_string())),
                (
                    "arguments",
                    Value::Object(arguments.cloned().unwrap_or_default()),
                ),
            ]),
            false,
        )?;
        object_payload(payload, "MCP Prompt 返回结果必须是 JSON 对象。")
    }

    /// 结束子进程并拒绝后续调用；与超时后的自动重启区分。
    fn close(&self) {
        {
            let _guard = self.lock(&self.lifecycle_lock);
            self.closed.store(true, Ordering::SeqCst);
        }
        self.invalidate("MCP 连接已关闭。");
    }
}

fn object_payload(payload: Value, message: &str) -> Result<Map<String, Value>, McpCallError> {
    match payload {
        Value::Object(map) => Ok(map),
        _ => Err(McpClientError::new(message).into()),
    }
}

fn read_stdout_loop(shared: Arc<Shared>, stdout: ChildStdout) {
    let mut reader = BufReader::new(stdout);
    loop {
        let preview = || shared.stderr_preview();
        let outcome = read_frame(&mut reader, FrameKind::Response, preview);
        let message = match outcome {
            Ok(Some(message)) => message,
            Ok(None) => return,
            Err(error) => {
                // Python `_as_mcp_client_error`：非协议错误补上 Server 名便于多 Server 定位。
                let message = error.message().to_string();
                let error = if message.starts_with("MCP ") {
                    error
                } else {
                    McpClientError::new(format!(
                        "MCP Server {} 响应读取失败：{message}",
                        shared.server_name
                    ))
                };
                shared.fail_pending(error);
                return;
            }
        };
        if message.contains_key("method") {
            // Server → Client 的请求/通知：当前实现不处理，也不能与客户端请求 id 混淆。
            continue;
        }
        let Some(request_id) = message.get("id").and_then(|value| value.as_u64()) else {
            continue;
        };
        shared.dispatch(request_id, message);
    }
}

fn read_stderr_loop(shared: Arc<Shared>, stderr: ChildStderr) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        shared.push_stderr(text);
    }
}

/// 关闭管道并结束子进程；清理失败不得影响主流程。
fn terminate_process(child: Option<Child>) {
    let Some(mut child) = child else {
        return;
    };
    if child
        .try_wait()
        .map(|status| status.is_none())
        .unwrap_or(false)
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn params(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

/// 解析 stdio Server 命令到真实可执行路径。
///
/// Windows 的 CreateProcess 在 shell=False 且传入列表参数时，不总是按 PATHEXT 找到
/// `npx.cmd` 这类 shim；先自己按 PATH/PATHEXT 解析，可以保留非 shell 启动方式，
/// 同时兼容 Node/npm 等常见命令。解析不到时原样返回，交给 spawn 报错。
fn resolve_command(command: &str) -> Result<String, McpClientError> {
    let stripped = command.trim();
    if stripped.is_empty() {
        return Err(McpClientError::new("stdio MCP Server 缺少 command。"));
    }
    Ok(which(stripped).unwrap_or_else(|| stripped.to_string()))
}

fn which(name: &str) -> Option<String> {
    let path_env = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|item| !item.trim().is_empty())
            .map(|item| item.trim().to_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    let has_separator = name.contains('/') || name.contains('\\');
    let candidates: Vec<PathBuf> = if has_separator {
        vec![PathBuf::from(name)]
    } else {
        std::env::split_paths(&path_env)
            .map(|directory| directory.join(name))
            .collect()
    };

    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
        if !extensions.is_empty() && candidate.extension().is_none() {
            for extension in &extensions {
                let extended = PathBuf::from(format!("{}{extension}", candidate.display()));
                if extended.is_file() {
                    return Some(extended.to_string_lossy().to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MCP_TRANSPORT_STDIO;

    #[test]
    fn missing_command_is_reported() {
        let server = McpServerConfig::new("files", MCP_TRANSPORT_STDIO);
        let connection = StdioMcpConnection::new(&server, Path::new("."));
        let error = connection.discover().unwrap_err();
        assert_eq!(error.message(), "stdio MCP Server 缺少 command。");
    }

    #[test]
    fn closed_connection_refuses_new_requests() {
        let mut server = McpServerConfig::new("files", MCP_TRANSPORT_STDIO);
        server.command = Some("definitely-not-a-real-command-omnicrawl".to_string());
        let connection = StdioMcpConnection::new(&server, Path::new("."));
        connection.close();
        let error = connection.discover().unwrap_err();
        assert_eq!(error.message(), "MCP 连接已关闭。");
    }

    #[test]
    fn command_resolution_keeps_unresolvable_text() {
        assert_eq!(
            resolve_command("definitely-not-a-real-command-omnicrawl").expect("原样返回"),
            "definitely-not-a-real-command-omnicrawl"
        );
        assert!(resolve_command("   ").is_err());
    }

    #[test]
    fn stderr_preview_keeps_the_tail() {
        let shared = Shared {
            server_name: "files".to_string(),
            pending: Mutex::new(Vec::new()),
            stderr_lines: Mutex::new(VecDeque::new()),
        };
        for index in 0..(MAX_STDERR_TAIL_LINES + 10) {
            shared.push_stderr(&format!("line-{index}"));
        }
        let preview = shared.stderr_preview();
        assert!(preview.contains("line-73"), "{preview}");
        assert!(!preview.contains("line-0 |"), "{preview}");
    }
}
