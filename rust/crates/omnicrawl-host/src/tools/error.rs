//! 工作区工具的错误与结果信封。
//!
//! 语义基准是 `omnicrawl/workspace/tools.py` 的 `WorkspaceToolError`：错误既有给模型看的
//! 文本，也有机器可识别的稳定错误码与可重试标记；结果信封对应 `omnicrawl-core` 的
//! `ToolResult`（`output` 同时进 `full_output`，本宿主暂不做输出预算归档）。

use omnicrawl_core::ToolResult;

/// 工具参数校验或执行失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
    pub code: Option<String>,
    pub retryable: bool,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            retryable: false,
        }
    }

    pub fn coded(message: impl Into<String>, code: &str, retryable: bool) -> Self {
        Self {
            message: message.into(),
            code: Some(code.to_string()),
            retryable,
        }
    }

    /// 模型可见文本：带错误码与重试提示（与 Python 的 `formatted_message` 一致）。
    pub fn formatted(&self) -> String {
        match &self.code {
            Some(code) => {
                let retry_hint = if self.retryable { "；可重试" } else { "" };
                format!("错误码：{code}{retry_hint}；{}", self.message)
            }
            None => self.message.clone(),
        }
    }

    pub fn to_result(&self) -> ToolResult {
        ToolResult {
            ok: false,
            output: self.formatted(),
            full_output: self.formatted(),
            error_code: self.code.clone(),
            retryable: self.retryable,
        }
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

pub type ToolOutcome = Result<String, ToolError>;

pub fn text_success(output: String) -> ToolResult {
    ToolResult {
        ok: true,
        full_output: output.clone(),
        output,
        error_code: None,
        retryable: false,
    }
}

pub fn text_failure(error: &ToolError) -> ToolResult {
    error.to_result()
}

/// 命令类工具自带成功/失败语义（退出码），不与文本工具共用。
pub fn command_result(ok: bool, output: String) -> ToolResult {
    ToolResult {
        ok,
        full_output: output.clone(),
        output,
        error_code: None,
        retryable: false,
    }
}
