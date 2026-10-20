//! robots.txt 的取回与缓存：每个 origin 只取一次，抓取期间复用。
//!
//! 缓存按 `scheme://host[:port]` 分键，进程内存驻留、不做持久化：robots.txt 会变，
//! 但也无需为一次工具调用引入磁盘状态。并发抓取同一站点时用一把锁包住取回，
//! 避免多个线程同时打同一个 `robots.txt`。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use super::error::ToolError;
use super::robots::{request_path, Robots, Verdict};
use super::web_transport::{WebRequest, WebTransport};

/// robots.txt 的取回超时：它只是判定的前置条件，不该拖慢主请求。
const ROBOTS_TIMEOUT: Duration = Duration::from_secs(5);
/// 缓存条目上限：够覆盖单次工具调用涉及的全部站点，超出即整体清空重来。
const MAX_CACHED_ORIGINS: usize = 256;

#[derive(Debug, Clone)]
enum Cached {
    Parsed(Robots),
    /// 取不到 robots.txt：按允许处理，`reason` 留作诊断与测试用。
    Unavailable {
        #[allow(dead_code)]
        reason: String,
    },
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// origin 键：`scheme://host[:port]`。解析不出 host 时返回 `None`（调用方直接放行）。
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// 取该站点的 robots.txt（命中缓存则复用）。
fn load(transport: &dyn WebTransport, url: &str, user_agent: &str) -> Cached {
    let Some(origin) = origin_of(url) else {
        return Cached::Unavailable {
            reason: "URL 无法解析出站点".to_string(),
        };
    };
    if let Ok(cache) = cache().lock() {
        if let Some(hit) = cache.get(&origin) {
            return hit.clone();
        }
    }

    let target = format!("{origin}/robots.txt");
    let request = WebRequest::new(
        target,
        vec![("User-Agent".to_string(), user_agent.to_string())],
        ROBOTS_TIMEOUT,
    );
    let fetched = match transport.send(&request) {
        Ok(response) => match response.status {
            200 => match String::from_utf8(response.body) {
                Ok(text) => Cached::Parsed(Robots::parse(&text)),
                Err(_) => Cached::Unavailable {
                    reason: "robots.txt 不是合法 UTF-8".to_string(),
                },
            },
            // 404 / 410：站点明确没有限制文件，按允许处理。
            404 | 410 => Cached::Unavailable {
                reason: format!("站点没有 robots.txt（HTTP {}）", response.status),
            },
            other => Cached::Unavailable {
                reason: format!("robots.txt 返回 HTTP {other}"),
            },
        },
        Err(error) => Cached::Unavailable {
            reason: format!("robots.txt 取回失败：{}", error.message),
        },
    };

    if let Ok(mut cache) = cache().lock() {
        if cache.len() >= MAX_CACHED_ORIGINS {
            cache.clear();
        }
        cache.insert(origin, fetched.clone());
    }
    fetched
}

/// 判断该 URL 是否允许抓取。
///
/// 返回 `Err` 表示明确禁止：文案直接面向用户，说明命中的规则与出路。
pub fn ensure_allowed(
    transport: &dyn WebTransport,
    url: &str,
    user_agent: &str,
) -> Result<(), ToolError> {
    match load(transport, url, user_agent) {
        Cached::Parsed(robots) => match robots.verdict(user_agent, &request_path(url)) {
            Verdict::Disallowed { rule } => Err(ToolError::new(format!(
                "该站点通过 robots.txt 明确禁止抓取此路径（规则 `Disallow: {rule}`）：{url}\n\
                 robots.txt 是站点声明的抓取许可，本工具遵守它，不会抓取该地址。\n\
                 如果这与你的预期不符，请在浏览器中直接访问，或改用站点提供的公开接口。"
            ))),
            _ => Ok(()),
        },
        Cached::Unavailable { .. } => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::web_transport::{WebError, WebResponse};
    use std::sync::Arc;

    /// 固定回一个 robots.txt 的传输层。
    struct StubTransport {
        status: u16,
        body: String,
        requests: Mutex<Vec<String>>,
    }

    impl WebTransport for StubTransport {
        fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
            self.requests
                .lock()
                .expect("请求锁")
                .push(request.url.clone());
            Ok(WebResponse {
                status: self.status,
                final_url: request.url.clone(),
                body: self.body.clone().into_bytes(),
                headers: Vec::new(),
            })
        }
    }

    fn stub(status: u16, body: &str) -> Arc<StubTransport> {
        Arc::new(StubTransport {
            status,
            body: body.to_string(),
            requests: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn disallowed_path_is_rejected_with_the_rule() {
        // 用独立 origin 避免与其它用例共享缓存。
        let transport = stub(200, "User-agent: *\nDisallow: /search\n");
        let error = ensure_allowed(
            transport.as_ref() as &dyn WebTransport,
            "https://deny.example.com/search?q=1",
            "TestBot/1.0",
        )
        .expect_err("应当拒绝");
        assert!(error.message.contains("Disallow: /search"), "{}", error.message);
        assert!(error.message.contains("不会抓取该地址"), "{}", error.message);
    }

    #[test]
    fn allowed_path_passes_and_hits_cache_once() {
        let transport = stub(200, "User-agent: *\nDisallow: /private/\n");
        let as_dyn: &dyn WebTransport = transport.as_ref();
        ensure_allowed(as_dyn, "https://allow.example.com/public", "TestBot/1.0").expect("允许");
        ensure_allowed(as_dyn, "https://allow.example.com/other", "TestBot/1.0").expect("允许");
        // 同一 origin 只取一次 robots.txt。
        let requests = transport.requests.lock().expect("请求锁");
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0], "https://allow.example.com/robots.txt");
    }

    #[test]
    fn missing_robots_txt_allows_by_design() {
        let transport = stub(404, "");
        ensure_allowed(
            transport.as_ref() as &dyn WebTransport,
            "https://missing.example.com/anything",
            "TestBot/1.0",
        )
        .expect("没有 robots.txt 时按允许处理");
    }

    #[test]
    fn server_error_on_robots_txt_allows_by_design() {
        let transport = stub(503, "service unavailable");
        ensure_allowed(
            transport.as_ref() as &dyn WebTransport,
            "https://broken.example.com/page",
            "TestBot/1.0",
        )
        .expect("robots.txt 不可用时按允许处理");
    }

    #[test]
    fn origin_key_keeps_scheme_and_port() {
        assert_eq!(
            origin_of("https://Example.com/a?b=1").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            origin_of("http://example.com:8080/x").as_deref(),
            Some("http://example.com:8080")
        );
        assert_eq!(
            origin_of("https://user:pw@e.com/x"),
            Some("https://e.com".into())
        );
        assert_eq!(origin_of("not a url"), None);
    }
}
