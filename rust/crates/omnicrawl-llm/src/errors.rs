//! 内核运行时的错误面。
//!
//! 语义基准是 `omnicrawl/llm/errors.py`：状态码阶梯（`_http_status_error`）与基于错误文案的
//! 分类启发式（`map_openai_exception`：配额、鉴权、上下文超限、连接与 TLS 关键词）。
//! 内核没有 SDK 异常对象，用 [`ExceptionView`] 描述一次失败的等价字段（错误文本、类型名、
//! 结构化错误体、状态码），分类结果的文案与可重试标记逐字对齐 Python。

use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeErrorKind {
    /// 配置或请求组装问题：重试同一个请求没有意义。
    Configuration,
    /// 回合被取消。
    Cancelled,
    /// 传输层失败或 HTTP 状态码失败。
    RequestFailed,
    /// 流式回复中断：连接断开、负载错误或工具调用被截断。
    StreamInterrupted,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub message: String,
    pub retryable: bool,
    pub status_code: Option<u16>,
}

impl RuntimeError {
    pub fn configuration(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::Configuration,
            message: message.into(),
            retryable: false,
            status_code: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: RuntimeErrorKind::Cancelled,
            message: "回合已被取消。".to_string(),
            retryable: false,
            status_code: None,
        }
    }

    /// 分类结果 → 内核错误面：文案前缀与 Python SDK 失败包装（`Agent 请求失败：{mapped.message}`）
    /// 一致，状态码与可重试标记照搬分类结果。
    pub fn from_model_error(error: ModelError) -> Self {
        Self {
            kind: RuntimeErrorKind::RequestFailed,
            message: format!("Agent 请求失败：{}", error.message),
            retryable: error.retryable,
            status_code: error.status_code,
        }
    }

    /// 流中断：默认按可重试处理，与 Python 侧 `STREAM_INTERRUPTED` 一致。
    pub fn stream_interrupted(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::StreamInterrupted,
            message: message.into(),
            retryable: true,
            status_code: None,
        }
    }

    /// Provider 在流里下发 `error` 负载。
    ///
    /// Python 侧这会变成 SDK 的 `APIError`，再经错误映射表；映射表认不出时给的就是这条
    /// 通用文案（上游 message 不会落到用户可见文本里），且不标记可重试。
    pub fn provider_error_stream() -> Self {
        Self {
            kind: RuntimeErrorKind::StreamInterrupted,
            message: "Agent 流式回复中断：模型请求失败，但未能识别具体原因。请检查网络、模型服务地址和本地配置。错误类型：APIError。"
                .to_string(),
            retryable: false,
            status_code: None,
        }
    }

    /// HTTP 状态码失败：文案与可重试标记按 Python 的状态码阶梯。
    pub fn http_status(status: u16) -> Self {
        Self::from_model_error(http_status_error(status))
    }

    /// 网关是否因为不认 `prompt_cache_key` 而拒绝：认出来才能摘掉该字段重发。
    ///
    /// 判定条件与 Python `is_unsupported_prompt_cache_error` 一致：
    /// 文案里同时出现字段名与「未知/不支持」一类标记。
    pub fn is_unsupported_prompt_cache_error(text: &str) -> bool {
        let lowered = text.to_lowercase();
        if !lowered.contains("prompt_cache_key") {
            return false;
        }
        [
            "unknown",
            "unsupported",
            "unexpected",
            "unrecognized",
            "extra",
            "invalid",
            "not permitted",
        ]
        .iter()
        .any(|marker| lowered.contains(marker))
    }
}

/// Python `ModelErrorCode` 里由错误分类与配置校验产出的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelErrorCode {
    ModelNotFound,
    ServiceUnavailable,
    RateLimited,
    ContextLengthExceeded,
    ConnectionFailed,
    RequestTimeout,
    AuthenticationFailed,
    PermissionDenied,
    InvalidRequest,
    ConfigurationError,
    Unknown,
}

impl ModelErrorCode {
    /// 与 Python 同名的字符串码。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelNotFound => "MODEL_NOT_FOUND",
            Self::ServiceUnavailable => "SERVICE_UNAVAILABLE",
            Self::RateLimited => "RATE_LIMITED",
            Self::ContextLengthExceeded => "CONTEXT_LENGTH_EXCEEDED",
            Self::ConnectionFailed => "CONNECTION_FAILED",
            Self::RequestTimeout => "REQUEST_TIMEOUT",
            Self::AuthenticationFailed => "AUTHENTICATION_FAILED",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::InvalidRequest => "INVALID_REQUEST",
            Self::ConfigurationError => "CONFIGURATION_ERROR",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// 一次模型请求失败的分类结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelError {
    pub code: ModelErrorCode,
    pub message: String,
    pub retryable: bool,
    pub status_code: Option<u16>,
}

