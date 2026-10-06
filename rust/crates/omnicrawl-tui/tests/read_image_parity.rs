//! 对照：Rust `read_image` vs Python `read_image_file`。
//!
//! 数据集是冻结的对照契约：图片以 Base64 存进 fixture，
//! 两侧用同一份样本铺工作区；工作区根在两侧不同（临时目录），因此比对前统一规范化为
//! `{WORKSPACE}`（路径大小写不敏感）。

use std::path::PathBuf;

use base64::Engine;
use regex::Regex;
use serde_json::{Map, Value};

use omnicrawl_tui::tools::read_image;
use omnicrawl_tui::tools::WorkspacePaths;

const FIXTURE: &str = include_str!("fixtures/read_image_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 把工作区路径（大小写不敏感）替换成占位符，避免两侧绝对路径差异干扰对照。
fn normalize(text: &str, workspace: &str) -> String {
    let pattern = format!("(?i){}", regex::escape(workspace));
    Regex::new(&pattern)
        .expect("工作区正则应当合法")
        .replace_all(text, "{WORKSPACE}")
        .to_string()
}

fn prepare_workspace(name: &str, files: &Value) -> (WorkspacePaths, PathBuf) {
    let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-parity-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    for file in files.as_array().expect("图片清单") {
        let path = root.join(file["path"].as_str().unwrap_or_default());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file["base64"].as_str().unwrap_or_default())
            .expect("图片样本应当是合法 Base64");
        std::fs::write(path, bytes).expect("写图片样本");
    }
    (WorkspacePaths::new(&root), root)
}

/// 把用例参数里的 `{WORKSPACE}` 占位符换成本次运行的临时工作区路径。
fn substitute(value: &Value, workspace: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace("{WORKSPACE}", workspace)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute(item, workspace))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, workspace)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
#[cfg(windows)]
// 数据集里的期望值一律是 Windows 形态（路径分隔符、盘符与平台常量），
// 被测实现也按 win32 分支做字符串化：POSIX 主机上必然形态不符。
// 这条对照只在 Windows 主机上有意义。
fn read_image_cases_match_python() {
    let data = fixture();
    let (paths, root) = prepare_workspace("cases", &data["files"]);
    let workspace = paths.root().to_string_lossy().to_string();
    let _ = root;

    for case in data["cases"].as_array().expect("用例") {
        let args = arguments(&substitute(&case["arguments"], &workspace));
        let expected = normalize(case["output"].as_str().unwrap_or_default(), &workspace);
        let ok = case["ok"].as_bool().unwrap_or(false);
        let expected_images = case["images"].as_array().expect("附件清单");

        match read_image::read_image(&paths, &args) {
            Ok(outcome) => {
                assert!(ok, "本应失败，实际成功：{}", outcome.output);
                assert_eq!(
                    normalize(&outcome.output, &workspace),
                    expected,
                    "载荷不一致：{:?}",
                    case["arguments"]
                );
                assert_eq!(outcome.images.len(), expected_images.len());
                for (actual, expected) in outcome.images.iter().zip(expected_images) {
                    assert_eq!(actual.media_type, expected["media_type"]);
                    assert_eq!(actual.filename, expected["filename"]);
                    assert_eq!(actual.detail, expected["detail"]);
                    assert_eq!(actual.data_base64, expected["data_base64"]);
                }
            }
            Err(error) => {
                assert!(!ok, "本应成功，实际失败：{}", error.message);
                assert_eq!(
                    normalize(&error.message, &workspace),
                    expected,
                    "错误文案不一致：{:?}",
                    case["arguments"]
                );
            }
        }
    }
}
