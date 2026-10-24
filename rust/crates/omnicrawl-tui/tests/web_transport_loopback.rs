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
            captured.lock().expect("头锁").push((path.clone(), cookie));

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

/// 跨站跳转服务：`/cross` 下发 cookie 后跳到**另一个站点**的同端口路径。
///
/// 用 `127.0.0.1` → `127.0.0.2` 构造跨站（两个回环 IP 指向同一进程、同端口，但站点不同）。
/// 这是 `SameSite` 的关键场景：源站的 `Strict` cookie 不应出现在跨站目标上。
///
/// 另起 `/same` 走同站跳转，用于对照——它必须带得上 cookie，否则「没带上」就分不清
/// 是 `SameSite` 生效还是接线断了。
fn serve_cross_site() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = TcpListener::bind("0.0.0.0:0").expect("绑定本地端口");
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
            captured.lock().expect("头锁").push((path.clone(), cookie));

            let response = match path.as_str() {
                // 跨站：跳到另一个回环 IP。
                "/cross" => format!(
                    "HTTP/1.1 302 Found
Location: http://127.0.0.2:{port}/target
Set-Cookie: strict=1; Path=/; SameSite=Strict
Set-Cookie: lax=1; Path=/
Content-Length: 0
Connection: close

"
                ),
                // 同站对照：跳回同一主机，cookie 必须带得上。
                "/same" => format!(
                    "HTTP/1.1 302 Found
Location: http://127.0.0.1:{port}/target
Set-Cookie: strict=1; Path=/; SameSite=Strict
Set-Cookie: lax=1; Path=/
Content-Length: 0
Connection: close

"
                ),
                "/target" => http_response(200, PLAIN_BODY),
                _ => http_response(404, "not found"),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// 同站跳转必须带上 cookie（对照），跨站跳转必须让 `SameSite=Strict` 的 cookie 消失。
///
/// 这是端到端的接线断言——只测 `cookie_header_in` 无法证明传输层真的把发起方传了下去。
#[test]
fn samesite_is_enforced_across_a_cross_site_redirect() {
    // 先跑对照：同站跳转必须带得上，否则说明接线断了而不是 SameSite 生效。
    let (base, seen) = serve_cross_site();
    let same_options = options();
    fetcher::fetcher(&same_options, &arguments(&format!("{base}/same"))).expect("同站跳转应当成功");
    let same_target = seen
        .lock()
        .expect("头锁")
        .iter()
        .find(|(path, _)| path == "/target")
        .map(|(_, cookie)| cookie.clone())
        .unwrap_or_default();
    assert!(
        same_target.contains("strict=1") && same_target.contains("lax=1"),
        "同站跳转必须带全部 cookie（对照失败说明接线断了）：{same_target}"
    );

    // 跨站：新罐，避免上一轮的 cookie 混进来。
    let (base, seen) = serve_cross_site();
    let cross_options = options();
    fetcher::fetcher(&cross_options, &arguments(&format!("{base}/cross")))
        .expect("跨站跳转应当成功");
    let cross_target = seen
        .lock()
        .expect("头锁")
        .iter()
        .find(|(path, _)| path == "/target")
        .map(|(_, cookie)| cookie.clone())
        .unwrap_or_default();
    assert!(
        !cross_target.contains("strict=1"),
        "SameSite=Strict 不该出现在跨站目标上：{cross_target}"
    );
}

/// 记录每一跳 `Referer` 的本地服务：`/start` 跳到同源 `/same`，`/cross` 跳到另一站。
fn serve_referer() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let listener = TcpListener::bind("0.0.0.0:0").expect("绑定本地端口");
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
            // 记录「主机 + 路径」：跨站用例要靠主机名区分是谁收到的。
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let mut referer = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header.trim().is_empty() {
                    break;
                }
                if header.to_lowercase().starts_with("referer:") {
                    referer = header
                        .split_once(':')
                        .map(|(_, value)| value.trim().to_string())
                        .unwrap_or_default();
                }
            }
            captured.lock().expect("头锁").push((path.clone(), referer));

            let response = match path.as_str() {
                "/start" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/same/inner\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                "/cross" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:{port}/other\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                // 再跳一次，验证策略在链上逐跳累积。
                "/same/inner" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/same/final\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                "/other" => http_response(200, PLAIN_BODY),
                _ => http_response(200, PLAIN_BODY),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// 跟随重定向时必须带 `Referer`，且值按默认策略（strict-origin-when-cross-origin）算：
/// 同源交完整 URL，跨源只交源。
///
/// 端到端断言的重点是接线——`referer_for` 单测再全，也证明不了传输层真的把发起方传了下去。
#[test]
fn referer_is_sent_on_every_redirect_hop_and_truncated_to_origin_across_origins() {
    let (base, seen) = serve_referer();
    let options = options();

    fetcher::fetcher(&options, &arguments(&format!("{base}/start"))).expect("同源跳转应当成功");

    let hops = seen.lock().expect("头锁").clone();
    let referer_for = |path: &str| {
        hops.iter()
            .find(|(seen_path, _)| seen_path == path)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    };

    // 首跳没有发起方：地址栏直接输入不该带 `Referer`。
    assert_eq!(referer_for("/start"), "", "首跳不该带 Referer：{hops:?}");
    // 同源：完整 URL（含路径）。
    assert_eq!(
        referer_for("/same/inner"),
        format!("{base}/start"),
        "同源必须交完整路径：{hops:?}"
    );
    // 同源第二跳：发起方是上一跳，仍然是完整 URL。
    assert_eq!(
        referer_for("/same/final"),
        format!("{base}/same/inner"),
        "链上每一跳都要带：{hops:?}"
    );

    // 跨源：只交源，不交路径。
    let (cross_base, cross_seen) = serve_referer();
    fetcher::fetcher(&options, &arguments(&format!("{cross_base}/cross")))
        .expect("跨源跳转应当成功");
    let cross_hops = cross_seen.lock().expect("头锁").clone();
    let other = cross_hops
        .iter()
        .find(|(path, _)| path == "/other")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    assert_eq!(
        other, cross_base,
        "跨源只能交源（不含路径）：{cross_hops:?}"
    );
}

/// 记录每一跳 `sec-fetch-*` 的服务：`/start` 同源跳转，`/cross` 跨站跳转。
fn serve_fetch_metadata() -> (String, Arc<Mutex<Vec<(String, String, String, String)>>>) {
    // 每跳记录 (路径, site, dest, mode)。
    let listener = TcpListener::bind("0.0.0.0:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("本地地址").port();
    let seen: Arc<Mutex<Vec<(String, String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
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
            let mut site = String::new();
            let mut dest = String::new();
            let mut mode = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header.trim().is_empty() {
                    break;
                }
                let lower = header.to_lowercase();
                if let Some((name, value)) = lower.split_once(':') {
                    let value = value.trim().to_string();
                    match name {
                        "sec-fetch-site" => site = value,
                        "sec-fetch-dest" => dest = value,
                        "sec-fetch-mode" => mode = value,
                        _ => {}
                    }
                }
            }
            captured
                .lock()
                .expect("头锁")
                .push((path.clone(), site, dest, mode));

            let response = match path.as_str() {
                "/start" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/same/inner\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                "/cross" => format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.2:{port}/other\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                _ => http_response(200, PLAIN_BODY),
            };
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// `sec-fetch-site` 必须跟着发起方走：首跳 `none`、同源跳 `same-origin`、跨站跳 `cross-site`。
///
/// 这条断言钉的正是原先的缺陷——它恒为 `none`，等于把跨站重定向描述成「地址栏直接输入」，
/// 服务端据此做的判断会与真实浏览器不同。
#[test]
fn sec_fetch_site_follows_the_initiator_across_redirects() {
    let (base, seen) = serve_fetch_metadata();
    let options = options();

    fetcher::fetcher(&options, &arguments(&format!("{base}/start"))).expect("同源跳转应当成功");
    let hops = seen.lock().expect("头锁").clone();
    let hop = |path: &str| {
        hops.iter()
            .find(|(seen_path, _, _, _)| seen_path == path)
            .cloned()
            .unwrap_or_default()
    };
    let (_, start_site, start_dest, start_mode) = hop("/start");
    assert_eq!(start_site, "none", "首跳没有发起方");
    assert_eq!(start_dest, "document");
    assert_eq!(start_mode, "navigate");
    let (_, same_site, _, _) = hop("/same/inner");
    assert_eq!(same_site, "same-origin", "同源跳转：{hops:?}");

    // 跨站：必须如实说 cross-site。
    let (cross_base, cross_seen) = serve_fetch_metadata();
    fetcher::fetcher(&options, &arguments(&format!("{cross_base}/cross")))
        .expect("跨站跳转应当成功");
    let cross_hops = cross_seen.lock().expect("头锁").clone();
    let other = cross_hops
        .iter()
        .find(|(path, _, _, _)| path == "/other")
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        other.1, "cross-site",
        "跨站重定向必须报 cross-site：{cross_hops:?}"
    );
}

#[test]
fn cookies_set_on_a_redirect_are_replayed_on_the_next_hop_and_request() {
    let (base, seen) = serve_session();
    let options = options();

    // 一次抓取内的重定向：/login 下发的 cookie 必须带到 /home。
    let login =
        fetcher::fetcher(&options, &arguments(&format!("{base}/login"))).expect("登录跳转应当成功");
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
