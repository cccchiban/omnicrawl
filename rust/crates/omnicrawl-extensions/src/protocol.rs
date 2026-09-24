//! `omnicrawl/extensions/plugin_protocol.py` 的 Rust 移植：Plugin Worker 协议客户端。
//!
//! stdin/stdout 用 NDJSON 承载 JSON-RPC 2.0。stdout 只能是协议消息；
//! 插件日志与 console 输出走 stderr，宿主只做收集，不当作协议输入。
//!
//! 与 Python 的差异：Rust 侧没有 `__file__`，runner 路径改由
//! [`WorkerLauncher::resolve`] 按固定搜索顺序定位（见 [`runner_search_dirs`]），
//! 不再依赖包内相对路径；[`node_runner_path`] 仍只负责拼文件名。
//! 关停一律强杀子进程，不再区分 `terminate` 与 `kill`——两者的可观察差别只在
//! Unix 的 SIGTERM 优雅窗口。
//!
//! 起点统一：CLI 的插件安装（冒烟测试）、宿主的运行期 Worker 启动与 TUI/API 的
//! 诊断都走同一份 [`WorkerLauncher`]，避免「CLI 能装、宿主起不来」这类分裂。

use crate::error::PluginProtocolError;
use crate::models::{
    parse_hook_result, HookResult, DEFAULT_MAX_MESSAGE_BYTES, HOOK_API_VERSION, OMNICRAWL_VERSION,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const NODE_RUNNER_FILENAME: &str = "node_runner.mjs";

/// runner 目录的环境变量名：显式指定时优先于所有自动搜索。
pub const RUNNER_DIR_ENV: &str = "OMNICRAWL_RUNNER_DIR";

/// Worker 环境变量白名单里明确屏蔽的键。
const BLOCKED_ENV_KEYS: [&str; 17] = [
    "OPENAI_API_KEY",
    "API_KEY",
    "ANTHROPIC_API_KEY",
    "DEEPSEEK_API_KEY",
    "AI_CONFIG_FILE",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
    "AZURE_OPENAI_API_KEY",
    "GITHUB_TOKEN",
    "NPM_TOKEN",
    "NODE_AUTH_TOKEN",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

const BLOCKED_ENV_SUBSTRINGS: [&str; 6] = [
    "API_KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "CREDENTIAL",
    "COOKIE",
];

/// Worker → Host 请求回调（如 custom.emit）；返回 `Err` 则回 error。
pub type HostRequestHandler =
    Arc<dyn Fn(&str, &Map<String, Value>) -> Result<Map<String, Value>, String> + Send + Sync>;

pub type StderrHandler = Arc<dyn Fn(&str) + Send + Sync>;

/// runner 所在目录下的 Node 入口文件名（目录由调用方决定，见模块文档）。
pub fn node_runner_path(runner_dir: &Path) -> PathBuf {
    runner_dir.join(NODE_RUNNER_FILENAME)
}

/// runner 目录的搜索顺序（去重后按序探测）：
///
/// 1. `OMNICRAWL_RUNNER_DIR`（显式指定，npm 启动器与打包脚本用它注入）；
/// 2. 可执行文件目录及其各级祖先下的 `extensions/`（Rust 二进制同级的载荷布局）；
/// 3. 同样祖先下的 `rust/assets/extensions/`（仓库检出里的单一来源，脱离 Python 包树）；
/// 4. 同样祖先下的 `omnicrawl/extensions/`（旧源码仓库与 PyInstaller 载荷布局）;
/// 5. 进程工作目录下的上述三级（开发联调时直接从仓库根运行）。
pub fn runner_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |dir: PathBuf| {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };

    if let Some(value) = std::env::var_os(RUNNER_DIR_ENV) {
        let text = value.to_string_lossy().trim().to_string();
        if !text.is_empty() {
            push(PathBuf::from(text));
        }
    }

    if let Ok(executable) = std::env::current_exe() {
        let mut current = executable.parent().map(Path::to_path_buf);
        while let Some(directory) = current {
            push(directory.join("extensions"));
            push(directory.join("rust").join("assets").join("extensions"));
            push(directory.join("omnicrawl").join("extensions"));
            current = directory.parent().map(Path::to_path_buf);
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        push(cwd.join("extensions"));
        push(cwd.join("rust").join("assets").join("extensions"));
        push(cwd.join("omnicrawl").join("extensions"));
    }

    dirs
}

/// 按 [`runner_search_dirs`] 找到的第一个真实存在的 `node_runner.mjs`。
pub fn resolve_runner_path() -> Option<PathBuf> {
    runner_search_dirs()
        .into_iter()
        .map(|directory| node_runner_path(&directory))
        .find(|candidate| candidate.is_file())
}

/// 搜索失败时的诊断文案：列出探测过的目录，便于用户一次性定位。
pub fn describe_runner_search() -> String {
    let dirs: Vec<String> = runner_search_dirs()
        .into_iter()
        .map(|directory| directory.to_string_lossy().to_string())
        .collect();
    let listed = if dirs.is_empty() {
        "（无候选目录）".to_string()
    } else {
        dirs.join("、")
    };
    format!(
        "未找到插件 Worker 入口 {NODE_RUNNER_FILENAME}；已探测：{listed}。\
         可用环境变量 {RUNNER_DIR_ENV} 指定它所在目录。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库检出里的 runner 归属：`rust/assets/extensions` 必须先于旧的 `omnicrawl/extensions`
    /// （后者只剩旧源码布局与 PyInstaller 载荷的兼容角色），否则脱钩后这里会悄悄读回 Python 包树。
    #[test]
    fn runner_search_prefers_rust_assets() {
        if std::env::var_os(RUNNER_DIR_ENV).is_some() {
            // 显式指定 runner 目录时优先它，那条路径不参与本次判定。
            return;
        }
        let found = resolve_runner_path().expect("仓库检出里应能找到插件 Worker 入口");
        assert!(
            found
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("rust/assets/extensions/node_runner.mjs"),
            "应优先命中 rust/assets/extensions：{found:?}"
        );
    }
}

/// Worker 启动所需的两个路径：Node 可执行文件与 `node_runner.mjs`。
///
/// 宿主与 CLI 共用这一份解析，保证「安装期冒烟」与「运行期启动」用的是同一个 runner。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLauncher {
    pub node_executable: String,
    pub runner_path: PathBuf,
}

impl WorkerLauncher {
    /// 显式指定两个路径（测试与打包脚本用）。
    pub fn new(node_executable: impl Into<String>, runner_path: impl Into<PathBuf>) -> Self {
        Self {
            node_executable: node_executable.into(),
            runner_path: runner_path.into(),
        }
    }

    /// 按搜索顺序解析；Node 与 runner 任一缺失都返回错误，错误文案可直接展示给用户。
    pub fn resolve() -> Result<Self, PluginProtocolError> {
        let node_executable = resolve_node_executable()?;
        let Some(runner_path) = resolve_runner_path() else {
            return Err(PluginProtocolError::new(describe_runner_search()));
        };
        Ok(Self {
            node_executable,
            runner_path,
        })
    }

    /// 转成 [`WorkerConfig`] 的两个可选字段（`WorkerConfig` 保持「不填就报错」的语义）。
    pub fn worker_config_fields(&self) -> (Option<String>, Option<PathBuf>) {
        (
            Some(self.node_executable.clone()),
            Some(self.runner_path.clone()),
        )
    }
}

/// 在 PATH 上查找可执行文件（`shutil.which` 的可用子集）。
pub fn which(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

/// 在 PATH 上查找 Node.js 可执行文件。
pub fn resolve_node_executable() -> Result<String, PluginProtocolError> {
    for name in ["node.exe", "node.cmd", "node"] {
        if let Some(path) = which(name) {
            return Ok(path);
        }
    }
    Err(PluginProtocolError::new(
        "未找到 Node.js，请安装 Node.js 20+ 并确保 node 在 PATH 中。",
    ))
}

/// 构造 Worker 环境变量白名单，避免把 API Key / Token 等敏感变量传给插件。
pub fn build_worker_env(base: Option<Vec<(String, String)>>) -> Vec<(String, String)> {
    let source = base.unwrap_or_else(current_environment);
    let mut env: Vec<(String, String)> = Vec::new();
    for (key, value) in source {
        let upper = key.to_uppercase();
        if BLOCKED_ENV_KEYS.contains(&key.as_str()) || BLOCKED_ENV_KEYS.contains(&upper.as_str()) {
            continue;
        }
        if BLOCKED_ENV_SUBSTRINGS
            .iter()
            .any(|part| upper.contains(part))
        {
            continue;
        }
        env.push((key, value));
    }
    // 明确提供最小运行信息，不包含凭据。
    env.push(("OMNICRAWL_PLUGIN_WORKER".to_string(), "1".to_string()));
    env.push((
        "OMNICRAWL_VERSION".to_string(),
        OMNICRAWL_VERSION.to_string(),
    ));
    env.push((
        "OMNICRAWL_HOOK_API_VERSION".to_string(),
        HOOK_API_VERSION.to_string(),
    ));
    env
}

fn current_environment() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.to_str()?.to_string(), value.to_str()?.to_string())))
        .collect()
}

