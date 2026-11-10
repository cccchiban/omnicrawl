//! 重排序模型的线格式：把一个查询与一批候选发给 OpenAI 兼容的 `POST {base_url}/rerank`，
//! 把回执里的每条相关度取回来。
//!
//! 这里是**纯线格式**：不含渠道选择、降级策略与批拆分，那些在 [`crate::embedding_client`]。
//! 与 `embedding_wire` 同构——只认 OpenAI 兼容口径，`base_url` 已含 `/v1`。
//!
//! 请求形状（Jina / Cohere / SiliconFlow 通用）：
//! ```json
//! {"model": "...", "query": "...", "documents": ["...", "..."], "top_n": 3}
//! ```
//! 响应形状：
//! ```json
//! {"results": [{"index": 0, "relevance_score": 0.9}, ...]}
//! ```
//! `index` 是候选在 `documents` 里的下标；服务端通常已按分数降序返回，但不作假设，
//! 归位后按分数重排。

use serde_json::{json, Value};

/// 一次重排请求的正文。`top_n` 为 0 表示要求服务端返回全部候选。
pub fn request_body(model: &str, query: &str, documents: &[String], top_n: usize) -> Value {
    let mut body = json!({
        "model": model,
        "query": query,
        "documents": documents,
    });
    if top_n > 0 {
        if let Some(object) = body.as_object_mut() {
            object.insert("top_n".to_string(), json!(top_n));
        }
    }
    body
}

/// 从响应体里取出 (候选下标, 相关度)，按相关度降序。
///
/// 越界或重复的下标、缺失分数的条目一律丢弃。`index` 缺失时按返回顺序补位——部分服务
/// 不回 index 但保持顺序，那种情况下顺序本身就是名次。
///
/// 整池都解析不出来才算失败：只要有一条可用，调用方就能据此把最相关的挪到前面。
pub fn parse_rerank(text: &str) -> Result<Vec<(usize, f32)>, String> {
    let payload: Value =
        serde_json::from_str(text).map_err(|error| format!("重排响应不是合法 JSON：{error}"))?;
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("重排响应缺少 results 数组：{}", truncate(text)))?;
    let mut scored: Vec<(usize, f32)> = Vec::with_capacity(results.len());
    for (position, item) in results.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(position);
        let Some(score) = item
            .get("relevance_score")
            .and_then(Value::as_f64)
            .or_else(|| item.get("score").and_then(Value::as_f64))
        else {
            continue;
        };
        scored.push((index, score as f32));
    }
    if scored.is_empty() {
        return Err(format!("重排响应没有可用条目：{}", truncate(text)));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(scored)
}

fn truncate(text: &str) -> String {
    let limit = 200;
    if text.chars().count() <= limit {
        return text.trim().to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{}…", head.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sorts_by_score() {
        let text = r#"{"results":[
            {"index":2,"relevance_score":0.2},
            {"index":0,"relevance_score":0.9},
            {"index":1,"relevance_score":0.5}
        ]}"#;
        let scored = parse_rerank(text).expect("应当解析成功");
        assert_eq!(scored, vec![(0, 0.9), (1, 0.5), (2, 0.2)]);
    }

    #[test]
    fn falls_back_to_response_order_without_index() {
        let text = r#"{"results":[{"relevance_score":0.9},{"relevance_score":0.4}]}"#;
        let scored = parse_rerank(text).expect("缺少 index 应按返回顺序补位");
        assert_eq!(scored, vec![(0, 0.9), (1, 0.4)]);
    }

    #[test]
    fn skips_entries_without_a_score_and_rejects_empty_payloads() {
        let text = r#"{"results":[{"index":0},{"index":1,"relevance_score":0.3}]}"#;
        assert_eq!(parse_rerank(text).expect("应保留有分数的条目"), vec![(1, 0.3)]);
        assert!(parse_rerank(r#"{"results":[]}"#).is_err());
        assert!(parse_rerank(r#"{"data":[]}"#).is_err());
        assert!(parse_rerank("not json").is_err());
    }

    #[test]
    fn body_carries_top_n_only_when_positive() {
        let documents = vec!["a".to_string(), "b".to_string()];
        let body = request_body("m", "q", &documents, 0);
        assert!(body.get("top_n").is_none());
        let body = request_body("m", "q", &documents, 2);
        assert_eq!(body.get("top_n"), Some(&json!(2)));
        assert_eq!(body.get("query"), Some(&json!("q")));
    }
}
