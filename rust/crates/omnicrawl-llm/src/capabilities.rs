//! 模型能力定义与合并规则。
//!
//! 语义基准是 Python `omnicrawl/llm/capabilities.py`：bool 字段用 `None` 表示「本层未声明」，
//! 合并时不得覆盖下层已有值；对外读取一律先 `resolved()`（`streaming` 缺省为真，其余为假）。

use serde_json::{Map, Value};

/// 模型能力：Host 关心的流式、工具、推理等开关，加上两个窗口值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub streaming: Option<bool>,
    pub tools: Option<bool>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning: Option<bool>,
    pub vision: Option<bool>,
    pub model_discovery: Option<bool>,
    pub prompt_cache: Option<bool>,
    pub context_window_tokens: i64,
    pub max_output_tokens: i64,
}

impl ModelCapabilities {
    /// 把 `None` 填成对外可用的布尔默认值，便于 Runtime 门禁判断。
    pub fn resolved(self) -> Self {
        Self {
            streaming: Some(self.streaming.unwrap_or(true)),
            tools: Some(self.tools.unwrap_or(false)),
            parallel_tool_calls: Some(self.parallel_tool_calls.unwrap_or(false)),
            reasoning: Some(self.reasoning.unwrap_or(false)),
            vision: Some(self.vision.unwrap_or(false)),
            model_discovery: Some(self.model_discovery.unwrap_or(false)),
            prompt_cache: Some(self.prompt_cache.unwrap_or(false)),
            context_window_tokens: self.context_window_tokens,
            max_output_tokens: self.max_output_tokens,
        }
    }

    /// Python `to_dict()`：键序一致，值先 resolve。
    pub fn to_map(self) -> Map<String, Value> {
        let resolved = self.resolved();
        let mut map = Map::new();
        for (key, value) in [
            ("streaming", resolved.streaming),
            ("tools", resolved.tools),
            ("parallel_tool_calls", resolved.parallel_tool_calls),
            ("reasoning", resolved.reasoning),
            ("vision", resolved.vision),
            ("model_discovery", resolved.model_discovery),
            ("prompt_cache", resolved.prompt_cache),
        ] {
            map.insert(key.to_string(), Value::Bool(value.unwrap_or_default()));
        }
        map.insert(
            "context_window_tokens".to_string(),
            Value::from(resolved.context_window_tokens),
        );
        map.insert(
            "max_output_tokens".to_string(),
            Value::from(resolved.max_output_tokens),
        );
        map
    }

    /// Python `capabilities_from_mapping`：未知字段忽略，非法值回落缺省。
    pub fn from_mapping(raw: &Value) -> Self {
        let Value::Object(entries) = raw else {
            return Self::default();
        };
        Self {
            streaming: bool_field(entries, "streaming"),
            tools: bool_field(entries, "tools"),
            parallel_tool_calls: bool_field(entries, "parallel_tool_calls"),
            reasoning: bool_field(entries, "reasoning"),
            vision: bool_field(entries, "vision"),
            model_discovery: bool_field(entries, "model_discovery"),
            prompt_cache: bool_field(entries, "prompt_cache"),
            context_window_tokens: int_field(entries, "context_window_tokens"),
            max_output_tokens: int_field(entries, "max_output_tokens"),
        }
    }

    /// Adapter 的保守默认值：OpenAI Chat Completions。
    pub fn conservative_openai_chat() -> Self {
        Self {
            streaming: Some(true),
            tools: Some(true),
            parallel_tool_calls: Some(true),
            reasoning: Some(false),
            vision: Some(false),
            model_discovery: Some(true),
            prompt_cache: Some(false),
            ..Self::default()
        }
    }

    /// Adapter 的保守默认值：OpenAI Responses。
    pub fn conservative_openai_responses() -> Self {
        Self {
            streaming: Some(true),
            tools: Some(true),
            parallel_tool_calls: Some(true),
            reasoning: Some(true),
            vision: Some(false),
            model_discovery: Some(true),
            prompt_cache: Some(false),
            ..Self::default()
        }
    }

    /// Adapter 的保守默认值：Anthropic Messages。
    pub fn conservative_anthropic() -> Self {
        Self {
            streaming: Some(true),
            tools: Some(true),
            parallel_tool_calls: Some(true),
            reasoning: Some(true),
            vision: Some(true),
            model_discovery: Some(true),
            prompt_cache: Some(true),
            ..Self::default()
        }
    }

    /// Adapter 的保守默认值：Gemini Generate Content。
    pub fn conservative_gemini() -> Self {
        Self {
            streaming: Some(true),
            tools: Some(true),
            parallel_tool_calls: Some(true),
            reasoning: Some(false),
            vision: Some(true),
            model_discovery: Some(true),
            prompt_cache: Some(false),
            ..Self::default()
        }
    }
}

/// 按优先级合并能力层（后者覆盖前者中「已声明」的字段）。
///
/// bool 只在 `Some` 时覆盖（可显式写 `false`），整数只在正数时覆盖；
/// 调用时按从低到高传入：adapter 保守默认 < Provider 自动发现 < 用户显式配置。
pub fn merge_capabilities(layers: &[Option<ModelCapabilities>]) -> ModelCapabilities {
    let mut result = ModelCapabilities::default();
    for layer in layers.iter().flatten() {
        if let Some(value) = layer.streaming {
            result.streaming = Some(value);
        }
        if let Some(value) = layer.tools {
            result.tools = Some(value);
        }
        if let Some(value) = layer.parallel_tool_calls {
            result.parallel_tool_calls = Some(value);
        }
        if let Some(value) = layer.reasoning {
            result.reasoning = Some(value);
        }
        if let Some(value) = layer.vision {
            result.vision = Some(value);
        }
        if let Some(value) = layer.model_discovery {
            result.model_discovery = Some(value);
        }
        if let Some(value) = layer.prompt_cache {
            result.prompt_cache = Some(value);
        }
        if layer.context_window_tokens > 0 {
            result.context_window_tokens = layer.context_window_tokens;
        }
        if layer.max_output_tokens > 0 {
            result.max_output_tokens = layer.max_output_tokens;
        }
    }
    result.resolved()
}

/// 缺字段为 `None`，非布尔值同样为 `None`（Python 只认真正的 bool）。
fn bool_field(entries: &Map<String, Value>, key: &str) -> Option<bool> {
    match entries.get(key) {
        Some(Value::Bool(flag)) => Some(*flag),
        _ => None,
    }
}

/// 缺字段、布尔、非整数与负数一律回落 0（与 Python 的 `_int` 判定一致）。
fn int_field(entries: &Map<String, Value>, key: &str) -> i64 {
    match entries.get(key).and_then(Value::as_i64) {
        Some(value) if value >= 0 => value,
        _ => 0,
    }
}
