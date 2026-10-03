//! 图像生成与编辑工具执行体（OpenAI 兼容 Image API）。
//!
//! 语义基准是 `omnicrawl/media/image_gen.py`：`POST {base_url}/images/generations`（文本生成）与
//! `POST {base_url}/images/edits`（参考图编辑，multipart），响应优先取 `data[].b64_json`（缺 b64 时
//! 下载 `url`）落盘，并按 `已生成 N 张图片（模型 X）：` 的格式回报。配置来源见 [`ImageGenOptions`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use omnicrawl_controllers::json::python_number_text;
use serde_json::{Map, Value};

use super::error::{ToolError, ToolOutcome};
use super::web_transport::{
    detect_windows_proxy, host_of, is_private_host, WebErrorKind, WebRequest, WebResponse,
    WebTransport, MAX_REDIRECTS,
};

pub const DEFAULT_OUTPUT_DIR: &str = ".omnicrawl/.agent_tmp/images";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-image-2";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
const DEFAULT_OUTPUT_FORMAT: &str = "png";
const MAX_ERROR_CHARS: usize = 300;

/// 图像生成配置：字段与 Python 的 `image_gen` 配置段同义，来源不同（见 crate README）。
#[derive(Clone)]
pub struct ImageGenOptions {
    pub transport: Arc<dyn WebTransport>,
    /// 相对路径（保存位置与参考图）的基准目录。
    pub workspace_root: PathBuf,
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub api_key_env: String,
    pub model: String,
    pub size: String,
    pub quality: String,
    pub output_format: String,
    pub n: i64,
    pub timeout_seconds: f64,
    /// `None`：自动检测 Windows 系统代理；`Some("")`：不使用代理；其他为显式地址。
    pub proxy: Option<String>,
}

impl Default for ImageGenOptions {
    fn default() -> Self {
        Self {
            transport: Arc::new(super::web_transport::UreqWebTransport::new()),
            workspace_root: PathBuf::from("."),
            enabled: false,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: String::new(),
            api_key_env: DEFAULT_API_KEY_ENV.to_string(),
            model: DEFAULT_MODEL.to_string(),
            size: "auto".to_string(),
            quality: "auto".to_string(),
            output_format: DEFAULT_OUTPUT_FORMAT.to_string(),
            n: 1,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            proxy: None,
        }
    }
}

impl ImageGenOptions {
    /// 生效的 API Key：只认配置里的明文密钥。
    pub fn resolve_api_key(&self) -> String {
        self.api_key.trim().to_string()
    }
}

/// 请求失败的两类原因：HTTP 状态与传输层。
enum GenFailure {
    Http { status: u16, body: String },
    Transport { kind: WebErrorKind, message: String },
}

impl GenFailure {
    fn message(&self) -> String {
        match self {
            GenFailure::Http { status, body } => {
                let summary = summarize(body);
                if summary.is_empty() {
                    format!("HTTP {status}")
                } else {
                    format!("HTTP {status}：{summary}")
                }
            }
            GenFailure::Transport { kind, message } => match kind {
                WebErrorKind::Timeout => "请求超时。".to_string(),
                WebErrorKind::Connect => "无法连接目标（网络不可达或目标拒绝连接）。".to_string(),
                WebErrorKind::Tls => "TLS 证书校验失败。".to_string(),
                WebErrorKind::Other => format!("网络请求失败：{message}"),
            },
        }
    }
}

struct RequestParams {
    n: i64,
    size: String,
    quality: String,
    output_format: String,
}

pub fn image_gen(options: &ImageGenOptions, arguments: &Map<String, Value>) -> ToolOutcome {
    let prompt = argument_text(arguments.get("prompt"));
    if prompt.is_empty() {
        return Err(ToolError::new("需要提供 prompt。"));
    }
    let image = argument_text(arguments.get("image"));
    let params = RequestParams {
        n: optional_int(arguments.get("n")).unwrap_or(options.n),
        size: optional_str(arguments.get("size")).unwrap_or_else(|| options.size.clone()),
        quality: optional_str(arguments.get("quality")).unwrap_or_else(|| options.quality.clone()),
        output_format: optional_str(arguments.get("output_format"))
            .unwrap_or_else(|| options.output_format.clone()),
    };
    let output_path = optional_str(arguments.get("path"));

    if image.is_empty() {
        generate(options, &prompt, &params, output_path.as_deref())
    } else {
        edit(options, &prompt, &image, &params, output_path.as_deref())
    }
}

