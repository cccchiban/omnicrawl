//! 真实传输冒烟：本地 `TcpListener` 起最小 HTTP 服务，验证 `UreqWebTransport` 与 `fetcher`
//! 的重定向跟随、meta refresh、状态码处理与浏览器请求头。全程只打 127.0.0.1，不访问公网。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use omnicrawl_tui::tools::fetcher::{self, FetcherOptions};
use omnicrawl_tui::tools::web_transport::UreqWebTransport;

const PLAIN_BODY: &str =
    "<html><head><title>本地页</title></head><body><main><p>本地正文</p></main></body></html>";
const META_BODY: &str =
    "<html><body><meta http-equiv=\"refresh\" content=\"0; url=/plain\"></body></html>";

fn http_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// 起一个只服务 `requests` 次连接的最小 HTTP 服务，返回基地址与记录到的 User-Agent。
fn serve(requests: usize) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let mut user_agent = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header.trim().is_empty() {
                    break;
                }
                if header.to_lowercase().starts_with("user-agent:") {
                    user_agent = header.trim().to_string();
                }
            }
            captured.lock().expect("头锁").push(user_agent);

            let response = match path.as_str() {
                "/plain" => http_response(200, PLAIN_BODY),
                "/abs" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                "/rel" => "HTTP/1.1 301 Moved Permanently\r\nLocation: plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                "/meta" => http_response(200, META_BODY),
                _ => http_response(404, "not found"),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn options() -> FetcherOptions {
    FetcherOptions {
        transport: Arc::new(UreqWebTransport::new()),
        // 显式禁用代理：冒烟只打本机。
        proxy: Some(String::new()),
        ..FetcherOptions::default()
    }
}

fn arguments(url: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("urls".to_string(), Value::from(url));
    map.insert("parallel".to_string(), Value::from(false));
    map
}

#[test]
fn fetcher_walks_local_redirects_and_meta_refresh() {
    let (base, seen) = serve(12);
    let options = options();

    let plain = fetcher::fetcher(&options, &arguments(&format!("{base}/plain")))
        .expect("抓取本地页面应当成功");
    assert!(plain.contains("标题: 本地页"), "{plain}");
    assert!(plain.contains("内容: 本地正文"), "{plain}");
    assert!(
        plain.contains(&format!("最终地址: {base}/plain")),
        "{plain}"
    );

    let absolute =
        fetcher::fetcher(&options, &arguments(&format!("{base}/abs"))).expect("绝对重定向应当成功");
    assert!(
        absolute.contains(&format!("最终地址: {base}/plain")),
        "{absolute}"
    );

    let relative =
        fetcher::fetcher(&options, &arguments(&format!("{base}/rel"))).expect("相对重定向应当成功");
    assert!(
        relative.contains(&format!("最终地址: {base}/plain")),
        "{relative}"
    );

    let meta = fetcher::fetcher(&options, &arguments(&format!("{base}/meta")))
        .expect("meta refresh 应当成功");
    assert!(meta.contains(&format!("最终地址: {base}/plain")), "{meta}");
    assert!(meta.contains("内容: 本地正文"), "{meta}");

    let missing = fetcher::fetcher(&options, &arguments(&format!("{base}/missing")))
        .expect("抓取失败也会返回结果文本");
    assert!(
        missing.contains("失败: HTTP 404（目标返回错误状态码）"),
        "{missing}"
    );

    let agents = seen.lock().expect("头锁").clone();
    assert!(
        agents
            .iter()
            .any(|value| value.to_lowercase().contains("chrome/124")),
        "浏览器 UA 未随请求发出：{agents:?}"
    );
    assert!(
        agents.iter().all(|value| !value.is_empty()),
        "每个请求都应带 UA：{agents:?}"
    );
}

#[test]
fn multiple_urls_are_fetched_in_input_order() {
    let (base, _seen) = serve(12);
    let options = options();
    let mut map = Map::new();
    map.insert(
        "urls".to_string(),
        json!([
            format!("{base}/plain"),
            format!("{base}/missing"),
            format!("{base}/meta")
        ]),
    );
    map.insert("parallel".to_string(), Value::from(true));

    let output = fetcher::fetcher(&options, &map).expect("并行抓取应当成功");
    let positions: BTreeMap<usize, usize> = ["/plain", "/missing", "/meta"]
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let needle = format!(". {base}{path}");
            (output.find(&needle).unwrap_or(usize::MAX), index)
        })
        .collect();
    let mut ordered: Vec<(usize, usize)> = positions.into_iter().collect();
    ordered.sort_by_key(|(position, _)| *position);
    let order: Vec<usize> = ordered.into_iter().map(|(_, index)| index).collect();
    assert_eq!(order, vec![0, 1, 2], "结果应当按输入顺序回填：{output}");
}

/// 最小会话服务：`/login` 下发 cookie 并跳到 `/home`，两者都记录收到的 `Cookie` 头。
fn serve_session() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("克隆流"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                continue;
            }
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let mut cookie = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header.trim().is_empty() {
                    break;
                }
                if header.to_lowercase().starts_with("cookie:") {
                    cookie = header
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                }
            }
            captured
                .lock()
                .expect("头锁")
                .push((path.clone(), cookie));

            let response = match path.as_str() {
                "/login" => "HTTP/1.1 302 Found\r\nLocation: /home\r\nSet-Cookie: sid=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                "/home" | "/me" => http_response(200, PLAIN_BODY),
                _ => http_response(404, "not found"),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[test]
fn cookies_set_on_a_redirect_are_replayed_on_the_next_hop_and_request() {
    let (base, seen) = serve_session();
    let options = options();

    // 一次抓取内的重定向：/login 下发的 cookie 必须带到 /home。
    let login = fetcher::fetcher(&options, &arguments(&format!("{base}/login")))
        .expect("登录跳转应当成功");
    assert!(login.contains(&format!("最终地址: {base}/home")), "{login}");

    // 跨请求：同一个罐（`FetcherOptions` 里那份）在后续抓取里继续生效。
    let me = fetcher::fetcher(&options, &arguments(&format!("{base}/me"))).expect("应当成功");
    assert!(me.contains("状态: 200"), "{me}");

    let hops = seen.lock().expect("头锁").clone();
    let cookie_for = |path: &str| {
        hops.iter()
            .find(|(seen_path, _)| seen_path == path)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };
    assert_eq!(cookie_for("/login"), "", "第一跳还没有 cookie：{hops:?}");
    assert_eq!(cookie_for("/home"), "sid=abc", "重定向后必须回带：{hops:?}");
    assert_eq!(cookie_for("/me"), "sid=abc", "跨请求必须回带：{hops:?}");
}
