//! `PUT /settings/mcp` 的端到端测试：脚本化假内核 + 真实回环 HTTP + 隔离配置目录。
//!
//! 这个端点在 Python 侧没有对应物（Textual 工作台在进程内改配置），断言因此对着
//! 「Rust 设置面的既有约定」写：白名单字段、未传字段保持原值、写盘后立即重连、
//! 非法候选回滚而磁盘不留半份状态。
//!
//! 所有 Server 都带 `enabled = false`：发现流程会跳过未启用的 Server，用例因此既不
//! 拉起子进程也不发网络请求，只验证配置与接线本身。

use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use omnicrawl_api::{build_router_with_state, AgentService, ApiConfig, ApiState, ServiceOptions};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::tools::RegistryOptions;
use omnicrawl_host::turn::{RunnerOptions, TurnRunner};
use omnicrawl_ipc::bridge::KernelModelConfig;
use serde_json::{json, Value};

const TOKEN: &str = "parity-token";

/// 初始配置：一个 `[llm]` 段（验证写 MCP 不会碰其他段）与一个停用的 stdio Server。
const INITIAL_CONFIG: &str = r#"[llm]
model = "test-model"

[mcp]
enabled = false
default_timeout_seconds = 30

[mcp.servers.existing]
enabled = false
transport = "stdio"
command = "node"
timeout_seconds = 45
"#;

/// 假内核的输出：脚本喂完就阻塞等更多输入（避免「脚本读完 = 内核退出」的假信号）。
#[derive(Clone, Default)]
struct Script {
    inner: Arc<(Mutex<ScriptInner>, Condvar)>,
}

#[derive(Default)]
struct ScriptInner {
    buffer: VecDeque<u8>,
    closed: bool,
}

impl Script {
    fn push(&self, line: &str) {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("脚本未被毒化");
        inner.buffer.extend(line.as_bytes());
        inner.buffer.push_back(b'\n');
        wake.notify_all();
    }

    /// 回执一次 `session.settings`（服务端每次热更新都会发一帧并等回应）。
    fn acknowledge_session_settings(&self, id: u32) {
        self.push(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#));
    }
}

impl Read for Script {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("脚本未被毒化");
        loop {
            if !inner.buffer.is_empty() {
                let take = out.len().min(inner.buffer.len());
                for slot in out.iter_mut().take(take) {
                    *slot = inner.buffer.pop_front().unwrap_or_default();
                }
                return Ok(take);
            }
            if inner.closed {
                return Ok(0);
            }
            inner = wake.wait(inner).expect("等待脚本");
        }
    }
}

/// 宿主写出的帧记录器：用来断言「服务真的把新工具声明下发给了内核」。
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Recorder {
    fn methods(&self) -> Vec<String> {
        let bytes = self.0.lock().expect("记录缓冲未被毒化").clone();
        String::from_utf8(bytes)
            .expect("帧是 UTF-8")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|frame| {
                frame
                    .get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    /// 取最后一帧带 `method` 的参数，用来检查下发的工具声明。
    fn last_params(&self, method: &str) -> Option<Value> {
        let bytes = self.0.lock().expect("记录缓冲未被毒化").clone();
        String::from_utf8(bytes)
            .expect("帧是 UTF-8")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .rfind(|frame| frame.get("method").and_then(Value::as_str) == Some(method))
            .and_then(|frame| frame.get("params").cloned())
    }
}

impl Write for Recorder {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("记录缓冲未被毒化")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 一套「隔离配置目录 + 脚本化内核 + Agent 服务 + 回环 HTTP」。
struct Harness {
    address: SocketAddr,
    config_path: PathBuf,
    recorder: Recorder,
    script: Script,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let root = temp_dir(&format!("oc-api-mcp-{tag}"));
        let config_path = root.join("home").join(".OmniCrawl").join("config.toml");
        std::fs::create_dir_all(config_path.parent().expect("配置目录")).expect("建隔离配置目录");
        std::fs::write(&config_path, INITIAL_CONFIG).expect("写入初始配置");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("建立工作区");

        let script = Script::default();
        let recorder = Recorder::default();
        let client = KernelClient::from_streams(
            Box::new(BufReader::new(script.clone())),
            Box::new(recorder.clone()),
        );
        // 握手响应（id 1）：内核接受 `initialize`。
        script.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
        let mut runner = TurnRunner::new(
            client,
            runner_options(&workspace),
            &RegistryOptions::default(),
        )
        .expect("工具表应当建成");
        runner.handshake(&mut |_| {}).expect("握手应当成功");

        let mut options = ServiceOptions::new(workspace, model());
        // 隔离配置：只认注入的 home（配置固定读 `~/.OmniCrawl/config.toml`），
        // 不碰开发机上的真实配置。
        options.env = ConfigEnvironment::new(root.join("home"), std::env::consts::OS);
        let service = Arc::new(AgentService::with_runner(options, runner));
        let address = spawn(build_router_with_state(
            ApiState::new(config()).with_service(service),
        ));
        Self {
            address,
            config_path,
            recorder,
            script,
        }
    }

    fn put(&self, body: Value) -> Reply {
        request("PUT", self.address, "/api/v1/settings/mcp", Some(body))
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config_path).expect("读回配置")
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时目录");
    path
}

fn model() -> KernelModelConfig {
    KernelModelConfig {
        model: "test-model".to_string(),
        provider: String::new(),
        protocol: String::new(),
        base_url: String::new(),
        api_key_env: String::new(),
        user_agent: "omnicrawl-test".to_string(),
        system_prompt: "系统提示".to_string(),
        context_messages: Vec::new(),
        tools: Vec::new(),
        options: json!({}),
        request_timeout_seconds: None,
        context_window_tokens: 0,
        prompt_cache_capable: false,
        prompt_cache_identity: Default::default(),
        native_vision: false,
        request_retry_count: 1,
    }
}

fn runner_options(root: &Path) -> RunnerOptions {
    RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        attach_vision_images: false,
        client_name: "omnicrawl-api-test".to_string(),
        plugins: None,
        review: None,
        custody: None,
        prune: None,
        prompt: None,
    }
}

fn config() -> ApiConfig {
    ApiConfig::new(TOKEN, "127.0.0.1", 8765, Vec::new(), 300.0, 1).expect("合法配置")
}

fn spawn(app: axum::Router) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
    listener.set_nonblocking(true).expect("设为非阻塞");
    let address = listener.local_addr().expect("读取监听地址");
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听套接字");
            let _ = axum::serve(listener, app).await;
        });
    });
    address
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    fn data(&self) -> Value {
        self.json()["data"].clone()
    }
}