fn generate(
    options: &ImageGenOptions,
    prompt: &str,
    params: &RequestParams,
    output_path: Option<&str>,
) -> ToolOutcome {
    ensure_ready(options)?;
    let payload = serde_json::json!({
        "model": options.model,
        "prompt": prompt,
        "n": params.n,
        "size": params.size,
        "quality": params.quality,
        "output_format": params.output_format,
        "response_format": "b64_json",
    });
    let url = format!(
        "{}/images/generations",
        options.base_url.trim_end_matches('/')
    );
    let headers = json_headers(options);
    let request = WebRequest::post_json(
        url,
        headers,
        &payload.to_string(),
        timeout(options.timeout_seconds),
    );
    let response = request_api(options, request)
        .map_err(|failure| ToolError::new(format!("图像生成请求失败：{}", failure.message())))?;
    save_results(options, &response, output_path, "gen")
}

fn edit(
    options: &ImageGenOptions,
    prompt: &str,
    image: &str,
    params: &RequestParams,
    output_path: Option<&str>,
) -> ToolOutcome {
    ensure_ready(options)?;
    let image_path = resolve_local_path(options, image);
    if !image_path.is_file() {
        // 与 Python 一致：显示用户给的原路径（只做 ~ 展开），不做工作区拼接。
        return Err(ToolError::new(format!(
            "图片文件不存在：{}",
            expand_user(image.trim())
        )));
    }
    let bytes = std::fs::read(&image_path).map_err(|error| {
        ToolError::new(format!("读取图片失败：{}，{error}", image_path.display()))
    })?;
    let file_name = image_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "image.png".to_string());
    let boundary = format!("----omnicrawl{}", unique_suffix());
    let body = multipart_body(&boundary, &file_name, &bytes, prompt, options, params);
    let headers = vec![
        (
            "Content-Type".to_string(),
            format!("multipart/form-data; boundary={boundary}"),
        ),
        (
            "Authorization".to_string(),
            format!("Bearer {}", options.resolve_api_key()),
        ),
    ];
    let url = format!("{}/images/edits", options.base_url.trim_end_matches('/'));
    let request = WebRequest::post_bytes(url, headers, body, timeout(options.timeout_seconds));
    let response = request_api(options, request)
        .map_err(|failure| ToolError::new(format!("图像编辑请求失败：{}", failure.message())))?;
    save_results(options, &response, output_path, "edit")
}

fn ensure_ready(options: &ImageGenOptions) -> Result<(), ToolError> {
    if !options.enabled {
        return Err(ToolError::new(
            "图像生成未启用：请在 config.toml 的 [image_gen] 段配置 base_url / model / api_key 并启用。",
        ));
    }
    if options.resolve_api_key().trim().is_empty() {
        return Err(ToolError::new(
            "缺少 API Key：请在 config.toml 的 [image_gen] 段填写 api_key。".to_string(),
        ));
    }
    Ok(())
}

fn json_headers(options: &ImageGenOptions) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {}", options.resolve_api_key()),
        ),
    ]
}

fn request_api(options: &ImageGenOptions, request: WebRequest) -> Result<WebResponse, GenFailure> {
    let mut request = request;
    request.proxy = resolve_proxy(&request.url, options);
    request.max_redirects = MAX_REDIRECTS;
    let response = options
        .transport
        .send(&request)
        .map_err(|error| GenFailure::Transport {
            kind: error.kind,
            message: error.message,
        })?;
    if response.status >= 400 {
        return Err(GenFailure::Http {
            status: response.status,
            body: response.text(),
        });
    }
    Ok(response)
}

fn resolve_proxy(url: &str, options: &ImageGenOptions) -> Option<String> {
    let host = host_of(url).unwrap_or_default();
    if is_private_host(&host) {
        return None;
    }
    match options.proxy.as_deref() {
        None => detect_windows_proxy(),
        Some("") => None,
        Some(explicit) => Some(explicit.to_string()),
    }
}

