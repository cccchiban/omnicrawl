//! 对照：Rust TTS 音频 I/O 与声线库 vs Python `omnicrawl/tts/{audio,custom_voices}.py`。
//!
//! 数据集是冻结的对照契约：WAV 样本以 Base64 内嵌，
//! 写入用例记录 Python 写出的完整文件字节。

use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::Value;

use omnicrawl_tts::audio::{
    load_reference_audio, read_wav, resample_linear, write_wav, AudioBuffer,
};
use omnicrawl_tts::voices::{
    add_custom_voice, custom_voices_path_in, delete_custom_voice, list_custom_voice_names,
    validate_voice_name,
};

const FIXTURE: &str = include_str!("fixtures/tts_io_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-tts-io-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时目录");
    root
}

fn decode(base64_text: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(base64_text)
        .expect("样本应当是合法 Base64")
}

fn buffer_of(value: &Value) -> AudioBuffer {
    value
        .as_array()
        .expect("波形是数组")
        .iter()
        .map(|lane| {
            lane.as_array()
                .expect("声道是数组")
                .iter()
                .map(|sample| sample.as_f64().unwrap_or(0.0) as f32)
                .collect()
        })
        .collect()
}

fn assert_buffers(actual: &AudioBuffer, expected: &Value, context: &str) {
    let expected_rows = expected.as_array().expect("期望波形是数组");
    assert_eq!(actual.len(), expected_rows.len(), "{context}：声道数");
    for (lane, expected_lane) in actual.iter().zip(expected_rows) {
        let expected_samples = expected_lane.as_array().expect("期望声道是数组");
        assert_eq!(lane.len(), expected_samples.len(), "{context}：采样数");
        for (sample, expected_sample) in lane.iter().zip(expected_samples) {
            let expected_value = expected_sample.as_f64().unwrap_or(0.0) as f32;
            assert!(
                (sample - expected_value).abs() < 1e-6,
                "{context}：{sample} vs {expected_value}"
            );
        }
    }
}

#[test]
fn wav_reading_matches_python() {
    let data = fixture();
    let root = workspace("read");
    for case in data["read"].as_array().expect("读取用例") {
        let name = case["name"].as_str().unwrap_or_default();
        let path = root.join(name);
        std::fs::write(&path, decode(case["base64"].as_str().unwrap_or_default())).expect("写样本");
        let (waveform, sample_rate) =
            read_wav(&path).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            sample_rate as u64,
            case["sample_rate"].as_u64().unwrap_or(0),
            "{name}：采样率"
        );
        assert_buffers(&waveform, &case["expected"], name);
    }
}

#[test]
fn resampling_matches_python() {
    let data = fixture();
    for case in data["resample"].as_array().expect("重采样用例") {
        let source = buffer_of(&case["source"]);
        let source_rate = case["source_rate"].as_u64().unwrap_or(0) as u32;
        let target_rate = case["target_rate"].as_u64().unwrap_or(0) as u32;
        let actual = resample_linear(&source, source_rate, target_rate);
        assert_buffers(
            &actual,
            &case["expected"],
            &format!("{source_rate}->{target_rate}"),
        );
    }
}

#[test]
fn reference_audio_loading_matches_python() {
    let data = fixture();
    let root = workspace("reference");
    for case in data["reference"].as_array().expect("参考音频用例") {
        let name = case["name"].as_str().unwrap_or_default();
        let path = root.join(name);
        if !path.is_file() {
            // 样本来自读取用例的 Base64：这里复用同名样本。
            let source = data["read"]
                .as_array()
                .expect("读取用例")
                .iter()
                .find(|item| item["name"] == name)
                .expect("样本存在");
            std::fs::write(&path, decode(source["base64"].as_str().unwrap_or_default()))
                .expect("写样本");
        }
        let actual = load_reference_audio(
            &path,
            case["target_sample_rate"].as_u64().unwrap_or(0) as u32,
            case["target_channels"].as_u64().unwrap_or(0) as usize,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_buffers(&actual, &case["expected"], name);
    }
}

#[test]
fn wav_writing_matches_python_bytes() {
    let data = fixture();
    let root = workspace("write");
    for case in data["write"].as_array().expect("写入用例") {
        let name = case["name"].as_str().unwrap_or_default();
        let waveform = buffer_of(&case["waveform"]);
        let path = root.join(name);
        write_wav(
            &path,
            &waveform,
            case["sample_rate"].as_u64().unwrap_or(0) as u32,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            std::fs::read(&path).expect("读回"),
            decode(case["base64"].as_str().unwrap_or_default()),
            "{name}：字节不一致"
        );
    }
}

#[test]
fn voice_name_validation_matches_python() {
    let data = fixture();
    for case in data["validate"].as_array().expect("校验用例") {
        let input = case["input"].as_str().unwrap_or_default();
        let expected_ok = case["ok"].as_bool().unwrap_or(false);
        match validate_voice_name(input) {
            Ok(name) => {
                assert!(expected_ok, "{input:?} 本应失败");
                assert_eq!(
                    name,
                    case["result"].as_str().unwrap_or_default(),
                    "{input:?}"
                );
            }
            Err(error) => {
                assert!(!expected_ok, "{input:?} 本应成功");
                assert_eq!(
                    error,
                    case["error"].as_str().unwrap_or_default(),
                    "{input:?}"
                );
            }
        }
    }
}

#[test]
fn custom_voice_library_matches_python() {
    let data = fixture();
    let root = workspace("library");
    for case in data["library"].as_array().expect("音色库用例") {
        match case["op"].as_str().unwrap_or_default() {
            "add" => {
                let entry = add_custom_voice(
                    &root,
                    "Fairy",
                    &[vec![1, 2, 3], vec![4, 5]],
                    "CN 我的克隆音色",
                    "fairy_ref.wav",
                    "source/ref.wav",
                )
                .expect("写入应当成功");
                assert_eq!(entry, case["entry"]);
            }
            "overwrite" => {
                add_custom_voice(&root, "Fairy", &[vec![9]], "", "", "").expect("覆盖应当成功");
                let names: Vec<Value> = list_custom_voice_names(&root)
                    .into_iter()
                    .map(Value::from)
                    .collect();
                assert_eq!(Value::Array(names), case["names"]);
            }
            "library_text" => {
                let text = std::fs::read_to_string(custom_voices_path_in(&root)).expect("读音色库");
                assert_eq!(text, case["text"].as_str().unwrap_or_default());
            }
            "delete" | "delete-again" => {
                let removed = delete_custom_voice(&root, "Fairy").expect("删除");
                assert_eq!(removed, case["removed"].as_bool().unwrap_or(false));
            }
            "names-after-delete" => {
                let names: Vec<Value> = list_custom_voice_names(&root)
                    .into_iter()
                    .map(Value::from)
                    .collect();
                assert_eq!(Value::Array(names), case["names"]);
            }
            other => panic!("未知操作：{other}"),
        }
    }
    let _: &Path = &root;
}
