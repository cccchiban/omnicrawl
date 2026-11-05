//! 沙盒的出网通道：宿主侧的本地 HTTP 代理。
//!
//! 为什么需要它：AppContainer 不给网络能力时**连回环都被拒**（实测超时）；给了
//! `internetClient` 那就是"能上任何网"，审批就没有着力点了。实测出来的最优组合是
//! **不给任何网络能力 + 只加回环豁免**：容器在内核层出不了网，唯一通路是打到本机这个代理，
//! 于是"能不能出网"完全由代理的判定决定，而且绕过代理的裸 `socket()` 是**失败**而非泄漏。
//!
//! 代理形态是 HTTP 代理（`HTTP_PROXY`/`HTTPS_PROXY` 注入），因为 `curl`、`pip`、`npm`、
//! `git http(s)`、`cargo`、`go mod` 都尊重它。覆盖不到的是裸 TCP/UDP、SSH 等非 HTTP 协议——
//! 它们在容器里直接失败，这是清晰的边界，不是静默放行。
//!
//! 审批顺序严格照设计文档 §7.3：回环 → 已批准缓存 → 明确拒绝 → 策略（offline/ask/allowlist/online）。
//! 任何一步判定失败都**关闭**，不放行。

use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omnicrawl_config::features::sandbox::NetPolicy;

use super::{Sandbox, Verdict};

/// 单次读请求头的上限：够大以容纳常见请求，够小以挡住恶意长头。
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// 连接建立超时。沙盒内的客户端一般会自己重试，这里不设太长。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 审批请求：代理遇到 `ask` 策略、需要人（或上层）拍板时交出去的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub host: String,
    pub port: u16,
    pub scheme: &'static str,
}

