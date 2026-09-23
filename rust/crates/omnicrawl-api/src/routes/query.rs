//! 查询参数与请求体的取值校验：把 FastAPI/Pydantic 的校验失败形状搬过来。
//!
//! 只覆盖迁移中用到的形状：布尔、带上下界的整数、限长字符串。校验失败一律回
//! `VALIDATION_ERROR`（422），字段名进 `loc`，客户端能按字段定位。查询参数与请求体
//! 分开写，因为 Pydantic 对两者的 `loc` 前缀与 `type` 取值不同（`query` / `body`）。

use serde_json::{json, Value};

use crate::error::ApiError;

/// 查询参数里的布尔值；取值与 FastAPI 一致（真：true/1/yes/on，假：false/0/no/off）。
pub fn query_bool(name: &str, raw: Option<&str>, default: bool) -> Result<bool, ApiError> {
    let Some(text) = raw else {
        return Ok(default);
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => Err(validation(
            "query",
            name,
            "bool_parsing",
            "Input should be a valid boolean, unable to interpret input",
            json!(other),
            None,
        )),
    }
}

/// 查询参数里的整数，带闭区间 `[min, max]`。
pub fn query_int(
    name: &str,
    raw: Option<&str>,
    default: i64,
    min: i64,
    max: i64,
) -> Result<i64, ApiError> {
    let Some(text) = raw else {
        return Ok(default);
    };
    let value = text.trim().parse::<i64>().map_err(|_| {
        validation(
            "query",
            name,
            "int_parsing",
            "Input should be a valid integer, unable to parse string as an integer",
            json!(text),
            None,
        )
    })?;
    if value < min {
        return Err(validation(
            "query",
            name,
            "greater_than_equal",
            &format!("Input should be greater than or equal to {min}"),
            json!(value),
            Some(json!({ "ge": min })),
        ));
    }
    if value > max {
        return Err(validation(
            "query",
            name,
            "less_than_equal",
            &format!("Input should be less than or equal to {max}"),
            json!(value),
            Some(json!({ "le": max })),
        ));
    }
    Ok(value)
}

/// 查询参数里的必填字符串；缺失按 Pydantic 的 `missing` 报错。
pub fn query_text_required(name: &str, raw: Option<&str>) -> Result<String, ApiError> {
    match raw {
        Some(text) => Ok(text.to_string()),
        None => Err(ApiError::validation(json!([{
            "type": "missing",
            "loc": ["query", name],
            "msg": "Field required",
        }]))),
    }
}

/// 请求体里的必填字符串：缺字段、空串与超长分别按 Pydantic 的 `missing` /
/// `string_too_short` / `string_too_long` 报错。
pub fn body_text(name: &str, raw: Option<&str>, max_length: usize) -> Result<String, ApiError> {
    let value = optional_body_text(name, raw, "", max_length)?;
    if value.trim().is_empty() {
        return Err(validation(
            "body",
            name,
            "string_too_short",
            "String should have at least 1 character",
            raw.map_or(Value::Null, |text| json!(text)),
            Some(json!({ "min_length": 1 })),
        ));
    }
    Ok(value)
}

/// 请求体里的可选字符串：缺字段取默认值，超长按 Pydantic 报错。
pub fn optional_body_text(
    name: &str,
    raw: Option<&str>,
    default: &str,
    max_length: usize,
) -> Result<String, ApiError> {
    let Some(text) = raw else {
        return Ok(default.to_string());
    };
    if text.chars().count() > max_length {
        return Err(validation(
            "body",
            name,
            "string_too_long",
            &format!("String should have at most {max_length} characters"),
            json!(text),
            Some(json!({ "max_length": max_length })),
        ));
    }
    Ok(text.to_string())
}

/// 请求体里的布尔字段；缺字段取默认值，类型不符按 Pydantic 报错。
pub fn body_bool(name: &str, raw: Option<&Value>, default: bool) -> Result<bool, ApiError> {
    match raw {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(other) => Err(validation(
            "body",
            name,
            "bool_type",
            "Input should be a valid boolean",
            other.clone(),
            None,
        )),
    }
}

