//! 载荷与 JSON Patch 的校验规则。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// 对自定义事件 payload 做 V1 轻量 schema 校验；不引入 jsonschema 依赖，避免安装面扩大。
pub fn validate_payload_against_schema(
    payload: &Value,
    schema: Option<&Map<String, Value>>,
) -> Result<(), PluginManifestError> {
    let schema = match schema {
        Some(value) if !value.is_empty() => value,
        _ => {
            if !payload.is_null() && !payload.is_object() {
                return Err(PluginManifestError::new(
                    "自定义事件 payload 默认必须是对象。",
                ));
            }
            return Ok(());
        }
    };

    let expected_type = text_of(schema.get("type"));
    if expected_type == "object" {
        let Some(payload_map) = payload.as_object() else {
            return Err(PluginManifestError::new("自定义事件 payload 必须是对象。"));
        };
        // Python 里 properties 缺省是 `{}`、显式 null 是 `None`，两者的 additionalProperties
        // 行为不同：前者把未声明字段全判为多余，后者跳过检查。
        let properties = match schema.get("properties") {
            Some(Value::Object(value)) => Some(value.clone()),
            Some(Value::Null) => None,
            None => Some(Map::new()),
            Some(_) => {
                return Err(PluginManifestError::new("schema.properties 必须是对象。"));
            }
        };
        let required = match schema.get("required") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.clone(),
            Some(_) => {
                return Err(PluginManifestError::new("schema.required 必须是数组。"));
            }
        };
        for key in &required {
            let key_text = text_of(Some(key));
            if !payload_map.contains_key(&key_text) {
                return Err(PluginManifestError::new(format!(
                    "自定义事件 payload 缺少必填字段：{key_text}"
                )));
            }
        }
        let additional = bool_of(schema.get("additionalProperties"), true);
        if !additional {
            if let Some(declared) = &properties {
                let mut extras: Vec<String> = payload_map
                    .keys()
                    .filter(|key| !declared.contains_key(*key))
                    .cloned()
                    .collect();
                if !extras.is_empty() {
                    extras.sort();
                    return Err(PluginManifestError::new(format!(
                        "自定义事件 payload 含未声明字段：{}",
                        extras.join(", ")
                    )));
                }
            }
        }
        if let Some(declared) = &properties {
            for (key, child_schema) in declared {
                let Some(value) = payload_map.get(key) else {
                    continue;
                };
                let Some(child_map) = child_schema.as_object() else {
                    continue;
                };
                let child_type = text_of(child_map.get("type"));
                let mismatch = match child_type.as_str() {
                    "string" => !value.is_string(),
                    "number" => !value.is_number(),
                    "integer" => value.as_i64().is_none(),
                    "boolean" => !value.is_boolean(),
                    "object" => !value.is_object(),
                    "array" => !value.is_array(),
                    _ => false,
                };
                if mismatch {
                    return Err(PluginManifestError::new(format!(
                        "字段 {key} 必须是 {child_type}"
                    )));
                }
            }
        }
        return Ok(());
    }
    if expected_type == "array" {
        if !payload.is_array() {
            return Err(PluginManifestError::new("自定义事件 payload 必须是数组。"));
        }
        return Ok(());
    }
    if expected_type == "string" {
        if !payload.is_string() {
            return Err(PluginManifestError::new(
                "自定义事件 payload 必须是 string。",
            ));
        }
        return Ok(());
    }
    // 未识别 type 时仅要求可 JSON 序列化对象/标量。
    if !payload.is_null()
        && !payload.is_object()
        && !payload.is_array()
        && !payload.is_string()
        && !payload.is_number()
        && !payload.is_boolean()
    {
        return Err(PluginManifestError::new(
            "自定义事件 payload 类型不受支持。",
        ));
    }
    Ok(())
}

/// 判断 JSON Pointer 是否命中 allowlist；支持单段 `*` 通配。
pub fn path_matches_allowlist(path: &str, allowlist: &[&str]) -> bool {
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    allowlist
        .iter()
        .any(|pattern| pointer_match(&normalized, pattern))
}

fn pointer_match(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').skip(1).collect();
    let pattern_parts: Vec<&str> = pattern.split('/').skip(1).collect();
    if path_parts.len() != pattern_parts.len() {
        // 允许数组尾插：/payload/tags/- 与 /payload/tags 同级规则由 allowlist 显式列出。
        return false;
    }
    path_parts
        .iter()
        .zip(pattern_parts.iter())
        .all(|(actual, expected)| *expected == "*" || actual == expected)
}