impl ApprovalRequest {
    /// 审批面板与日志里的展示形式。
    pub fn target(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 一句话说明（给审批提示用）。
    pub fn describe(&self) -> String {
        format!("沙盒内的进程想连接 {}://{}", self.scheme, self.target())
    }
}

/// 审批回调：返回 `true` 表示放行。
///
/// 由宿主注入。**没有注入时 `ask` 一律拒绝**——网络审批的失败方向必须是关闭，
/// 否则"默认配置"等于静默放行。
pub type Approver = Arc<dyn Fn(&ApprovalRequest) -> bool + Send + Sync>;

/// 一个运行中的代理。
pub struct Proxy {
    port: u16,
    stop: Arc<AtomicBool>,
    /// 判定器：每次连接都回问 `Sandbox`，因此策略改动无需重启代理。
    sandbox: Arc<Sandbox>,
    approver: Option<Approver>,
    /// 本次会话已批准的 `host:port`。
    approved: Arc<Mutex<HashSet<String>>>,
    refused: Arc<Mutex<Vec<(String, String)>>>,
}

impl Proxy {
    /// 启动代理：绑定回环随机端口，后台线程接受连接。
    ///
    /// `approver` 为 `None` 时 `ask` 策略下的目标全部拒绝。
    pub fn start(sandbox: Arc<Sandbox>, approver: Option<Approver>) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("无法绑定回环代理端口：{error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("无法取得代理端口：{error}"))?
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let approved = Arc::new(Mutex::new(HashSet::new()));
        let refused = Arc::new(Mutex::new(Vec::new()));

        let proxy = Self {
            port,
            stop: Arc::clone(&stop),
            sandbox: Arc::clone(&sandbox),
            approver,
            approved: Arc::clone(&approved),
            refused: Arc::clone(&refused),
        };

        let shared = Arc::new(Shared {
            sandbox,
            approver: proxy.approver.clone(),
            approved,
            refused,
            stop: Arc::clone(&stop),
        });
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                if shared.stop.load(Ordering::Relaxed) {
                    break;
                }
                match incoming {
                    Ok(stream) => {
                        let shared = Arc::clone(&shared);
                        std::thread::spawn(move || {
                            if let Err(error) = shared.handle(stream) {
                                if error.kind() != ErrorKind::UnexpectedEof
                                    && error.kind() != ErrorKind::ConnectionReset
                                {
                                    shared.note_refusal("代理处理失败", &error.to_string());
                                }
                            }
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(proxy)
    }

    /// 代理监听端口（注入 `HTTP_PROXY` 用）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 注入沙盒环境的代理地址。
    pub fn proxy_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// 本次会话已批准的目标（诊断用）。
    pub fn approved_targets(&self) -> Vec<String> {
        let mut targets: Vec<String> = self
            .approved
            .lock()
            .map(|guard| guard.iter().cloned().collect())
            .unwrap_or_default();
        targets.sort();
        targets
    }

    /// 被拒绝的目标及原因（诊断与 `/sandbox` 展示用）。
    pub fn refusals(&self) -> Vec<(String, String)> {
        self.refused
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 后台线程共享的状态。
struct Shared {
    sandbox: Arc<Sandbox>,
    approver: Option<Approver>,
    approved: Arc<Mutex<HashSet<String>>>,
    refused: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
}

impl Shared {
    fn note_refusal(&self, target: &str, reason: &str) {
        if let Ok(mut guard) = self.refused.lock() {
            // 只留最近若干条，避免长会话里无限增长。
            if guard.len() >= 256 {
                guard.remove(0);
            }
            guard.push((target.to_string(), reason.to_string()));
        }
    }

    /// 判定一个目标能否出网。所有路径都经过这里，规则只有一处。
    fn decide(&self, host: &str, port: u16, scheme: &'static str) -> Result<(), String> {
        let target = format!("{host}:{port}");
        // 代理自己不能被代理：客户端把同一条连接指回来会成环。
        let verdict = self.sandbox.check_target(&target);
        match verdict {
            Verdict::Allow => Ok(()),
            Verdict::Deny(reason) => {
                self.note_refusal(&target, &reason);
                Err(reason)
            }
            Verdict::Ask(reason) => {
                // 会话内已批准过同一目标，直接放行。
                let remembered = self
                    .approved
                    .lock()
                    .map(|guard| guard.contains(&target))
                    .unwrap_or(false);
                if remembered {
                    return Ok(());
                }
                match &self.approver {
                    Some(approver) => {
                        let request = ApprovalRequest {
                            host: host.to_string(),
                            port,
                            scheme,
                        };
                        if approver(&request) {
                            if let Ok(mut guard) = self.approved.lock() {
                                guard.insert(target);
                            }
                            Ok(())
                        } else {
                            self.note_refusal(&target, "审批被拒绝");
                            Err(format!("{reason}（审批被拒绝）"))
                        }
                    }
                    // 没有审批通道时 ask 等于拒绝：默认配置不能变成静默放行。
                    None => {
                        self.note_refusal(&target, "ask 策略但没有审批通道");
                        Err(format!("{reason}（无审批通道，按拒绝处理）"))
                    }
                }
            }
        }
    }

    fn handle(&self, mut client: TcpStream) -> std::io::Result<()> {
        client.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        client.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let head = match read_head(&mut client)? {
            Some(head) => head,
            None => return Ok(()),
        };
        let request = match parse_request(&head) {
            Some(request) => request,
            None => {
                respond(&mut client, 400, "无法解析代理请求")?;
                return Ok(());
            }
        };

        match &request {
            Request::Connect { host, port } => {
                if let Err(reason) = self.decide(host, *port, "https") {
                    respond(&mut client, 403, &reason)?;
                    return Ok(());
                }
                let mut upstream = match TcpStream::connect((host.as_str(), *port)) {
                    Ok(stream) => stream,
                    Err(error) => {
                        respond(
                            &mut client,
                            502,
                            &format!("连接 {host}:{port} 失败：{error}"),
                        )?;
                        return Ok(());
                    }
                };
                upstream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
                upstream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
                client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
                client.flush()?;
                tunnel(client, upstream);
                Ok(())
            }
            Request::Absolute {
                method,
                uri,
                version,
                host,
                port,
                rest,
            } => {
                if let Err(reason) = self.decide(host, *port, "http") {
                    respond(&mut client, 403, &reason)?;
                    return Ok(());
                }
                let mut upstream = match TcpStream::connect((host.as_str(), *port)) {
                    Ok(stream) => stream,
                    Err(error) => {
                        respond(
                            &mut client,
                            502,
                            &format!("连接 {host}:{port} 失败：{error}"),
                        )?;
                        return Ok(());
                    }
                };
                upstream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
                upstream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
                // 转成 origin-form：代理只负责转发，不做内容改写。
                let origin = origin_form(uri);
                let mut forwarded = format!("{method} {origin} {version}\r\n").into_bytes();
                forwarded.extend_from_slice(rest.as_bytes());
                upstream.write_all(&forwarded)?;
                upstream.flush()?;
                tunnel(client, upstream);
                Ok(())
            }
        }
    }
}

/// 解析出来的代理请求。
#[derive(Debug)]
enum Request {
    /// `CONNECT host:port`（HTTPS）。
    Connect { host: String, port: u16 },
    /// 绝对 URI 形式（HTTP 代理）。
    Absolute {
        method: String,
        uri: String,
        version: String,
        host: String,
        port: u16,
        /// 除请求行外的其余头部与可能的正文起点。
        rest: String,
    },
}

/// 读完整请求头（到空行为止）。返回 `None` 表示对端在发头之前就关了。
fn read_head(client: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = client.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Ok(None);
        }
    }
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

/// 解析请求行与头部，得到目标。
fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    // CONNECT 的 uri 就是 host:port。
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&uri, 443)?;
        return Some(Request::Connect { host, port });
    }

    // 绝对 URI：http://host[:port]/path
    let (scheme, remainder) = uri.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") {
        // 非 http 的绝对 URI 不该出现在 HTTP 代理请求里。
        return None;
    }
    let (authority, path) = match remainder.find('/') {
        Some(index) => (&remainder[..index], &remainder[index..]),
        None => (remainder, "/"),
    };
    let (host, port) = split_host_port(authority, 80)?;
    let mut rest = String::new();
    for line in lines {
        rest.push_str(line);
        rest.push_str("\r\n");
    }
    let mut uri = String::from("http://");
    uri.push_str(authority);
    uri.push_str(path);
    Some(Request::Absolute {
        method,
        uri,
        version,
        host,
        port,
        rest,
    })
}

/// 拆 `host:port`；没有端口时用 `default_port`。
fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    // IPv6 字面量形如 `[::1]:443`。
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => default_port,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            Some((host.to_string(), port.parse().ok()?))
        }
        _ => Some((authority.to_string(), default_port)),
    }
}