fn save_results(
    options: &ImageGenOptions,
    response: &WebResponse,
    output_path: Option<&str>,
    prefix: &str,
) -> ToolOutcome {
    let value: Value = serde_json::from_str(&response.text()).unwrap_or(Value::Null);
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if data.is_empty() {
        return Err(ToolError::new("接口未返回任何图片数据。"));
    }

    let first_format = data
        .first()
        .and_then(|item| item.get("output_format"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let format = if first_format.is_empty() {
        options.output_format.clone()
    } else {
        first_format
    };
    let extension = extension_for_format(&format);

    let mut lines = Vec::new();
    for (index, item) in data.iter().enumerate() {
        let target = resolve_target_path(options, output_path, prefix, index, &extension);
        match item.get("b64_json").and_then(Value::as_str) {
            Some(raw) if !raw.is_empty() => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .map_err(|error| ToolError::new(format!("图片 base64 解码失败：{error}")))?;
                write_bytes(&target, &bytes)?;
            }
            _ => {
                let Some(url) = item.get("url").and_then(Value::as_str) else {
                    return Err(ToolError::new(format!(
                        "第 {} 张图片既无 base64 也无 URL。",
                        index + 1
                    )));
                };
                download(options, url, &target)?;
            }
        }
        let size = std::fs::metadata(&target)
            .map(|meta| meta.len())
            .unwrap_or(0);
        lines.push(format!(
            "{}. {}（{size} 字节）",
            index + 1,
            display_path(options, &target)
        ));
    }

    Ok(format!(
        "已生成 {} 张图片（模型 {}）：\n{}",
        data.len(),
        options.model,
        lines.join("\n")
    ))
}

fn download(options: &ImageGenOptions, url: &str, target: &Path) -> Result<(), ToolError> {
    let request = WebRequest::new(
        url.to_string(),
        Vec::new(),
        timeout(options.timeout_seconds),
    );
    let response = request_api(options, request)
        .map_err(|failure| ToolError::new(format!("下载图片失败：{}", failure.message())))?;
    write_bytes(target, &response.body)
}

fn write_bytes(target: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ToolError::new(format!("创建图片目录失败：{error}")))?;
    }
    std::fs::write(target, bytes)
        .map_err(|error| ToolError::new(format!("写入图片失败：{}，{error}", target.display())))
}

/// 解析保存路径：`path` 带扩展名视为完整文件名（多张图追加序号），否则视为目录。
fn resolve_target_path(
    options: &ImageGenOptions,
    output_path: Option<&str>,
    prefix: &str,
    index: usize,
    extension: &str,
) -> PathBuf {
    let stamp = timestamp();
    let file_name = format!("image_{stamp}_{prefix}_{}.{extension}", index + 1);
    match output_path.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => {
            let candidate = resolve_local_path(options, raw);
            if candidate.extension().is_some() {
                if index == 0 {
                    candidate
                } else {
                    numbered_sibling(&candidate, index)
                }
            } else {
                candidate.join(file_name)
            }
        }
        None => default_output_dir(options).join(file_name),
    }
}

/// 默认输出目录：按工作区逐段拼接，避免常量里的 `/` 与 Windows 分隔符混用。
fn default_output_dir(options: &ImageGenOptions) -> PathBuf {
    options
        .workspace_root
        .join(".omnicrawl")
        .join(".agent_tmp")
        .join("images")
}

fn numbered_sibling(path: &Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = if extension.is_empty() {
        format!("{stem}_{}", index + 1)
    } else {
        format!("{stem}_{}.{extension}", index + 1)
    };
    path.with_file_name(name)
}

fn resolve_local_path(options: &ImageGenOptions, raw: &str) -> PathBuf {
    let candidate = PathBuf::from(expand_user(raw.trim()));
    if candidate.is_absolute() {
        candidate
    } else {
        options.workspace_root.join(candidate)
    }
}

fn expand_user(path: &str) -> String {
    let home = || -> PathBuf {
        for name in ["USERPROFILE", "HOME"] {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return PathBuf::from(value);
                }
            }
        }
        PathBuf::from(".")
    };
    if path == "~" {
        return home().to_string_lossy().to_string();
    }
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home().join(rest).to_string_lossy().to_string(),
        None => path.to_string(),
    }
}

/// 展示路径：工作区内显示相对路径（与 Python 的同名行为一致），否则显示绝对路径。
fn display_path(options: &ImageGenOptions, target: &Path) -> String {
    match target.strip_prefix(&options.workspace_root) {
        Ok(relative) => relative.to_string_lossy().to_string(),
        Err(_) => target.to_string_lossy().to_string(),
    }
}

fn extension_for_format(format: &str) -> String {
    match format.trim().to_lowercase().as_str() {
        "jpeg" => "jpg".to_string(),
        "png" | "webp" => format.trim().to_lowercase(),
        _ => DEFAULT_OUTPUT_FORMAT.to_string(),
    }
}

