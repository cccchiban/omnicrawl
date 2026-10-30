//! JSON Patch 的应用（`add` / `replace` / `remove`）。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// 校验 transform Patch，并返回规范化后的操作列表。
pub fn validate_json_patch(
    patch: &Value,
    hook: &str,
) -> Result<Vec<Map<String, Value>>, PluginManifestError> {
    let allowlist = hook_patch_allowlist(hook);
    if allowlist.is_empty() {
        return Err(PluginManifestError::new(format!(
            "Hook {hook} 不允许 transform Patch。"
        )));
    }
    let Some(items) = patch.as_array() else {
        return Err(PluginManifestError::new("patch 必须是数组。"));
    };
    let mut normalized: Vec<Map<String, Value>> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(map) = item.as_object() else {
            return Err(PluginManifestError::new(format!(
                "patch[{index}] 必须是对象。"
            )));
        };
        let op = text_of(map.get("op")).trim().to_string();
        let path = text_of(map.get("path")).trim().to_string();
        if !JSON_PATCH_OPS.contains(&op.as_str()) {
            return Err(PluginManifestError::new(format!(
                "patch[{index}] 不支持 op={op}，V1 仅允许 add/replace/remove。"
            )));
        }
        if !path.starts_with('/') {
            return Err(PluginManifestError::new(format!(
                "patch[{index}].path 必须是 JSON Pointer：{path}"
            )));
        }
        if !path_matches_allowlist(&path, allowlist) {
            return Err(PluginManifestError::new(format!(
                "patch[{index}].path 不在 Hook {hook} 白名单：{path}"
            )));
        }
        let mut entry = Map::new();
        entry.insert("op".to_string(), Value::from(op.clone()));
        entry.insert("path".to_string(), Value::from(path));
        if op == "add" || op == "replace" {
            let Some(value) = map.get("value") else {
                return Err(PluginManifestError::new(format!(
                    "patch[{index}] 缺少 value。"
                )));
            };
            entry.insert("value".to_string(), value.clone());
        }
        normalized.push(entry);
    }
    Ok(normalized)
}

/// 在深拷贝上应用受限 JSON Patch：只实现 V1 需要的 add/replace/remove，以及数组尾插 `-`。
pub fn apply_json_patch(
    document: &Map<String, Value>,
    patch: &[Map<String, Value>],
) -> Result<Map<String, Value>, PluginManifestError> {
    let mut result = Value::Object(document.clone());
    for operation in patch {
        let op = text_of(operation.get("op"));
        let path = text_of(operation.get("path"));
        let parts: Vec<String> = path.split('/').skip(1).map(decode_pointer_token).collect();
        if parts.is_empty() {
            return Err(PluginManifestError::new("不允许 Patch 根文档。"));
        }
        let mut cursor = &mut result;
        for part in &parts[..parts.len() - 1] {
            cursor = descend(cursor, part)?;
        }
        let last = parts[parts.len() - 1].clone();
        apply_operation(cursor, &last, &op, operation.get("value"), &path)?;
    }
    match result {
        Value::Object(map) => Ok(map),
        _ => Err(PluginManifestError::new("Patch 结果不是对象。")),
    }
}

fn descend<'a>(cursor: &'a mut Value, key: &str) -> Result<&'a mut Value, PluginManifestError> {
    if let Value::Array(items) = cursor {
        let index: usize = key
            .parse()
            .map_err(|_| PluginManifestError::new(format!("Patch 路径的数组下标非法：{key}")))?;
        if index >= items.len() {
            return Err(PluginManifestError::new(format!(
                "Patch 路径的数组下标越界：{key}"
            )));
        }
        return Ok(&mut items[index]);
    }
    if !cursor.is_object() {
        *cursor = Value::Object(Map::new());
    }
    let map = cursor.as_object_mut().expect("已归一化为对象");
    let needs_container = !matches!(map.get(key), Some(Value::Object(_)) | Some(Value::Array(_)));
    if needs_container {
        map.insert(key.to_string(), Value::Object(Map::new()));
    }
    Ok(map.get_mut(key).expect("刚刚写入"))
}

fn apply_operation(
    parent: &mut Value,
    last: &str,
    op: &str,
    value: Option<&Value>,
    path: &str,
) -> Result<(), PluginManifestError> {
    match op {
        "add" => {
            let value = value.cloned().unwrap_or(Value::Null);
            if let Value::Array(items) = parent {
                if last == "-" {
                    items.push(value);
                } else {
                    let index: usize = last.parse().map_err(|_| {
                        PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                    })?;
                    // Python 的 list.insert 越界会追加到末尾，这里保持同一语义。
                    if index >= items.len() {
                        items.push(value);
                    } else {
                        items.insert(index, value);
                    }
                }
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                map.insert(last.to_string(), value);
            }
        }
        "replace" => {
            let value = value.cloned().unwrap_or(Value::Null);
            if let Value::Array(items) = parent {
                let index: usize = last.parse().map_err(|_| {
                    PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                })?;
                if index >= items.len() {
                    return Err(PluginManifestError::new(format!(
                        "replace 目标不存在：{path}"
                    )));
                }
                items[index] = value;
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                if !map.contains_key(last) {
                    return Err(PluginManifestError::new(format!(
                        "replace 目标不存在：{path}"
                    )));
                }
                map.insert(last.to_string(), value);
            }
        }
        "remove" => {
            if let Value::Array(items) = parent {
                let index: usize = last.parse().map_err(|_| {
                    PluginManifestError::new(format!("Patch 路径的数组下标非法：{last}"))
                })?;
                if index >= items.len() {
                    return Err(PluginManifestError::new(format!(
                        "remove 目标不存在：{path}"
                    )));
                }
                items.remove(index);
            } else {
                let map = parent.as_object_mut().expect("非数组即对象");
                if map.remove(last).is_none() {
                    return Err(PluginManifestError::new(format!(
                        "remove 目标不存在：{path}"
                    )));
                }
            }
        }
        other => {
            return Err(PluginManifestError::new(format!(
                "不支持的 Patch op：{other}"
            )));
        }
    }
    Ok(())
}

fn decode_pointer_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}
