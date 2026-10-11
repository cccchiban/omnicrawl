//! 脱敏占位符还原的模糊纠错：全半角归一化与大小写容忍。
//!
//! 生成端只发全角规范形 `｛Desensitized:n｝`，但模型偶发把整段占位符打成全角字母、
//! 全角数字或混用大小写。这些变体仍要还原成原文，而不是当成畸形占位符留在回复里。
//!
//! 用例里的占位符一律拼接构造：本仓库自己就是宿主，写完整字面量会被还原成原文。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{
    find_placeholders, has_placeholder_prefix, PlaceholderCycle, SequenceRegistry, StreamRestorer,
};

/// 全角 ASCII 变体：`U+FF01..=U+FF5E` 是半角 `U+0021..=U+007E` 平移 `0xFEE0`。
fn wide(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == ' ' || character.is_ascii_graphic() {
                char::from_u32(character as u32 + 0xfee0).unwrap_or(character)
            } else {
                character
            }
        })
        .collect()
}

fn cycle_with(values: &[&str]) -> (SequenceRegistry, PlaceholderCycle, Vec<u64>) {
    let counter = Arc::new(AtomicU64::new(0));
    let source = move || counter.fetch_add(1, Ordering::SeqCst) + 1;
    let mut registry = SequenceRegistry::with_sequence_source(source);
    let (mut cycle, _) = registry.begin_cycle("模糊纠错");
    let sequences = values
        .iter()
        .map(|value| cycle.seq_for_value(value).0)
        .collect();
    (registry, cycle, sequences)
}

#[test]
fn fullwidth_digits_and_letters_still_match() {
    let (mut registry, cycle, sequences) = cycle_with(&["第一处原文", "第二处原文"]);
    let mut restorer = StreamRestorer::new(&cycle, &mut registry.stats, false);

    // 序号 1 的整段占位符全角化（标记、冒号、数字全是全角）。
    let all_wide = wide(&format!("Desensitized:{}", sequences[0]));
    let text = format!("\u{ff5b}{all_wide}\u{ff5d}");
    let restored = restorer.restore_string(&text).expect("还原");
    assert_eq!(restored, "第一处原文");

    // 序号 2：半角花括号 + 大小写混排 + 全角冒号与全角数字。
    let mixed = format!(
        "{{dEsEnSiTiZeD\u{ff1a}{}}}",
        wide(&sequences[1].to_string())
    );
    let restored = restorer.restore_string(&mixed).expect("还原");
    assert_eq!(restored, "第二处原文");
    assert_eq!(restorer.stats().restore_unresolved, 0, "不应记成未注册");
    assert_eq!(restorer.stats().restore_malformed, 0, "不应记成畸形");
}

#[test]
fn mixed_variants_are_recognized_by_the_parser() {
    let all_wide = wide("Desensitized:7");
    let cases = [
        format!("\u{ff5b}{all_wide}\u{ff5d}"),
        "{DESENSITIZED:7}".to_string(),
        "{ Desensitized \u{ff1a} 7 }".to_string(),
        format!("\u{ff5b}desensitized:{}\u{ff5d}", wide("7")),
    ];
    for text in cases {
        let found = find_placeholders(&text);
        assert_eq!(found.len(), 1, "应识别为占位符：{text:?}");
        assert_eq!(found[0].2, 7, "序号应解析为 7：{text:?}");
        assert_eq!(&text[found[0].0..found[0].1], text, "应覆盖整段：{text:?}");
    }
}

#[test]
fn partial_fullwidth_prefix_is_held_for_the_next_chunk() {
    let (mut registry, cycle, sequences) = cycle_with(&["跨分片原文"]);
    let mut restorer = StreamRestorer::new(&cycle, &mut registry.stats, false);

    // 分片把全角占位符切在序号中间：前半段必须挂起，等后半段到齐再还原。
    let first = format!("\u{ff5b}{}", wide("Desensitized:"));
    let head = restorer.feed_text(&first).expect("首片");
    assert!(head.is_empty(), "半截前缀应挂起而不是外发：{head:?}");
    assert!(
        has_placeholder_prefix(&first),
        "全角标记也算疑似前缀：{first:?}"
    );

    let tail = format!("{}\u{ff5d}", wide(&sequences[0].to_string()));
    let body = restorer.feed_text(&tail).expect("续片");
    assert_eq!(body, "跨分片原文");
}
