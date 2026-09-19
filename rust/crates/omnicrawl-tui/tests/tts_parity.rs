//! 对照：Rust TTS 文本归一化 vs Python `omnicrawl/tts/normalize.py`。
//!
//! 数据集由 `python rust/tools/gen_tts_fixture.py` 生成，覆盖稳健清洗管道、中文 WeText 连字符保护、
//! 语言推断与合成前预处理。

use serde_json::Value;

use omnicrawl_tui::tools::tts::normalize::{
    normalize_tts_text, prepare_tts_request_texts, resolve_text_normalization_language,
    rewrite_hyphens_before_zh_wetext,
};

const FIXTURE: &str = include_str!("fixtures/tts_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

#[test]
fn normalization_cases_match_python() {
    let data = fixture();
    for case in data["normalize"].as_array().expect("normalize 用例") {
        let text = case["text"].as_str().unwrap_or_default();
        assert_eq!(
            normalize_tts_text(text),
            case["expected"].as_str().unwrap_or_default(),
            "文本 {text:?}"
        );
    }
}

#[test]
fn hyphen_guards_match_python() {
    let data = fixture();
    for case in data["hyphens"].as_array().expect("hyphens 用例") {
        let text = case["text"].as_str().unwrap_or_default();
        assert_eq!(
            rewrite_hyphens_before_zh_wetext(text),
            case["expected"].as_str().unwrap_or_default(),
            "文本 {text:?}"
        );
    }
}

#[test]
fn language_detection_matches_python() {
    let data = fixture();
    for case in data["languages"].as_array().expect("语言用例") {
        let text = case["text"].as_str().unwrap_or_default();
        let voice = case["voice"].as_str().unwrap_or_default();
        assert_eq!(
            resolve_text_normalization_language(text, voice),
            case["expected"].as_str().unwrap_or_default(),
            "文本 {text:?} 音色 {voice:?}"
        );
    }
}

#[test]
fn pipeline_payloads_match_python() {
    let data = fixture();
    for case in data["pipeline"].as_array().expect("管道用例") {
        let text = case["text"].as_str().unwrap_or_default();
        let prompt_text = case["prompt_text"].as_str().unwrap_or_default();
        let enable_normalize = case["enable_normalize_tts_text"].as_bool().unwrap_or(true);
        let payload =
            prepare_tts_request_texts(text, prompt_text, "Junhao", false, enable_normalize)
                .expect("稳健清洗应当成功");
        assert_eq!(
            payload, case["expected"],
            "文本 {text:?} enable_normalize={enable_normalize}"
        );
    }
}
