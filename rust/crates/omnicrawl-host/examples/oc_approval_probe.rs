//! 临时探针：真机验证 `ask` 档网络审批的端到端行为。
//!
//! 三件事必须同时成立才算"接通"：
//! 1. 有界面时 `ask` 会问，批准后请求真的出去；
//! 2. 拒绝后请求真的不通；
//! 3. **没有界面时立即拒绝**（不空等超时，也不静默放行）。
use std::sync::Arc;
use std::time::{Duration, Instant};

use omnicrawl_config::features::sandbox::{NetPolicy, SandboxConfig, SandboxMode};
use omnicrawl_host::sandbox::Sandbox;

fn config(policy: NetPolicy) -> SandboxConfig {
    SandboxConfig {
        enabled: true,
        mode: SandboxMode::Landlock,
        net_policy: policy,
        net_approval_timeout_seconds: 3,
        ..SandboxConfig::default()
    }
}

fn main() {
    let base = std::env::temp_dir().join("oc-approval-probe");
    let _ = std::fs::create_dir_all(&base);
    let sandbox = Arc::new(Sandbox::new(config(NetPolicy::Ask), &base, &base));

    // 拉起代理（`ensure_network` 是唯一入口，它会把审批站接上）。
    let status = sandbox.ensure_network(None);
    println!("net 状态: {status:?}");
    let port = match status {
        omnicrawl_host::sandbox::NetStatus::Proxy(port) => port,
        other => {
            println!("没有代理，后续无从验证: {other:?}");
            return;
        }
    };
    let proxy_url = format!("http://127.0.0.1:{port}");
    println!("代理: {proxy_url}");

    let station = sandbox.approvals();

    println!("--- 情形 1：没有界面接管 → 应立即拒绝 ---");
    let started = Instant::now();
    let allowed = proxy_get(&proxy_url, "http://example.com/");
    println!(
        "  结果: {allowed}，耗时 {:?}（应远小于 3 秒超时）",
        started.elapsed()
    );
    println!("  拒绝计数: {}", station.refusals());

    println!("--- 情形 2：有界面并批准 → 应放行 ---");
    station.set_interactive(true);
    let approver = std::thread::spawn({
        let station = Arc::clone(&station);
        move || {
            // 等请求进队列后批准。
            let deadline = Instant::now() + Duration::from_secs(10);
            while station.pending().is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let pending = station.pending();
            if let Some(first) = pending.first() {
                println!("  界面看到: {}（剩余 {} 秒）", first.target(), first.remaining_seconds(Instant::now()));
                station.answer(first.id, true);
            } else {
                println!("  界面没看到请求");
            }
        }
    });
    let allowed = proxy_get(&proxy_url, "http://example.com/");
    approver.join().ok();
    println!("  结果: {allowed}（应为 true/false 之一，看是否能出网）");

    println!("--- 情形 3：有界面但拒绝 → 应不通 ---");
    let denier = std::thread::spawn({
        let station = Arc::clone(&station);
        move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while station.pending().is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Some(first) = station.pending().first() {
                station.answer(first.id, false);
            }
        }
    });
    let allowed = proxy_get(&proxy_url, "http://example.org/");
    denier.join().ok();
    println!("  结果: {allowed}（应 false）");

    println!("--- 情形 4：有界面但没人回答 → 超时拒绝 ---");
    let started = Instant::now();
    let allowed = proxy_get(&proxy_url, "http://example.net/");
    println!(
        "  结果: {allowed}，耗时 {:?}（应接近 3 秒超时）",
        started.elapsed()
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// 用一个极简的 HTTP 客户端走代理，返回是否拿到 `200`。
///
/// 不依赖 curl：只发 `GET` 的绝对 URI 形式，读第一行状态码。
fn proxy_get(proxy: &str, target: &str) -> bool {
    use std::io::{Read, Write};
    let host_port = proxy.trim_start_matches("http://");
    let mut stream = match std::net::TcpStream::connect(host_port) {
        Ok(stream) => stream,
        Err(error) => {
            println!("  连代理失败: {error}");
            return false;
        }
    };
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        target
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("")
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buffer = [0u8; 256];
    match stream.read(&mut buffer) {
        Ok(0) => false,
        Ok(read) => {
            let text = String::from_utf8_lossy(&buffer[..read]);
            println!("  代理回: {}", text.lines().next().unwrap_or("").trim());
            text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")
        }
        Err(_) => false,
    }
}
