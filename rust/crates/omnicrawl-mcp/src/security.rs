//! MCP 安全面：审批归属、参数体积与轻量 JSON Schema 校验、审计文本脱敏
//! （对应 `omnicrawl/mcp/security.py`）。

use std::sync::OnceLock;

use omnicrawl_controllers::json::{python_dumps, python_number_text};
use regex::Regex;
use serde_json::{Map, Value};

use crate::config::McpPolicyConfig;
use crate::registry::McpToolMeta;

const SENSITIVE_FIELD_NAMES: [&str; 12] = [
    "api_key",
    "apikey",
    "access_key",
    "secret_key",
    "authorization",
    "cookie",
    "password",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
];

/// 参数 JSON 文本的硬上限（Python 侧按 `json.dumps` 默认分隔符计数）。
const MAX_ARGUMENT_CHARS: usize = 100_000;
const DEFAULT_STRING_MAX_LENGTH: i64 = 20_000;
const DEFAULT_ARRAY_MAX_ITEMS: i64 = 200;
/// 脱敏只保留列表前若干项，避免超大参数让审计开销失控。
const REDACTION_LIST_LIMIT: usize = 100;

/// 所有 MCP Tool 默认需要通过 Host 审批（与 Python 同：策略参数暂不改变结论）。
pub fn mcp_tool_requires_confirmation(_meta: &McpToolMeta, _policy: &McpPolicyConfig) -> bool {
    true
}

/// 限制 MCP Tool 参数体积和基本形状。
///
/// MCP Server 会用自己的 JSON Schema 再校验一次；Host 侧先挡掉明显异常的大对象，
/// 避免模型把超大内容或非 JSON 对象直接塞给外部进程。返回 `Err` 时的文本即
/// Python 侧 `ValueError` 的文案。
pub fn validate_tool_arguments(
    arguments: &Map<String, Value>,
    input_schema: Option<&Map<String, Value>>,
) -> Result<(), String> {
    let payload = python_dumps(&Value::Object(arguments.clone()), 0);
    if payload.chars().count() > MAX_ARGUMENT_CHARS {
        return Err(format!("MCP Tool 参数超过 {MAX_ARGUMENT_CHARS} 字符。"));
    }
    if let Some(schema) = input_schema {
        validate_schema_object(arguments, schema)?;
    }
    Ok(())
}

/// 执行轻量 JSON Schema 校验，覆盖 MCP Tool 常见入参边界。
fn validate_schema_object(
    arguments: &Map<String, Value>,
    schema: &Map<String, Value>,
) -> Result<(), String> {
    match schema.get("type") {
        None => {}
        Some(Value::String(text)) if text == "object" => {}
        Some(_) => return Ok(()),
    }

    if let Some(Value::Array(required)) = schema.get("required") {
        for key in required {
            if let Value::String(name) = key {
                if !arguments.contains_key(name) {
                    return Err(format!("MCP Tool 参数缺少必填字段：{name}。"));
                }
            }
        }
    }

    let Some(Value::Object(properties)) = schema.get("properties") else {
        return Ok(());
    };
    for (key, value) in arguments {
        if let Some(Value::Object(field_schema)) = properties.get(key) {
            validate_schema_value(key, value, field_schema)?;
        }
    }
    Ok(())
}

