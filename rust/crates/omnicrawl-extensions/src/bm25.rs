//! BM25 排序：Skill 检索与联网搜索的本地排序共用这一份实现。
//!
//! 参数取 Lucene 默认值（`k1=1.2`、`b=0.75`）：词频饱和让同一词重复出现不再无限加分，
//! 长度归一让长文档不因为词多就占优——Skill 正文与搜索结果的摘要长度差异都很大，缺了归一
//! 会让「写得长」压过「写得贴题」。

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// 一次 BM25 命中：文档下标、得分与被命中的检索词。
pub struct Bm25Hit {
    pub index: usize,
    pub score: f64,
    pub matched: Vec<String>,
}

/// BM25 排序：只返回与查询有交集的文档，得分高的在前（同分按下标升序，保持稳定）。
pub fn rank(query: &[String], documents: &[Vec<String>]) -> Vec<Bm25Hit> {
    if query.is_empty() || documents.is_empty() {
        return Vec::new();
    }
    let total = documents.len() as f64;
    let average_length = documents.iter().map(Vec::len).sum::<usize>() as f64 / total;
    let mut frequencies: Vec<HashMap<&str, usize>> = Vec::with_capacity(documents.len());
    for document in documents {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for token in document {
            *counts.entry(token.as_str()).or_insert(0) += 1;
        }
        frequencies.push(counts);
    }
    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for token in query {
        let count = frequencies
            .iter()
            .filter(|counts| counts.contains_key(token.as_str()))
            .count();
        document_frequency.insert(token.as_str(), count);
    }

    let mut hits: Vec<Bm25Hit> = Vec::new();
    for (index, counts) in frequencies.iter().enumerate() {
        let length = documents[index].len() as f64;
        let mut score = 0.0;
        let mut matched: Vec<String> = Vec::new();
        for token in query {
            let Some(&frequency) = counts.get(token.as_str()) else {
                continue;
            };
            let seen = document_frequency
                .get(token.as_str())
                .copied()
                .unwrap_or_default() as f64;
            let idf = (1.0 + (total - seen + 0.5) / (seen + 0.5)).ln();
            let frequency = frequency as f64;
            let denominator = if average_length > 0.0 {
                frequency + K1 * (1.0 - B + B * length / average_length)
            } else {
                frequency + K1
            };
            score += idf * frequency * (K1 + 1.0) / denominator;
            if !matched.iter().any(|item| item == token) {
                matched.push(token.clone());
            }
        }
        if score > 0.0 {
            hits.push(Bm25Hit {
                index,
                score,
                matched,
            });
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.index.cmp(&right.index))
    });
    hits
}

/// 检索分词：英文与数字取整词（小写），CJK 连续片段按相邻二字切分。
///
/// 用 bigram 而不是整段：中文没有空格分隔，「代码审查」与「审查代码」只有靠二字片段
/// 才能对上；整段词只有一个能命中，召回会明显偏低。
pub fn tokenize(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if is_cjk(ch) {
            let start = index;
            while index < chars.len() && is_cjk(chars[index]) {
                index += 1;
            }
            let run = &chars[start..index];
            if run.len() == 1 {
                tokens.push(run[0].to_string());
            } else {
                for pair in run.windows(2) {
                    tokens.push(pair.iter().collect());
                }
            }
            continue;
        }
        if ch.is_ascii_alphanumeric() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            tokens.push(chars[start..index].iter().collect());
            continue;
        }
        index += 1;
    }
    tokens
}

/// 按 BM25 给候选文本排序，返回下标顺序。
///
/// 没有任何候选与查询有交集时退回「保持原顺序」——此时 BM25 无从判断，改动顺序只会
/// 引入噪声；调用方拿到的顺序与输入一致。
pub fn order(query: &str, candidates: &[String]) -> Vec<usize> {
    let tokens = tokenize(query);
    let documents: Vec<Vec<String>> = candidates.iter().map(|text| tokenize(text)).collect();
    let hits = rank(&tokens, &documents);
    if hits.is_empty() {
        return (0..candidates.len()).collect();
    }
    let mut order: Vec<usize> = hits.into_iter().map(|hit| hit.index).collect();
    // 没命中的候选按原顺序接在后面：排序是「把相关的提前」，不是「丢掉不相关的」。
    for index in 0..candidates.len() {
        if !order.contains(&index) {
            order.push(index);
        }
    }
    order
}

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4e00}'..='\u{9fff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{3040}'..='\u{30ff}'
        | '\u{ac00}'..='\u{d7af}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn a_document_with_more_query_terms_ranks_higher() {
        let candidates = texts(&["无关内容", "包含 BM25 排序", "包含 BM25 排序 与 长度归一"]);
        let order = order("BM25 排序 长度归一", &candidates);
        assert_eq!(order[0], 2, "命中更多词的排最前：{order:?}");
        assert_eq!(order[1], 1, "{order:?}");
        assert_eq!(order[2], 0, "没命中的接在最后：{order:?}");
    }

    #[test]
    fn chinese_bigrams_ignore_word_order() {
        let candidates = texts(&["代码审查要点", "图片压缩"]);
        assert_eq!(order("审查代码", &candidates), vec![0, 1]);
    }

    #[test]
    fn without_any_hit_the_original_order_is_kept() {
        let candidates = texts(&["甲", "乙", "丙"]);
        assert_eq!(order("完全无关", &candidates), vec![0, 1, 2]);
    }

    #[test]
    fn a_shorter_document_is_not_outscored_by_padding() {
        let candidates = texts(&["检索", &"填充 ".repeat(200)]);
        let documents: Vec<Vec<String>> = candidates.iter().map(|text| tokenize(text)).collect();
        let hits = rank(&tokenize("检索"), &documents);
        assert_eq!(hits[0].index, 0, "短文档不该被长文档压下去");
    }
}
