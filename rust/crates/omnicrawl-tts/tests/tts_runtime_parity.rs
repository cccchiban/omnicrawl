//! 推理链路对照：greedy 模式（不使用随机数）的生成帧必须与 Python 实现逐帧一致。
//!
//! 数据集由 `rust/tools/gen_tts_runtime_fixture.py` 从 Python 真实现生成，需要本机
//! 已下载模型（`~/.omnicrawl/tts/models`）。缺少数据集或模型时跳过，便于无模型环境
//! 仍然跑通其它测试。
//!
//! 重新生成：
//!
//!     python rust/tools/gen_tts_runtime_fixture.py
//!     cd rust && cargo test -p omnicrawl-tts --test tts_runtime_parity

use std::path::PathBuf;

use serde_json::Value;

use omnicrawl_tts::config::TtsConfig;
use omnicrawl_tts::download::models_ready;
use omnicrawl_tts::engine::TtsEngine;
use omnicrawl_tts::normalize::prepare_tts_request_texts;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tts_runtime_parity.json")
}

fn json_ids(value: &Value) -> Vec<Vec<i32>> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| item.as_i64().unwrap_or(0) as i32)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn greedy_generation_matches_python_frames() {
    let path = fixture_path();
    if !path.is_file() {
        eprintln!("跳过：缺少数据集 {}", path.display());
        return;
    }
    if !models_ready(None) {
        eprintln!("跳过：模型未就绪");
        return;
    }

    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).expect("读取数据集"))
        .expect("解析数据集");
    let output_dir = std::env::temp_dir().join("omnicrawl-tts-parity");
    std::fs::create_dir_all(&output_dir).expect("创建临时输出目录");

    let mut engine = TtsEngine::new(TtsConfig {
        device: Some("cpu".to_string()),
        sample_mode: "greedy".to_string(),
        do_sample: false,
        streaming: true,
        output_dir,
        ..TtsConfig::default()
    })
    .expect("引擎应当可用");

    let cases = fixture["cases"].as_array().expect("cases 应当是数组");
    assert!(!cases.is_empty(), "数据集不应为空");

    for case in cases {
        let text = case["text"].as_str().expect("text");
        let voice = case["voice"].as_str().expect("voice");
        let expected_chunks: Vec<String> = case["text_chunks"]
            .as_array()
            .expect("text_chunks")
            .iter()
            .map(|item| item.as_str().unwrap_or_default().to_string())
            .collect();
        let expected_frames = json_ids(&case["generated_frames"]);

        let prompt_codes = engine
            .resolve_prompt_audio_codes(Some(voice), None)
            .expect("音色应当存在");
        // 与 `synthesize` 一致：先做文本归一化，再按音色克隆的 token 预算分块。
        let prepared =
            prepare_tts_request_texts(text, "", voice, false, true).expect("归一化应当成功");
        let prepared_text = prepared["text"].as_str().unwrap_or_default();
        let chunks = engine
            .split_voice_clone_text(prepared_text, 75)
            .expect("分块应当成功");
        assert_eq!(chunks, expected_chunks, "文本分块不一致：{text}");

        let mut frames: Vec<Vec<i32>> = Vec::new();
        for chunk in &chunks {
            let token_ids = engine.encode_text(chunk).expect("分词应当成功");
            let request = engine
                .runtime
                .build_voice_clone_request_rows(&prompt_codes, &token_ids);
            let generated = engine
                .runtime
                .generate_audio_frames(&request, false)
                .expect("生成应当成功");
            frames.extend(generated.generated_frames);
        }

        assert_eq!(
            frames.len(),
            expected_frames.len(),
            "帧数不一致：{text}（Rust {} vs Python {}）",
            frames.len(),
            expected_frames.len()
        );
        assert_eq!(frames, expected_frames, "生成帧不一致：{text}");
    }
}
