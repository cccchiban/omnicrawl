//! `[tts_api]` 段（Rust 侧新增的语音合成接口）的读取、校验与写回契约。
//!
//! 这段没有 Python 对映实现，因此不进 `config_features_extra_parity.json`：
//! 那份数据集比对的是 `[tts]`（本地 ONNX 推理）的读回值与写回文本，逐字节钉住，
//! 加字段就必须同步改 Python。这里单独固定接口段的默认值、归一化与密钥解析。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::tts_api::{
    load_tts_api_configuration, save_tts_api_configuration, TtsApiConfiguration,
    DEFAULT_TTS_API_BASE_URL, DEFAULT_TTS_API_KEY_ENV, DEFAULT_TTS_API_MODEL,
    DEFAULT_TTS_API_VOICE,
};

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-tts-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn user_dir(root: &Path) -> PathBuf {
    let dir = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&dir).expect("建立用户配置目录");
    dir
}

fn write_text(path: &Path, text: &str) {
    std::fs::write(path, text).expect("写配置");
}

/// 没有 `tts_api` 段时：默认开启接口合成，地址/模型/音色/密钥变量名都给默认值。
#[test]
fn missing_section_yields_api_defaults() {
    let root = temp_root("empty");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_tts_api_configuration(&env, None).expect("缺段时用默认值");
    assert!(config.enabled, "默认走接口合成（发布构建不含本地推理）");
    assert_eq!(config.base_url, DEFAULT_TTS_API_BASE_URL);
    assert_eq!(config.model, DEFAULT_TTS_API_MODEL);
    assert_eq!(config.voice, DEFAULT_TTS_API_VOICE);
    assert_eq!(config.api_key_env, DEFAULT_TTS_API_KEY_ENV);
    assert_eq!(config.response_format, "wav");
    assert_eq!(config.speed, 1.0);
    assert_eq!(config.timeout_seconds, 120);
    assert_eq!(
        config.speech_url(),
        "https://api.openai.com/v1/audio/speech",
        "接口地址按 base_url + /audio/speech 拼"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 全字段读取：地址去尾斜杠、格式转小写、字符串数值按十进制解析。
#[test]
fn full_section_is_normalized() {
    let root = temp_root("full");
    let user = user_dir(&root);
    write_text(
        &user.join("config.toml"),
        r#"
[tts_api]
enabled = false
base_url = "  https://tts.example.com/v1/  "
api_key = " plain-key "
api_key_env = "MY_TTS_KEY"
model = " cosyvoice-2 "
voice = " longxiaochun "
response_format = "WAV"
speed = "1.25"
timeout_seconds = 90
"#,
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_tts_api_configuration(&env, None).expect("读取接口配置");
    assert!(!config.enabled);
    assert_eq!(config.base_url, "https://tts.example.com/v1");
    assert_eq!(config.api_key, "plain-key");
    assert_eq!(config.model, "cosyvoice-2");
    assert_eq!(config.voice, "longxiaochun");
    assert_eq!(config.response_format, "wav");
    assert_eq!(config.speed, 1.25);
    assert_eq!(config.timeout_seconds, 90);
    assert_eq!(
        config.speech_url(),
        "https://tts.example.com/v1/audio/speech"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 密钥解析：配置里的明文优先，否则读 `api_key_env`（含环境变量注入口径）。
#[test]
fn api_key_comes_from_inline_only() {
    let root = temp_root("key");
    let user = user_dir(&root);
    write_text(
        &user.join("config.toml"),
        r#"
[tts_api]
api_key = "inline-key"
api_key_env = "OC_TEST_TTS_KEY"
"#,
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32")
        .with_env_value("OC_TEST_TTS_KEY", "env-key");
    let config = load_tts_api_configuration(&env, None).expect("读取接口配置");
    assert_eq!(config.resolve_api_key(&env), "inline-key");

    // 环境变量通道已移除：没有明文 key 时解析结果为空串（由调用方判「缺凭据」）。
    let mut without_inline = config.clone();
    without_inline.api_key = String::new();
    assert_eq!(without_inline.resolve_api_key(&env), "");

    // 空密钥变量名回落到默认名，避免写成空串后永远取不到密钥。
    let mut blank_env_name = config.clone();
    blank_env_name.api_key_env = "   ".to_string();
    assert_eq!(
        blank_env_name.normalize().expect("归一化").api_key_env,
        DEFAULT_TTS_API_KEY_ENV
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 非法取值报错：这些都是用户手改配置时最常见的错法。
///
/// 注意空字符串不会报错：读写口径沿用前面几段的 `str(x) or fallback`，
/// `model = ""` 会被当成「没填」落到默认模型，而不是拒绝启动。
#[test]
fn invalid_values_are_rejected() {
    let cases: [(&str, &str); 5] = [
        (
            "base_url = \"ftp://x\"",
            "tts_api.base_url 必须以 http:// 或 https:// 开头。",
        ),
        (
            "response_format = \"mp3\"",
            "tts_api.response_format 仅支持 wav。",
        ),
        (
            "speed = 9.0",
            "tts_api.speed 必须是 0.25~4.0 之间的数值。",
        ),
        (
            "speed = \"fast\"",
            "配置段 tts_api.speed 无效：could not convert string to float: 'fast'",
        ),
        (
            "timeout_seconds = 9999",
            "tts_api.timeout_seconds 必须是 1~600 的整数。",
        ),
    ];
    for (index, (line, expected)) in cases.iter().enumerate() {
        let root = temp_root(&format!("bad-{index}"));
        let user = user_dir(&root);
        write_text(&user.join("config.toml"), &format!("[tts_api]\n{line}\n"));
        let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
        let error = match load_tts_api_configuration(&env, None) {
            Ok(value) => panic!("用例 {index}（{line}）应当报错，实际得到 {value:?}"),
            Err(error) => error,
        };
        assert_eq!(error.message(), *expected, "用例 {index}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 写回只动 `tts_api` 段，其他段（含 `[tts]`）保持原样。
#[test]
fn save_keeps_other_sections_and_round_trips() {
    let root = temp_root("save");
    let user = user_dir(&root);
    let config_path = user.join("config.toml");
    write_text(
        &config_path,
        "[llm]\nmodel = \"keep-me\"\n\n[tts]\nvoice = \"Junhao\"\n",
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let configuration = TtsApiConfiguration {
        enabled: true,
        base_url: "https://tts.example.com/v1".to_string(),
        model: "cosyvoice-2".to_string(),
        voice: "longxiaochun".to_string(),
        speed: 1.5,
        ..TtsApiConfiguration::default()
    };
    save_tts_api_configuration(&env, &configuration, None).expect("写回接口配置");

    let text = std::fs::read_to_string(&config_path).expect("读回配置");
    assert!(text.contains("[llm]\nmodel = \"keep-me\""), "其他段保持原样：{text}");
    assert!(text.contains("[tts]\nvoice = \"Junhao\""), "`[tts]` 不得被改写：{text}");

    let reloaded = load_tts_api_configuration(&env, None).expect("重新读取");
    assert_eq!(reloaded, configuration);
    let _ = std::fs::remove_dir_all(&root);
}
