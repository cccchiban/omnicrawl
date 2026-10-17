//! 长文本按句切块：两家的单次请求都有长度上限，超过就切块再拼接。

/// 按句末标点/换行切句，再贪心合并到不超过 `max_chars`；单句超限时按字符硬切。
pub fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    let limit = max_chars.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(text) {
        for piece in hard_split(&sentence, limit) {
            if current.chars().count() + piece.chars().count() > limit && !current.is_empty() {
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

/// 单句超过上限时按字符硬切；未超限时原样返回。
fn hard_split(sentence: &str, limit: usize) -> Vec<String> {
    if sentence.chars().count() <= limit {
        return vec![sentence.to_string()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in sentence.chars() {
        current.push(ch);
        if current.chars().count() >= limit {
            pieces.push(std::mem::take(&mut current));
        }
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
}
