//! 启动参数：内核路径、模型端点、会话根与审批模式。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::models::llm::load_llm_config;

/// 阶段一的默认系统提示词；给了 `--system-prompt` 或环境变量时以它们为准。
const DEFAULT_SYSTEM_PROMPT: &str = "你是 OmniCrawl 助手，回答保持简洁。";

// 审批模式归宿主执行层所有（批次定调要用），这里再导出给启动参数与界面用。
pub use omnicrawl_host::approval::ApprovalMode;

/// 一次启动的完整配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub kernel: PathBuf,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub system_prompt: String,
    pub session_root: Option<PathBuf>,
    pub context_window_tokens: Option<u64>,
    pub approval: ApprovalMode,
    /// 命令类工具的默认超时（秒），与 Python 侧 `command_timeout_seconds` 同义。
    pub command_timeout_seconds: i64,
    /// 单个工具执行的最长等待（秒），与 Python 侧 `tool_timeout_seconds` 同义；
    /// 超时按批次绝对截止时间计算，超时工具的结果被丢弃、回合继续推进。
    pub tool_timeout_seconds: i64,
    /// 模型原生支持视觉：开启后 `read_image` 的图片会作为观察注入模型请求。
    pub native_vision: bool,
    /// 图像生成（OpenAI 兼容 Image API）：与 Python 的 `image_gen` 配置段同义。
    pub image_gen: ImageGenArgs,
    /// 顾问策略：与 Python 的 `advisor` 配置段同义。
    pub advisor: AdvisorArgs,
}

/// 顾问配置：命令行与环境变量（Python 从 config.toml 读取）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdvisorArgs {
    pub enabled: bool,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub effort: String,
    pub disabled_for_models: Vec<String>,
}

/// 图像生成配置：命令行开关与环境变量（Python 从 config.toml 读取，差异见 crate README）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenArgs {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
}

impl Default for ImageGenArgs {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-image-2".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
        }
    }
}

/// 解析结果：正常配置，或用户要看的帮助文本。
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Run(Box<Options>),
    Help(String),
    Version(String),
}

pub const USAGE: &str = "\
用法：omnicrawl-tui [选项]

启动内核协议 v1 的宿主前端（全屏终端工作台）。内核默认取 $OMNICRAWL_BINARY，
否则用与本程序同目录的 omnicrawl，再退回 PATH。

选项：
  --kernel <路径>          内核可执行文件
  --model <名称>           模型名（默认 $OMNICRAWL_MODEL / $OPENAI_MODEL，
                           再退回 config.toml 里的当前模型）
  --base-url <地址>        模型接口基地址（默认 $OPENAI_BASE_URL）
  --api-key-env <变量名>   存放凭据的环境变量名（默认 OPENAI_API_KEY）
  --system-prompt <文本>   系统提示词
  --session-root <目录>    会话根目录；给了就让内核自己持有会话
  --context-window <N>     HUD 上下文占用条的分母（token）
  --approval <manual|auto> 工具审批模式（默认 manual）
  --command-timeout <秒>   命令类工具默认超时（默认 360）
  --tool-timeout <秒>      单个工具执行的最长等待（默认 600）
  --native-vision          模型原生支持视觉：把 read_image 的图片注入请求
  --image-gen              启用图像生成（默认 $OMNICRAWL_IMAGE_GEN_ENABLED）
  --image-gen-base-url <地址>   图像接口基地址（默认 $OMNICRAWL_IMAGE_GEN_BASE_URL）
  --image-gen-model <名称>      图像模型（默认 $OMNICRAWL_IMAGE_GEN_MODEL）
  --image-gen-api-key-env <变量> 图像 API Key 的环境变量名（默认 $OMNICRAWL_IMAGE_GEN_API_KEY_ENV）
  --advisor-model <名称>        顾问模型（给了即启用；默认 $OMNICRAWL_ADVISOR_MODEL）
  --advisor-base-url <地址>     顾问接口基地址（默认 $OMNICRAWL_ADVISOR_BASE_URL，再回落主模型）
  --advisor-api-key-env <变量>  顾问凭据的环境变量名（默认 $OMNICRAWL_ADVISOR_API_KEY_ENV，再回落主模型）
  --advisor-effort <强度>       顾问推理强度（默认 $OMNICRAWL_ADVISOR_EFFORT）
  --version, -V            打印版本
  --help, -h               打印本说明";

