//! HTTP/1.1 最小客户端编解码：把请求写成字节、把响应拆成头与体。
//!
//! 为什么需要它：ALPN 由服务端决定，浏览器档案里同时列了 `h2` 与 `http/1.1`。真实
//! 站点里仍有相当一部分只回 `http/1.1`（小站、老版本反向代理、部分 API 网关）。若这
//! 一层缺位，切到自研 TLS 通路就等于「只支持一半的站点」——而 TLS 指纹对两类站点同样
//! 需要。
//!
//! 范围刻意收敛：这是**客户端**侧的编码器与解析器，不实现服务端、不做连接池、不支持
//! 升级（`Upgrade`）、不支持 `100-continue` 之外的 1xx 语义（1xx 由驱动层跳过）。
//! 支持的三类响应体定界（RFC 9112 §6.3）覆盖了实际会遇到的全部情况：
//! `Content-Length`、`Transfer-Encoding: chunked`、以及「读到连接关闭为止」。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http1Error {
    /// 报文结构不合规（状态行、头部、分块格式）。
    Malformed(String),
    /// 响应头里同时给了 `Content-Length` 与 `chunked`，或重复的 `Content-Length`。
    AmbiguousFraming(String),
}

impl fmt::Display for Http1Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(formatter, "HTTP/1.1 报文不合规：{detail}"),
            Self::AmbiguousFraming(detail) => write!(formatter, "响应体定界有歧义：{detail}"),
        }
    }
}

impl std::error::Error for Http1Error {}

/// 响应头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub reason: String,
    /// 原始头部，名字保留对端的大小写。
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 同名头部的全部值（`Set-Cookie` 之类需要）。
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// 响应体的定界方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// 没有响应体（HEAD、204、304，以及 1xx——后者由驱动层跳过）。
    Empty,
    Length(usize),
    Chunked,
    /// 没有 `Content-Length` 也不是分块：读到对端关闭连接为止。
    UntilEof,
}