impl ModelError {
    /// 配置类错误（Python 侧 `code=CONFIGURATION_ERROR` 的那些分支）。
    pub fn configuration(message: impl Into<String>) -> Self {
        Self {
            code: ModelErrorCode::ConfigurationError,
            message: message.into(),
            retryable: false,
            status_code: None,
        }
    }
}

/// 内核没有 SDK 异常对象：这里给出 `map_openai_exception` 用到的等价字段。
#[derive(Debug, Clone, Copy, Default)]
pub struct ExceptionView<'a> {
    /// `str(exc)`。
    pub message: &'a str,
    /// `type(exc).__name__`：只进「未能识别」的兜底文案。
    pub type_name: &'a str,
    /// `exc.body`：SDK 已解析的错误体（字符串或对象）。
    pub body: Option<&'a Value>,
    /// `exc.response.json()` 的结果；`None` 表示响应体不是 JSON（与 Python 侧跳过等价）。
    pub response_json: Option<&'a Value>,
    /// `exc.status_code`。
    pub status_code: Option<i64>,
    /// `exc.response.status_code`：Python 依次看这两个属性，取第一个落在 400–599 的值。
    pub response_status_code: Option<i64>,
}

/// 状态码阶梯（Python `_http_status_error`）。
pub fn http_status_error(status: u16) -> ModelError {
    let (code, message, retryable): (ModelErrorCode, String, bool) = match status {
        400 => (
            ModelErrorCode::InvalidRequest,
            "模型服务拒绝了请求参数（HTTP 400）。请检查模型名称、思考配置、消息格式或网关兼容性。".to_string(),
            false,
        ),
        401 => (
            ModelErrorCode::AuthenticationFailed,
            "模型服务鉴权失败（HTTP 401）。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。".to_string(),
            false,
        ),
        403 => (
            ModelErrorCode::PermissionDenied,
            "当前 API Key 没有访问该模型或接口的权限（HTTP 403）。请检查模型权限、账号权限或网关配置。".to_string(),
            false,
        ),
        404 => (
            ModelErrorCode::ModelNotFound,
            "模型或接口地址不存在（HTTP 404）。请检查 base_url 和 model。".to_string(),
            false,
        ),
        408 => (
            ModelErrorCode::RequestTimeout,
            "模型服务请求超时（HTTP 408）。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。".to_string(),
            true,
        ),
        409 => (
            ModelErrorCode::ServiceUnavailable,
            "模型服务暂时无法处理该请求（HTTP 409）。请稍后重试。".to_string(),
            true,
        ),
        422 => (
            ModelErrorCode::InvalidRequest,
            "模型服务无法处理当前请求内容（HTTP 422）。请检查模型参数、消息格式或网关兼容性。".to_string(),
            false,
        ),
        429 => (
            ModelErrorCode::RateLimited,
            "模型服务限流或额度不足（HTTP 429）。请稍后重试，或检查账号额度和并发限制。".to_string(),
            true,
        ),
        500..=599 => (
            ModelErrorCode::ServiceUnavailable,
            format!(
                "模型服务网关暂时不可用（HTTP {status}）。请稍后重试；如果持续出现，请检查网关或上游模型服务状态。"
            ),
            true,
        ),
        _ => (
            ModelErrorCode::Unknown,
            format!("模型服务返回错误状态（HTTP {status}）。请检查模型配置、网络和网关状态。"),
            false,
        ),
    };
    ModelError {
        code,
        message,
        retryable,
        status_code: Some(status),
    }
}

