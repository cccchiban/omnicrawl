//! HTTP 缓存**协商**（RFC 9111 的条件请求）：`ETag` / `Last-Modified` 存下来，下次带
//! `If-None-Match` / `If-Modified-Since`，服务端回 304 时用缓存副本还原出完整响应。
//!
//! ## 只做协商，不用缓存跳过请求
//!
//! 浏览器在 `max-age` 内会直接用缓存副本、根本不发请求。这里**故意不这样做**：抓取工具的
//! 语义是「取当前内容」，静默返回一份可能已经过期的副本比多花一次往返更糟。本实现等价于
//! 浏览器的 reload 路径——而且导航头里本来就带着 `Cache-Control: no-cache`（每次使用前必须
//! 验证），两者语义一致。
//!
//! ## 304 必须还原成 200
//!
//! 304 没有正文，也不带 `content-type`。若原样交给上层，抓取结果会是「状态 304、正文为空、
//! 无标题」——比抓取失败更难排查。因此这里用缓存副本还原正文与相关响应头，把状态改回 200：
//! 对这个进程的调用方来说，它拿到的是「服务器认可的、当前仍然有效的那份内容」。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 单条目正文上限：网页正文远小于此；超限不缓存（宁可少省一次往返，也不让内存失控）。
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// 条目数上限：够覆盖一次工具调用涉及的 URL，超出整体清空。
const MAX_ENTRIES: usize = 64;

/// 一条可复用的缓存副本。
#[derive(Debug, Clone)]
struct Entry {
    body: Vec<u8>,
    /// 原始响应头（名字已小写）：304 不带 `content-type`，正文解码要靠这份。
    headers: Vec<(String, String)>,
    etag: Option<String>,
    last_modified: Option<String>,
}

fn entries() -> &'static Mutex<HashMap<String, Entry>> {
    static STORE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存键：URL（去 fragment）加上这次请求带的 `cookie`。
///
/// 只按 URL 分键会串味：同一地址在登录前后是两个不同的表示，拿登录前的验证器去问，
/// 服务端可能回「没变」，于是把一份未登录的正文当成当前内容。浏览器是按 `Vary` 逐头比较的，
/// 这里只跟 `cookie` 这一个最关键的维度——本实现不缓存需要其它请求头区分的响应
/// （`Vary: *` 直接不存）。
pub fn key_for(url: &str, request_headers: &[(String, String)]) -> String {
    let url = match url.find('#') {
        Some(index) => &url[..index],
        None => url,
    };
    match find_header(request_headers, "cookie") {
        Some(cookie) => format!(
            "{url}
{cookie}"
        ),
        None => url.to_string(),
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// `cache-control` 是否禁止存储（`no-store`）。
fn forbids_storage(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("cache-control"))
        .flat_map(|(_, value)| value.split(','))
        .any(|token| {
            token
                .split('=')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("no-store")
        })
}

/// `Vary: *` 表示「响应因每个请求而异」，不可复用。
fn varies_on_everything(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("vary"))
        .flat_map(|(_, value)| value.split(','))
        .any(|token| token.trim() == "*")
}

/// 为这次请求补条件头：有可复用副本时加 `If-None-Match` / `If-Modified-Since`。
///
/// 两个验证器都带：服务端按规范优先用 `If-None-Match`（强校验），只有它不存在时才看时间。
/// 只给 GET 加——条件头用在别的请求上没有意义。
pub fn apply_conditional(key: &str, method: &str, headers: &mut Vec<(String, String)>) {
    if !method.eq_ignore_ascii_case("GET") {
        return;
    }
    let Ok(store) = entries().lock() else {
        return;
    };
    let Some(entry) = store.get(key) else {
        return;
    };
    headers.retain(|(name, _)| {
        !name.eq_ignore_ascii_case("if-none-match")
            && !name.eq_ignore_ascii_case("if-modified-since")
    });
    if let Some(etag) = &entry.etag {
        headers.push(("if-none-match".to_string(), etag.clone()));
    }
    if let Some(last_modified) = &entry.last_modified {
        headers.push(("if-modified-since".to_string(), last_modified.clone()));
    }
}

