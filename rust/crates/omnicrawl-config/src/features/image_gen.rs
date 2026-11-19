//! 图像生成（OpenAI 兼容 Image API）配置（对应 `omnicrawl/config/features/image_gen.py`）。

use std::path::{Path, PathBuf};

use crate::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};
use crate::value::{python_str, truthy};

pub const DEFAULT_IMAGE_GEN_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_IMAGE_GEN_MODEL: &str = "gpt-image-2";

/// Gemini 通道的默认端点与模型（`provider = "gemini"` 且沿用默认值时自动折算）。
pub const DEFAULT_IMAGE_GEN_GEMINI_BASE_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_IMAGE_GEN_GEMINI_MODEL: &str = "gemini-2.5-flash-image";

/// xAI（Grok）通道的默认端点与模型。
pub const DEFAULT_IMAGE_GEN_GROK_BASE_URL: &str = "https://api.x.ai/v1";
pub const DEFAULT_IMAGE_GEN_GROK_MODEL: &str = "grok-imagine-image-2.0";

/// DALL·E 3 通道：端点是 OpenAI 的，模型固定为 `dall-e-3`。
pub const DEFAULT_IMAGE_GEN_DALLE3_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_IMAGE_GEN_DALLE3_MODEL: &str = "dall-e-3";

/// Midjourney 代理通道的默认端点（本机代理根地址，接口在其下的 `/mj` 路径）。
pub const DEFAULT_IMAGE_GEN_MJ_BASE_URL: &str = "http://127.0.0.1:8080";
pub const DEFAULT_IMAGE_GEN_MJ_MODEL: &str = "midjourney";

/// 可选出图服务商：OpenAI 兼容 Image API、Gemini 的 `generateContent`、
/// xAI 的 `/images/generations`、DALL·E 3 与 Midjourney 代理。
pub const IMAGE_GEN_PROVIDERS: [&str; 5] = ["openai", "gemini", "grok", "dall-e-3", "mj"];

/// 默认服务商（保持既有 `[image_gen]` 段的语义不变）。
pub const DEFAULT_IMAGE_GEN_PROVIDER: &str = "openai";

/// Gemini 的画面比例取值域（官方 14 种）。
pub const IMAGE_GEN_ASPECT_RATIOS: [&str; 14] = [
    "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9", "1:4", "4:1", "1:8",
    "8:1",
];

/// Gemini 的分辨率档位。
pub const IMAGE_GEN_IMAGE_SIZES: [&str; 3] = ["1K", "2K", "4K"];

const IMAGE_GEN_QUALITIES: [&str; 4] = ["auto", "low", "medium", "high"];
const IMAGE_GEN_FORMATS: [&str; 3] = ["png", "jpeg", "webp"];

/// DALL·E 3 的固定尺寸档；`quality` 只认 `standard` / `hd`（与 gpt-image 系列不同）。
pub const IMAGE_GEN_DALLE3_SIZES: [&str; 3] = ["1024x1024", "1792x1024", "1024x1792"];
const IMAGE_GEN_DALLE3_QUALITIES: [&str; 3] = ["auto", "standard", "hd"];

/// Grok 的分辨率档位（xAI 只支持 1k / 2k，下发时折成小写）。
const IMAGE_GEN_GROK_IMAGE_SIZES: [&str; 2] = ["1K", "2K"];

/// 图像生成服务的开关与接口参数。
///
/// 一个段承载两家：`provider` 决定这次出图走哪条通道，其余键按通道解释。
/// `size`/`quality` 是 OpenAI 的口径，`aspect_ratio`/`image_size` 是 Gemini 的口径，
/// 切换服务商不会丢掉另一家的取值。
#[derive(Debug, Clone, PartialEq)]
pub struct ImageGenConfiguration {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub size: String,
    pub quality: String,
    pub output_format: String,
    /// Gemini 的画面比例；空串表示不下发（由模型取默认）。
    pub aspect_ratio: String,
    /// Gemini 的分辨率档位（1K/2K/4K）；空串表示不下发。
    pub image_size: String,
    pub n: i64,
    pub timeout_seconds: i64,
}

impl Default for ImageGenConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: DEFAULT_IMAGE_GEN_PROVIDER.to_string(),
            base_url: DEFAULT_IMAGE_GEN_BASE_URL.to_string(),
            api_key: String::new(),
            model: DEFAULT_IMAGE_GEN_MODEL.to_string(),
            size: "auto".to_string(),
            quality: "auto".to_string(),
            output_format: "png".to_string(),
            aspect_ratio: String::new(),
            image_size: String::new(),
            n: 1,
            timeout_seconds: 120,
        }
    }
}

