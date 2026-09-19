//! TTS 文本归一化：`omnicrawl/tts/normalize.py` 的 Rust 等价实现。
//!
//! 管道顺序与 Python 一致：基础清洗 → Markdown/行处理 → 箭头 → 保护高风险 token →
//! 清理朗读噪声 → 下划线与空白归一化 → 结构标点与重复标点 → 还原保护内容 →
//! 按行补终止标点。WeTextProcessing 需要 pynini，Rust 侧不提供，`enable_wetext` 为真时
//! 按 Python 的方式报错。

use std::sync::OnceLock;

use fancy_regex::Regex;
use serde_json::{json, Value};
use unicode_general_category::{get_general_category, GeneralCategory};

macro_rules! cached_regex {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static CELL: OnceLock<Regex> = OnceLock::new();
            CELL.get_or_init(|| Regex::new($pattern).expect("TTS 正则应当合法"))
        }
    };
}

// 不依赖空格分词的脚本：汉字 + 日文假名
const CJK_CHARS: &str = r"\u{3400}-\u{4dbf}\u{4e00}-\u{9fff}\u{3040}-\u{30ff}";

const PROT: &str = r"___PROT\d+___";

const TRAILING_CLOSERS: [char; 12] = [
    '"', '\'', ')', ']', '}', '）', '】', '》', '〉', '」', '』', '’',
];

const APOSTROPHE_KEEP: &str = "～ＡＰＯＳ～";

const ENGLISH_VOICES: [&str; 5] = ["Trump", "Ava", "Bella", "Adam", "Nathan"];
const ZH_WETEXT_KEEP_HYPHEN: &str = "___KEEP_HYPHEN_BEFORE_ZH_WETEXT___";

cached_regex!(url_re, r"https?://[^\s\u3000，。！？；、）】》〉」』]+");
cached_regex!(
    email_re,
    r"(?<![\\w.+-])[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}(?![\\w.-])"
);
cached_regex!(mention_re, r"(?<![A-Za-z0-9_])@[A-Za-z0-9_]{1,32}");
cached_regex!(reddit_re, r"(?<![A-Za-z0-9_])(?:u|r)/[A-Za-z0-9_]+");
cached_regex!(
    dot_token_re,
    r"(?<![A-Za-z0-9_])\.(?=[A-Za-z0-9._-]*[A-Za-z0-9])[A-Za-z0-9._-]+"
);
cached_regex!(
    date_token_re,
    r"(?<![A-Za-z0-9_])(?:\d{4}[/-]\d{1,2}[/-]\d{1,2}|\d{1,2}[/-]\d{1,2}[/-]\d{4})(?![A-Za-z0-9_])"
);
cached_regex!(
    filelike_re,
    r"(?<![A-Za-z0-9_])(?=[A-Za-z0-9._/+:-]*[A-Za-z])(?=[A-Za-z0-9._/+:-]*[.:])[A-Za-z0-9][A-Za-z0-9._/+:-]*(?![A-Za-z0-9_])"
);
cached_regex!(zero_width_re, r"[\u200b-\u200d\ufeff]");

fn latinish() -> String {
    format!(r"(?:{PROT}|(?=[A-Za-z0-9._/+:-]*[A-Za-z])[A-Za-z0-9][A-Za-z0-9._/+:-]*)")
}

fn cjk() -> String {
    format!("[{CJK_CHARS}]")
}

fn regex_of(pattern: &str) -> Regex {
    Regex::new(pattern).expect("TTS 正则应当合法")
}