/// 存下一次 200 响应，供后续协商使用；不满足存储条件时静默跳过。
///
/// 存的是**已解码**的正文：出口已经把 `content-encoding` 还原过，若存原始压缩字节，
/// 304 还原出来的就是压缩体，会被上层当作乱码。
pub fn store(key: &str, status: u16, body: &[u8], headers: &[(String, String)]) {
    if status != 200 || body.len() > MAX_BODY_BYTES {
        return;
    }
    if forbids_storage(headers) || varies_on_everything(headers) {
        return;
    }
    let etag = find_header(headers, "etag").map(str::to_string);
    let last_modified = find_header(headers, "last-modified").map(str::to_string);
    // 两个验证器都没有：服务端下次无法回答「没变」，存下来也协商不了。
    if etag.is_none() && last_modified.is_none() {
        return;
    }
    let Ok(mut store) = entries().lock() else {
        return;
    };
    if store.len() >= MAX_ENTRIES && !store.contains_key(key) {
        store.clear();
    }
    store.insert(
        key.to_string(),
        Entry {
            body: body.to_vec(),
            headers: headers.to_vec(),
            etag,
            last_modified,
        },
    );
}

/// 一次 304 的还原结果。
pub struct Reused {
    pub body: Vec<u8>,
    /// 合并后的响应头：304 带来的头覆盖缓存里的同名头，其余保留。
    ///
    /// 保留是必需项而不是兼容性：304 通常不带 `content-type`，丢掉它会让 GBK 站点在
    /// 还原路径上被按 UTF-8 解码——内容还会「看起来正常」，只是全是替换字符。
    pub headers: Vec<(String, String)>,
}

/// 用缓存副本还原一次 304；没有副本时返回 `None`（调用方应如实上报 304）。
pub fn reuse(key: &str, fresh_headers: &[(String, String)]) -> Option<Reused> {
    let Ok(mut store) = entries().lock() else {
        return None;
    };
    let entry = store.get(key)?;
    // 304 可能刷新验证器（例如换了一个新的 ETag），据此更新条目。
    let mut updated = entry.clone();
    for (name, value) in fresh_headers {
        if name.eq_ignore_ascii_case("etag") {
            updated.etag = Some(value.clone());
            updated.last_modified = None;
        } else if name.eq_ignore_ascii_case("last-modified") {
            updated.last_modified = Some(value.clone());
        }
    }
    let mut headers = entry.headers.clone();
    for (name, value) in fresh_headers {
        // `content-type` / `content-encoding` 描述的是正文，304 里的值可能缺失或为空；
        // 缓存里的那份才是与正文对应的。这几个头一律以缓存副本为准。
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "content-encoding" | "content-length" | "date"
        ) {
            continue;
        }
        match headers
            .iter_mut()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
        {
            Some(slot) => slot.1 = value.clone(),
            None => headers.push((name.to_ascii_lowercase(), value.clone())),
        }
    }
    let body = entry.body.clone();
    store.insert(key.to_string(), updated);
    Some(Reused { body, headers })
}

/// 丢弃某个键的副本。
///
/// 用在「服务端回了 304，但本地没有副本」这条路上：留着这个键只会让下一次继续带
/// 验证器、继续拿 304，形成无法自愈的死循环。剔掉之后下一次是干净的无条件请求。
pub fn forget(key: &str) {
    if let Ok(mut store) = entries().lock() {
        store.remove(key);
    }
}

/// 清空缓存（测试用；生产路径不需要）。
#[cfg(test)]
pub(crate) fn reset() {
    if let Ok(mut store) = entries().lock() {
        store.clear();
    }
}