fn request(method: &str, address: SocketAddr, path: &str, body: Option<Value>) -> Reply {
    let url = format!("http://{address}{path}");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let response = match method {
        "PUT" => {
            let builder = agent
                .put(&url)
                .header("Authorization", &format!("Bearer {TOKEN}"));
            match body {
                Some(payload) => builder
                    .header("Content-Type", "application/json")
                    .send(payload.to_string())
                    .expect("请求应当到达服务端"),
                None => builder.send_empty().expect("请求应当到达服务端"),
            }
        }
        other => panic!("未覆盖的方法：{other}"),
    };
    let status = response.status().as_u16();
    let mut reader = response.into_body().into_reader();
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    Reply { status, body: text }
}

/// 单条 Server 增改：未传字段沿用原值，全局阈值与策略按请求更新，写盘后立即重连。
#[test]
fn upsert_keeps_unset_server_fields_and_reloads_runtime() {
    let harness = Harness::new("upsert");
    harness.script.acknowledge_session_settings(2);

    let reply = harness.put(json!({
        "enabled": true,
        "default_timeout_seconds": 60,
        "policy": {"audit_log_enabled": false, "allow_external_network_tools": true},
        "server": {"name": "existing", "args": ["--verbose"]},
    }));

    assert_eq!(reply.status, 200, "响应体：{}", reply.body);
    let data = reply.data();
    assert_eq!(data["applied"], json!(true));
    assert_eq!(data["requires_restart"], json!(false));
    assert_eq!(data["mcp"]["enabled"], json!(true));
    assert_eq!(data["mcp"]["default_timeout_seconds"], json!(60));
    // 只写了的策略字段变，其余保持原值。
    assert_eq!(
        data["mcp"]["policy"],
        json!({
            "require_confirmation_for_write": true,
            "require_confirmation_for_command": true,
            "allow_external_network_tools": true,
            "audit_log_enabled": false,
        })
    );
    // Server 的 command / transport / risk_level / timeout 都沿用原值；args 换成新的。
    assert_eq!(
        data["mcp"]["servers"],
        json!([{
            "name": "existing",
            "enabled": false,
            "transport": "stdio",
            "command": "node",
            "args": ["--verbose"],
            "url": "",
            "env": {},
            "headers": {},
            "timeout_seconds": 45,
            "risk_level": "restricted",
        }])
    );
    assert_eq!(data["tool_count"], json!(0));
    assert!(data["saved_path"]
        .as_str()
        .expect("带 saved_path")
        .ends_with("config.toml"));

    // 磁盘：`[mcp]` 是新值，`[llm]` 原样保留。
    let text = harness.config_text();
    assert!(text.contains("model = \"test-model\""), "配置：{text}");
    assert!(
        text.contains("default_timeout_seconds = 60"),
        "配置：{text}"
    );
    // 序列化器把非空数组写成多行，这里只断言取值落盘了。
    assert!(text.contains("\"--verbose\""), "配置：{text}");

    // 运行期：新工具声明确实下发给内核了。
    let methods = harness.recorder.methods();
    assert!(
        methods.iter().any(|method| method == "session.settings"),
        "出站方法：{methods:?}"
    );
    let params = harness
        .recorder
        .last_params("session.settings")
        .expect("取到 session.settings 参数");
    assert!(
        params["model"]["tools"].is_array(),
        "工具声明应当整体替换：{params}"
    );
}

