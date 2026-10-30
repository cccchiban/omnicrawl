//! 图像生成与编辑工具执行体（两条通道：OpenAI 兼容 Image API 与 Gemini `generateContent`）。
//!
//! OpenAI 通道的语义基准是 `omnicrawl/media/image_gen.py`：`POST {base_url}/images/generations`
//! （文本生成）与 `POST {base_url}/images/edits`（参考图编辑，multipart），响应优先取
//! `data[].b64_json`（缺 b64 时下载 `url`）落盘，并按 `已生成 N 张图片（模型 X）：` 的格式回报。
//!
//! Gemini 通道走 `POST {base_url}/models/{model}:generateContent`：图片以 base64 内联在
//! `candidates[].content.parts[].inlineData.data`，文生图与图生图共用这条端点（有参考图即图生图，
//! 参考图作为 `inlineData` 进同一轮的 `parts`）；接口没有批量字段，`n > 1` 时逐张请求。
//! 配置来源见 [`ImageGenOptions`]。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use omnicrawl_controllers::json::python_number_text;
use serde_json::{Map, Value};

use super::error::{ToolError, ToolOutcome};
use super::paths::expand_user;
use super::web_transport::{
    detect_windows_proxy, host_of, is_private_host, WebErrorKind, WebRequest, WebResponse,
    WebTransport, MAX_REDIRECTS,
};

pub const DEFAULT_OUTPUT_DIR: &str = ".omnicrawl/.agent_tmp/images";
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-image-2";
pub const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 120.0;
pub const DEFAULT_PROVIDER: &str = "openai";
const DEFAULT_OUTPUT_FORMAT: &str = "png";
const MAX_ERROR_CHARS: usize = 300;

/// 图像生成配置：字段与 Python 的 `image_gen` 配置段同义，来源不同（见 crate README）。
#[derive(Clone)]
pub struct ImageGenOptions {
    pub transport: Arc<dyn WebTransport>,
    /// 相对路径（保存位置与参考图）的基准目录。
    pub workspace_root: PathBuf,
    pub enabled: bool,
    /// `openai` 走 OpenAI 兼容 Image API，`gemini` 走 `generateContent`。
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub size: String,
    pub quality: String,
    pub output_format: String,
    /// Gemini 的画面比例（如 `16:9`）；空串表示不下发。
    pub aspect_ratio: String,
    /// Gemini 的分辨率档位（`1K`/`2K`/`4K`）；空串表示不下发。
    pub image_size: String,
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
            provider: DEFAULT_PROVIDER.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            size: "auto".to_string(),
            quality: "auto".to_string(),
            output_format: DEFAULT_OUTPUT_FORMAT.to_string(),
            aspect_ratio: String::new(),
            image_size: String::new(),
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

    /// 这次出图是否走 Gemini 通道。
    fn is_gemini(&self) -> bool {
        self.provider.trim().eq_ignore_ascii_case("gemini")
    }
}

