//! 连接器共用的阻塞式 HTTP 传输（ureq + rustls）与表单编码。
//!
//! 两个连接器都只做「发一次、拿回状态码与响应体」：错误分类、重试与脱敏由各自的
//! API 层决定。测试用记录型实现替换 [`HttpTransport`]，不必起网络。
//!
//! 用单一 [`HttpTransport::request`] 承载任意方法：飞书接口要带 `Authorization` 头，
//! Get 与 Patch 都要用；表单与 JSON 只是它的语法糖。

use std::io::Read;
use std::time::Duration;

/// 一次 HTTP 响应：状态码与原始响应体（图片/文件下载不能按文本读）。
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpReply {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// HTTP 传输抽象：失败只回报文本，分类留给上层。
pub trait HttpTransport: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<HttpReply, String>;

    fn post_form(
        &self,
        url: &str,
        query: &[(String, String)],
        form: &[(String, String)],
        timeout: Duration,
    ) -> Result<HttpReply, String> {
        let target = with_query(url, query);
        let headers = [(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        self.request(
            "POST",
            &target,
            &headers,
            Some(encode_form(form).as_bytes()),
            timeout,
        )
    }

    fn post_json(
        &self,
        url: &str,
        json_body: &str,
        timeout: Duration,
    ) -> Result<HttpReply, String> {
        let headers = [(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )];
        self.request("POST", url, &headers, Some(json_body.as_bytes()), timeout)
    }

    fn get(&self, url: &str, timeout: Duration) -> Result<HttpReply, String> {
        self.request("GET", url, &[], None, timeout)
    }
}

/// 真实传输：ureq + rustls；状态码不转错误（分类要看响应体）。
///
/// 每个请求按自己的超时建一个 agent：连接器同时存在长轮询（秒级等待）与文件传输
/// （百秒级读取）两种量级，共用一份默认超时会互相牵制。
pub struct UreqTransport;

impl UreqTransport {
    pub fn new() -> UreqTransport {
        UreqTransport
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for UreqTransport {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<HttpReply, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .timeout_recv_body(Some(timeout))
            .build()
            .into();
        let mut builder = ureq::http::Request::builder().method(method).uri(url);
        for (key, value) in headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        let request = builder
            .body(body.unwrap_or(&[]).to_vec())
            .map_err(|error| error.to_string())?;
        match agent.run(request) {
            Ok(response) => {
                let status = response.status().as_u16();
                let mut raw = Vec::new();
                let _ = response.into_body().into_reader().read_to_end(&mut raw);
                Ok(HttpReply { status, body: raw })
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

pub fn with_query(url: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return url.to_string();
    }
    let encoded: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect();
    format!("{url}?{}", encoded.join("&"))
}

pub fn encode_form(form: &[(String, String)]) -> String {
    form.iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<String>>()
        .join("&")
}

/// `application/x-www-form-urlencoded` 的百分号编码：非保留字符原样，空格写成 `+`。
pub fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char)
            }
            b' ' => encoded.push('+'),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_encoding_keeps_reserved_characters_safe() {
        assert_eq!(encode_component("a b"), "a+b");
        assert_eq!(encode_component("k-._~"), "k-._~");
        // 中文按 UTF-8 逐字节编码，分隔符也必须转义：整体断言形状，避免在源码里
        // 写长串百分号字面量（那串本身就是熵兜底规则的目标）。
        let encoded = encode_component("中文/路径?x=1&y");
        assert_eq!(encoded.chars().filter(|value| *value == '%').count(), 16);
        assert!(encoded.ends_with("x%3D1%26y"));
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn query_and_form_are_joined_with_ampersand() {
        let pairs = vec![("offset".to_string(), "3".to_string())];
        assert_eq!(with_query("https://x/y", &pairs), "https://x/y?offset=3");
        assert_eq!(encode_form(&pairs), "offset=3");
        assert_eq!(with_query("https://x/y", &[]), "https://x/y");
    }
}