/// 客户端构造参数（对应 Python 的关键字参数）。
#[derive(Clone, Default)]
pub struct WorkerConfig {
    pub plugin_root: PathBuf,
    pub plugin_name: String,
    pub timeout_ms: Option<i64>,
    pub max_message_bytes: Option<i64>,
    pub node_executable: Option<String>,
    pub runner_path: Option<PathBuf>,
    pub env: Option<Vec<(String, String)>>,
    pub on_stderr: Option<StderrHandler>,
    pub on_host_request: Option<HostRequestHandler>,
}

enum PendingPayload {
    Response(Map<String, Value>),
    Failure(PluginProtocolError),
}

struct WorkerInner {
    plugin_name: String,
    plugin_root: PathBuf,
    node_executable: String,
    runner_path: PathBuf,
    env: Vec<(String, String)>,
    default_timeout_ms: i64,
    max_message_bytes: i64,
    on_stderr: Option<StderrHandler>,
    on_host_request: Option<HostRequestHandler>,
    process: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<i64, Sender<PendingPayload>>>,
    next_id: AtomicI64,
    closed: AtomicBool,
}

/// 单个插件的常驻 Node Worker 客户端。
#[derive(Clone)]
pub struct PluginWorkerClient {
    inner: Arc<WorkerInner>,
}