/// 按 `[image_gen]` 段装配出图运行期（TUI 与 API 共用同一口径）。
///
/// 读不出配置时用默认选项（未启用 + 无凭据）：调用时如实报「未启用」，而不是让工具
/// 整组不可用（工具始终进表，与 Python 一致）。凭据只认段里的明文 `api_key`。
pub fn image_gen_options_from_config(
    environment: &omnicrawl_config::core::runtime::ConfigEnvironment,
) -> ImageGenOptions {
    let configuration =
        omnicrawl_config::features::image_gen::load_image_gen_configuration(environment, None)
            .unwrap_or_default();
    ImageGenOptions {
        enabled: configuration.enabled,
        provider: configuration.provider,
        base_url: configuration.base_url,
        api_key: configuration.api_key,
        model: configuration.model,
        size: configuration.size,
        quality: configuration.quality,
        output_format: configuration.output_format,
        aspect_ratio: configuration.aspect_ratio,
        image_size: configuration.image_size,
        n: configuration.n,
        timeout_seconds: configuration.timeout_seconds as f64,
        ..ImageGenOptions::default()
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
                WebErrorKind::Handshake => format!("TLS 握手失败：{message}"),
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

    if options.is_gemini() {
        return gemini(options, &prompt, &image, params.n, output_path.as_deref());
    }
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

/// Gemini 通道：文生图与图生图共用 `generateContent`，逐张请求。
///
/// 接口没有 `n` 与输出格式字段，`n` 由这里展开成多次调用；参考图作为 `inlineData`
/// 与文本进同一轮的 `parts`（有参考图即图生图）。
fn gemini(
    options: &ImageGenOptions,
    prompt: &str,
    image: &str,
    n: i64,
    output_path: Option<&str>,
) -> ToolOutcome {
    ensure_ready(options)?;
    let reference = if image.trim().is_empty() {
        None
    } else {
        let image_path = resolve_local_path(options, image);
        if !image_path.is_file() {
            // 与 OpenAI 通道一致：显示用户给的原路径（只做 ~ 展开），不做工作区拼接。
            return Err(ToolError::new(format!(
                "图片文件不存在：{}",
                expand_user(image.trim())
            )));
        }
        let bytes = std::fs::read(&image_path).map_err(|error| {
            ToolError::new(format!("读取图片失败：{}，{error}", image_path.display()))
        })?;
        Some((mime_type_for(&image_path), bytes))
    };

    let body = gemini_request_body(prompt, reference.as_ref(), options);
    let url = format!(
        "{}/models/{}:generateContent",
        options.base_url.trim_end_matches('/'),
        options.model.trim()
    );
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("x-goog-api-key".to_string(), options.resolve_api_key()),
    ];

    // 接口一次只回一张图，因此按 n 逐张请求；任一张失败即整体失败，已落盘的不回滚。
    let mut lines = Vec::new();
    let count = n.max(1);
    for index in 0..count {
        let request = WebRequest::post_json(
            url.clone(),
            headers.clone(),
            &body,
            timeout(options.timeout_seconds),
        );
        let response = request_api(options, request).map_err(|failure| {
            ToolError::new(format!("图像生成请求失败：{}", failure.message()))
        })?;
        let bytes = gemini_image_bytes(&response.text())?;
        let target = resolve_target_path(options, output_path, "gen", index as usize, "png");
        write_bytes(&target, &bytes)?;
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
        count,
        options.model,
        lines.join("\n")
    ))
}

/// 请求体：`responseModalities` 必须含 `IMAGE`，否则模型只回文本。
fn gemini_request_body(
    prompt: &str,
    reference: Option<&(String, Vec<u8>)>,
    options: &ImageGenOptions,
) -> String {
    let mut parts = vec![serde_json::json!({ "text": prompt })];
    if let Some((mime_type, bytes)) = reference {
        parts.push(serde_json::json!({
            "inlineData": {
                "mimeType": mime_type,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            },
        }));
    }
    let mut generation_config = Map::new();
    generation_config.insert(
        "responseModalities".to_string(),
        serde_json::json!(["IMAGE"]),
    );
    let mut image_config = Map::new();
    if !options.aspect_ratio.trim().is_empty() {
        image_config.insert(
            "aspectRatio".to_string(),
            Value::String(options.aspect_ratio.trim().to_string()),
        );
    }
    if !options.image_size.trim().is_empty() {
        image_config.insert(
            "imageSize".to_string(),
            Value::String(options.image_size.trim().to_string()),
        );
    }
    if !image_config.is_empty() {
        generation_config.insert("imageConfig".to_string(), Value::Object(image_config));
    }
    let body = serde_json::json!({
        "contents": [{ "role": "user", "parts": parts }],
        "generationConfig": Value::Object(generation_config),
    });
    body.to_string()
}

/// 取 `candidates[0].content.parts[].inlineData.data`：一轮可能分多个 part，按序拼接。
fn gemini_image_bytes(body: &str) -> Result<Vec<u8>, ToolError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        ToolError::new(format!(
            "解析 Gemini 响应失败：{error}：{}",
            summarize(body)
        ))
    })?;
    let parts = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut bytes: Vec<u8> = Vec::new();
    for part in &parts {
        let Some(encoded) = part
            .get("inlineData")
            .and_then(|inline| inline.get("data"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                ToolError::new(format!("Gemini 返回的图片 base64 解码失败：{error}"))
            })?;
        bytes.extend_from_slice(&decoded);
    }
    if bytes.is_empty() {
        // 被安全策略拦下时这里通常有 finishReason / promptFeedback，带出来便于排障。
        return Err(ToolError::new(format!(
            "Gemini 响应里没有图片数据：{}",
            summarize(body)
        )));
    }
    Ok(bytes)
}