fn multipart_body(
    boundary: &str,
    file_name: &str,
    bytes: &[u8],
    prompt: &str,
    options: &ImageGenOptions,
    params: &RequestParams,
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"{file_name}\"\r\n\
Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    let n_text = params.n.to_string();
    for (name, value) in [
        ("model", options.model.as_str()),
        ("prompt", prompt),
        ("n", n_text.as_str()),
        ("size", params.size.as_str()),
        ("quality", params.quality.as_str()),
        ("output_format", params.output_format.as_str()),
        ("response_format", "b64_json"),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

fn timeout(seconds: f64) -> Duration {
    Duration::from_secs_f64(if seconds <= 0.0 { 1.0 } else { seconds })
}

fn summarize(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() > MAX_ERROR_CHARS {
        format!("{}…", chars[..MAX_ERROR_CHARS].iter().collect::<String>())
    } else {
        collapsed
    }
}

fn optional_int(value: Option<&Value>) -> Option<i64> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::Bool(flag)) => Some(i64::from(*flag)),
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|item| item.trunc() as i64)),
        Some(Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn optional_str(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(other) => Some(argument_text(Some(other))),
    }
}

fn argument_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Number(number)) => python_number_text(&Value::Number(number.clone()))
            .trim()
            .to_string(),
        Some(Value::Bool(flag)) => if *flag { "True" } else { "False" }.to_string(),
        Some(other) => omnicrawl_controllers::json::python_repr(other)
            .trim()
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn arguments(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn workspace(name: &str) -> (ImageGenOptions, PathBuf) {
        let root = std::env::temp_dir().join(format!("omnicrawl-tui-image-gen-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时工作区");
        let options = ImageGenOptions {
            workspace_root: root.clone(),
            enabled: true,
            api_key: "test-key".to_string(),
            ..ImageGenOptions::default()
        };
        (options, root)
    }

    #[test]
    fn prompt_and_configuration_are_validated_first() {
        let (options, _root) = workspace("config");
        assert_eq!(
            image_gen(&options, &arguments(json!({"prompt": "  "})))
                .expect_err("空 prompt 应当被拒绝")
                .message,
            "需要提供 prompt。"
        );

        let disabled = ImageGenOptions {
            enabled: false,
            ..options.clone()
        };
        assert!(image_gen(&disabled, &arguments(json!({"prompt": "x"})))
            .expect_err("未启用应当被拒绝")
            .message
            .starts_with("图像生成未启用："));

        // 凭据只认明文 api_key：环境变量名填了也没用。
        let no_key = ImageGenOptions {
            api_key: String::new(),
            api_key_env: "OMNICRAWL_TUI_IMAGE_GEN_MISSING".to_string(),
            ..options.clone()
        };
        let error = image_gen(&no_key, &arguments(json!({"prompt": "x"})))
            .expect_err("缺 API Key 应当被拒绝");
        assert!(
            error.message.contains("[image_gen]") && error.message.contains("api_key"),
            "{}",
            error.message
        );
    }

    #[test]
    fn edit_requires_an_existing_local_image() {
        let (options, _root) = workspace("edit");
        let error = image_gen(
            &options,
            &arguments(json!({"prompt": "x", "image": "missing.png"})),
        )
        .expect_err("缺少参考图应当被拒绝");
        assert!(
            error.message.starts_with("图片文件不存在："),
            "{}",
            error.message
        );
    }

    #[test]
    fn target_paths_follow_python_rules() {
        let (options, root) = workspace("paths");
        let single = resolve_target_path(&options, None, "gen", 0, "png");
        assert!(single.starts_with(root.join(DEFAULT_OUTPUT_DIR)));
        assert!(single
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .ends_with("_gen_1.png"));

        let directory = resolve_target_path(&options, Some("out"), "gen", 1, "jpg");
        assert!(directory.starts_with(root.join("out")));
        assert!(directory
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .ends_with("_gen_2.jpg"));

        assert_eq!(
            resolve_target_path(&options, Some("pic.png"), "gen", 0, "png"),
            root.join("pic.png")
        );
        assert_eq!(
            resolve_target_path(&options, Some("pic.png"), "gen", 1, "png"),
            root.join("pic_2.png")
        );
    }

    #[test]
    fn extensions_follow_the_requested_format() {
        assert_eq!(extension_for_format("jpeg"), "jpg");
        assert_eq!(extension_for_format("PNG"), "png");
        assert_eq!(extension_for_format("webp"), "webp");
        assert_eq!(extension_for_format("bmp"), "png");
    }
}