/// 按「命令行 → 环境变量 → 默认值」解析参数；`env` 便于测试注入。
pub fn parse(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    exe_dir: &Path,
) -> Result<Parsed, String> {
    parse_with(args, env, exe_dir, &configured_model)
}

/// [`parse`] 的实现。模型名的最后一级回退（读 config.toml）也作为参数注入：
/// 测试才能在不依赖开发机真实配置的前提下覆盖「配置里也没有模型」这一分支。
fn parse_with(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    exe_dir: &Path,
    configured: &dyn Fn() -> Result<String, String>,
) -> Result<Parsed, String> {
    let mut kernel: Option<PathBuf> = None;
    let mut model: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut api_key_env: Option<String> = None;
    let mut system_prompt: Option<String> = None;
    let mut session_root: Option<PathBuf> = None;
    let mut context_window: Option<u64> = None;
    let mut approval: Option<ApprovalMode> = None;
    let mut command_timeout: Option<i64> = None;
    let mut tool_timeout: Option<i64> = None;
    let mut native_vision = false;
    let mut image_gen_enabled = false;
    let mut image_gen_base_url: Option<String> = None;
    let mut image_gen_model: Option<String> = None;
    let mut image_gen_api_key_env: Option<String> = None;
    let mut advisor_model: Option<String> = None;
    let mut advisor_base_url: Option<String> = None;
    let mut advisor_api_key_env: Option<String> = None;
    let mut advisor_effort: Option<String> = None;

    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--help" | "-h" => return Ok(Parsed::Help(USAGE.to_string())),
            "--version" | "-V" => {
                return Ok(Parsed::Version(format!(
                    "omnicrawl-tui {}",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "--kernel" => kernel = Some(PathBuf::from(take_value(args, &mut index, flag)?)),
            "--model" => model = Some(take_value(args, &mut index, flag)?),
            "--base-url" => base_url = Some(take_value(args, &mut index, flag)?),
            "--api-key-env" => api_key_env = Some(take_value(args, &mut index, flag)?),
            "--system-prompt" => system_prompt = Some(take_value(args, &mut index, flag)?),
            "--session-root" => {
                let value = take_value(args, &mut index, flag)?;
                if !value.trim().is_empty() {
                    session_root = Some(PathBuf::from(value));
                }
            }
            "--context-window" => {
                let value = take_value(args, &mut index, flag)?;
                context_window = Some(
                    value
                        .trim()
                        .parse::<u64>()
                        .map_err(|_| format!("--context-window 需要正整数，收到：{value}"))?,
                );
            }
            "--approval" => {
                approval = Some(ApprovalMode::parse(&take_value(args, &mut index, flag)?)?)
            }
            "--command-timeout" => {
                let value = take_value(args, &mut index, flag)?;
                let parsed = value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("--command-timeout 需要正整数秒，收到：{value}"))?;
                command_timeout = Some(parsed.clamp(1, 360));
            }
            "--tool-timeout" => {
                let value = take_value(args, &mut index, flag)?;
                let parsed = value
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| format!("--tool-timeout 需要正整数秒，收到：{value}"))?;
                tool_timeout = Some(parsed.clamp(1, 3600));
            }
            "--native-vision" => native_vision = true,
            "--image-gen" => image_gen_enabled = true,
            "--image-gen-base-url" => {
                image_gen_base_url = Some(take_value(args, &mut index, flag)?);
            }
            "--image-gen-model" => {
                image_gen_model = Some(take_value(args, &mut index, flag)?);
            }
            "--image-gen-api-key-env" => {
                image_gen_api_key_env = Some(take_value(args, &mut index, flag)?);
            }
            "--advisor-model" => {
                advisor_model = Some(take_value(args, &mut index, flag)?);
            }
            "--advisor-base-url" => {
                advisor_base_url = Some(take_value(args, &mut index, flag)?);
            }
            "--advisor-api-key-env" => {
                advisor_api_key_env = Some(take_value(args, &mut index, flag)?);
            }
            "--advisor-effort" => {
                advisor_effort = Some(take_value(args, &mut index, flag)?);
            }
            other => return Err(format!("未知参数：{other}\n\n{USAGE}")),
        }
        index += 1;
    }

    // 模型名来源顺序：`--model` → `OMNICRAWL_MODEL` → `OPENAI_MODEL` → config.toml 的当前模型。
    // 最后一段是对齐 Python 的关键（`config/models/llm.py` 的
    // `model=_read_required_config_text(llm_section, "model", "OPENAI_MODEL")`：
    // 模型是配置项，环境变量只是回退）；Rust 侧原来只认前三级，配置里明明配好了
    // `[llm.active_model]` 也必须再敲一遍 `--model` 才能启动。
    let model = match model
        .or_else(|| non_empty(env("OMNICRAWL_MODEL")))
        .or_else(|| non_empty(env("OPENAI_MODEL")))
    {
        Some(model) => model,
        None => configured().map_err(|error| {
            format!(
                "--model 未给出，环境变量 OMNICRAWL_MODEL / OPENAI_MODEL 也是空的，\
且从 config.toml 取模型失败：{error}\
可在 config.toml 中配置模型，或设置 OPENAI_MODEL 切换。"
            )
        })?,
    };

    let disabled_for_models: Vec<String> = non_empty(env("OMNICRAWL_ADVISOR_DISABLED_FOR_MODELS"))
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let advisor_model = advisor_model
        .or_else(|| non_empty(env("OMNICRAWL_ADVISOR_MODEL")))
        .unwrap_or_default();

    Ok(Parsed::Run(Box::new(Options {
        kernel: kernel.unwrap_or_else(|| resolve_kernel_path(env, exe_dir)),
        model,
        base_url: base_url
            .or_else(|| non_empty(env("OPENAI_BASE_URL")))
            .unwrap_or_default(),
        api_key_env: api_key_env
            .or_else(|| non_empty(env("OMNICRAWL_API_KEY_ENV")))
            .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        system_prompt: system_prompt
            .or_else(|| non_empty(env("OMNICRAWL_SYSTEM_PROMPT")))
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        session_root: session_root
            .or_else(|| non_empty(env("OMNICRAWL_SESSION_ROOT")).map(PathBuf::from)),
        context_window_tokens: context_window,
        approval: approval.unwrap_or(ApprovalMode::Manual),
        command_timeout_seconds: command_timeout.unwrap_or(360),
        tool_timeout_seconds: tool_timeout
            .or_else(|| {
                non_empty(env("AGENT_TOOL_TIMEOUT_SECONDS"))
                    .and_then(|value| value.trim().parse::<i64>().ok())
                    .map(|value| value.clamp(1, 3600))
            })
            .unwrap_or(600),
        native_vision: native_vision
            || non_empty(env("OMNICRAWL_NATIVE_VISION"))
                .map(|value| {
                    matches!(
                        value.trim().to_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )
                })
                .unwrap_or(false),
        image_gen: ImageGenArgs {
            enabled: image_gen_enabled
                || non_empty(env("OMNICRAWL_IMAGE_GEN_ENABLED"))
                    .map(|value| {
                        matches!(
                            value.trim().to_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    })
                    .unwrap_or(false),
            base_url: image_gen_base_url
                .or_else(|| non_empty(env("OMNICRAWL_IMAGE_GEN_BASE_URL")))
                .unwrap_or_else(|| ImageGenArgs::default().base_url),
            model: image_gen_model
                .or_else(|| non_empty(env("OMNICRAWL_IMAGE_GEN_MODEL")))
                .unwrap_or_else(|| ImageGenArgs::default().model),
            api_key_env: image_gen_api_key_env
                .or_else(|| non_empty(env("OMNICRAWL_IMAGE_GEN_API_KEY_ENV")))
                .unwrap_or_else(|| ImageGenArgs::default().api_key_env),
        },
        advisor: AdvisorArgs {
            enabled: !advisor_model.trim().is_empty(),
            model: advisor_model,
            base_url: advisor_base_url
                .or_else(|| non_empty(env("OMNICRAWL_ADVISOR_BASE_URL")))
                .unwrap_or_default(),
            api_key_env: advisor_api_key_env
                .or_else(|| non_empty(env("OMNICRAWL_ADVISOR_API_KEY_ENV")))
                .unwrap_or_default(),
            effort: advisor_effort
                .or_else(|| non_empty(env("OMNICRAWL_ADVISOR_EFFORT")))
                .unwrap_or_default(),
            disabled_for_models,
        },
    })))
}

/// 内核可执行文件：显式环境变量优先，其次与本程序同目录的 `omnicrawl`，最后交给 PATH。
fn resolve_kernel_path(env: &dyn Fn(&str) -> Option<String>, exe_dir: &Path) -> PathBuf {
    if let Some(explicit) = non_empty(env("OMNICRAWL_BINARY")) {
        return PathBuf::from(explicit);
    }
    let suffix = std::env::consts::EXE_SUFFIX;
    let sibling = exe_dir.join(format!("omnicrawl{suffix}"));
    if sibling.is_file() {
        return sibling;
    }
    PathBuf::from(format!("omnicrawl{suffix}"))
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} 缺少取值"))
}