cached_regex!(
    emoji_re,
    "[\u{1f1e6}-\u{1f1ff}\u{1f300}-\u{1f5ff}\u{1f600}-\u{1f64f}\u{1f680}-\u{1f6ff}\u{1f700}-\u{1f77f}\u{1f780}-\u{1f7ff}\u{1f800}-\u{1f8ff}\u{1f900}-\u{1f9ff}\u{1fa00}-\u{1fa6f}\u{1fa70}-\u{1faff}\u{2600}-\u{27bf}\u{2b00}-\u{2bff}\u{fe0f}\u{200d}]+"
);
cached_regex!(
    decorative_symbol_re,
    "[\u{2300}-\u{23ff}\u{25a0}-\u{25ff}\u{2b00}-\u{2bff}\u{00a9}\u{00ae}]+"
);
cached_regex!(strip_chars_re, r"[\*#~·•◦∙●○◎◇◆★☆※→←↑↓↔↕]");
cached_regex!(quotes_re, r#"[\"'`´‘’“”]"#);
cached_regex!(inline_code_re, r"`[^`\n]+`");
cached_regex!(apostrophe_re, r"(?<=[A-Za-z])['’](?=[A-Za-z])");
cached_regex!(
    flow_arrows_re,
    r"\s*(?:<[-=]+>|[-=]+>|<[-=]+|[→←↔⇒⇐⇔⟶⟵⟷⟹⟸⟺↦↤↪↩])\s*"
);
cached_regex!(markdown_link_re, r"\[([^\[\]]+?)\]\((https?://[^)\s]+)\)");
cached_regex!(heading_re, r"^#{1,6}\s+");
cached_regex!(quote_re, r"^>\s+");
cached_regex!(unordered_list_re, r"^[-*+]\s+");
cached_regex!(ordered_list_re, r"^\d+[.)]\s+");
cached_regex!(cjk_core_re, r"[\u3400-\u9fff]");
cached_regex!(ascii_letter_re, r"[A-Za-z]");

fn separator_cjk_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(&format!(r"(?<=[{CJK_CHARS}])[\\/|](?=[{CJK_CHARS}])")))
}

fn separator_space_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[\\/|]"))
}

fn spaces_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[ \t\r\f\v]+"))
}

fn spaces_many_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r" {2,}"))
}

fn structural_bracket_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"\[\s*([^\[\]]+?)\s*\]"))
}

fn structural_brace_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"\{\s*([^{}]+?)\s*\}"))
}

fn structural_corner_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[【〖『「]\s*([^】〗』」]+?)\s*[】〗』」]"))
}

fn book_title_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        regex_of(
            r"(^|[。！？!?；;]\s*)《([^》]+)》(?=\s*(?:___PROT\d+___|[—–―-]{2,}|$|[。！？!?；;，,]))",
        )
    })
}

fn long_dash_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"\s*(?:—|–|―|-){2,}\s*"))
}

fn ellipsis_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"(?:\.{3,}|…{2,}|……+)"))
}

fn repeated_period_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[。．]{2,}"))
}

fn repeated_comma_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[，,]{2,}"))
}

fn repeated_bang_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[!！]{2,}"))
}

fn repeated_question_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[?？]{2,}"))
}

fn mixed_question_bang_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| regex_of(r"[!?！？]{2,}"))
}

/// 对 TTS 输入做鲁棒性正则化（纯清洗，不做语义展开）。
pub fn normalize_tts_text(text: &str) -> String {
    let text = base_cleanup(text);
    let text = normalize_markdown_and_lines(&text);
    let text = normalize_flow_arrows(&text);
    let (text, protected) = protect_spans(&text);

    let text = strip_speech_noise(&text);
    let text = normalize_visible_underscores(&text);
    let text = normalize_spaces(&text);
    let text = normalize_structural_punctuation(&text);
    let text = normalize_repeated_punctuation(&text);
    let text = normalize_spaces(&text);
    let text = restore_noise_placeholders(&text);

    let text = restore_spans(&text, &protected);
    ensure_terminal_punctuation_by_line(text.trim())
}

fn strip_speech_noise(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let text = inline_code_re().replace_all(text, "").to_string();
    let text = emoji_re().replace_all(&text, "").to_string();
    let text = decorative_symbol_re().replace_all(&text, "").to_string();
    let text = apostrophe_re()
        .replace_all(&text, APOSTROPHE_KEEP)
        .to_string();
    let text = quotes_re().replace_all(&text, "").to_string();
    let text = strip_chars_re().replace_all(&text, "").to_string();
    let text = separator_cjk_re().replace_all(&text, "、").to_string();
    separator_space_re().replace_all(&text, " ").to_string()
}