impl ImageGenConfiguration {
    /// Python `ImageGenConfiguration.__post_init__`：取值域校验与归一化。
    ///
    /// 校验口径随 `provider` 变：OpenAI 通道认 `size`/`quality` 与三种输出格式，
    /// Gemini 通道认 `aspect_ratio`/`image_size`，且只出 PNG。
    pub fn normalize(mut self) -> Result<Self, ConfigError> {
        let provider = self.provider.trim().to_lowercase();
        let provider = if provider.is_empty() {
            DEFAULT_IMAGE_GEN_PROVIDER.to_string()
        } else {
            provider
        };
        if !IMAGE_GEN_PROVIDERS.contains(&provider.as_str()) {
            return Err(ConfigError::new(format!(
                "image_gen.provider 仅支持 {}。",
                IMAGE_GEN_PROVIDERS.join("、")
            )));
        }
        // 切到 Gemini 却仍留着 OpenAI 的默认端点/模型时折算成 Gemini 的默认值：
        // 否则请求会打到 `api.openai.com/v1/models/gpt-image-2:generateContent`。
        // 用户显式填过的值一律不动。
        if provider == "gemini" {
            if is_stock_base_url(&self.base_url) {
                self.base_url = DEFAULT_IMAGE_GEN_GEMINI_BASE_URL.to_string();
            }
            if is_stock_model(&self.model) {
                self.model = DEFAULT_IMAGE_GEN_GEMINI_MODEL.to_string();
            }
        }
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(ConfigError::new("image_gen.base_url 不能为空。"));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(ConfigError::new(
                "image_gen.base_url 必须以 http:// 或 https:// 开头。",
            ));
        }
        let mut model = self.model.trim().to_string();
        if model.is_empty() {
            return Err(ConfigError::new("image_gen.model 不能为空。"));
        }
        // Grok / DALL·E 3 / MJ 各有自己的端点与模型：切过去时把「还是别家的出厂值」
        // 折算成本通道的，否则会拿着 `gpt-image-2` 打 Grok、或拿着 Gemini 的模型打 DALL·E 3。
        if provider == "grok" {
            if is_stock_base_url(&self.base_url) {
                self.base_url = DEFAULT_IMAGE_GEN_GROK_BASE_URL.to_string();
            }
            if is_stock_model(&model) {
                model = DEFAULT_IMAGE_GEN_GROK_MODEL.to_string();
            }
        }
        if provider == "dall-e-3" {
            if is_stock_base_url(&self.base_url) {
                self.base_url = DEFAULT_IMAGE_GEN_DALLE3_BASE_URL.to_string();
            }
            // DALL·E 3 的模型名是固定的：该通道下忽略别处填来的模型。
            model = DEFAULT_IMAGE_GEN_DALLE3_MODEL.to_string();
        }
        if provider == "mj" {
            if is_stock_base_url(&self.base_url) {
                self.base_url = DEFAULT_IMAGE_GEN_MJ_BASE_URL.to_string();
            }
            if is_stock_model(&model) {
                model = DEFAULT_IMAGE_GEN_MJ_MODEL.to_string();
            }
        }

