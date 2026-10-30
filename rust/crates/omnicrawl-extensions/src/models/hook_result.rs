//! Hook 结果的解析与 Handler 派发结论。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

pub fn parse_hook_result(
    raw: Option<&Value>,
    handler_key: &str,
    elapsed_ms: f64,
) -> Result<HookResult, PluginProtocolError> {
    let Some(raw) = raw else {
        return Ok(HookResult::continue_result(
            handler_key,
            Map::new(),
            elapsed_ms,
            "continue",
        ));
    };
    if raw.is_null() {
        return Ok(HookResult::continue_result(
            handler_key,
            Map::new(),
            elapsed_ms,
            "continue",
        ));
    }
    let Some(map) = raw.as_object() else {
        return Err(PluginProtocolError::new(format!(
            "Handler {handler_key} 返回值必须是对象。"
        )));
    };
    let action = match map.get("action") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => "continue".to_string(),
    };
    // Python 的 `raw.get("annotations") or {}`：空列表、空串、null 都退化成空对象。
    let annotations = match map.get("annotations") {
        None => Map::new(),
        Some(value) if is_falsy(value) => Map::new(),
        Some(Value::Object(value)) => value.clone(),
        Some(_) => {
            return Err(PluginProtocolError::new(format!(
                "Handler {handler_key} annotations 必须是对象。"
            )));
        }
    };
    match action.as_str() {
        "continue" => Ok(HookResult::continue_result(
            handler_key,
            annotations,
            elapsed_ms,
            "continue",
        )),
        "approve" => Err(PluginProtocolError::new(format!(
            "Handler {handler_key} 返回了非法 action=approve；插件只能 deny，不能批准。"
        ))),
        "deny" => {
            let reason = {
                let text = text_of(map.get("reason")).trim().to_string();
                if text.is_empty() {
                    "插件拒绝操作".to_string()
                } else {
                    text
                }
            };
            let code = text_of(map.get("code")).trim().to_string();
            Ok(HookResult::deny_result(
                reason.as_str(),
                &code,
                handler_key,
                elapsed_ms,
            ))
        }
        "patch" => {
            let patch = match map.get("patch") {
                Some(Value::Array(items)) => items.clone(),
                None | Some(Value::Null) => Vec::new(),
                Some(_) => {
                    return Err(PluginProtocolError::new(format!(
                        "Handler {handler_key} patch 必须是数组。"
                    )));
                }
            };
            let mut normalized: Vec<Map<String, Value>> = Vec::new();
            for item in &patch {
                let Some(value) = item.as_object() else {
                    return Err(PluginProtocolError::new(format!(
                        "Handler {handler_key} patch 必须是数组。"
                    )));
                };
                normalized.push(value.clone());
            }
            Ok(HookResult::patch_result(
                normalized,
                handler_key,
                annotations,
                elapsed_ms,
            ))
        }
        other => Err(PluginProtocolError::new(format!(
            "Handler {handler_key} action 非法：{other}"
        ))),
    }
}
