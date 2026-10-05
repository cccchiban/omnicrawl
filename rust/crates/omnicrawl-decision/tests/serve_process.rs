//! 常驻服务的真实进程测试：拉起 `omnicrawl-decision serve`，确认端口真的监听了。
//!
//! 这条路径无法用进程内状态覆盖——「进程真的起来并绑定端口」是它与宿主唯一的交互点，
//! 因此必须走真实子进程。测试用隔离的配置根（临时 HOME + 决策配置）驱动服务自身读配置。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 隔离的配置根：写一份启用了 `[api]` 的 `decision_models.toml`。
fn isolated_home(tag: &str, port: u16) -> PathBuf {
    let root = std::env::temp_dir().join(format!("oc-decision-serve-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let user = root.join(".OmniCrawl");
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    let config = format!(
        r#"version = 1
default_key = "main"

[channels.main]
name = "测试渠道"
mode = "jev"
base_url = "http://127.0.0.1:1"
api_key = "test-key"
api_key_env = "JEV_API_KEY"
model = "jev-test"
enabled = true

[api]
enabled = true
host = "127.0.0.1"
port = {port}
"#
    );
    std::fs::write(user.join("decision_models.toml"), config).expect("写决策配置");
    root
}

/// 隔离的配置根：`[api]` 未启用（`serve` 必须拒绝启动）。
fn isolated_home_disabled(tag: &str, port: u16) -> PathBuf {
    let root = std::env::temp_dir().join(format!("oc-decision-serve-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let user = root.join(".OmniCrawl");
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    let config = format!(
        r#"version = 1
default_key = "main"

[channels.main]
name = "测试渠道"
mode = "jev"
base_url = "http://127.0.0.1:1"
api_key = "test-key"
api_key_env = "JEV_API_KEY"
model = "jev-test"
enabled = true

[api]
enabled = false
host = "127.0.0.1"
port = {port}
"#
    );
    std::fs::write(user.join("decision_models.toml"), config).expect("写决策配置");
    root
}

/// 找一个空闲端口（绑定后立刻释放；测试内无并发争用）。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    listener.local_addr().expect("地址").port()
}

fn binary() -> PathBuf {
    // 集成测试的产物与二进制同目录（cargo 约定）；找不到时退回 PATH。
    let mut path = std::env::current_exe().expect("当前测试可执行文件");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let candidate = path.join(if cfg!(windows) {
        "omnicrawl-decision.exe"
    } else {
        "omnicrawl-decision"
    });
    if candidate.is_file() {
        return candidate;
    }
    PathBuf::from("omnicrawl-decision")
}

/// 等到端口监听；超时返回 false。
fn wait_listening(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 一次最小 HTTP 请求，返回（状态码，正文）。
fn request(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("连接服务");
    let head = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).expect("写请求");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("读响应");
    let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((reply.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

fn spawn_serve(home: &Path) -> std::process::Child {
    Command::new(binary())
        .arg("serve")
        // 用隔离的 HOME 驱动服务读那份临时配置（与 Python 的 `Path.home()` 同源）。
        .env("USERPROFILE", home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("拉起决策接口服务")
}

/// 配置到位时 `serve` 真的监听，所有端点都可直接调用。
#[test]
fn serve_listens_and_serves_without_credentials() {
    let port = free_port();
    let home = isolated_home("ok", port);
    let mut child = spawn_serve(&home);
    let listening = wait_listening(port, Duration::from_secs(10));
    if !listening {
        let _ = child.kill();
        let _ = child.wait();
        panic!("决策接口未在 10 秒内监听 127.0.0.1:{port}");
    }

    let (health_status, health_body) = request(port, "/health");
    assert_eq!(health_status, 200, "{health_body}");
    assert!(health_body.contains("omnicrawl-decision"), "{health_body}");

    // 不带任何凭据：直接拿到状态。
    let (status, body) = request(port, "/v1/status");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ready\":true"), "{body}");
    // 渠道密钥不该出现在响应里。
    assert!(!body.contains("test-key"), "{body}");

    let _ = child.kill();
    let _ = child.wait();
}

/// 未启用 `[api]` 时拒绝启动（且不监听）。
#[test]
fn serve_refuses_to_start_when_the_api_is_disabled() {
    let port = free_port();
    let home = isolated_home_disabled("disabled", port);
    let mut child = Command::new(binary())
        .arg("serve")
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("拉起决策接口服务");
    let status = child.wait().expect("等待退出");
    assert!(!status.success(), "未启用时进程必须失败退出");
    assert!(!wait_listening(port, Duration::from_secs(1)), "不该监听");
}