fn restore_noise_placeholders(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let text = quotes_re().replace_all(text, "").to_string();
    text.replace(APOSTROPHE_KEEP, "'")
}

fn base_cleanup(text: &str) -> String {
    let text = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\u{3000}', " ");
    let text = zero_width_re().replace_all(&text, "").to_string();
    text.chars()
        .filter(|ch| matches!(ch, '\n' | '\t' | ' ') || !is_control_category(*ch))
        .collect()
}

fn is_control_category(ch: char) -> bool {
    matches!(
        get_general_category(ch),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::Surrogate
            | GeneralCategory::PrivateUse
            | GeneralCategory::Unassigned
    )
}

fn is_punctuation_category(ch: char) -> bool {
    matches!(
        get_general_category(ch),
        GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::OtherPunctuation
            | GeneralCategory::OpenPunctuation
    )
}

fn normalize_markdown_and_lines(text: &str) -> String {
    let text = markdown_link_re()
        .replace_all(text, "${1} ${2}")
        .to_string();
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let line = heading_re().replace(line, "").to_string();
        let line = quote_re().replace(&line, "").to_string();
        let line = unordered_list_re().replace(&line, "").to_string();
        let line = ordered_list_re().replace(&line, "").to_string();
        lines.push(line);
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut merged: Vec<String> = vec![lines[0].clone()];
    for line in &lines[1..] {
        if let Some(previous) = merged.last().cloned() {
            let fixed = ensure_terminal_punctuation(&previous);
            *merged.last_mut().expect("非空") = fixed;
        }
        merged.push(line.clone());
    }
    merged.join("")
}

fn protect_spans(text: &str) -> (String, Vec<String>) {
    let mut protected: Vec<String> = Vec::new();
    let mut current = text.to_string();
    for pattern in [
        url_re(),
        email_re(),
        mention_re(),
        reddit_re(),
        dot_token_re(),
        date_token_re(),
        filelike_re(),
    ] {
        let mut result = String::new();
        let mut last = 0usize;
        for found in pattern.find_iter(&current) {
            let Ok(found) = found else {
                continue;
            };
            result.push_str(&current[last..found.start()]);
            result.push_str(&format!("___PROT{}___", protected.len()));
            protected.push(found.as_str().to_string());
            last = found.end();
        }
        result.push_str(&current[last..]);
        current = result;
    }
    (current, protected)
}

fn restore_spans(text: &str, protected: &[String]) -> String {
    let mut result = text.to_string();
    for (index, original) in protected.iter().enumerate() {
        result = result.replace(&format!("___PROT{index}___"), original);
    }
    result
}

fn normalize_visible_underscores(text: &str) -> String {
    let pattern = regex_of(PROT);
    let mut out = String::new();
    let mut last = 0usize;
    for found in pattern.find_iter(text) {
        let Ok(found) = found else {
            continue;
        };
        out.push_str(&text[last..found.start()].replace('_', " "));
        out.push_str(found.as_str());
        last = found.end();
    }
    out.push_str(&text[last..].replace('_', " "));
    out
}

fn normalize_flow_arrows(text: &str) -> String {
    flow_arrows_re().replace_all(text, "，").to_string()
}

fn normalize_spaces(text: &str) -> String {
    let cjk = cjk();
    let latinish = latinish();
    let mut current = spaces_re().replace_all(text, " ").to_string();
    for (pattern, replacement) in [
        (format!(r"({cjk})\s+(?={cjk})"), "${1}".to_string()),
        (format!(r"({cjk})\s+(?=\d)"), "${1}".to_string()),
        (format!(r"(\d)\s+(?={cjk})"), "${1}".to_string()),
        (format!(r"({cjk})(?=({latinish}))"), "${1} ".to_string()),
        (format!(r"(({latinish}))(?={cjk})"), "${1} ".to_string()),
    ] {
        let compiled = regex_of(&pattern);
        current = compiled
            .replace_all(&current, replacement.as_str())
            .to_string();
    }
    current = spaces_many_re().replace_all(&current, " ").to_string();
    for (pattern, replacement) in [
        (r"\s+([，。！？；：、”’」』】）》])", "${1}"),
        (r"([（【「『《“‘])\s+", "${1}"),
        (r"([，。！？；：、])\s*", "${1}"),
        (r"\s+([,.;!?])", "${1}"),
    ] {
        let compiled = regex_of(pattern);
        current = compiled.replace_all(&current, replacement).to_string();
    }
    spaces_many_re()
        .replace_all(&current, " ")
        .trim()
        .to_string()
}

