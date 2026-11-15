//! 序号 ↔ 单词组占位符标签的双射编解码。
//!
//! 纯数字序号的信息量太低：模型搬运它时偶发记错一位（`12` → `21`、`7` → `1`），
//! 还原查不到该序号就只能按原样保留，占位符随即落进代码与脚本里变成语法错误。
//! 单词组把「一个整数」换成「四个互不相同的语义维度」（颜色 · 体型 · 公母 · 动物），
//! 组合数在万级，且每一段都是有意义的常见词，模型复述时不易串形。
//!
//! 编码是**双射**：`seq ↔ 四元组` 一一对应，注册表、稳定索引、还原逻辑一概不动——同一个
//! 值仍然拿同一个序号，历史逐字可复现的性质（前缀缓存）因此天然保留。
//!
//! 变长扩展：序号超过 `single_len()^4` 时追加第五段，再超则继续追加，保证任意 `u64`
//! 都能编码；解码按段数还原进制，不依赖固定宽度。

/// 颜色段（36 个）。
const COLORS: [&str; 36] = [
    "red",
    "orange",
    "yellow",
    "green",
    "cyan",
    "blue",
    "purple",
    "pink",
    "brown",
    "black",
    "white",
    "gray",
    "silver",
    "gold",
    "amber",
    "violet",
    "indigo",
    "teal",
    "navy",
    "olive",
    "maroon",
    "beige",
    "coral",
    "crimson",
    "ivory",
    "khaki",
    "lavender",
    "lime",
    "magenta",
    "mint",
    "ochre",
    "peach",
    "plum",
    "ruby",
    "salmon",
    "scarlet",
];

/// 体型段（24 个）。
const SIZES: [&str; 24] = [
    "tiny",
    "small",
    "little",
    "slim",
    "short",
    "tall",
    "long",
    "lean",
    "thin",
    "stout",
    "broad",
    "chubby",
    "plump",
    "round",
    "stocky",
    "medium",
    "large",
    "big",
    "huge",
    "giant",
    "massive",
    "heavy",
    "grand",
    "vast",
];

/// 性别段（8 个）。
const GENDERS: [&str; 8] = [
    "male", "female", "boy", "girl", "man", "woman", "king", "queen",
];

/// 动物段（48 个）。
const ANIMALS: [&str; 48] = [
    "cat",
    "dog",
    "fox",
    "wolf",
    "bear",
    "deer",
    "hare",
    "mouse",
    "otter",
    "seal",
    "whale",
    "shark",
    "dolphin",
    "tiger",
    "lion",
    "panda",
    "koala",
    "rabbit",
    "horse",
    "zebra",
    "camel",
    "llama",
    "goat",
    "sheep",
    "cow",
    "pig",
    "hen",
    "duck",
    "goose",
    "swan",
    "crane",
    "heron",
    "eagle",
    "hawk",
    "owl",
    "crow",
    "robin",
    "finch",
    "lark",
    "sparrow",
    "parrot",
    "raven",
    "falcon",
    "badger",
    "beaver",
    "lynx",
    "moose",
    "bison",
];

/// 单个四段组合的容量（`36 * 24 * 8 * 48 = 331_776`）。
fn single_len() -> u64 {
    (COLORS.len() * SIZES.len() * GENDERS.len() * ANIMALS.len()) as u64
}

/// 每个维度词表的引用（编码 / 解码共用的段序）。
fn dimensions() -> [&'static [&'static str]; 4] {
    [&COLORS, &SIZES, &GENDERS, &ANIMALS]
}