        let aspect_ratio = self.aspect_ratio.trim().to_string();
        let mut image_size = self.image_size.trim().to_uppercase();
        if provider == "gemini" {
            if !aspect_ratio.is_empty() && !IMAGE_GEN_ASPECT_RATIOS.contains(&aspect_ratio.as_str())
            {
                return Err(ConfigError::new(format!(
                    "image_gen.aspect_ratio 仅支持 {}。",
                    IMAGE_GEN_ASPECT_RATIOS.join("、")
                )));
            }
            if !image_size.is_empty() && !IMAGE_GEN_IMAGE_SIZES.contains(&image_size.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.image_size 仅支持 {}。",
                    IMAGE_GEN_IMAGE_SIZES.join("、")
                )));
            }
            // `size` / `quality` / `output_format` 是 OpenAI 通道的口径：Gemini 不使用它们
            // （出图恒为 PNG、尺寸由 `aspect_ratio` + `image_size` 决定），因此原样保留、
            // 既不校验也不改写——切回 OpenAI 时那三个值还在。
        } else if provider == "grok" {
            // Grok：尺寸由 aspect_ratio 决定，`size` 不下发；`quality`/`output_format`
            // 沿用 OpenAI 的取值域，切回 openai 时无损。
            if !IMAGE_GEN_QUALITIES.contains(&self.quality.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.quality 仅支持 {}。",
                    IMAGE_GEN_QUALITIES.join("、")
                )));
            }
            if !IMAGE_GEN_FORMATS.contains(&self.output_format.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.output_format 仅支持 {}。",
                    IMAGE_GEN_FORMATS.join("、")
                )));
            }
            if !image_size.is_empty() && !IMAGE_GEN_GROK_IMAGE_SIZES.contains(&image_size.as_str())
            {
                // 从 Gemini 切来时可能是 4K：Grok 不支持，按默认（1K）回落，留空即不下发。
                image_size = String::new();
            }
        } else if provider == "dall-e-3" {
            // DALL·E 3：尺寸与质量都是固定枚举，不接受 auto。切换通道时草稿里可能还是
            // 别家的取值（auto / low / …），这里按通道语义回落，而不是把保存拦下来。
            if !IMAGE_GEN_DALLE3_SIZES.contains(&self.size.as_str()) {
                self.size = IMAGE_GEN_DALLE3_SIZES[0].to_string();
            }
            if !IMAGE_GEN_DALLE3_QUALITIES.contains(&self.quality.as_str()) {
                self.quality = "auto".to_string();
            }
        } else if provider == "mj" {
            // Midjourney：尺寸与质量由提示词里的 `--ar` 等参数表达，这里只管落盘格式。
            if !IMAGE_GEN_FORMATS.contains(&self.output_format.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.output_format 仅支持 {}。",
                    IMAGE_GEN_FORMATS.join("、")
                )));
            }
        } else {
            if self.size != "auto" && !is_size_shape(&self.size) {
                return Err(ConfigError::new(
                    "image_gen.size 必须是 auto 或 宽x高 形式（例如 1024x1024、1536x1024）。",
                ));
            }
            if !IMAGE_GEN_QUALITIES.contains(&self.quality.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.quality 仅支持 {}。",
                    IMAGE_GEN_QUALITIES.join("、")
                )));
            }
            if !IMAGE_GEN_FORMATS.contains(&self.output_format.as_str()) {
                return Err(ConfigError::new(format!(
                    "image_gen.output_format 仅支持 {}。",
                    IMAGE_GEN_FORMATS.join("、")
                )));
            }
        }
        if !(1..=10).contains(&self.n) {
            return Err(ConfigError::new("image_gen.n 必须是 1~10 的整数。"));
        }
        if !(1..=600).contains(&self.timeout_seconds) {
            return Err(ConfigError::new(
                "image_gen.timeout_seconds 必须是 1~600 的整数。",
            ));
        }
        self.provider = provider;
        self.base_url = base_url;
        self.model = model;
        self.aspect_ratio = aspect_ratio;
        self.image_size = image_size;
        self.api_key = self.api_key.trim().to_string();
        Ok(self)
    }

    /// 生效的 API Key：只认配置里的明文 `api_key`。
    pub fn resolve_api_key(&self, _env: &ConfigEnvironment) -> String {
        self.api_key.trim().to_string()
    }
}

/// 是否为任一家出厂端点（`/` 差异不计）：是则视为用户没填过、该按通道折算。
fn is_stock_base_url(text: &str) -> bool {
    let normalized = text.trim().trim_end_matches('/');
    [
        DEFAULT_IMAGE_GEN_BASE_URL,
        DEFAULT_IMAGE_GEN_GEMINI_BASE_URL,
        DEFAULT_IMAGE_GEN_GROK_BASE_URL,
        DEFAULT_IMAGE_GEN_MJ_BASE_URL,
    ]
    .iter()
    .any(|stock| stock.trim_end_matches('/') == normalized)
}

/// 是否为任一家出厂模型名。
fn is_stock_model(text: &str) -> bool {
    let normalized = text.trim();
    [
        DEFAULT_IMAGE_GEN_MODEL,
        DEFAULT_IMAGE_GEN_GEMINI_MODEL,
        DEFAULT_IMAGE_GEN_GROK_MODEL,
        DEFAULT_IMAGE_GEN_DALLE3_MODEL,
        DEFAULT_IMAGE_GEN_MJ_MODEL,
    ]
    .iter()
    .any(|stock| *stock == normalized)
}

