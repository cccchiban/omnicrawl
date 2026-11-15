//! 长文本按句切块：各家的单次请求都有长度上限，超过就切块再拼接。

/// 按句末标点/换行切句，再贪心合并到不超过 `max_chars`；单句超限时按字符硬切。
pub fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    split_with(text, |chunk| chunk.chars().count(), max_chars)
}

/// 按「计费字符」切块：汉字算 2、其余算 1（DeepSeek 的长度上限就是这个口径）。
pub fn split_text_billed(text: &str, max_billed: usize) -> Vec<String> {
    split_with(text, |chunk| crate::deepseek::billed_chars(chunk), max_billed)
}

/// 通用切块：`weight` 给出一个片段的度量，合并后不得超过 `limit`。
fn split_with(text: &str, weight: impl Fn(&str) -> usize, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(text) {
        for piece in hard_split_weighted(&sentence, &weight, limit) {
            let merged = weight(&current) + weight(&piece);
            if merged > limit && !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current.push_str(&piece);
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    // 纯空白输入不应产出空块。
    chunks.retain(|chunk| !chunk.trim().is_empty());
    chunks
}

/// 按句末标点与换行切句，标点跟着前一句（保留原有的停顿观感）。
fn split_sentences(text: &str) -> Vec<String> {
    const ENDINGS: [char; 8] = ['。', '！', '？', '；', '!', '?', ';', '\n'];
    let mut sentences: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ENDINGS.contains(&ch) {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    sentences
}

/// 单个片段超过度量上限时按字符硬切；未超限时原样返回。
fn hard_split_weighted(
    sentence: &str,
    weight: &impl Fn(&str) -> usize,
    limit: usize,
) -> Vec<String> {
    if weight(sentence) <= limit {
        return vec![sentence.to_string()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in sentence.chars() {
        let mut probe = current.clone();
        probe.push(ch);
        if weight(&probe) > limit && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_chunk() {
        assert_eq!(split_text("你好，世界。", 100), vec!["你好，世界。"]);
    }

    #[test]
    fn long_text_is_split_on_sentence_endings_within_budget() {
        let chunks = split_text("第一句。第二句。第三句。", 8);
        assert_eq!(chunks, vec!["第一句。第二句。", "第三句。"]);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 8));
    }

    #[test]
    fn single_sentence_over_budget_is_hard_split() {
        assert_eq!(
            split_text("一二三四五六七八九十", 4),
            vec!["一二三四", "五六七八", "九十"]
        );
    }

    #[test]
    fn blank_text_yields_no_chunks() {
        assert!(split_text("   \n  ", 100).is_empty());
    }

    /// 计费口径：汉字算 2、句号算 1，因此「你好。」是 5 而不是 3；上限 10 恰好放下两句。
    #[test]
    fn billed_splitting_weights_han_as_two() {
        let chunks = split_text_billed("你好。世界。再见。", 10);
        assert_eq!(chunks, vec!["你好。世界。", "再见。"]);
        assert!(chunks
            .iter()
            .all(|chunk| split_text_billed_weight(chunk) <= 10));
    }

    fn split_text_billed_weight(chunk: &str) -> usize {
        crate::deepseek::billed_chars(chunk)
    }

    /// 单句超限时按计费口径硬切，切出的每块都不越界。
    #[test]
    fn billed_hard_split_respects_the_budget() {
        let chunks = split_text_billed("你好你好你好你好", 4);
        assert_eq!(chunks, vec!["你好", "你好", "你好", "你好"]);
    }
}
