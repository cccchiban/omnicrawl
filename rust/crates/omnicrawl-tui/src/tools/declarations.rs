//! 工具声明：把工具表的参数契约转成 OpenAI functions 形状。
//!
//! 语义基准是 `omnicrawl/agent/runtime/llm_protocol.py` 的 `build_tool_declaration` /
//! `tool_parameters_schema` / `infer_tool_property_schema`（该模块尚未搬入内核，因此在宿主侧
//! 实现）：工具表里 `read` / `Edit_file` / `write_file` 的参数契约写的是**示例值**而不是
//! JSON Schema，声明前必须按示例推断类型，并用 `minProperties` 兜底禁止空对象调用。

use omnicrawl_controllers::tool_args::{compact_tool_description, compact_tool_schema};
use omnicrawl_controllers::tool_catalog::ToolSpec;
use serde_json::{json, Map, Value};

/// 一条工具的 provider 声明。
pub fn declaration(spec: &ToolSpec) -> Value {
    let inferred = tool_parameters_schema(&spec.argument_schema);
    let compacted = serde_json::to_string(&inferred)
        .map(|text| compact_tool_schema(&text))
        .unwrap_or_else(|_| json!({"type": "object", "properties": {}}));
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": compact_tool_description(&spec.description),
            "parameters": compacted,
        }
    })
}

/// 示例式契约 → 真 JSON Schema。
pub fn tool_parameters_schema(argument_schema: &str) -> Value {
    let raw = serde_json::from_str::<Value>(argument_schema)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let is_real_schema = raw.get("type").and_then(Value::as_str) == Some("object")
        && raw.get("properties").map(Value::is_object).unwrap_or(false);
    if is_real_schema {
        let mut schema = raw;
        apply_defaults(&mut schema);
        return schema;
    }

    let mut properties = Map::new();
    if let Some(example) = raw.as_object() {
        for (key, value) in example {
            properties.insert(key.clone(), infer_tool_property_schema(value));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::from("object"));
    // 示例值风格没有显式 required：只要声明了参数，就禁止空对象调用。
    if !properties.is_empty() {
        schema.insert("minProperties".to_string(), Value::from(1));
    }
    schema.insert("properties".to_string(), Value::Object(properties));
    Value::Object(schema)
}

fn apply_defaults(schema: &mut Value) {
    if let Some(object) = schema.as_object_mut() {
        object
            .entry("type".to_string())
            .or_insert_with(|| Value::from("object"));
        object
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// 按示例值推断属性类型。
pub fn infer_tool_property_schema(example: &Value) -> Value {
    match example {
        Value::Bool(_) => json!({"type": "boolean"}),
        Value::Number(number) => {
            if number.is_i64() || number.is_u64() {
                json!({"type": "integer"})
            } else {
                json!({"type": "number"})
            }
        }
        Value::Array(_) => json!({"type": "array", "items": {"type": "string"}}),
        Value::Object(_) => json!({"type": "object"}),
        _ => json!({"type": "string"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_contract_is_inferred_into_a_schema() {
        let schema = tool_parameters_schema(
            r#"{"path":"main.py","start_line":1,"max_lines":200,"function_name":"Class.method","text":"目标片段","context_lines":20}"#,
        );
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
        assert_eq!(schema["properties"]["start_line"]["type"], "integer");
        assert_eq!(schema["properties"]["context_lines"]["type"], "integer");
        assert_eq!(schema["minProperties"], 1);
        // 键序跟着示例走（保序 map），与 Python 的 dict 顺序一致。
        let keys: Vec<&String> = schema["properties"]
            .as_object()
            .expect("properties 是对象")
            .keys()
            .collect();
        assert_eq!(keys.first().map(|key| key.as_str()), Some("path"));
    }

    #[test]
    fn real_schema_is_kept_as_is() {
        let schema = tool_parameters_schema(
            r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"],"additionalProperties":false}"#,
        );
        assert_eq!(schema["required"][0], "command");
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema.get("minProperties").is_none());
    }

    #[test]
    fn array_and_bool_examples_infer_types() {
        assert_eq!(
            infer_tool_property_schema(&json!(["a", "b"])),
            json!({"type": "array", "items": {"type": "string"}})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(true)),
            json!({"type": "boolean"})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(1.5)),
            json!({"type": "number"})
        );
        assert_eq!(
            infer_tool_property_schema(&json!(null)),
            json!({"type": "string"})
        );
    }
}