/// 测试串行锁：缓存是进程级全局状态，并发用例的 `reset()` 会互相清空。
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn value_of(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    #[test]
    fn a_stored_response_produces_both_validators() {
        let _serial = test_guard();
        reset();
        let key = "https://cache-a.example.com/page";
        let mut request = headers(&[]);
        apply_conditional(key, "GET", &mut request);
        assert!(request.is_empty(), "没存过就不该带条件头");

        store(
            key,
            200,
            b"body",
            &headers(&[
                ("etag", "\"v1\""),
                ("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
            ]),
        );
        apply_conditional(key, "GET", &mut request);
        assert_eq!(
            value_of(&request, "if-none-match").as_deref(),
            Some("\"v1\"")
        );
        assert_eq!(
            value_of(&request, "if-modified-since").as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
    }

    /// 304 不带 `content-type`：丢掉缓存里那份会让 GBK 页面解成替换字符。
    #[test]
    fn a_304_restores_the_body_and_keeps_the_content_type() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-b.example.com/gbk";
        store(
            url,
            200,
            &[0xD6, 0xD0, 0xCE, 0xC4],
            &headers(&[
                ("etag", "\"v1\""),
                ("content-type", "text/html; charset=gbk"),
            ]),
        );
        let reused = reuse(url, &headers(&[("etag", "\"v1\"")])).expect("应当命中缓存副本");
        assert_eq!(reused.body, vec![0xD6, 0xD0, 0xCE, 0xC4]);
        assert_eq!(
            value_of(&reused.headers, "content-type").as_deref(),
            Some("text/html; charset=gbk")
        );

        // 走一遍真实的解码路径：还原出来的正文必须是原文。
        let response = crate::tools::web_transport::WebResponse {
            status: 200,
            final_url: url.to_string(),
            body: reused.body,
            headers: reused.headers,
        };
        assert_eq!(response.text(), "中文");
    }

    /// 304 带来新 `etag` 时必须更新条目，否则下次还拿旧值去问，服务端会一直回 304。
    #[test]
    fn a_304_with_a_new_validator_updates_the_entry() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-c.example.com/page";
        store(
            url,
            200,
            b"one",
            &headers(&[
                ("etag", "\"v1\""),
                ("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
            ]),
        );
        reuse(url, &headers(&[("etag", "\"v2\"")])).expect("命中");
        let mut request = headers(&[]);
        apply_conditional(url, "GET", &mut request);
        assert_eq!(
            value_of(&request, "if-none-match").as_deref(),
            Some("\"v2\"")
        );
        // 换了强验证器之后不该再带时间条件头。
        assert_eq!(value_of(&request, "if-modified-since"), None);
    }

    #[test]
    fn no_store_is_never_kept() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-d.example.com/page";
        store(
            url,
            200,
            b"secret",
            &headers(&[("etag", "\"v1\""), ("cache-control", "no-store")]),
        );
        let mut request = headers(&[]);
        apply_conditional(url, "GET", &mut request);
        assert!(request.is_empty());
    }

    #[test]
    fn a_response_without_validators_is_not_stored() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-e.example.com/page";
        store(url, 200, b"x", &headers(&[("content-type", "text/html")]));
        let mut request = headers(&[]);
        apply_conditional(url, "GET", &mut request);
        assert!(request.is_empty());
    }

    #[test]
    fn non_get_requests_never_carry_conditional_headers() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-f.example.com/page";
        store(url, 200, b"x", &headers(&[("etag", "\"v1\"")]));
        let mut request = headers(&[]);
        apply_conditional(url, "POST", &mut request);
        assert!(request.is_empty());
    }

    #[test]
    fn the_fragment_is_not_part_of_the_key() {
        let _serial = test_guard();
        reset();
        assert_eq!(
            key_for("https://cache-g.example.com/page#one", &[]),
            "https://cache-g.example.com/page"
        );
    }

    /// 登录前后的同一 URL 是两个不同的表示：只按 URL 分键会把未登录的正文当成当前内容。
    #[test]
    fn the_cookie_separates_two_representations_of_one_url() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-j.example.com/page";
        let anonymous = key_for(url, &[]);
        let logged_in = key_for(url, &headers(&[("cookie", "sid=abc")]));
        assert_ne!(anonymous, logged_in, "带不同 cookie 必须是不同的键");

        store(
            &anonymous,
            200,
            b"guest",
            &headers(&[("etag", "\"guest\"")]),
        );
        // 带登录 cookie 的请求不得命中匿名副本。
        let mut request = headers(&[("cookie", "sid=abc")]);
        apply_conditional(&logged_in, "GET", &mut request);
        assert_eq!(value_of(&request, "if-none-match"), None);
    }

    /// `Vary: *` 意味着响应因每个请求而异，复用它会给出错误内容。
    #[test]
    fn vary_star_disables_reuse() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-h.example.com/page";
        store(
            url,
            200,
            b"x",
            &headers(&[("etag", "\"v1\""), ("vary", "*")]),
        );
        let mut request = headers(&[]);
        apply_conditional(url, "GET", &mut request);
        assert!(request.is_empty());
    }

    #[test]
    fn an_oversized_body_is_not_cached() {
        let _serial = test_guard();
        reset();
        let url = "https://cache-i.example.com/big";
        let body = vec![b'x'; MAX_BODY_BYTES + 1];
        store(url, 200, &body, &headers(&[("etag", "\"v1\"")]));
        let mut request = headers(&[]);
        apply_conditional(url, "GET", &mut request);
        assert!(request.is_empty());
    }
}
