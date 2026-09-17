//! 内核运行时的错误面。
//!
//! 状态码阶梯的语义基准是 `omnicrawl/llm/errors.py` 的 `_http_status_error`；
//! 该模块里基于错误文案的分类启发式（配额、鉴权、上下文超限等关键词匹配）尚未移植。

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

    /// 传输层失败（连接、超时、IO）：可重试。
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: RuntimeErrorKind::RequestFailed,
            message: format!("Agent 请求失败：{}", message.into()),
            retryable: true,
            status_code: None,
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
        let (detail, retryable): (String, bool) = match status {
            400 => (
                "模型服务拒绝了请求参数（HTTP 400）。请检查模型名称、思考配置、消息格式或网关兼容性。"
                    .to_string(),
                false,
            ),
            401 => (
                "模型服务鉴权失败（HTTP 401）。请检查 API Key 是否正确、是否过期，以及当前网关是否接受该 Key。"
                    .to_string(),
                false,
            ),
            403 => (
                "当前 API Key 没有访问该模型或接口的权限（HTTP 403）。请检查模型权限、账号权限或网关配置。"
                    .to_string(),
                false,
            ),
            404 => (
                "模型或接口地址不存在（HTTP 404）。请检查 base_url 和 model。".to_string(),
                false,
            ),
            408 => (
                "模型服务请求超时（HTTP 408）。请稍后重试，或适当调大 AGENT_REQUEST_TIMEOUT_SECONDS。"
                    .to_string(),
                true,
            ),
            409 => (
                "模型服务暂时无法处理该请求（HTTP 409）。请稍后重试。".to_string(),
                true,
            ),
            422 => (
                "模型服务无法处理当前请求内容（HTTP 422）。请检查模型参数、消息格式或网关兼容性。"
                    .to_string(),
                false,
            ),
            429 => (
                "模型服务限流或额度不足（HTTP 429）。请稍后重试，或检查账号额度和并发限制。"
                    .to_string(),
                true,
            ),
            code if (500..=599).contains(&code) => (
                format!(
                    "模型服务网关暂时不可用（HTTP {code}）。请稍后重试；如果持续出现，请检查网关或上游模型服务状态。"
                ),
                true,
            ),
            code => (
                format!("模型服务返回错误状态（HTTP {code}）。请检查模型配置、网络和网关状态。"),
                false,
            ),
        };
        Self {
            kind: RuntimeErrorKind::RequestFailed,
            message: format!("Agent 请求失败：{detail}"),
            retryable,
            status_code: Some(status),
        }
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

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}