/// 把一次失败映射成统一错误（Python `map_openai_exception`）：分支顺序与关键词逐条对齐，
/// 「先文案后状态码」的先后也一致——文案分支优先，认不出才落到状态码阶梯。
pub fn map_exception(view: &ExceptionView<'_>, known_models: &[&str]) -> ModelError {
    let message = view.message.trim();
    let detail = structured_error_text(view);
    let lowered = [message, detail.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
        .to_lowercase();
    let status_code = extract_http_status_code(view, message);

    if lowered.contains("model not found") || lowered.contains("invalid_model") {
        let models = if known_models.is_empty() {
            "请检查模型名与账号权限".to_string()
        } else {
            known_models.join("、")
        };
        return ModelError {
            code: ModelErrorCode::ModelNotFound,
            message: format!("模型不存在或当前账号无权使用该模型。可用模型示例：{models}。"),
            retryable: false,
            status_code: status_code.or(Some(404)),
        };
    }

    if looks_like_html_error(message) {
        if let Some(code) = status_code {
            return http_status_error(code);
        }
        return ModelError {
            code: ModelErrorCode::ServiceUnavailable,
            message: "模型服务返回了非 JSON 错误页面，可能是网关、反向代理或上游服务异常。请稍后重试，或检查 base_url 对应的服务状态。".to_string(),
            retryable: true,
            status_code: None,
        };
    }

    if contains_any(
        &lowered,
        &[
            "rate limit",
            "too many requests",
            "insufficient_quota",
            "quota",
            "429",
        ],
    ) {
        return ModelError {
            code: ModelErrorCode::RateLimited,
            message: "模型服务限流或额度不足。请稍后重试，或检查账号额度和并发限制。".to_string(),
            retryable: true,
            status_code: Some(429),
        };
    }

    if contains_any(
        &lowered,
        &[
            "context length",
            "context window",
            "maximum context",
            "max context",
            "context limit",
            "too many tokens",
            "token limit",
            "input is too long",
            "prompt is too long",
            "请求过长",
            "上下文过长",
            "上下文长度",
            "超过上下文",
            "超出上下文",
            "token 超限",
            "令牌超限",
        ],
    ) {
        return ModelError {
            code: ModelErrorCode::ContextLengthExceeded,
            message: "模型服务拒绝请求：输入上下文超过该模型的容量上限。".to_string(),
            retryable: false,
            status_code,
        };
    }

    if let Some(code) = status_code {
        return http_status_error(code);
    }

    if contains_any(
        &lowered,
        &["peer closed connection", "incomplete chunked read"],
    ) {
        return ModelError {
            code: ModelErrorCode::ConnectionFailed,
            message: "模型服务连接提前断开。系统会按重试策略重新请求；如果持续失败，请稍后重试或检查网关稳定性。"
                .to_string(),
            retryable: true,
            status_code: None,
        };
    }

    if contains_any(
        &lowered,
        &[
            "remote protocol error",
            "server disconnected",
            "connection reset",
            "connection aborted",
            "broken pipe",
        ],
    ) {
        return ModelError {
            code: ModelErrorCode::ConnectionFailed,
            message:
                "模型服务连接被中途断开。请稍后重试；如果频繁出现，请检查网络或模型网关稳定性。"
                    .to_string(),
            retryable: true,
            status_code: None,
        };
    }

    if contains_any(
        &lowered,
        &["timeout", "timed out", "readtimeout", "connecttimeout"],
    ) {
        return ModelError {
            code: ModelErrorCode::RequestTimeout,
            message: "模型服务请求超时。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。"
                .to_string(),
            retryable: true,
            status_code: None,
        };
    }

    if contains_any(
        &lowered,
        &["invalid_api_key", "authentication", "unauthorized", "401"],
    ) {
        return ModelError {
            code: ModelErrorCode::AuthenticationFailed,
            message:
                "模型服务鉴权失败。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。"
                    .to_string(),
            retryable: false,
            status_code: Some(401),
        };
    }

    if contains_any(&lowered, &["permission", "forbidden", "403"]) {
        return ModelError {
            code: ModelErrorCode::PermissionDenied,
            message:
                "当前 API Key 没有访问该模型或接口的权限。请检查模型权限、账号权限或网关配置。"
                    .to_string(),
            retryable: false,
            status_code: Some(403),
        };
    }

    if contains_any(
        &lowered,
        &[
            "connection",
            "connecterror",
            "dns",
            "name resolution",
            "temporary failure",
            "failed to resolve",
            "nodename",
        ],
    ) {
        return ModelError {
            code: ModelErrorCode::ConnectionFailed,
            message: "无法连接模型服务。请检查网络、代理配置和 base_url 是否可达。".to_string(),
            retryable: true,
            status_code: None,
        };
    }

    if contains_any(&lowered, &["ssl", "certificate", "tls"]) {
        return ModelError {
            code: ModelErrorCode::ConnectionFailed,
            message: "模型服务 TLS/证书校验失败。请检查网关证书、代理或本机证书配置。".to_string(),
            retryable: true,
            status_code: None,
        };
    }

    ModelError {
        code: ModelErrorCode::Unknown,
        message: format!(
            "模型请求失败，但未能识别具体原因。请检查网络、模型服务地址和本地配置。错误类型：{}。",
            view.type_name
        ),
        retryable: false,
        status_code: None,
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Python `_extract_structured_error_text` → `_collect_error_text_fragments`：
/// 只取标准错误字段，深度 ≤ 4、最多 12 段、每段截到 512 个字符。
fn structured_error_text(view: &ExceptionView<'_>) -> String {
    let mut fragments: Vec<String> = Vec::new();
    for payload in [view.body, view.response_json].into_iter().flatten() {
        collect_error_text_fragments(payload, &mut fragments, 0);
    }
    fragments.join("\n")
}

fn collect_error_text_fragments(value: &Value, fragments: &mut Vec<String>, depth: usize) {
    if depth > 4 || fragments.len() >= 12 {
        return;
    }
    let Value::Object(entries) = value else {
        if let Value::String(text) = value {
            fragments.push(truncate_chars(text, 512));
        }
        return;
    };
    for key in ["message", "type", "code", "error", "detail"] {
        let Some(item) = entries.get(key) else {
            continue;
        };
        match item {
            Value::String(text) => fragments.push(truncate_chars(text, 512)),
            Value::Object(_) => collect_error_text_fragments(item, fragments, depth + 1),
            Value::Array(items) => {
                for nested in items.iter().take(4) {
                    collect_error_text_fragments(nested, fragments, depth + 1);
                }
            }
            _ => {}
        }
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Python `_extract_http_status_code`：先依次看异常自带的两个状态码属性，再在文案里找
/// `\b[45]\d{2}\b`。
fn extract_http_status_code(view: &ExceptionView<'_>, message: &str) -> Option<u16> {
    for value in [view.status_code, view.response_status_code]
        .into_iter()
        .flatten()
    {
        if (400..=599).contains(&value) {
            return Some(value as u16);
        }
    }
    find_http_status_in_text(message)
}

fn find_http_status_in_text(message: &str) -> Option<u16> {
    let characters: Vec<char> = message.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        if !matches!(character, '4' | '5') {
            continue;
        }
        if index > 0 && is_word_character(characters[index - 1]) {
            continue;
        }
        if !characters[index + 1..]
            .iter()
            .take(2)
            .all(|item| item.is_ascii_digit())
        {
            continue;
        }
        if characters
            .get(index + 3)
            .is_some_and(|item| is_word_character(*item))
        {
            continue;
        }
        let text: String = characters[index..index + 3].iter().collect();
        return text.parse().ok();
    }
    None
}

/// Python `_looks_like_html_error`：`<!doctype\s+html|<html\b|<head\b|<body\b|<h1\b|</html>`。
fn looks_like_html_error(message: &str) -> bool {
    let lowered: Vec<char> = message.to_lowercase().chars().collect();
    for (index, character) in lowered.iter().enumerate() {
        if *character != '<' {
            continue;
        }
        if starts_with_at(&lowered, index, "</html>") {
            return true;
        }
        if starts_with_at(&lowered, index, "<!doctype") {
            let mut cursor = index + 9;
            let mut whitespace = 0;
            while lowered.get(cursor).is_some_and(|item| item.is_whitespace()) {
                cursor += 1;
                whitespace += 1;
            }
            if whitespace > 0 && starts_with_at(&lowered, cursor, "html") {
                return true;
            }
        }
        for tag in ["<html", "<head", "<body", "<h1"] {
            if starts_with_at(&lowered, index, tag)
                && !lowered
                    .get(index + tag.chars().count())
                    .is_some_and(|item| is_word_character(*item))
            {
                return true;
            }
        }
    }
    false
}

fn starts_with_at(characters: &[char], index: usize, needle: &str) -> bool {
    let mut cursor = index;
    for expected in needle.chars() {
        match characters.get(cursor) {
            Some(actual) if *actual == expected => cursor += 1,
            _ => return false,
        }
    }
    true
}

/// Python `\w` 的近似：字母数字（含非 ASCII）与下划线。
fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}