fn validate_schema_value(
    key: &str,
    value: &Value,
    schema: &Map<String, Value>,
) -> Result<(), String> {
    let expected_types: Vec<String> = match schema.get("type") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(|text| text.to_string()))
            .collect(),
        Some(Value::String(text)) => vec![text.clone()],
        _ => Vec::new(),
    };

    if !expected_types.is_empty()
        && !expected_types
            .iter()
            .any(|expected| matches_json_type(value, expected))
    {
        return Err(format!(
            "MCP Tool 参数 {key} 类型应为 {}。",
            expected_types.join("/")
        ));
    }

    if let Value::String(text) = value {
        let max_length = read_schema_int(schema.get("maxLength"), DEFAULT_STRING_MAX_LENGTH);
        let min_length = read_schema_int(schema.get("minLength"), 0);
        let length = text.chars().count() as i64;
        if length > max_length {
            return Err(format!("MCP Tool 参数 {key} 超过 {max_length} 字符。"));
        }
        if length < min_length {
            return Err(format!("MCP Tool 参数 {key} 少于 {min_length} 字符。"));
        }
    }

    if let Value::Array(items) = value {
        let max_items = read_schema_int(schema.get("maxItems"), DEFAULT_ARRAY_MAX_ITEMS);
        let min_items = read_schema_int(schema.get("minItems"), 0);
        let length = items.len() as i64;
        if length > max_items {
            return Err(format!("MCP Tool 参数 {key} 超过 {max_items} 项。"));
        }
        if length < min_items {
            return Err(format!("MCP Tool 参数 {key} 少于 {min_items} 项。"));
        }
    }

    if is_number_like(value) {
        let number = number_of(value).unwrap_or(0.0);
        if let Some(maximum) = schema.get("maximum") {
            if let Some(bound) = number_of(maximum) {
                if number > bound {
                    return Err(format!(
                        "MCP Tool 参数 {key} 不能大于 {}。",
                        number_text(maximum)
                    ));
                }
            }
        }
        if let Some(minimum) = schema.get("minimum") {
            if let Some(bound) = number_of(minimum) {
                if number < bound {
                    return Err(format!(
                        "MCP Tool 参数 {key} 不能小于 {}。",
                        number_text(minimum)
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Python 的 `isinstance(value, (int, float))`：布尔值在 Python 里也算数字。
fn is_number_like(value: &Value) -> bool {
    matches!(value, Value::Number(_) | Value::Bool(_))
}

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn number_text(value: &Value) -> String {
    match value {
        Value::Bool(flag) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        other => python_number_text(other),
    }
}

fn matches_json_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// Python 的 `_read_int`：布尔与非法取值一律回落默认值。
fn read_schema_int(value: Option<&Value>, default: i64) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(default),
        _ => default,
    }
}

/// 递归脱敏常见密钥字段，供审计和错误输出使用。
pub fn redact_sensitive_values(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut redacted = Map::new();
            for (key, item) in map {
                let normalized = key.trim().to_lowercase().replace('-', "_");
                let header_secret = normalized
                    .strip_prefix("x_")
                    .map(|rest| SENSITIVE_FIELD_NAMES.contains(&rest))
                    .unwrap_or(false);
                if SENSITIVE_FIELD_NAMES.contains(&normalized.as_str()) || header_secret {
                    redacted.insert(key.clone(), Value::String("***".to_string()));
                } else {
                    redacted.insert(key.clone(), redact_sensitive_values(item));
                }
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(REDACTION_LIST_LIMIT)
                .map(redact_sensitive_values)
                .collect(),
        ),
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        other => other.clone(),
    }
}

/// 清理审计和诊断文本中的常见明文凭据。
pub fn redact_sensitive_text(text: &str) -> String {
    let redacted = assignment_pattern().replace_all(text, "${1}***");
    let redacted = authorization_pattern().replace_all(&redacted, "${1}Bearer ***");
    let redacted = bearer_pattern().replace_all(&redacted, "Bearer ***");
    provider_pattern().replace_all(&redacted, "***").to_string()
}

fn assignment_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?i)((?:api[_-]?key|apikey|access[_-]?key|secret[_-]?key|access[_-]?token|refresh[_-]?token|id[_-]?token|cookie|password|secret|token)\s*[:=]\s*["']?)([^"'\s,;]+)"#,
        )
        .expect("参数赋值脱敏规则必须是合法正则")
    })
}

fn authorization_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r#"(?i)(\bauthorization\s*[:=]\s*["']?)(?:Bearer\s+)?([^"'\s,;]+)"#)
            .expect("Authorization 脱敏规则必须是合法正则")
    })
}

fn bearer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("Bearer 脱敏规则必须是合法正则")
    })
}

fn provider_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\b(?:sk|ak|ah)-[A-Za-z0-9_-]{24,}\b").expect("服务商密钥规则必须是合法正则")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn required_field_is_reported() {
        let error = validate_schema_object(
            &Map::new(),
            &schema(json!({"type": "object", "required": ["path"]})),
        )
        .unwrap_err();
        assert_eq!(error, "MCP Tool 参数缺少必填字段：path。");
    }

    #[test]
    fn oversized_arguments_are_rejected_before_schema_check() {
        let mut arguments = Map::new();
        arguments.insert("text".to_string(), Value::String("啊".repeat(120_000)));
        let error = validate_tool_arguments(&arguments, None).unwrap_err();
        assert_eq!(error, "MCP Tool 参数超过 100000 字符。");
    }

    #[test]
    fn bearer_and_assignment_secrets_are_redacted() {
        let text = "Authorization: Bearer abcdefghijklmnop api_key=sk-0123456789abcdefghijklmn";
        let redacted = redact_sensitive_text(text);
        assert!(redacted.contains("Bearer ***"), "{redacted}");
        assert!(!redacted.contains("abcdefghijklmnop"), "{redacted}");
        assert!(!redacted.contains("sk-0123456789"), "{redacted}");
    }

    #[test]
    fn header_secret_keys_are_redacted_recursively() {
        let value = json!({"X-Api-Key": "abc", "nested": {"cookie": "c"}, "items": [1, 2]});
        let redacted = redact_sensitive_values(&value);
        assert_eq!(redacted["X-Api-Key"], json!("***"));
        assert_eq!(redacted["nested"]["cookie"], json!("***"));
        assert_eq!(redacted["items"], json!([1, 2]));
    }
}
