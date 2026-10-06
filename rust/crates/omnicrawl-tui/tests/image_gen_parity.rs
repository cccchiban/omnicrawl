//! 对照：Rust `image_gen` vs Python `ImageGenerator`。
//!
//! 数据集是冻结的对照契约：Python 侧注入桩客户端（不联网），
//! 因此可以逐字对照结果文本、落盘文件名与文件内容。文件名与输出里的时间戳规范化成 `{STAMP}`，
//! 工作区路径规范化成 `{WORKSPACE}`。
//!
//! 少数用例标记 `compare: "prefix"`：Python 的文案里提到它自己的设置面板（`/settings`），
//! 本客户端没有设置面板，只保留共同前缀。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use regex::Regex;
use serde_json::{Map, Value};

use omnicrawl_tui::tools::image_gen::{self, ImageGenOptions};
use omnicrawl_tui::tools::web_transport::{WebError, WebRequest, WebResponse, WebTransport};

const FIXTURE: &str = include_str!("fixtures/image_gen_parity.json");
const STAMP_PLACEHOLDER: &str = "{STAMP}";

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("对照数据集必须是合法 JSON")
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// 用例响应：返回数据集给定的图片数据（或错误状态），并记录收到的请求。
struct GenTransport {
    body: Mutex<String>,
    status: Mutex<u16>,
    requests: Mutex<Vec<(String, String)>>,
}

impl GenTransport {
    fn new(status: u16, body: &str) -> Self {
        Self {
            body: Mutex::new(body.to_string()),
            status: Mutex::new(status),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl WebTransport for GenTransport {
    fn send(&self, request: &WebRequest) -> Result<WebResponse, WebError> {
        self.requests
            .lock()
            .expect("请求锁")
            .push((request.method.clone(), request.url.clone()));
        Ok(WebResponse {
            status: *self.status.lock().expect("状态锁"),
            final_url: request.url.clone(),
            body: self.body.lock().expect("响应锁").clone().into_bytes(),
        })
    }
}

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

/// 把运行时的文件名时间戳与工作区路径都换成占位符，便于两侧对照。
fn normalize(text: &str, workspace: &str) -> String {
    let stamp = Regex::new(r"image_\d{8}_\d{6}_").expect("时间戳正则");
    let with_stamp = stamp.replace_all(text, format!("image_{STAMP_PLACEHOLDER}_"));
    let escaped = regex::escape(workspace);
    Regex::new(&format!("(?i){}", escaped))
        .expect("工作区正则")
        .replace_all(&with_stamp, "{WORKSPACE}")
        .to_string()
}

fn collect_files(root: &Path, workspace_root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .map(|name| name == "ref.png")
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                continue;
            };
            let text = relative.to_string_lossy().replace('\\', "/");
            let bytes = std::fs::read(&path).unwrap_or_default();
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            files.push((normalize(&text, &workspace_root.to_string_lossy()), encoded));
        }
    }
    files.sort();
    files
}

#[test]
fn image_gen_cases_match_python() {
    let data = fixture();
    let reference = data["reference_base64"].as_str().unwrap_or_default();

    for case in data["cases"].as_array().expect("用例") {
        let root = std::env::temp_dir().join(format!(
            "omnicrawl-tui-image-gen-parity-{}",
            case["name"].as_str().unwrap_or("case")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        std::fs::write(
            root.join("ref.png"),
            base64::engine::general_purpose::STANDARD
                .decode(reference)
                .expect("参考图样本"),
        )
        .expect("写参考图");

        let workspace = root.to_string_lossy().to_string();
        let config = &case["config"];
        let status = case["status"].as_u64().unwrap_or(200) as u16;
        let response_body = match case["response_body"].as_str() {
            Some(body) => body.to_string(),
            None => serde_json::json!({"data": case["data"]}).to_string(),
        };
        let transport = Arc::new(GenTransport::new(status, &response_body));
        let options = ImageGenOptions {
            transport: transport.clone(),
            workspace_root: root.clone(),
            enabled: config["enabled"].as_bool().unwrap_or(false),
            base_url: config["base_url"].as_str().unwrap_or_default().to_string(),
            api_key: config["api_key"].as_str().unwrap_or_default().to_string(),
            api_key_env: config["api_key_env"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            model: config["model"].as_str().unwrap_or_default().to_string(),
            size: config["size"].as_str().unwrap_or_default().to_string(),
            quality: config["quality"].as_str().unwrap_or_default().to_string(),
            output_format: config["output_format"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            n: config["n"].as_i64().unwrap_or(1),
            // 显式禁用代理：对照不读系统代理、不起网络。
            proxy: Some(String::new()),
            ..ImageGenOptions::default()
        };

        let args = arguments(&substitute(&case["arguments"], &workspace));
        let expected = case["output"].as_str().unwrap_or_default();
        let outcome = image_gen::image_gen(&options, &args);

        let actual = match outcome {
            Ok(output) => {
                assert!(
                    case["ok"].as_bool().unwrap_or(false),
                    "本应失败，实际成功：{output}"
                );
                normalize(&output, &workspace)
            }
            Err(error) => {
                assert!(
                    !case["ok"].as_bool().unwrap_or(true),
                    "本应成功，实际失败：{}",
                    error.message
                );
                normalize(&error.message, &workspace)
            }
        };
        if case["compare"].as_str().unwrap_or("exact") == "prefix" {
            let prefix = case["prefix"].as_str().unwrap_or_default();
            assert!(
                actual.starts_with(prefix),
                "用例 {} 前缀不符：\n实际：{actual}\n期望前缀：{prefix}",
                case["name"]
            );
        } else {
            assert_eq!(actual, expected, "用例 {}", case["name"]);
        }

        // 落盘文件（相对路径 + 内容）也要一致。
        let expected_files: Vec<(String, String)> = case["files"]
            .as_array()
            .expect("文件清单")
            .iter()
            .map(|file| {
                (
                    file["path"].as_str().unwrap_or_default().to_string(),
                    file["base64"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let mut expected_files = expected_files;
        expected_files.sort();
        assert_eq!(
            collect_files(&root, &root),
            expected_files,
            "用例 {} 的落盘文件不一致",
            case["name"]
        );

        let _: PathBuf = root.clone();
    }
}