/// 把绝对 URI 转成 origin-form（`/path?query`），转发给上游。
fn origin_form(uri: &str) -> String {
    let without_scheme = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    match without_scheme.find('/') {
        Some(index) => without_scheme[index..].to_string(),
        None => "/".to_string(),
    }
}

/// 双向泵数据，直到两端任一关闭。
fn tunnel(mut client: TcpStream, mut upstream: TcpStream) {
    let client_clone = match client.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let upstream_clone = match upstream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    };
    let up = std::thread::spawn(move || {
        let mut source = client_clone;
        let mut sink = upstream_clone;
        let _ = std::io::copy(&mut source, &mut sink);
        let _ = sink.shutdown(Shutdown::Write);
    });
    let mut source = upstream;
    let mut sink = client;
    let _ = std::io::copy(&mut source, &mut sink);
    let _ = sink.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// 回一条最小的错误响应。
fn respond(client: &mut TcpStream, status: u16, message: &str) -> std::io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let body = message.as_bytes();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes())?;
    client.write_all(body)?;
    client.flush()
}

/// 让 `[sandbox.net]` 的四种策略在代理层生效所需的最小信息。
///
/// 只是为了把配置读出来做展示与校验；判定统一走 [`Sandbox::check_target`]。
pub fn policy_label(policy: NetPolicy) -> &'static str {
    policy.as_str()
}

/// 代理地址是否指向本机（注入 `NO_PROXY` 时用）。
pub fn is_local_address(address: &SocketAddr) -> bool {
    address.ip().is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line_is_parsed_with_default_port() {
        let head = "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
        match parse_request(head) {
            Some(Request::Connect { host, port }) => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            other => panic!("应当解析成 CONNECT：{other:?}"),
        }
    }

    #[test]
    fn connect_without_port_uses_443() {
        let head = "CONNECT example.com HTTP/1.1\r\n\r\n";
        match parse_request(head) {
            Some(Request::Connect { host, port }) => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            other => panic!("应当解析成 CONNECT：{other:?}"),
        }
    }

    #[test]
    fn absolute_uri_is_parsed_into_host_and_path() {
        let head = "GET http://example.com:8080/a?b=1 HTTP/1.1\r\nHost: example.com\r\n\r\n";
        match parse_request(head) {
            Some(Request::Absolute {
                method,
                host,
                port,
                uri,
                ..
            }) => {
                assert_eq!(method, "GET");
                assert_eq!(host, "example.com");
                assert_eq!(port, 8080);
                assert_eq!(origin_form(&uri), "/a?b=1");
            }
            other => panic!("应当解析成绝对 URI：{other:?}"),
        }
    }

    #[test]
    fn absolute_uri_without_path_uses_root() {
        let head = "GET http://example.com HTTP/1.1\r\n\r\n";
        match parse_request(head) {
            Some(Request::Absolute { uri, port, .. }) => {
                assert_eq!(port, 80);
                assert_eq!(origin_form(&uri), "/");
            }
            other => panic!("应当解析成绝对 URI：{other:?}"),
        }
    }

    #[test]
    fn ipv6_authority_is_split_correctly() {
        let (host, port) = split_host_port("[::1]:8080", 80).expect("应当解析");
        assert_eq!(host, "::1");
        assert_eq!(port, 8080);
    }

    #[test]
    fn non_http_absolute_uri_is_rejected() {
        // `https://` 走 CONNECT，不该以绝对 URI 出现。
        assert!(parse_request("GET https://example.com/ HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn garbage_request_line_is_rejected() {
        assert!(parse_request("\r\n\r\n").is_none());
        assert!(parse_request("GET\r\n\r\n").is_none());
    }
}
