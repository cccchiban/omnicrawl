//! 向量模型的线格式：把一批文本发给 OpenAI 兼容的 `POST {base_url}/embeddings`，
//! 把响应里的向量取回来。
//!
//! 这里是**纯线格式**：不含渠道选择、缓存与降级策略，那些在 [`crate::embedding_client`]。
//! 与 `decision_wire` 同构——同样只认 OpenAI 兼容口径，`base_url` 已含 `/v1`。
//!
//! 响应形状（OpenAI 兼容）：
//! ```json
//! {"data": [{"index": 0, "embedding": [0.1, -0.2, ...]}, ...], "model": "..."}
//! ```
//! 归位优先按 `index`，但并非所有服务都给出可信的 `index`（见 [`parse_embeddings`]）。

use serde_json::{json, Value};

/// 一次嵌入请求的正文。`dimensions` 为 0 表示不下发该字段（由服务端决定维度）。
pub fn request_body(model: &str, inputs: &[String], dimensions: i64) -> Value {
    let mut body = json!({
        "model": model,
        "input": inputs,
        "encoding_format": "float",
    });
    if dimensions > 0 {
        if let Some(object) = body.as_object_mut() {
            object.insert("dimensions".to_string(), json!(dimensions));
        }
    }
    body
}

/// 从响应体里取出向量，并按输入顺序归位成等长的列表。
///
/// `index` 只用来**判定**能否按 index 归位，不作为唯一依据：部分 OpenAI 兼容服务
/// （如 SiliconFlow 的批量接口）按内部子批从 0 重新编号，等长响应里会出现
/// `0..7, 0..7` 这类重复 index，但条目顺序仍与输入一致。因此口径是：
/// 当 `index` 恰好构成 `0..expected` 的一个排列时按 index 归位，否则按返回顺序归位。
///
/// 任何一条维度不一致、条数不符都算整次失败：宁可让调用方降级到规则排序，也不要拿
/// 半份向量去算相似度（那会让排序悄悄错位）。
pub fn parse_embeddings(text: &str, expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let payload: Value =
        serde_json::from_str(text).map_err(|error| format!("向量响应不是合法 JSON：{error}"))?;
    let data = payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("向量响应缺少 data 数组：{}", truncate(text)))?;
    if data.len() != expected {
        return Err(format!(
            "向量条数不符：期望 {expected} 条，实际 {} 条。",
            data.len()
        ));
    }
    let mut ordered: Vec<Vec<f32>> = Vec::with_capacity(expected);
    let mut indices: Vec<Option<usize>> = Vec::with_capacity(expected);
    let mut width: Option<usize> = None;
    for item in data {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let label = index
            .map(|index| index.to_string())
            .unwrap_or_else(|| "?".to_string());
        let values = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("向量条目 {label} 缺少 embedding 数组。"))?;
        let mut vector: Vec<f32> = Vec::with_capacity(values.len());
        for value in values {
            let Some(number) = value.as_f64() else {
                return Err(format!("向量条目 {label} 含非数值元素。"));
            };
            vector.push(number as f32);
        }
        if vector.is_empty() {
            return Err(format!("向量条目 {label} 是空向量。"));
        }
        match width {
            None => width = Some(vector.len()),
            Some(expected_width) if expected_width != vector.len() => {
                return Err(format!(
                    "向量维度不一致：{} 与 {expected_width}。",
                    vector.len()
                ))
            }
            Some(_) => {}
        }
        indices.push(index);
        ordered.push(vector);
    }
    if !is_permutation(&indices, expected) {
        return Ok(ordered);
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (index, vector) in indices.into_iter().zip(ordered) {
        if let Some(index) = index {
            slots[index] = Some(vector);
        }
    }
    Ok(slots.into_iter().flatten().collect())
}

/// `indices` 是否恰好是 `0..expected` 的一个排列。
fn is_permutation(indices: &[Option<usize>], expected: usize) -> bool {
    if indices.len() != expected {
        return false;
    }
    let mut seen = vec![false; expected];
    for index in indices.iter().flatten() {
        if *index >= expected || seen[*index] {
            return false;
        }
        seen[*index] = true;
    }
    seen.iter().all(|flag| *flag)
}

/// 两次向量之间的余弦相似度。零向量返回 0（无法定义方向）。
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut left_norm = 0.0f32;
    let mut right_norm = 0.0f32;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
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
    fn parses_in_index_order() {
        let text =
            r#"{"data":[{"index":1,"embedding":[3.0,4.0]},{"index":0,"embedding":[1.0,0.0]}]}"#;
        let vectors = parse_embeddings(text, 2).expect("应当解析成功");
        assert_eq!(vectors[0], vec![1.0, 0.0]);
        assert_eq!(vectors[1], vec![3.0, 4.0]);
    }

    #[test]
    fn rejects_missing_or_mismatched_entries() {
        assert!(parse_embeddings(r#"{"data":[{"index":0,"embedding":[1.0]}]}"#, 2).is_err());
        assert!(parse_embeddings(
            r#"{"data":[{"index":0,"embedding":[1.0]},{"index":1,"embedding":[1.0,2.0]}]}"#,
            2
        )
        .is_err());
        assert!(parse_embeddings("not json", 1).is_err());
    }

    #[test]
    fn falls_back_to_response_order_when_indices_repeat() {
        // SiliconFlow 这类服务按内部子批把 index 从 0 重置成 `0,1,0,1`，
        // 但条目顺序与输入一致，不能因此整批作废。
        let text = r#"{"data":[
            {"index":0,"embedding":[1.0]},
            {"index":1,"embedding":[2.0]},
            {"index":0,"embedding":[3.0]},
            {"index":1,"embedding":[4.0]}
        ]}"#;
        let vectors = parse_embeddings(text, 4).expect("重复 index 应按返回顺序归位");
        assert_eq!(vectors, vec![vec![1.0], vec![2.0], vec![3.0], vec![4.0]]);
    }

    #[test]
    fn falls_back_to_response_order_when_index_is_absent_or_out_of_range() {
        let text = r#"{"data":[{"embedding":[1.0]},{"index":7,"embedding":[2.0]}]}"#;
        let vectors = parse_embeddings(text, 2).expect("缺少或越界 index 应按返回顺序归位");
        assert_eq!(vectors, vec![vec![1.0], vec![2.0]]);
    }

    #[test]
    fn cosine_is_bounded_and_ignores_zero_vectors() {
        let same = cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]);
        assert!((same - 1.0).abs() < 1e-6);
        let orthogonal = cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]);
        assert!(orthogonal.abs() < 1e-6);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn body_omits_zero_dimensions() {
        let body = request_body("m", &["a".to_string()], 0);
        assert!(body.get("dimensions").is_none());
        let body = request_body("m", &["a".to_string()], 512);
        assert_eq!(body.get("dimensions"), Some(&json!(512)));
    }
}
