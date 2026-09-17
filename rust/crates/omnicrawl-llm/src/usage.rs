//! Token usage 归一化：OpenAI Responses / Chat Completions 负载 → 统一 TokenUsage。
//!
//! 语义基准：`omnicrawl/llm/usage.py` 的 `usage_from_openai_payload`。

use omnicrawl_protocol::TokenUsage;
use serde_json::Value;

/// 从响应或流事件负载中提取用量；输入与输出两路 token 都缺失时返回 None。
pub fn usage_from_openai_payload(payload: &Value) -> Option<TokenUsage> {
    let usage = find_usage_payload(payload)?;
    let mut input_tokens = read_usage_int(usage, &["input_tokens", "prompt_tokens"]);
    if input_tokens.is_none() {
        input_tokens = read_deepseek_input_tokens(usage);
    }
    let output_tokens = read_usage_int(usage, &["output_tokens", "completion_tokens"]);
    if input_tokens.is_none() && output_tokens.is_none() {
        return None;
    }

    let cached_input_tokens = read_cached_input_tokens(usage).unwrap_or(0);
    let mut reasoning_tokens =
        read_usage_int(usage, &["reasoning_tokens", "output_reasoning_tokens"]).unwrap_or(0);
    if reasoning_tokens == 0 {
        // Responses 用 output_tokens_details，Chat Completions 用 completion_tokens_details。
        // Python 侧是 `a or b`：前者为「假值」（null、空对象等）时继续看后者。
        let details = as_truthy(usage.get("output_tokens_details"))
            .or_else(|| as_truthy(usage.get("completion_tokens_details")));
        reasoning_tokens = details
            .and_then(|value| read_usage_int(value, &["reasoning_tokens"]))
            .unwrap_or(0);
    }

    Some(TokenUsage {
        input_tokens: non_negative(input_tokens),
        output_tokens: non_negative(output_tokens),
        cached_input_tokens: non_negative(Some(cached_input_tokens)),
        reasoning_tokens: non_negative(Some(reasoning_tokens)),
    })
}

fn find_usage_payload(payload: &Value) -> Option<&Value> {
    if let Some(usage) = payload.get("usage").filter(|value| !value.is_null()) {
        return Some(usage);
    }
    payload
        .get("response")
        .and_then(|response| response.get("usage"))
        .filter(|value| !value.is_null())
}

fn read_usage_int(usage: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| usage.get(*key).and_then(Value::as_i64))
}

/// Python 真值判定：null、空对象、空数组、空串、false、0 都是假值，假值不参与 `or`。
fn as_truthy(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    })
}

/// DeepSeek 风格：命中与未命中缓存两段相加才是输入总量。
fn read_deepseek_input_tokens(usage: &Value) -> Option<i64> {
    let hit_tokens = read_usage_int(usage, &["prompt_cache_hit_tokens"])?;
    let miss_tokens = read_usage_int(usage, &["prompt_cache_miss_tokens"])?;
    Some(hit_tokens + miss_tokens)
}

fn read_cached_input_tokens(usage: &Value) -> Option<i64> {
    let flat_keys = [
        "cached_tokens",
        "cached_input_tokens",
        "input_cached_tokens",
        "prompt_cache_hit_tokens",
    ];
    for key in flat_keys {
        if let Some(value) = read_usage_int(usage, &[key]) {
            return Some(value);
        }
    }
    for details_key in ["input_tokens_details", "prompt_tokens_details"] {
        if let Some(details) = usage.get(details_key) {
            if let Some(value) = read_usage_int(details, &["cached_tokens"]) {
                return Some(value);
            }
        }
    }
    None
}

fn non_negative(value: Option<i64>) -> u64 {
    value.unwrap_or(0).max(0) as u64
}
