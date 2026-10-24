//! 缓存协商与 HSTS 的接线验证：本地 `TcpListener` 起最小 HTTP 服务，只打回环地址。
//!
//! 这两个特性都容易「模块单测全过、接线没接上」：单测证明算法对，这里证明请求头真的发出去了、
//! 304 真的被还原了、明文声明真的没被学习。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use omnicrawl_tui::tools::fetcher::{self, FetcherOptions};
use omnicrawl_tui::tools::web_transport::UreqWebTransport;

use serde_json::{json, Map, Value};

const BODY: &str =
    "<html><head><title>缓存页</title></head><body><main><p>缓存正文内容测试</p></main></body></html>";

fn options() -> FetcherOptions {
    FetcherOptions {
        transport: Arc::new(UreqWebTransport::new()),
        // 显式禁用代理：只打本机。
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

/// 读一次请求，返回（路径，请求头）。
fn read_request(stream: &std::net::TcpStream) -> Option<(String, Vec<(String, String)>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return None;
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Some((path, headers))
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 会校验验证器的服务：`/page` 首次回 200 + ETag，带对验证器的请求回 304（无正文、无
/// `content-type`——这就是真实 304 的形态）。
fn serve_conditional() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let Some((path, headers)) = read_request(&stream) else {
                continue;
            };
            let response = match path.as_str() {
                // robots.txt 取回是抓取的前置条件，必须能正常应答。
                "/robots.txt" => {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                }
                "/page" => {
                    let validator = header_of(&headers, "if-none-match").map(str::to_string);
                    captured.lock().expect("头锁").push(validator.clone());
                    if validator.as_deref() == Some("\"v1\"") {
                        // 真实的 304：没有正文，也没有 content-type。
                        "HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nContent-Length: 0\r\n\
Connection: close\r\n\r\n"
                            .to_string()
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
ETag: \"v1\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                            BODY.len()
                        )
                    }
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// 304 必须还原成完整响应：抓取工具拿到的是「服务器认可的当前内容」，而不是空页面。
///
/// 只测 `http_cache` 无法证明条件头真的发出去了，也证明不了传输层真的走了还原分支。
#[test]
fn a_validated_copy_is_restored_when_the_server_answers_304() {
    let (base, seen) = serve_conditional();
    let options = options();
    let url = format!("{base}/page");

    let first = fetcher::fetcher(&options, &arguments(&url)).expect("首次抓取应当成功");
    assert!(first.contains("状态: 200"), "{first}");
    assert!(first.contains("标题: 缓存页"), "{first}");
    assert!(first.contains("缓存正文内容测试"), "{first}");

    // 第二次：条件头必须带上，304 必须被还原成 200 + 完整正文。
    let second = fetcher::fetcher(&options, &arguments(&url)).expect("协商命中应当成功");
    assert!(second.contains("状态: 200"), "304 应还原成 200：{second}");
    assert!(
        second.contains("标题: 缓存页"),
        "还原路径必须保留正文：{second}"
    );
    assert!(second.contains("缓存正文内容测试"), "{second}");

    let validators = seen.lock().expect("头锁").clone();
    assert_eq!(
        validators.first().map(|value| value.clone()),
        Some(None),
        "首次请求不该带 If-None-Match：{validators:?}"
    );
    assert_eq!(
        validators.get(1).map(|value| value.clone()),
        Some(Some("\"v1\"".to_string())),
        "第二次请求必须带缓存里的验证器：{validators:?}"
    );
}

/// 明文响应里的 `Strict-Transport-Security` 必须被忽略。
///
/// 实现若在明文上也学习，第二次请求会被升级到 https 打到一个明文端口上直接失败——
/// 而且这条策略一旦写进进程状态，同主机的所有后续请求都会坏掉。
#[test]
fn a_plaintext_strict_transport_security_header_is_not_learned() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let hits = Arc::new(Mutex::new(0usize));
    let captured = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let Some((path, _headers)) = read_request(&stream) else {
                continue;
            };
            let response = if path == "/robots.txt" {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            } else {
                *captured.lock().expect("计数锁") += 1;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
Strict-Transport-Security: max-age=31536000\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                    BODY.len()
                )
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    let base = format!("http://127.0.0.1:{port}");
    let options = options();

    for round in 0..2 {
        let output = fetcher::fetcher(&options, &arguments(&format!("{base}/page")))
            .unwrap_or_else(|error| panic!("第 {round} 次抓取应当成功：{}", error.message));
        assert!(output.contains("状态: 200"), "第 {round} 次：{output}");
    }
    assert_eq!(
        *hits.lock().expect("计数锁"),
        2,
        "明文响应里的 STS 头不该让第二次请求被升级"
    );
}

/// 已学习到的策略必须让明文请求升级，且**升级失败也不回退**。
///
/// 用 `127.0.0.9` 与其它用例隔离：这里会往进程级状态里写策略。
/// 目标端口上是明文 HTTP 服务，TLS 握手必然失败——正好用来证明没有偷偷退回明文。
#[test]
fn an_hsts_host_is_never_downgraded_to_plaintext() {
    let listener = TcpListener::bind("127.0.0.9:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    // 只要连上就说明有人退回了明文（TLS 握手不会走到这里）。
    let plaintext_hits = Arc::new(Mutex::new(0usize));
    let captured = plaintext_hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            if read_request(&stream).is_some() {
                *captured.lock().expect("计数锁") += 1;
            }
            let mut stream = stream;
            let _ = stream.write_all(
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".as_bytes(),
            );
        }
    });

    // 从 https 响应「学习」到策略：这是唯一合法的学习来源。
    omnicrawl_tui::tools::hsts::record(
        "https://127.0.0.9/",
        &[(
            "strict-transport-security".to_string(),
            "max-age=31536000".to_string(),
        )],
    );

    let options = options();
    let output = fetcher::fetcher(
        &options,
        &arguments(&format!("http://127.0.0.9:{port}/page")),
    )
    .expect("抓取失败也会返回结果文本");
    assert!(
        output.contains("失败:"),
        "升级到 https 后握手应当失败：{output}"
    );
    assert!(
        !output.contains("状态: 200"),
        "绝不能回退到明文成功：{output}"
    );
    assert_eq!(
        *plaintext_hits.lock().expect("计数锁"),
        0,
        "升级后不该有任何明文请求到达目标"
    );
}

/// 未声明的站点不受影响：没有策略时明文请求照常走，不能被误升级。
#[test]
fn a_host_without_a_policy_still_uses_plaintext() {
    assert!(
        omnicrawl_tui::tools::hsts::upgrade("http://127.0.0.10:8080/x").is_none(),
        "没有策略的主机不该被升级"
    );
    let _ = json!(null);
}

/// 带着 cookie 的请求也必须能命中并还原自己的缓存副本。
///
/// 这条钉的是「存」与「查」的键必须同源：存的时候若用去掉 cookie 的键、查的时候用带上
/// cookie 的键，两边永远对不上——表现为协商彻底失效（每次都回 200），而且不报任何错。
#[test]
fn a_request_with_a_cookie_still_validates_its_own_copy() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let sent_validators: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = sent_validators.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let Some((path, headers)) = read_request(&stream) else {
                continue;
            };
            let response = match path.as_str() {
                "/robots.txt" => {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                }
                "/login" => "HTTP/1.1 302 Found\r\nLocation: /private\r\n\
Set-Cookie: sid=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
                "/private" => {
                    captured
                        .lock()
                        .expect("头锁")
                        .push(header_of(&headers, "if-none-match").map(str::to_string));
                    if header_of(&headers, "if-none-match") == Some("\"p1\"") {
                        "HTTP/1.1 304 Not Modified\r\nETag: \"p1\"\r\nContent-Length: 0\r\n\
Connection: close\r\n\r\n"
                            .to_string()
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
ETag: \"p1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
                            BODY.len()
                        )
                    }
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    let base = format!("http://127.0.0.1:{port}");
    let options = options();

    // 第一次经 /login 拿到 cookie 并落到 /private（这次会写入缓存）。
    let first =
        fetcher::fetcher(&options, &arguments(&format!("{base}/login"))).expect("登录跳转应当成功");
    assert!(first.contains("状态: 200"), "{first}");

    // 第二次带着同一个 cookie 直接抓 /private：必须带上验证器并命中 304。
    let second =
        fetcher::fetcher(&options, &arguments(&format!("{base}/private"))).expect("协商应当成功");
    assert!(
        second.contains("标题: 缓存页"),
        "304 必须还原正文：{second}"
    );

    let validators = sent_validators.lock().expect("头锁").clone();
    assert_eq!(
        validators.last().map(|value| value.clone()),
        Some(Some("\"p1\"".to_string())),
        "带 cookie 的请求也必须带上缓存里的验证器：{validators:?}"
    );
}

/// 一次响应在**同一跳**里既下发 cookie、又给验证器时，也必须能协商。
///
/// 钉的是真实站点抓出来的缺陷：缓存键含 Cookie，若在收割 `Set-Cookie` 之后才定键，
/// 「存」会落到带新 cookie 的键上、「查」却用旧 cookie 的键，两边永远对不上——
/// 表现为第二次直接 304 却没有副本，抓取整个失败。
#[test]
fn a_hop_that_sets_a_cookie_and_an_etag_still_validates_later() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            let Some((path, headers)) = read_request(&stream) else {
                continue;
            };
            let response = match path.as_str() {
                "/robots.txt" => {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                }
                "/both" => {
                    let validator = header_of(&headers, "if-none-match").map(str::to_string);
                    captured.lock().expect("头锁").push(validator.clone());
                    if validator.as_deref() == Some("\"b1\"") {
                        // 304 也照样下发 cookie：这会改变罐里的状态。
                        "HTTP/1.1 304 Not Modified\r\nETag: \"b1\"\r\n\
Set-Cookie: seen=1; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string()
                    } else {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
ETag: \"b1\"\r\nSet-Cookie: sid=xyz; Path=/\r\nContent-Length: {}\r\n\
Connection: close\r\n\r\n{BODY}",
                            BODY.len()
                        )
                    }
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    let base = format!("http://127.0.0.1:{port}");
    let options = options();
    let url = format!("{base}/both");

    let first = fetcher::fetcher(&options, &arguments(&url)).expect("首次抓取");
    assert!(first.contains("状态: 200"), "{first}");
    let second = fetcher::fetcher(&options, &arguments(&url)).expect("第二跳必须能协商");
    assert!(
        second.contains("标题: 缓存页"),
        "304 必须被还原而不是报错：{second}"
    );

    // 第二次请求带着上一跳下发的 cookie，与首次的表示不同（键含 Cookie），
    // 因此这里**不该**命中缓存——关键是必须干净地回 200，而不是「304 无副本」报错。
    let validators = seen.lock().expect("头锁").clone();
    assert_eq!(
        validators.last().map(|value| value.clone()),
        Some(None),
        "cookie 变了就不该复用旧副本：{validators:?}"
    );
    // 但同一 cookie 状态下的再次抓取必须命中并还原。
    let third = fetcher::fetcher(&options, &arguments(&url)).expect("第三跳必须能协商");
    assert!(
        third.contains("标题: 缓存页"),
        "同一 cookie 状态下必须命中并还原：{third}"
    );
    let validators = seen.lock().expect("头锁").clone();
    assert_eq!(
        validators.last().map(|value| value.clone()),
        Some(Some("\"b1\"".to_string())),
        "第三跳必须带上第二跳写入的验证器：{validators:?}"
    );
}