/// 读取图像生成配置；缺少 `image_gen` 段时返回关闭的默认配置。
pub fn load_image_gen_configuration(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<ImageGenConfiguration, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = match data.get("image_gen") {
        None => Table::new(),
        Some(Value::String(text)) if text.is_empty() => Table::new(),
        Some(Value::Table(section)) => section.clone(),
        Some(_) => return Err(ConfigError::new("配置段 image_gen 必须是对象。")),
    };
    let defaults = ImageGenConfiguration::default();
    ImageGenConfiguration {
        enabled: bool_field(&section, "enabled", false),
        provider: text_field(&section, "provider", DEFAULT_IMAGE_GEN_PROVIDER),
        base_url: text_field(&section, "base_url", DEFAULT_IMAGE_GEN_BASE_URL),
        api_key: text_field(&section, "api_key", ""),
        model: text_field(&section, "model", DEFAULT_IMAGE_GEN_MODEL),
        size: text_field(&section, "size", "auto"),
        quality: text_field(&section, "quality", "auto"),
        output_format: text_field(&section, "output_format", "png"),
        aspect_ratio: text_field(&section, "aspect_ratio", ""),
        image_size: text_field(&section, "image_size", ""),
        n: int_field(&section, "n", defaults.n)?,
        timeout_seconds: int_field(&section, "timeout_seconds", defaults.timeout_seconds)?,
    }
    .normalize()
}

/// 保留其他配置段，只更新完整的图像生成配置。
///
/// 新增键只在**非默认值**时写回：`provider = "openai"` 与空的 Gemini 参数不落盘，
/// 这样纯 OpenAI 用户的写回文本与加这些键之前逐字相同。
pub fn save_image_gen_configuration(
    env: &ConfigEnvironment,
    configuration: &ImageGenConfiguration,
    config_path: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let configuration = configuration.clone().normalize()?;
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert("enabled".to_string(), Value::Boolean(configuration.enabled));
    if configuration.provider != DEFAULT_IMAGE_GEN_PROVIDER {
        section.insert(
            "provider".to_string(),
            Value::String(configuration.provider.clone()),
        );
    }
    section.insert(
        "base_url".to_string(),
        Value::String(configuration.base_url.clone()),
    );
    section.insert(
        "api_key".to_string(),
        Value::String(configuration.api_key.clone()),
    );
    section.insert(
        "model".to_string(),
        Value::String(configuration.model.clone()),
    );
    section.insert(
        "size".to_string(),
        Value::String(configuration.size.clone()),
    );
    section.insert(
        "quality".to_string(),
        Value::String(configuration.quality.clone()),
    );
    section.insert(
        "output_format".to_string(),
        Value::String(configuration.output_format.clone()),
    );
    if !configuration.aspect_ratio.is_empty() {
        section.insert(
            "aspect_ratio".to_string(),
            Value::String(configuration.aspect_ratio.clone()),
        );
    }
    if !configuration.image_size.is_empty() {
        section.insert(
            "image_size".to_string(),
            Value::String(configuration.image_size.clone()),
        );
    }
    section.insert("n".to_string(), Value::Integer(configuration.n));
    section.insert(
        "timeout_seconds".to_string(),
        Value::Integer(configuration.timeout_seconds),
    );
    data.insert("image_gen".to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// `^\d{2,4}x\d{2,4}$` 的等价匹配（ASCII 数字）。
fn is_size_shape(text: &str) -> bool {
    let Some((width, height)) = text.split_once('x') else {
        return false;
    };
    is_digit_run(width) && is_digit_run(height)
}

fn is_digit_run(text: &str) -> bool {
    let count = text.chars().count();
    (2..=4).contains(&count) && text.chars().all(|ch| ch.is_ascii_digit())
}

fn bool_field(section: &Table, name: &str, default: bool) -> bool {
    match section.get(name) {
        None => default,
        Some(value) => truthy(value),
    }
}

/// Python `str(section.get(name) or fallback)`。
fn text_field(section: &Table, name: &str, fallback: &str) -> String {
    let text = python_str(section.get(name));
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

/// Python `int(section.get(name) or default)`。
fn int_field(section: &Table, name: &str, default: i64) -> Result<i64, ConfigError> {
    let Some(value) = section.get(name) else {
        return Ok(default);
    };
    if !truthy(value) {
        return Ok(default);
    }
    match value {
        Value::Integer(number) => Ok(*number),
        Value::Boolean(flag) => Ok(i64::from(*flag)),
        Value::Float(number) => Ok(*number as i64),
        Value::String(text) => match text.trim().parse::<i64>() {
            Ok(number) => Ok(number),
            Err(_) => Err(numeric_error(text)),
        },
        _ => Err(numeric_error(&python_str(Some(value)))),
    }
}

fn numeric_error(text: &str) -> ConfigError {
    ConfigError::new(format!(
        "配置段 image_gen 数值字段无效：invalid literal for int() with base 10: '{text}'"
    ))
}