fn normalize_structural_punctuation(text: &str) -> String {
    let text = structural_bracket_re()
        .replace_all(text, r#""${1}""#)
        .to_string();
    let text = structural_brace_re()
        .replace_all(&text, r#""${1}""#)
        .to_string();
    let text = structural_corner_re()
        .replace_all(&text, r#""${1}""#)
        .to_string();
    let text = book_title_re().replace_all(&text, "${1}${2}").to_string();
    let text = normalize_flow_arrows(&text);
    long_dash_re().replace_all(&text, "。").to_string()
}

fn normalize_repeated_punctuation(text: &str) -> String {
    let text = ellipsis_re().replace_all(text, "。").to_string();
    let text = repeated_period_re().replace_all(&text, "。").to_string();
    let text = repeated_comma_re().replace_all(&text, "，").to_string();
    let text = repeated_bang_re().replace_all(&text, "！").to_string();
    let text = repeated_question_re().replace_all(&text, "？").to_string();

    let mut result = String::new();
    let mut last = 0usize;
    for found in mixed_question_bang_re().find_iter(&text) {
        let Ok(found) = found else {
            continue;
        };
        result.push_str(&text[last..found.start()]);
        let segment = found.as_str();
        let has_question = segment.contains('?') || segment.contains('？');
        let has_exclaim = segment.contains('!') || segment.contains('！');
        result.push_str(match (has_question, has_exclaim) {
            (true, true) => "？！",
            (true, false) => "？",
            _ => "！",
        });
        last = found.end();
    }
    result.push_str(&text[last..]);
    result
}

fn ensure_terminal_punctuation(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut index = chars.len() as isize - 1;
    while index >= 0 && chars[index as usize].is_whitespace() {
        index -= 1;
    }
    while index >= 0 && TRAILING_CLOSERS.contains(&chars[index as usize]) {
        index -= 1;
    }
    if index >= 0 && is_punctuation_category(chars[index as usize]) {
        return text.to_string();
    }
    format!("{text}。")
}

fn ensure_terminal_punctuation_by_line(text: &str) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let normalized: Vec<String> = text
        .split('\n')
        .map(|line| {
            let stripped = line.trim();
            if stripped.is_empty() {
                String::new()
            } else {
                ensure_terminal_punctuation(stripped)
            }
        })
        .collect();
    normalized.join("\n").trim().to_string()
}

/// 按文本内容（及音色）推断归一化语言。
pub fn resolve_text_normalization_language(text: &str, voice: &str) -> &'static str {
    if cjk_core_re().is_match(text).unwrap_or(false) {
        return "zh";
    }
    if ascii_letter_re().is_match(text).unwrap_or(false) {
        return "en";
    }
    if ENGLISH_VOICES.contains(&voice) {
        return "en";
    }
    "zh"
}

/// 避免中文 WeText 把非数字连字符读成「减」。
pub fn rewrite_hyphens_before_zh_wetext(text: &str) -> String {
    let mut rewritten = text.to_string();
    if !rewritten.contains('-') {
        return rewritten;
    }
    let keep = ZH_WETEXT_KEEP_HYPHEN;
    for (pattern, replacement) in [
        (r"(^\s*)-\s*(?=\d)", format!("${{1}}{keep}")),
        (
            r"([=:+*/,(，：:；;（【\[{])\s*-\s*(?=\d)",
            format!("${{1}}{keep}"),
        ),
        (r"([\u3400-\u9fff])\s*-\s*(?=\d)", format!("${{1}}{keep}")),
        (r"(\d)\s*-\s*(?=\d)", format!("${{1}}{keep}")),
        (
            r"([\u3400-\u9fff])\s*-\s*(?=[\u3400-\u9fff])",
            "${1}，".to_string(),
        ),
        (r"([^\s-])\s*-\s*(?=[^\s-])", "${1} ".to_string()),
    ] {
        let compiled = regex_of(&pattern);
        rewritten = compiled
            .replace_all(&rewritten, replacement.as_str())
            .to_string();
    }
    spaces_many_re()
        .replace_all(&rewritten, " ")
        .trim()
        .to_string()
        .replace(keep, "-")
}

/// 合成前的文本预处理：稳健清洗（Rust 侧不提供 WeText 语义归一化）。
pub fn prepare_tts_request_texts(
    text: &str,
    prompt_text: &str,
    // WeText 缺失时音色只影响语义归一化的语言选择，因此这里不参与计算。
    _voice: &str,
    enable_wetext: bool,
    enable_normalize_tts_text: bool,
) -> Result<Value, String> {
    if enable_wetext {
        return Err(
            "enable_wetext=True 但 WeTextProcessing 不可用；请安装 pynini 与 WeTextProcessing，或将 enable_wetext 设为 False。"
                .to_string(),
        );
    }

    let mut stages: Vec<&str> = Vec::new();
    let mut final_text = text.to_string();
    let mut final_prompt_text = prompt_text.to_string();
    if enable_normalize_tts_text {
        final_text = normalize_tts_text(text);
        final_prompt_text = if prompt_text.is_empty() {
            String::new()
        } else {
            normalize_tts_text(prompt_text)
        };
        stages.push("robust");
    }

    Ok(json!({
        "text": final_text,
        "prompt_text": final_prompt_text,
        "normalized_text": final_text,
        "normalized_prompt_text": final_prompt_text,
        "normalization_method": if stages.is_empty() { "none".to_string() } else { stages.join("+") },
        "text_normalization_language": "",
        "text_normalization_enabled": enable_wetext || enable_normalize_tts_text,
        "wetext_processing_enabled": enable_wetext,
        "normalize_tts_text_enabled": enable_normalize_tts_text,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_paths_survive_the_pipeline() {
        let text = normalize_tts_text("see https://example.com/a/b 与 app.js.map");
        assert!(text.contains("https://example.com/a/b"), "{text}");
        assert!(text.contains("app.js.map"), "{text}");
    }

    #[test]
    fn noise_symbols_and_separators_are_cleaned() {
        assert_eq!(normalize_tts_text("启用/禁用"), "启用、禁用。");
        assert_eq!(normalize_tts_text("A/B"), "A B。");
        assert_eq!(normalize_tts_text("😀★ hello"), "hello。");
    }

    #[test]
    fn terminal_punctuation_is_added_per_line() {
        // 行处理阶段用空串合并（与 Python 一致），因此换行不会保留。
        assert_eq!(normalize_tts_text("第一行\n第二行"), "第一行。第二行。");
        assert_eq!(normalize_tts_text("已经有句号。"), "已经有句号。");
    }

    #[test]
    fn language_detection_prefers_chinese_then_english() {
        assert_eq!(resolve_text_normalization_language("你好", "Trump"), "zh");
        assert_eq!(resolve_text_normalization_language("hello", "Junhao"), "en");
        assert_eq!(resolve_text_normalization_language("123", "Trump"), "en");
        assert_eq!(resolve_text_normalization_language("123", "Junhao"), "zh");
    }

    #[test]
    fn wetext_is_reported_as_unavailable() {
        let error = prepare_tts_request_texts("你好", "", "Junhao", true, true)
            .expect_err("WeText 不可用应当报错");
        assert!(
            error.starts_with("enable_wetext=True 但 WeTextProcessing 不可用"),
            "{error}"
        );

        let payload = prepare_tts_request_texts("你好。", "", "Junhao", false, true)
            .expect("稳健清洗应当成功");
        assert_eq!(payload["normalization_method"], "robust");
        assert_eq!(payload["text"], "你好。");
    }
}
