//! `omnicrawl/agent/types.py` 里与控制器域有关的数据契约。
//!
//! 只搬控制器域真正读写到的字段；`ui_artifact` 保持 JSON 值透传，内核不认识它的结构。

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: serde_json::Map<String, Value>,
    pub id: String,
    pub function_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolImageAttachment {
    pub media_type: String,
    pub data_base64: String,
    pub filename: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResult {
    pub ok: bool,
    pub output: String,
    pub full_output: String,
    pub ui_artifact: Value,
    pub model_images: Vec<ToolImageAttachment>,
    pub completed_at: Option<f64>,
    pub error_code: Option<String>,
    pub retryable: bool,
}