/// 整表替换 + `delete_server`：两种写法都能改 Server 列表。
#[test]
fn full_replacement_and_delete_rewrite_the_server_list() {
    let harness = Harness::new("replace");
    harness.script.acknowledge_session_settings(2);

    let reply = harness.put(json!({
        "servers": {
            "alpha": {"enabled": false, "transport": "stdio", "command": "node", "timeout_seconds": 12},
            "beta": {"enabled": false, "transport": "streamable_http", "url": "https://example.com/mcp"},
        },
    }));
    assert_eq!(reply.status, 200, "响应体：{}", reply.body);
    let servers = reply.data()["mcp"]["servers"].clone();
    let names: Vec<&str> = servers
        .as_array()
        .expect("servers 是数组")
        .iter()
        .filter_map(|server| server["name"].as_str())
        .collect();
    assert_eq!(names, vec!["alpha", "beta"], "整表替换应丢掉原 Server");
    // 单条不给超时 → 落成全局阈值（此处未改，仍是 30）。
    assert_eq!(servers[1]["timeout_seconds"], json!(30));

    // 第二次热更新：删掉一条。
    harness.script.acknowledge_session_settings(3);
    let removed = harness.put(json!({"delete_server": "alpha"}));
    assert_eq!(removed.status, 200, "响应体：{}", removed.body);
    let removed_data = removed.data();
    let names: Vec<&str> = removed_data["mcp"]["servers"]
        .as_array()
        .expect("servers 是数组")
        .iter()
        .filter_map(|server| server["name"].as_str())
        .collect();
    assert_eq!(names, vec!["beta"]);
    assert!(!harness.config_text().contains("alpha"));
}

/// 非法候选：预检与回读校验都拒绝，且磁盘保持上一次的合法状态。
#[test]
fn invalid_candidates_are_rejected_and_disk_is_rolled_back() {
    let harness = Harness::new("invalid");

    // 名字含非法字符：预检直接拒绝。
    let bad_name = harness.put(json!({"server": {"name": "Bad Name"}}));
    assert_eq!(bad_name.status, 400, "响应体：{}", bad_name.body);
    assert_eq!(bad_name.json()["error"]["code"], json!("INVALID_SETTING"));
    assert_eq!(harness.config_text(), INITIAL_CONFIG);

    // 传输不在白名单：预检拒绝，并把允许值列出来。
    let bad_transport = harness.put(json!({"server": {"name": "gamma", "transport": "sse"}}));
    assert_eq!(bad_transport.status, 400);
    assert!(
        bad_transport.json()["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("stdio"),
        "响应体：{}",
        bad_transport.body
    );
    assert_eq!(harness.config_text(), INITIAL_CONFIG);

    // 预检放行、读取器才拒绝的候选（启用的 stdio 没有 command）：写盘后回读失败 → 回滚。
    let no_command = harness.put(json!({
        "server": {"name": "gamma", "enabled": true, "transport": "stdio", "command": ""},
    }));
    assert_eq!(no_command.status, 400, "响应体：{}", no_command.body);
    assert!(
        no_command.json()["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("command"),
        "响应体：{}",
        no_command.body
    );
    assert_eq!(
        harness.config_text(),
        INITIAL_CONFIG,
        "非法候选不应留在磁盘上"
    );
}

/// `apply_runtime = false`：只写盘，不动运行期，也不给内核发帧。
#[test]
fn apply_runtime_false_only_writes_the_config() {
    let harness = Harness::new("write-only");

    let reply = harness.put(json!({"enabled": true, "apply_runtime": false}));
    assert_eq!(reply.status, 200, "响应体：{}", reply.body);
    assert_eq!(reply.data()["applied"], json!(false));
    assert_eq!(reply.data()["mcp"]["enabled"], json!(true));
    assert!(harness.config_text().contains("enabled = true"));
    assert!(
        !harness
            .recorder
            .methods()
            .iter()
            .any(|method| method == "session.settings"),
        "只写盘时不应下发设置"
    );
}