impl PluginWorkerClient {
    pub fn new(config: WorkerConfig) -> Result<Self, PluginProtocolError> {
        let node_executable = match config.node_executable {
            Some(value) => value,
            None => resolve_node_executable()?,
        };
        Ok(Self {
            inner: Arc::new(WorkerInner {
                plugin_name: config.plugin_name,
                plugin_root: crate::path::resolve_path(&config.plugin_root),
                node_executable,
                runner_path: crate::path::resolve_path(&config.runner_path.unwrap_or_default()),
                env: build_worker_env(config.env),
                default_timeout_ms: config.timeout_ms.unwrap_or(2000),
                max_message_bytes: config
                    .max_message_bytes
                    .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES),
                on_stderr: config.on_stderr,
                on_host_request: config.on_host_request,
                process: Mutex::new(None),
                stdin: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicI64::new(1),
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn plugin_name(&self) -> &str {
        &self.inner.plugin_name
    }

    pub fn alive(&self) -> bool {
        if self.inner.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut guard = lock(&self.inner.process);
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn start(&self) -> Result<(), PluginProtocolError> {
        self.inner.start()
    }

    pub fn initialize(
        &self,
        params: &Map<String, Value>,
        timeout_ms: Option<i64>,
    ) -> Result<Map<String, Value>, PluginProtocolError> {
        let result = self.request("initialize", Some(params), timeout_ms)?;
        match result {
            Value::Object(map) => Ok(map),
            _ => Err(PluginProtocolError::new(format!(
                "initialize 返回值必须是对象：{}",
                self.inner.plugin_name
            ))),
        }
    }

    pub fn invoke_handler(
        &self,
        handler_id: &str,
        event: &Map<String, Value>,
        timeout_ms: i64,
    ) -> Result<Map<String, Value>, PluginProtocolError> {
        let mut params = Map::new();
        params.insert("handlerId".to_string(), Value::from(handler_id));
        params.insert("event".to_string(), Value::Object(event.clone()));
        let result = self.request("hook.invoke", Some(&params), Some(timeout_ms))?;
        if result.is_null() {
            let mut fallback = Map::new();
            fallback.insert("action".to_string(), Value::from("continue"));
            return Ok(fallback);
        }
        match result {
            Value::Object(map) => Ok(map),
            _ => Err(PluginProtocolError::new(format!(
                "hook.invoke 返回值必须是对象：{}/{}",
                self.inner.plugin_name, handler_id
            ))),
        }
    }

    pub fn cancel(&self, request_id: i64) {
        let mut params = Map::new();
        params.insert("requestId".to_string(), Value::from(request_id));
        // 取消失败时由上层超时终止进程。
        let _ = self.notify("hook.cancel", Some(&params));
    }

    pub fn ping(&self, timeout_ms: i64) -> Result<Map<String, Value>, PluginProtocolError> {
        let result = self.request("ping", Some(&Map::new()), Some(timeout_ms))?;
        match result {
            Value::Object(map) => Ok(map),
            _ => Ok(Map::new()),
        }
    }

    pub fn shutdown(&self, timeout_ms: i64) {
        if !self.alive() {
            self.close();
            return;
        }
        let _ = self.request("shutdown", Some(&Map::new()), Some(timeout_ms));
        self.close();
    }

    /// 关闭 Worker：关 stdin、回收子进程并让所有等待方拿到「已关闭」错误。
    pub fn close(&self) {
        self.inner.close();
    }

    pub fn request(
        &self,
        method: &str,
        params: Option<&Map<String, Value>>,
        timeout_ms: Option<i64>,
    ) -> Result<Value, PluginProtocolError> {
        if !self.alive() {
            self.start()?;
        }
        let request_id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel();
        lock(&self.inner.pending).insert(request_id, sender);
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::from("2.0"));
        message.insert("id".to_string(), Value::from(request_id));
        message.insert("method".to_string(), Value::from(method));
        message.insert(
            "params".to_string(),
            Value::Object(params.cloned().unwrap_or_default()),
        );
        if let Err(error) = self.send(&message) {
            lock(&self.inner.pending).remove(&request_id);
            return Err(error);
        }

        let configured = timeout_ms
            .filter(|value| *value != 0)
            .unwrap_or(self.inner.default_timeout_ms);
        let timeout = configured.max(50);
        let deadline = Instant::now() + Duration::from_millis(timeout as u64);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.cancel(request_id);
                // 宽限期后再强制回收。
                std::thread::sleep(Duration::from_millis(50));
                if lock(&self.inner.pending).contains_key(&request_id) {
                    self.close();
                }
                return Err(PluginProtocolError::new(format!(
                    "插件调用超时：{} {method} > {configured}ms",
                    self.inner.plugin_name
                )));
            }
            let wait = remaining.min(Duration::from_millis(100));
            match receiver.recv_timeout(wait) {
                Ok(PendingPayload::Response(payload)) => {
                    lock(&self.inner.pending).remove(&request_id);
                    if let Some(error) = payload.get("error") {
                        let message_text = match error {
                            Value::Object(map) if !map.is_empty() => match map.get("message") {
                                Some(value) if !value.is_null() => value_text(value),
                                _ => value_text(error),
                            },
                            Value::Null => "null".to_string(),
                            other => value_text(other),
                        };
                        return Err(PluginProtocolError::new(format!(
                            "插件调用失败：{} {method}：{message_text}",
                            self.inner.plugin_name
                        )));
                    }
                    return Ok(payload.get("result").cloned().unwrap_or(Value::Null));
                }
                Ok(PendingPayload::Failure(error)) => {
                    lock(&self.inner.pending).remove(&request_id);
                    return Err(error);
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !self.alive() {
                        return Err(PluginProtocolError::new(format!(
                            "插件 Worker 已退出：{}",
                            self.inner.plugin_name
                        )));
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    lock(&self.inner.pending).remove(&request_id);
                    return Err(PluginProtocolError::new(format!(
                        "插件 Worker 已关闭：{}",
                        self.inner.plugin_name
                    )));
                }
            }
        }
    }

    pub fn notify(
        &self,
        method: &str,
        params: Option<&Map<String, Value>>,
    ) -> Result<(), PluginProtocolError> {
        if !self.alive() {
            self.start()?;
        }
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::from("2.0"));
        message.insert("method".to_string(), Value::from(method));
        message.insert(
            "params".to_string(),
            Value::Object(params.cloned().unwrap_or_default()),
        );
        self.send(&message)
    }

    fn send(&self, message: &Map<String, Value>) -> Result<(), PluginProtocolError> {
        let line = serde_json::to_string(message).unwrap_or_default();
        let encoded = line.as_bytes();
        if encoded.len() as i64 > self.inner.max_message_bytes {
            return Err(PluginProtocolError::new(format!(
                "协议消息超过上限 {} 字节：{}",
                self.inner.max_message_bytes, self.inner.plugin_name
            )));
        }
        let mut guard = lock(&self.inner.stdin);
        let Some(stdin) = guard.as_mut() else {
            return Err(PluginProtocolError::new(format!(
                "插件 Worker stdin 不可用：{}",
                self.inner.plugin_name
            )));
        };
        let result = stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush());
        if let Err(error) = result {
            return Err(PluginProtocolError::new(format!(
                "写入插件 Worker 失败：{}：{error}",
                self.inner.plugin_name
            )));
        }
        Ok(())
    }
}

impl WorkerInner {
    fn start(self: &Arc<Self>) -> Result<(), PluginProtocolError> {
        if self.is_alive() {
            return Ok(());
        }
        if !self.runner_path.is_file() {
            return Err(PluginProtocolError::new(format!(
                "缺少 Node runner：{}",
                self.runner_path.display()
            )));
        }
        if !self.plugin_root.is_dir() {
            return Err(PluginProtocolError::new(format!(
                "插件根目录不存在：{}",
                self.plugin_root.display()
            )));
        }

        let mut command = Command::new(&self.node_executable);
        command
            .arg(&self.runner_path)
            .arg("--plugin-root")
            .arg(&self.plugin_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&self.plugin_root)
            .env_clear();
        for (key, value) in &self.env {
            command.env(key, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|error| {
            PluginProtocolError::new(format!(
                "启动插件 Worker 失败（{}）：{error}",
                self.plugin_name
            ))
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *lock(&self.stdin) = stdin;
        self.closed.store(false, Ordering::SeqCst);
        *lock(&self.process) = Some(child);

        if let Some(stdout) = stdout {
            let inner = Arc::clone(self);
            let name = self.plugin_name.clone();
            let _ = std::thread::Builder::new()
                .name(format!("plugin-stdout-{name}"))
                .spawn(move || inner.read_stdout_loop(stdout));
        }
        if let Some(stderr) = stderr {
            let handler = self.on_stderr.clone();
            let name = self.plugin_name.clone();
            let _ = std::thread::Builder::new()
                .name(format!("plugin-stderr-{name}"))
                .spawn(move || read_stderr_loop(handler, stderr));
        }
        Ok(())
    }

    fn is_alive(&self) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut guard = lock(&self.process);
        match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(mut stdin) = lock(&self.stdin).take() {
            let _ = stdin.flush();
        }
        let child = lock(&self.process).take();
        if let Some(mut child) = child {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.fail_all_pending(PluginProtocolError::new(format!(
            "插件 Worker 已关闭：{}",
            self.plugin_name
        )));
    }

    fn read_stdout_loop(self: &Arc<Self>, stdout: impl Read) {
        let mut reader = BufReader::new(stdout);
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => {
                    self.fail_all_pending(PluginProtocolError::new(format!(
                        "读取插件 stdout 失败：{error}"
                    )));
                    return;
                }
            }
            let line = String::from_utf8_lossy(&buffer).trim().to_string();
            if line.is_empty() {
                continue;
            }
            if line.len() as i64 > self.max_message_bytes {
                self.fail_all_pending(PluginProtocolError::new(format!(
                    "插件 stdout 消息过大：{}",
                    self.plugin_name
                )));
                self.close();
                return;
            }
            let payload = match serde_json::from_str::<Value>(&line) {
                Ok(Value::Object(map)) => map,
                Ok(_) => {
                    self.fail_all_pending(PluginProtocolError::new(format!(
                        "插件协议消息必须是对象：{}",
                        self.plugin_name
                    )));
                    self.close();
                    return;
                }
                Err(_) => {
                    self.fail_all_pending(PluginProtocolError::new(format!(
                        "插件 stdout 非 JSON 协议消息：{}",
                        self.plugin_name
                    )));
                    self.close();
                    return;
                }
            };
            self.dispatch_incoming(payload);
        }
        if !self.closed.load(Ordering::SeqCst) {
            self.fail_all_pending(PluginProtocolError::new(format!(
                "插件 Worker stdout 已关闭：{}",
                self.plugin_name
            )));
        }
    }

    fn dispatch_incoming(&self, payload: Map<String, Value>) {
        // Worker → Host 的请求（如 custom.emit）。
        if payload.contains_key("method")
            && payload.contains_key("id")
            && !payload.contains_key("result")
            && !payload.contains_key("error")
        {
            let method = match payload.get("method") {
                Some(Value::String(text)) => text.clone(),
                _ => String::new(),
            };
            let request_id = match payload.get("id") {
                Some(Value::Number(number)) => number.as_i64(),
                _ => None,
            };
            let Some(request_id) = request_id else {
                return;
            };
            let params = match payload.get("params") {
                Some(Value::Object(map)) => map.clone(),
                _ => Map::new(),
            };
            let Some(handler) = self.on_host_request.clone() else {
                let mut error = Map::new();
                error.insert("code".to_string(), Value::from(-32601));
                error.insert(
                    "message".to_string(),
                    Value::from(format!("Host method not handled: {method}")),
                );
                self.send_response(request_id, None, Some(error));
                return;
            };
            match handler(&method, &params) {
                Ok(result) => {
                    let result = if result.is_empty() {
                        let mut fallback = Map::new();
                        fallback.insert("ok".to_string(), Value::from(true));
                        fallback
                    } else {
                        result
                    };
                    self.send_response(request_id, Some(Value::Object(result)), None);
                }
                Err(error) => {
                    let mut payload = Map::new();
                    payload.insert("code".to_string(), Value::from(-32000));
                    payload.insert("message".to_string(), Value::from(error));
                    self.send_response(request_id, None, Some(payload));
                }
            }
            return;
        }

        let Some(request_id) = payload.get("id") else {
            return;
        };
        let Some(request_id) = request_id.as_i64() else {
            self.fail_all_pending(PluginProtocolError::new(format!(
                "未知 request id：{}",
                self.plugin_name
            )));
            self.close();
            return;
        };
        let sender = lock(&self.pending).get(&request_id).cloned();
        let Some(sender) = sender else {
            // 重复响应或未知 ID：协议失败。
            self.fail_all_pending(PluginProtocolError::new(format!(
                "收到未知/重复 request id={request_id}：{}",
                self.plugin_name
            )));
            self.close();
            return;
        };
        let _ = sender.send(PendingPayload::Response(payload));
    }

    fn send_response(
        &self,
        request_id: i64,
        result: Option<Value>,
        error: Option<Map<String, Value>>,
    ) {
        let mut message = Map::new();
        message.insert("jsonrpc".to_string(), Value::from("2.0"));
        message.insert("id".to_string(), Value::from(request_id));
        match error {
            Some(payload) => {
                message.insert("error".to_string(), Value::Object(payload));
            }
            None => {
                message.insert("result".to_string(), result.unwrap_or(Value::Null));
            }
        }
        let line = serde_json::to_string(&message).unwrap_or_default();
        if line.len() as i64 > self.max_message_bytes {
            return;
        }
        let mut guard = lock(&self.stdin);
        if let Some(stdin) = guard.as_mut() {
            let _ = stdin
                .write_all(line.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush());
        }
    }

    fn fail_all_pending(&self, error: PluginProtocolError) {
        let pending: Vec<(i64, Sender<PendingPayload>)> = lock(&self.pending).drain().collect();
        for (_, sender) in pending {
            let _ = sender.send(PendingPayload::Failure(error.clone()));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// stderr 只做收集：回调失败不中断 Worker 输出读取。
fn read_stderr_loop(handler: Option<StderrHandler>, stderr: impl Read) {
    let mut reader = BufReader::new(stderr);
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let text = String::from_utf8_lossy(&buffer)
            .trim_end_matches('\n')
            .to_string();
        if text.is_empty() {
            continue;
        }
        if let Some(callback) = &handler {
            callback(&text);
        }
    }
}

/// 调用 Handler 并解析为 `HookResult`，耗时按真实往返计算。
pub fn invoke_and_parse(
    client: &PluginWorkerClient,
    handler_key: &str,
    handler_id: &str,
    event: &Map<String, Value>,
    timeout_ms: i64,
) -> Result<HookResult, PluginProtocolError> {
    let started = Instant::now();
    let raw = client.invoke_handler(handler_id, event, timeout_ms)?;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    parse_hook_result(Some(&Value::Object(raw)), handler_key, elapsed_ms)
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value).unwrap_or_default(),
    }
}