/// 从配置里取当前模型名；只负责说清「为什么取不到」，面向用户的引导语由
/// [`parse_with`] 统一追加（这样三处都缺模型时错误里一定同时点到环境变量与配置）。
///
/// 走的就是界面与内核共用的那条配置链（`load_llm_config` 内部按
/// `[llm.active_model]` / `[llm] model` / models.toml 解析），因此
/// 「TUI 里选中的模型」与「启动时用的模型」不会出现两套口径。
fn configured_model() -> Result<String, String> {
    let environment = ConfigEnvironment::from_process();
    let config = load_llm_config(&environment).map_err(|error| error.to_string())?;
    let model = config.model.trim().to_string();
    if model.is_empty() {
        return Err("config.toml 里没有可用的模型".to_string());
    }
    Ok(model)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn native_vision_env(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |key: &str| {
            if key == "OMNICRAWL_NATIVE_VISION" {
                Some(value.to_string())
            } else {
                None
            }
        }
    }

    #[test]
    fn native_vision_comes_from_flag_or_environment() {
        assert!(!options(&["--model", "m"], &no_env).native_vision);
        assert!(options(&["--model", "m", "--native-vision"], &no_env).native_vision);
        assert!(options(&["--model", "m"], &native_vision_env("yes")).native_vision);
        assert!(options(&["--model", "m"], &native_vision_env("TRUE")).native_vision);
        assert!(!options(&["--model", "m"], &native_vision_env("0")).native_vision);
        assert!(!options(&["--model", "m"], &native_vision_env("off")).native_vision);
    }

    #[test]
    fn image_gen_comes_from_flags_and_environment() {
        let defaults = options(&["--model", "m"], &no_env).image_gen;
        assert!(!defaults.enabled);
        assert_eq!(defaults.base_url, "https://api.openai.com/v1");
        assert_eq!(defaults.model, "gpt-image-2");
        assert_eq!(defaults.api_key_env, "OPENAI_API_KEY");

        assert!(
            options(&["--model", "m", "--image-gen"], &no_env)
                .image_gen
                .enabled
        );

        let env = |key: &str| match key {
            "OMNICRAWL_IMAGE_GEN_ENABLED" => Some("yes".to_string()),
            "OMNICRAWL_IMAGE_GEN_BASE_URL" => Some("https://relay.example/v1".to_string()),
            "OMNICRAWL_IMAGE_GEN_MODEL" => Some("gpt-image-9".to_string()),
            "OMNICRAWL_IMAGE_GEN_API_KEY_ENV" => Some("MY_IMAGE_KEY".to_string()),
            _ => None,
        };
        let parsed = options(&["--model", "m"], &env).image_gen;
        assert!(parsed.enabled);
        assert_eq!(parsed.base_url, "https://relay.example/v1");
        assert_eq!(parsed.model, "gpt-image-9");
        assert_eq!(parsed.api_key_env, "MY_IMAGE_KEY");

        let cli = options(
            &[
                "--model",
                "m",
                "--image-gen-base-url",
                "https://cli.example/v1",
                "--image-gen-model",
                "cli-model",
                "--image-gen-api-key-env",
                "CLI_KEY",
            ],
            &env,
        )
        .image_gen;
        assert_eq!(cli.base_url, "https://cli.example/v1");
        assert_eq!(cli.model, "cli-model");
        assert_eq!(cli.api_key_env, "CLI_KEY");
    }

    /// 测试默认的模型回退：配置里没有模型（真实配置属于开发机状态，不能进断言）。
    fn no_configured_model() -> Result<String, String> {
        Err("测试：config.toml 里没有模型。".to_string())
    }

    fn options(args: &[&str], env: &dyn Fn(&str) -> Option<String>) -> Options {
        let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        match parse_with(&args, env, Path::new("C:/tools"), &no_configured_model)
            .expect("参数应能解析")
        {
            Parsed::Run(options) => *options,
            other => panic!("期望运行配置，拿到 {other:?}"),
        }
    }

    #[test]
    fn command_line_wins_over_environment() {
        let env = |name: &str| match name {
            "OMNICRAWL_MODEL" => Some("env-model".to_string()),
            "OPENAI_BASE_URL" => Some("https://env.example/v1".to_string()),
            _ => None,
        };
        let options = options(
            &[
                "--model",
                "cli-model",
                "--base-url",
                "https://cli.example/v1",
            ],
            &env,
        );
        assert_eq!(options.model, "cli-model");
        assert_eq!(options.base_url, "https://cli.example/v1");
        assert_eq!(options.api_key_env, "OPENAI_API_KEY");
        assert_eq!(options.approval, ApprovalMode::Manual);
        assert_eq!(options.command_timeout_seconds, 360);
        assert_eq!(options.tool_timeout_seconds, 600);
        assert!(options.session_root.is_none());
    }

    #[test]
    fn environment_supplies_model_and_session() {
        let env = |name: &str| match name {
            "OPENAI_MODEL" => Some("env-model".to_string()),
            "OMNICRAWL_SESSION_ROOT" => Some("C:/sessions".to_string()),
            _ => None,
        };
        let options = options(&[], &env);
        assert_eq!(options.model, "env-model");
        assert_eq!(
            options.session_root.as_deref(),
            Some(Path::new("C:/sessions"))
        );
        assert_eq!(options.context_window_tokens, None);
    }

    /// 命令行、环境变量、配置三处都没有模型时才是错误（Python 同口径）。
    #[test]
    fn missing_model_everywhere_is_an_error() {
        let error = parse_with(
            &[],
            &no_env,
            Path::new("C:/tools"),
            &no_configured_model,
        )
        .expect_err("三处都没有模型名应报错");
        assert!(
            error.contains("OMNICRAWL_MODEL"),
            "错误应指出可用的环境变量：{error}"
        );
        assert!(error.contains("config.toml"), "错误应指出可改配置：{error}");
    }

    /// 命令行与环境变量都没有时，模型名从 config.toml 的当前模型来（对齐 Python）。
    #[test]
    fn config_supplies_model_when_no_flag_or_env() {
        let configured = || Ok("config-model".to_string());
        match parse_with(&[], &no_env, Path::new("C:/tools"), &configured)
            .expect("配置里有模型就应该能启动")
        {
            Parsed::Run(options) => assert_eq!(options.model, "config-model"),
            other => panic!("期望运行配置，拿到 {other:?}"),
        }
    }

    #[test]
    fn kernel_defaults_to_sibling_binary_then_path() {
        // 同目录没有 omnicrawl 时退回 PATH 上的名字（带平台后缀）。
        let options = options(&["--model", "m"], &no_env);
        let name = options
            .kernel
            .file_name()
            .expect("应有文件名")
            .to_string_lossy();
        assert!(name.starts_with("omnicrawl"), "实际：{name}");
    }

    #[test]
    fn explicit_kernel_and_approval_are_honoured() {
        let options = options(
            &[
                "--model",
                "m",
                "--kernel",
                "D:/k/omnicrawl.exe",
                "--approval",
                "auto",
            ],
            &no_env,
        );
        assert_eq!(options.kernel, PathBuf::from("D:/k/omnicrawl.exe"));
        assert_eq!(options.approval, ApprovalMode::Auto);
        assert_eq!(options.approval.label(), "AUTO");
    }

    #[test]
    fn command_timeout_is_clamped_to_the_python_range() {
        let clamped = options(&["--model", "m", "--command-timeout", "9999"], &no_env);
        assert_eq!(clamped.command_timeout_seconds, 360);
        let custom = options(&["--model", "m", "--command-timeout", "30"], &no_env);
        assert_eq!(custom.command_timeout_seconds, 30);
        let error = options_err(&["--model", "m", "--command-timeout", "abc"], &no_env);
        assert!(error.contains("--command-timeout"), "{error}");
    }

    #[test]
    fn tool_timeout_reads_flag_and_environment() {
        let custom = options(&["--model", "m", "--tool-timeout", "30"], &no_env);
        assert_eq!(custom.tool_timeout_seconds, 30);
        let clamped = options(&["--model", "m", "--tool-timeout", "9999"], &no_env);
        assert_eq!(clamped.tool_timeout_seconds, 3600, "上限与 Python 一致");

        let env = |name: &str| match name {
            "OPENAI_MODEL" => Some("m".to_string()),
            "AGENT_TOOL_TIMEOUT_SECONDS" => Some("120".to_string()),
            _ => None,
        };
        assert_eq!(options(&[], &env).tool_timeout_seconds, 120, "环境变量兜底");
        let flag_wins = options(&["--tool-timeout", "45"], &env);
        assert_eq!(flag_wins.tool_timeout_seconds, 45, "命令行优先于环境变量");

        let error = options_err(&["--model", "m", "--tool-timeout", "abc"], &no_env);
        assert!(error.contains("--tool-timeout"), "{error}");
    }

    #[test]
    fn invalid_approval_is_rejected() {
        let error = options_err(&["--model", "m", "--approval", "sometimes"], &no_env);
        // 文案与 `ApprovalMode::parse` 的三种模式（manual / review / auto）保持一致。
        assert!(error.contains("manual / review / auto"), "{error}");
    }

    fn options_err(args: &[&str], env: &dyn Fn(&str) -> Option<String>) -> String {
        let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        parse(&args, env, Path::new("C:/tools")).expect_err("应当解析失败")
    }

    #[test]
    fn help_and_version_short_circuit() {
        let help = parse(&["--help".to_string()], &no_env, Path::new(".")).expect("帮助应可解析");
        assert!(matches!(help, Parsed::Help(text) if text.contains("--kernel")));
        let version = parse(&["-V".to_string()], &no_env, Path::new(".")).expect("版本应可解析");
        assert!(matches!(version, Parsed::Version(text) if text.starts_with("omnicrawl-tui ")));
    }
}