/// 序号 → 单词组标签（段间用 `_` 连接）。
///
/// 序号从 1 开始（`0` 不是合法序号，编码成第一个组合）。
pub fn encode(seq: u64) -> String {
    let dims = dimensions();
    let group = single_len();
    let mut remaining = seq.saturating_sub(1);

    // 每四段是一个 `single_len()` 进制的「位」，高位在左；先算出总位数再逐位取。
    let mut groups: Vec<u64> = Vec::new();
    loop {
        groups.push(remaining % group);
        remaining /= group;
        if remaining == 0 {
            break;
        }
    }
    groups.reverse();

    let mut segments: Vec<&str> = Vec::with_capacity(groups.len() * dims.len());
    for value in groups {
        let mut rest = value;
        let mut digits: Vec<usize> = Vec::with_capacity(dims.len());
        for dimension in dims.iter().rev() {
            let radix = dimension.len() as u64;
            digits.push((rest % radix) as usize);
            rest /= radix;
        }
        digits.reverse();
        for (dimension, digit) in dims.iter().zip(digits.iter()) {
            segments.push(dimension[*digit]);
        }
    }
    segments.join("_")
}

/// 单词组标签 → 序号；段数不足或含未知词时返回 `None`。
///
/// 解码是编码的严格逆运算：按段数推断进制宽度，拒绝非四段整数倍的段数。
pub fn decode(label: &str) -> Option<u64> {
    let dims = dimensions();
    let words: Vec<&str> = label.split('_').collect();
    if words.is_empty() || words.len() % dims.len() != 0 {
        return None;
    }
    let mut total: u64 = 0;
    // 高位段在左：每段都把「已累计值」左移一个组合宽度，再加本段的值。
    for chunk in words.chunks(dims.len()) {
        let mut value: u64 = 0;
        for (dimension, word) in dims.iter().zip(chunk.iter()) {
            let index = dimension
                .iter()
                .position(|candidate| candidate.eq_ignore_ascii_case(word))?;
            value = value * dimension.len() as u64 + index as u64;
        }
        total = total.checked_mul(single_len())?.checked_add(value)?;
    }
    Some(total + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sequences_are_distinct_and_readable() {
        let first = encode(1);
        assert_eq!(first.split('_').count(), 4, "首个占位符应是四段");
        assert_eq!(first, "red_tiny_male_cat");
        assert_ne!(encode(2), first);
    }

    #[test]
    fn encode_and_decode_round_trip() {
        let mut cases: Vec<u64> = (1..2000).collect();
        cases.extend([
            10_000,
            331_776,
            331_777,
            500_000,
            1_000_000,
            12_345_678,
            u64::MAX / 2,
        ]);
        for seq in cases {
            let label = encode(seq);
            assert_eq!(decode(&label), Some(seq), "往返失败：{seq} -> {label}");
        }
    }

    #[test]
    fn decode_accepts_case_variants() {
        assert_eq!(decode("RED_TINY_MALE_CAT"), decode("red_tiny_male_cat"));
        assert_eq!(decode("Red_Tiny_Male_Cat"), decode("red_tiny_male_cat"));
    }

    #[test]
    fn decode_rejects_unknown_or_malformed() {
        assert_eq!(decode("red_tiny_male"), None, "段数不足");
        assert_eq!(decode("red_tiny_male_kitten"), None, "未知词");
        assert_eq!(decode("_"), None);
        assert_eq!(decode(""), None);
        assert_eq!(decode("red_tiny_male_cat_red"), None, "非四段整数倍");
    }

    #[test]
    fn labels_only_use_ascii_letters_and_underscores() {
        for seq in [1_u64, 7, 42, 999, 100_000, 331_776] {
            let label = encode(seq);
            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "标签只含小写字母与下划线：{label}"
            );
        }
    }

    #[test]
    fn growth_beyond_single_group_stays_bijective() {
        let boundary = single_len();
        let below = encode(boundary);
        let at = encode(boundary + 1);
        assert_eq!(below.split('_').count(), 4, "单组内仍是四段");
        assert_eq!(at.split('_').count(), 8, "越过容量后追加第二组");
        assert_ne!(below, at);
        assert_eq!(decode(&at), Some(boundary + 1));
    }
}