/// 取请求体里的字符串字段；非字符串按 Pydantic 的 `string_type` 报错。
pub fn body_string_field(body: &Value, name: &str) -> Result<Option<String>, ApiError> {
    match body.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(validation(
            "body",
            name,
            "string_type",
            "Input should be a valid string",
            other.clone(),
            None,
        )),
    }
}

/// 组装一条校验失败信封。
fn validation(
    location: &str,
    name: &str,
    kind: &str,
    message: &str,
    input: Value,
    context: Option<Value>,
) -> ApiError {
    let mut item = json!({
        "type": kind,
        "loc": [location, name],
        "msg": message,
        "input": input,
    });
    if let (Some(context), Some(object)) = (context, item.as_object_mut()) {
        object.insert("ctx".to_string(), context);
    }
    ApiError::validation(json!([item]))
}

/// 请求体里的必填布尔字段；缺字段按 Pydantic 的 `missing` 报错。
pub fn body_required_bool(name: &str, raw: Option<&Value>) -> Result<bool, ApiError> {
    match raw {
        None => Err(missing("body", name)),
        Some(value) => body_bool(name, Some(value), false),
    }
}

/// 请求体里的必填整数，带闭区间 `[min, max]`。
pub fn body_required_int(
    name: &str,
    raw: Option<&Value>,
    min: i64,
    max: i64,
) -> Result<i64, ApiError> {
    match body_int(name, raw, min, max)? {
        Some(value) => Ok(value),
        None => Err(missing("body", name)),
    }
}

/// 请求体里的可选整数，带闭区间 `[min, max]`；缺字段返回 `None`。
pub fn body_int(
    name: &str,
    raw: Option<&Value>,
    min: i64,
    max: i64,
) -> Result<Option<i64>, ApiError> {
    let Some(value) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(number) = value.as_i64() else {
        if value.is_number() {
            return Err(validation(
                "body",
                name,
                "int_type",
                "Input should be a valid integer",
                value.clone(),
                Some(json!({ "strict": true })),
            ));
        }
        return Err(validation(
            "body",
            name,
            "int_type",
            "Input should be a valid integer",
            value.clone(),
            None,
        ));
    };
    if number < min {
        return Err(validation(
            "body",
            name,
            "greater_than_equal",
            &format!("Input should be greater than or equal to {min}"),
            json!(number),
            Some(json!({ "ge": min })),
        ));
    }
    if number > max {
        return Err(validation(
            "body",
            name,
            "less_than_equal",
            &format!("Input should be less than or equal to {max}"),
            json!(number),
            Some(json!({ "le": max })),
        ));
    }
    Ok(Some(number))
}

/// 请求体里的可选浮点数；缺字段返回 `None`。
pub fn body_number(name: &str, raw: Option<&Value>) -> Result<Option<f64>, ApiError> {
    let Some(value) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    match value.as_f64() {
        Some(number) => Ok(Some(number)),
        None => Err(validation(
            "body",
            name,
            "float_type",
            "Input should be a valid number",
            value.clone(),
            None,
        )),
    }
}

/// 请求体里的字符串数组（如 `run_guard.guard.auto_retry_errors`）；非数组报类型错误。
pub fn body_text_list(name: &str, raw: Option<&Value>) -> Result<Option<Vec<String>>, ApiError> {
    let Some(value) = raw.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(validation(
            "body",
            name,
            "list_type",
            "Input should be a valid list",
            value.clone(),
            None,
        ));
    };
    let mut texts: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str() {
            Some(text) => texts.push(text.to_string()),
            None => {
                return Err(validation(
                    "body",
                    name,
                    "string_type",
                    "Input should be a valid string",
                    item.clone(),
                    None,
                ))
            }
        }
    }
    Ok(Some(texts))
}

/// 取请求体里的子对象（如 `run_guard.guard`）；缺字段或非对象都返回 `None`。
pub fn body_object(body: &Value, name: &str) -> Option<Value> {
    match body.get(name) {
        Some(Value::Object(_)) => body.get(name).cloned(),
        _ => None,
    }
}

/// 缺字段的统一形状（Pydantic 的 `missing` 不带 `input`）。
pub fn missing(location: &str, name: &str) -> ApiError {
    ApiError::validation(json!([{
        "type": "missing",
        "loc": [location, name],
        "msg": "Field required",
    }]))
}