/// 参考图的媒体类型：按扩展名给，认不出的一律按 PNG（Gemini 只认 image/* 命名）。
fn mime_type_for(path: &Path) -> String {
    match path
        .extension()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "webp" => "image/webp".to_string(),
        "gif" => "image/gif".to_string(),
        _ => "image/png".to_string(),
    }
}

/// OpenAI 通道的请求头（Bearer 鉴权）；Gemini 通道用 `x-goog-api-key`。
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

/// 文件名里的时间戳：精确到毫秒。
///
/// 只到秒会撞名：连续两次生成（同一轮对话里文生图接图生图）常落在同一秒，
/// 后一次直接覆盖前一次的图片。
fn timestamp() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S_%3f").to_string()
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

    /// Gemini 通道：请求体形状与响应里的内联图片解码。
    #[test]
    fn gemini_channel_builds_the_generate_content_request() {
        let (options, _root) = workspace("gemini");
        let options = ImageGenOptions {
            provider: "gemini".to_string(),
            base_url: "https://generativelanguage.googleapis.com/v1beta/".to_string(),
            model: "gemini-2.5-flash-image".to_string(),
            aspect_ratio: "16:9".to_string(),
            image_size: "2K".to_string(),
            ..options
        };

        let body = gemini_request_body("一只猫", None, &options);
        let value: Value = serde_json::from_str(&body).expect("请求体是 JSON");
        assert_eq!(
            value["generationConfig"]["responseModalities"],
            json!(["IMAGE"])
        );
        let image_config = &value["generationConfig"]["imageConfig"];
        assert_eq!(image_config["aspectRatio"], json!("16:9"));
        assert_eq!(image_config["imageSize"], json!("2K"));
        assert_eq!(value["contents"][0]["role"], json!("user"));
        assert_eq!(value["contents"][0]["parts"][0]["text"], json!("一只猫"));

        // 参考图作为 inlineData 与文本同轮下发（有参考图即图生图）。
        let reference = ("image/jpeg".to_string(), vec![9u8, 8, 7]);
        let with_image = gemini_request_body("改成夜景", Some(&reference), &options);
        let value: Value = serde_json::from_str(&with_image).expect("请求体是 JSON");
        let inline = &value["contents"][0]["parts"][1]["inlineData"];
        assert_eq!(inline["mimeType"], "image/jpeg");
        assert_eq!(
            inline["data"],
            base64::engine::general_purpose::STANDARD.encode([9u8, 8, 7])
        );

        // 比例与档位留空时不下发 imageConfig。
        let bare = ImageGenOptions {
            aspect_ratio: String::new(),
            image_size: String::new(),
            ..options.clone()
        };
        let value: Value =
            serde_json::from_str(&gemini_request_body("x", None, &bare)).expect("请求体是 JSON");
        assert!(value["generationConfig"].get("imageConfig").is_none());
    }

    #[test]
    fn gemini_response_images_are_decoded_and_concatenated() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8, 2]);
        let body = json!({
            "candidates": [{ "content": { "parts": [
                {"inlineData": {"data": encoded}},
                {"inlineData": {"data": encoded}},
            ] } }],
        })
        .to_string();
        assert_eq!(gemini_image_bytes(&body).expect("解码"), vec![1, 2, 1, 2]);

        // 被安全策略拦下时带出响应原文。
        let error = gemini_image_bytes(r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#)
            .expect_err("没有图片应当报错");
        assert!(error.message.contains("没有图片数据"), "{}", error.message);
        assert!(error.message.contains("SAFETY"), "{}", error.message);
    }

    #[test]
    fn reference_mime_type_follows_the_extension() {
        assert_eq!(mime_type_for(Path::new("a.jpg")), "image/jpeg");
        assert_eq!(mime_type_for(Path::new("a.JPEG")), "image/jpeg");
        assert_eq!(mime_type_for(Path::new("a.webp")), "image/webp");
        assert_eq!(mime_type_for(Path::new("a.png")), "image/png");
        assert_eq!(mime_type_for(Path::new("a.bmp")), "image/png");
    }
}