/// 编码一次请求（请求行 + 头部 + 空行），返回**不含请求体**的字节。
///
/// 请求体由调用方接在后面：这一层不知道也不该知道它有多大。`content-length` 在这里按
/// `body_len` 自动补上（分块上传不做——我们的请求体都是已知长度的内存切片）。
///
/// `authority` 进 `Host`。HTTP/1.1 要求必须有它（§7.2），缺了会被不少服务端直接 400。
pub fn encode_request(
    method: &str,
    target: &str,
    authority: &str,
    headers: &[(String, String)],
    body_len: Option<usize>,
) -> Result<Vec<u8>, Http1Error> {
    if method.is_empty() || method.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(Http1Error::Malformed(format!("HTTP 方法非法：{method}")));
    }
    if !target.starts_with('/') && target != "*" {
        // origin-form 必须以 `/` 开头；把绝对地址写进来会让服务端把主机名当成路径。
        return Err(Http1Error::Malformed(format!(
            "请求目标必须是 origin-form：{target}"
        )));
    }
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    let mut has_host = false;
    let mut has_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        // 头部值里的 CR/LF 是响应拆分（CRLF injection）的入口，直接拒绝。
        if value.contains('\r') || value.contains('\n') {
            return Err(Http1Error::Malformed(format!("头部 {name} 的值包含换行")));
        }
        if name.contains('\r') || name.contains('\n') || name.contains(':') || name.is_empty() {
            return Err(Http1Error::Malformed(format!("头部名非法：{name}")));
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has_host {
        out.extend_from_slice(b"Host: ");
        out.extend_from_slice(authority.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if let Some(length) = body_len.filter(|value| *value > 0) {
        if !has_length {
            out.extend_from_slice(format!("Content-Length: {length}\r\n").as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

/// 解析响应头；数据不足时返回 `Ok(None)`，由调用方再读一些。
///
/// 返回的 `usize` 是**头块消耗的字节数**（含结尾的空行），紧随其后的就是响应体。
pub fn parse_head(data: &[u8]) -> Result<Option<(Head, usize)>, Http1Error> {
    let Some(end) = find_head_end(data) else {
        // 头部上限防住「对端狂发头但没有空行」这种情况。
        if data.len() > 64 * 1024 {
            return Err(Http1Error::Malformed(
                "响应头超过 64 KB 仍未结束".to_string(),
            ));
        }
        return Ok(None);
    };
    let text = std::str::from_utf8(&data[..end])
        .map_err(|_| Http1Error::Malformed("响应头不是合法 UTF-8".to_string()))?;
    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| Http1Error::Malformed("响应缺少状态行".to_string()))?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(Http1Error::Malformed(format!(
            "不是 HTTP/1.x 响应：{status_line}"
        )));
    }
    let status = parts
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| Http1Error::Malformed(format!("状态码无法解析：{status_line}")))?;
    let reason = parts.next().unwrap_or_default().to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        // 折行（obs-fold）在 RFC 9112 里已被废弃；出现即当错误，不做兼容拼接。
        if line.starts_with(' ') || line.starts_with('\t') {
            return Err(Http1Error::Malformed("响应头出现废弃的折行".to_string()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Http1Error::Malformed(format!("头部缺少冒号：{line}")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(Some((
        Head {
            status,
            reason,
            headers,
        },
        end + 4,
    )))
}

fn find_head_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|window| window == b"\r\n\r\n")
}

/// 按 RFC 9112 §6.3 判定响应体如何定界。
///
/// `chunked` 优先于 `Content-Length`：两者同时出现时以分块为准**并报错**——那种报文
/// 是两个实现互相矛盾的信号，猜错一个就会把后面的响应读串。
pub fn body_kind(head: &Head, request_method: &str) -> Result<BodyKind, Http1Error> {
    if request_method.eq_ignore_ascii_case("HEAD")
        || head.status == 204
        || head.status == 304
        || (100..200).contains(&head.status)
    {
        return Ok(BodyKind::Empty);
    }
    let chunked = head
        .header_all("transfer-encoding")
        .iter()
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("chunked"));
    let lengths = head.header_all("content-length");
    if chunked {
        if !lengths.is_empty() {
            return Err(Http1Error::AmbiguousFraming(
                "同时给了 Transfer-Encoding: chunked 与 Content-Length".to_string(),
            ));
        }
        return Ok(BodyKind::Chunked);
    }
    if lengths.is_empty() {
        return Ok(BodyKind::UntilEof);
    }
    if lengths.len() > 1 {
        let mut unique = lengths.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() > 1 {
            return Err(Http1Error::AmbiguousFraming(format!(
                "多个互相矛盾的 Content-Length：{lengths:?}"
            )));
        }
    }
    let length = lengths[0]
        .parse::<usize>()
        .map_err(|_| Http1Error::Malformed(format!("Content-Length 不是数字：{}", lengths[0])))?;
    if length == 0 {
        return Ok(BodyKind::Empty);
    }
    Ok(BodyKind::Length(length))
}

/// 解分块响应体；不完整时返回 `Ok(None)`。
///
/// 返回的 `usize` 是已消耗字节数（含结束块与尾部字段），调用方据此丢掉前缀。
pub fn decode_chunked(data: &[u8]) -> Result<Option<(Vec<u8>, usize)>, Http1Error> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let Some(line_end) = find_crlf(&data[offset..]) else {
            return Ok(None);
        };
        let line = std::str::from_utf8(&data[offset..offset + line_end])
            .map_err(|_| Http1Error::Malformed("分块长度行不是 UTF-8".to_string()))?;
        // 分块扩展（`;name=value`）一律忽略，只取十六进制的长度。
        let size_text = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| Http1Error::Malformed(format!("分块长度不是十六进制：{line}")))?;
        offset += line_end + 2;
        if size == 0 {
            // 结束块之后是尾部字段，直到空行。
            loop {
                let Some(tail_end) = find_crlf(&data[offset..]) else {
                    return Ok(None);
                };
                offset += tail_end + 2;
                if tail_end == 0 {
                    return Ok(Some((out, offset)));
                }
            }
        }
        let needed = offset + size + 2;
        if data.len() < needed {
            return Ok(None);
        }
        out.extend_from_slice(&data[offset..offset + size]);
        // 每个分块自己的数据后面必须跟 CRLF，缺了说明长度行算错了。
        if &data[offset + size..needed] != b"\r\n" {
            return Err(Http1Error::Malformed("分块数据后缺少 CRLF".to_string()));
        }
        offset = needed;
    }
}

fn find_crlf(data: &[u8]) -> Option<usize> {
    data.windows(2).position(|window| window == b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> Vec<(String, String)> {
        vec![
            ("user-agent".to_string(), "UA/1.0".to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ]
    }

    #[test]
    fn a_request_carries_host_and_content_length() {
        let bytes =
            encode_request("GET", "/path?q=1", "example.com", &headers(), None).expect("应当编码");
        let text = String::from_utf8(bytes).expect("UTF-8");
        assert!(text.starts_with("GET /path?q=1 HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: example.com\r\n"), "{text}");
        assert!(
            !text.contains("Content-Length"),
            "无请求体不该带长度：{text}"
        );
        assert!(text.ends_with("\r\n\r\n"), "{text}");
    }

    #[test]
    fn a_request_body_gets_its_length() {
        let bytes = encode_request("POST", "/", "example.com", &headers(), Some(5)).expect("编码");
        let text = String::from_utf8(bytes).expect("UTF-8");
        assert!(text.contains("Content-Length: 5\r\n"), "{text}");
        // 长度为 0 时不写：写 `Content-Length: 0` 会让某些服务端等一个空体。
        let bytes = encode_request("GET", "/", "example.com", &headers(), Some(0)).expect("编码");
        assert!(!String::from_utf8(bytes)
            .expect("UTF-8")
            .contains("Content-Length"));
    }

    #[test]
    fn illegal_request_targets_and_header_values_are_rejected() {
        // 绝对地址会被服务端当成路径。
        assert!(encode_request("GET", "example.com/a", "example.com", &headers(), None).is_err());
        // CRLF 注入。
        let injected = vec![("x-a".to_string(), "v\r\nEvil: 1".to_string())];
        assert!(encode_request("GET", "/", "example.com", &injected, None).is_err());
        assert!(encode_request("", "/", "example.com", &headers(), None).is_err());
        assert!(encode_request("GE T", "/", "example.com", &headers(), None).is_err());
    }

    #[test]
    fn a_head_is_parsed_and_reports_bytes_consumed() {
        let raw = b"HTTP/1.1 301 Moved\r\nContent-Length: 0\r\nLocation: /a\r\n\r\nbody";
        let (head, used) = parse_head(raw).expect("应当解析").expect("应当完整");
        assert_eq!(head.status, 301);
        assert_eq!(head.reason, "Moved");
        assert_eq!(head.header("location"), Some("/a"));
        assert_eq!(head.header("LOCATION"), Some("/a"), "名字比较不分大小写");
        assert_eq!(&raw[used..], b"body", "消耗数应当恰好停在空行之后");
    }

    #[test]
    fn a_partial_head_asks_for_more_bytes() {
        assert_eq!(parse_head(b"HTTP/1.1 200 OK\r\n").expect("不算错误"), None);
        assert_eq!(parse_head(b"").expect("空也是不足"), None);
    }

    #[test]
    fn a_non_http_response_is_rejected() {
        let error = parse_head(b"NOT-HTTP 200 OK\r\n\r\n").expect_err("不是 HTTP");
        assert!(matches!(error, Http1Error::Malformed(_)), "{error}");
        let error = parse_head(b"HTTP/1.1 abc OK\r\n\r\n").expect_err("状态码非法");
        assert!(matches!(error, Http1Error::Malformed(_)), "{error}");
    }

    fn head_with(pairs: &[(&str, &str)], status: u16) -> Head {
        Head {
            status,
            reason: String::new(),
            headers: pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    #[test]
    fn body_framing_follows_the_rfc_rules() {
        assert_eq!(
            body_kind(&head_with(&[("Content-Length", "12")], 200), "GET").expect("判定"),
            BodyKind::Length(12)
        );
        assert_eq!(
            body_kind(&head_with(&[("Content-Length", "0")], 200), "GET").expect("判定"),
            BodyKind::Empty
        );
        assert_eq!(
            body_kind(&head_with(&[], 200), "GET").expect("判定"),
            BodyKind::UntilEof
        );
        assert_eq!(
            body_kind(&head_with(&[("Transfer-Encoding", "chunked")], 200), "GET").expect("判定"),
            BodyKind::Chunked
        );
        // HEAD / 204 / 304 一律没有体，哪怕给了 Content-Length。
        assert_eq!(
            body_kind(&head_with(&[("Content-Length", "99")], 200), "HEAD").expect("判定"),
            BodyKind::Empty
        );
        assert_eq!(
            body_kind(&head_with(&[("Content-Length", "99")], 204), "GET").expect("判定"),
            BodyKind::Empty
        );
        assert_eq!(
            body_kind(&head_with(&[("Content-Length", "99")], 304), "GET").expect("判定"),
            BodyKind::Empty
        );
    }

    #[test]
    fn contradictory_framing_is_an_error_not_a_guess() {
        let error = body_kind(
            &head_with(
                &[("Transfer-Encoding", "chunked"), ("Content-Length", "5")],
                200,
            ),
            "GET",
        )
        .expect_err("矛盾必须报错");
        assert!(matches!(error, Http1Error::AmbiguousFraming(_)), "{error}");

        let error = body_kind(
            &head_with(&[("Content-Length", "5"), ("Content-Length", "6")], 200),
            "GET",
        )
        .expect_err("重复且不一致");
        assert!(matches!(error, Http1Error::AmbiguousFraming(_)), "{error}");

        // 重复但一致：允许（有些反向代理会重复写同一个值）。
        assert_eq!(
            body_kind(
                &head_with(&[("Content-Length", "5"), ("Content-Length", "5")], 200),
                "GET"
            )
            .expect("一致即可"),
            BodyKind::Length(5)
        );
    }

    #[test]
    fn chunked_bodies_decode_across_arbitrary_boundaries() {
        let raw = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (body, used) = decode_chunked(raw).expect("应当解码").expect("应当完整");
        assert_eq!(body, b"hello world");
        assert_eq!(used, raw.len());

        // 逐字节喂进去：每一步要么「不足」要么在末尾成功。
        for cut in 1..raw.len() {
            let result = decode_chunked(&raw[..cut]).expect("前缀不该报错");
            assert!(result.is_none(), "截到 {cut} 字节不该算完整");
        }
    }

    #[test]
    fn chunked_extensions_and_trailers_are_ignored() {
        let raw = b"4;name=value\r\nbody\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let (body, used) = decode_chunked(raw).expect("解码").expect("完整");
        assert_eq!(body, b"body");
        assert_eq!(used, raw.len());
    }

    #[test]
    fn a_broken_chunk_size_is_rejected() {
        let error = decode_chunked(b"zz\r\nhello\r\n").expect_err("长度不是十六进制");
        assert!(matches!(error, Http1Error::Malformed(_)), "{error}");
        // 少了分块数据后的 CRLF。
        let error = decode_chunked(b"5\r\nhelloXX").expect_err("缺少 CRLF");
        assert!(matches!(error, Http1Error::Malformed(_)), "{error}");
    }
}
