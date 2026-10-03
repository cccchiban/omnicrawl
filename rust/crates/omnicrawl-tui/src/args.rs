//! 启动参数：内核路径、模型端点、会话根与审批模式。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::features::approval::load_approval_mode;
use omnicrawl_config::models::llm::load_llm_config;

// 审批模式归宿主执行层所有（批次定调要用），这里再导出给启动参数与界面用。
pub use omnicrawl_host::approval::ApprovalMode;

/// 一次启动的完整配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub kernel: PathBuf,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    /// system prompt 的**显式覆盖**：只有 `--system-prompt` 给了才有值。
    /// `None` 表示没给，提示词装配走 `rust/assets/templates/system_prompt.md` 模板——
    /// 这里不能回落成默认文案，否则模板会被一句话顶掉（曾经的 bug）。
    pub system_prompt: Option<String>,
    pub session_root: Option<PathBuf>,
    pub context_window_tokens: Option<u64>,
    pub approval: ApprovalMode,
    /// 命令类工具的默认超时（秒），与 Python 侧 `command_timeout_seconds` 同义。
    pub command_timeout_seconds: i64,
    /// 单个工具执行的最长等待（秒），与 Python 侧 `tool_timeout_seconds` 同义；
    /// 超时按批次绝对截止时间计算，超时工具的结果被丢弃、回合继续推进。
    pub tool_timeout_seconds: i64,
    /// 模型原生支持视觉：为真时 `read_image` 的图片作为视觉附件注入模型请求。
    /// 未给该开关时按配置里当前模型的 `native_vision` 决定（见 `app.rs` 的
    /// `effective_native_vision`）。
    pub native_vision: bool,
    /// 图像生成（OpenAI 兼容 Image API）：与 Python 的 `image_gen` 配置段同义。
    pub image_gen: ImageGenArgs,
    /// 顾问策略：与 Python 的 `advisor` 配置段同义。
    pub advisor: AdvisorArgs,
}

/// 顾问配置：命令行只承载显式覆盖，其余字段留给配置文件的 `[advisor]` 段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdvisorArgs {
    pub enabled: bool,
    pub model: String,
    pub base_url: String,
    pub effort: String,
    pub disabled_for_models: Vec<String>,
}

/// 图像生成配置：命令行只承载**显式覆盖**，未给的字段留给配置文件的 `[image_gen]` 段。
///
/// 三个 `Option` 是为了区分「用户明确指定」与「没指定」：`None` 时由宿主回落到配置文件，
/// 否则配置里配好的接口地址/模型永远会被命令行默认值顶掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageGenArgs {
    pub enabled: Option<bool>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
}

impl Default for ImageGenArgs {
    fn default() -> Self {
        Self {
            enabled: None,
            base_url: None,
            model: None,
            api_key_env: None,
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

启动内核协议 v1 的宿主前端（全屏终端工作台）。内核路径只用 `--kernel`、同目录的
omnicrawl 或 PATH。

选项：
  --kernel <路径>          内核可执行文件
  --model <名称>           模型名（未给时读 config.toml 里的当前模型）
  --base-url <地址>        模型接口基地址（未给时读 config.toml）
  --api-key-env <变量名>   存放凭据的环境变量名（未给时读 config.toml）
  --system-prompt <文本>   系统提示词；给了就整段替换内置模板
  --session-root <目录>    会话根目录；给了就让内核自己持有会话
  --context-window <N>     HUD 上下文占用条的分母（token）
  --approval <manual|review|auto>
                           工具审批模式（默认读 config.toml 的 [approval] mode，没配时为 review）
  --command-timeout <秒>   命令类工具默认超时（默认 360）
  --tool-timeout <秒>      单个工具执行的最长等待（默认 600）
  --native-vision          模型原生支持视觉：把 read_image 的图片注入请求
                           （未给时按配置里当前模型的 native_vision）
  --image-gen              启用图像生成（未给时读 config.toml 的 [image_gen]）
  --image-gen-base-url <地址>   图像接口基地址（未给时读 config.toml）
  --image-gen-model <名称>      图像模型（未给时读 config.toml）
  --image-gen-api-key-env <变量> 图像 API Key 的环境变量名（未给时读 config.toml）
  --advisor-model <名称>        顾问模型（给了即启用；未给时读 config.toml）
  --advisor-base-url <地址>     顾问接口基地址（未给时读 config.toml，再回落主模型）
  --advisor-effort <强度>       顾问推理强度（未给时读 config.toml）
  --version, -V            打印版本
  --help, -h               打印本说明";

/// 按「命令行 → 配置文件 → 默认值」解析参数。
pub fn parse(args: &[String], exe_dir: &Path) -> Result<Parsed, String> {
    parse_with(args, exe_dir, &configured_model, &configured_approval)
}

/// [`parse`] 的实现。最后一级回退（读 config.toml）也作为参数注入：
/// 测试才能在不依赖开发机真实配置的前提下覆盖「配置里也没有/取不到」这两个分支。
fn parse_with(
    args: &[String],
    exe_dir: &Path,
    configured_model: &dyn Fn() -> Result<String, String>,
    configured_approval: &dyn Fn() -> Result<ApprovalMode, String>,
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
    // 图像生成的启用开关：三态，`None` 表示命令行与环境变量都没有给，交给配置文件。
    let mut image_gen_switch: Option<bool> = None;
    let mut image_gen_base_url: Option<String> = None;
    let mut image_gen_model: Option<String> = None;
    let mut image_gen_api_key_env: Option<String> = None;
    let mut advisor_model: Option<String> = None;
    let mut advisor_base_url: Option<String> = None;
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
            "--image-gen" => image_gen_switch = Some(true),
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
            "--advisor-effort" => {
                advisor_effort = Some(take_value(args, &mut index, flag)?);
            }
            other => return Err(format!("未知参数：{other}\n\n{USAGE}")),
        }
        index += 1;
    }

    // 模型名来源顺序：`--model` → config.toml 的当前模型。
    let model = match model {
        Some(model) => model,
        None => configured_model().map_err(|error| {
            format!(
                "--model 未给出，且从 config.toml 取模型失败：{error}可在 config.toml 中配置模型，或用 --model 指定。"
            )
        })?,
    };

    let advisor_model = advisor_model.unwrap_or_default();

    Ok(Parsed::Run(Box::new(Options {
        kernel: kernel.unwrap_or_else(|| resolve_kernel_path(exe_dir)),
        model,
        base_url: base_url.unwrap_or_default(),
        api_key_env: api_key_env.unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
        system_prompt,
        session_root,
        context_window_tokens: context_window,
        // 审批模式对映 Python `load_approval_mode()`：命令行给 `--approval` 时以命令行
        // 为准，否则读 config.toml（`[approval] mode`，缺失时与 Python 一致回落「自动
        // 审查」）。配置读不出来/取值非法时直接报错，不静默降级：审批策略静默变化
        // 比启动失败危险得多（Python 侧同样是启动即抛）。
        approval: match approval {
            Some(mode) => mode,
            None => configured_approval().map_err(|error| {
                format!(
                    "从 config.toml 读取审批模式失败：{error}
可用 --approval <manual|review|auto> 覆盖。"
                )
            })?,
        },
        command_timeout_seconds: command_timeout.unwrap_or(360),
        tool_timeout_seconds: tool_timeout.unwrap_or(600),
        native_vision,
        image_gen: ImageGenArgs {
            // 命令行没给时留 `None`，由宿主读 `[image_gen]` 段补齐。
            enabled: image_gen_switch,
            base_url: image_gen_base_url,
            model: image_gen_model,
            api_key_env: image_gen_api_key_env,
        },
        advisor: AdvisorArgs {
            // `--advisor-model` 是唯一开关：没给就让宿主按配置决定。
            enabled: !advisor_model.trim().is_empty(),
            model: advisor_model,
            base_url: advisor_base_url.unwrap_or_default(),
            effort: advisor_effort.unwrap_or_default(),
            disabled_for_models: Vec::new(),
        },
    })))
}

/// 内核可执行文件：与本程序同目录的 `omnicrawl`，找不到就交给 PATH。
fn resolve_kernel_path(exe_dir: &Path) -> PathBuf {
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
/// [`parse_with`] 统一追加。
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

/// 从配置里取审批模式（`[approval] mode`；没配时与 Python 一样是「自动审查」）。
///
/// 解析（别名、默认值、报错文案）全在 `omnicrawl-config` 的 `load_approval_mode` 里，
/// 与 Python `omnicrawl/config/features/approval.py` 逐字对映，这里只做枚举转换。
fn configured_approval() -> Result<ApprovalMode, String> {
    let environment = ConfigEnvironment::from_process();
    let mode = load_approval_mode(&environment, None).map_err(|error| error.to_string())?;
    ApprovalMode::parse(&mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_comes_from_flag_only() {
        assert_eq!(options(&["--model", "m"]).base_url, "");
        assert_eq!(
            options(&["--model", "m", "--base-url", "https://cli.example/v1"]).base_url,
            "https://cli.example/v1"
        );
        let parsed = options(&["--model", "m"]);
        assert_eq!(parsed.api_key_env, "OPENAI_API_KEY");
        // 没给 `--approval` 时取配置兜底（上文的注入值），不再是硬编码的 manual。
        assert_eq!(parsed.approval, ApprovalMode::Review);
        assert_eq!(parsed.command_timeout_seconds, 360);
        assert_eq!(parsed.tool_timeout_seconds, 600);
        assert!(parsed.session_root.is_none());
    }

    #[test]
    fn image_gen_comes_from_flags_only() {
        let defaults = options(&["--model", "m"]).image_gen;
        assert_eq!(defaults.enabled, None, "命令行没给时留给配置文件");
        assert_eq!(defaults.base_url, None);
        assert_eq!(defaults.model, None);
        assert_eq!(defaults.api_key_env, None);

        assert_eq!(
            options(&["--model", "m", "--image-gen"]).image_gen.enabled,
            Some(true)
        );

        let cli = options(&[
            "--model",
            "m",
            "--image-gen-base-url",
            "https://cli.example/v1",
            "--image-gen-model",
            "cli-model",
            "--image-gen-api-key-env",
            "CLI_KEY",
        ])
        .image_gen;
        assert_eq!(cli.base_url.as_deref(), Some("https://cli.example/v1"));
        assert_eq!(cli.model.as_deref(), Some("cli-model"));
        assert_eq!(cli.api_key_env.as_deref(), Some("CLI_KEY"));
    }

    /// 测试默认的模型回退：配置里没有模型（真实配置属于开发机状态，不能进断言）。
    fn no_configured_model() -> Result<String, String> {
        Err("测试：config.toml 里没有模型。".to_string())
    }

    /// 测试默认的审批模式回退。真实链路里 `[approval]` 段缺失时是「自动审查」
    /// （见 `omnicrawl-config` 的 `load_approval_mode`），这里同样用 review 代表
    /// 「配置给不出更具体的信息」的那一档。
    fn no_configured_approval() -> Result<ApprovalMode, String> {
        Ok(ApprovalMode::Review)
    }

    fn options(args: &[&str]) -> Options {
        options_with_approval(args, &no_configured_approval)
    }

    /// 注入「config.toml 里写的审批模式」的版本（真实配置文件不能进断言）。
    fn options_with_approval(
        args: &[&str],
        approval: &dyn Fn() -> Result<ApprovalMode, String>,
    ) -> Options {
        let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        match parse_with(&args, Path::new("C:/tools"), &no_configured_model, approval)
            .expect("参数应能解析")
        {
            Parsed::Run(options) => *options,
            other => panic!("期望运行配置，拿到 {other:?}"),
        }
    }

    /// 命令行与配置都没有模型时才是错误。
    #[test]
    fn missing_model_everywhere_is_an_error() {
        let error = parse_with(
            &[],
            Path::new("C:/tools"),
            &no_configured_model,
            &no_configured_approval,
        )
        .expect_err("两处都没有模型名应报错");
        assert!(error.contains("config.toml"), "错误应指出可改配置：{error}");
    }

    /// 命令行没给时，模型名从 config.toml 的当前模型来。
    #[test]
    fn config_supplies_model_when_no_flag() {
        let configured = || Ok("config-model".to_string());
        match parse_with(
            &[],
            Path::new("C:/tools"),
            &configured,
            &no_configured_approval,
        )
        .expect("配置里有模型就应该能启动")
        {
            Parsed::Run(options) => assert_eq!(options.model, "config-model"),
            other => panic!("期望运行配置，拿到 {other:?}"),
        }
    }

    #[test]
    fn kernel_defaults_to_sibling_binary_then_path() {
        // 同目录没有 omnicrawl 时退回 PATH 上的名字（带平台后缀）。
        let parsed = options(&["--model", "m"]);
        let name = parsed
            .kernel
            .file_name()
            .expect("应有文件名")
            .to_string_lossy();
        assert!(name.starts_with("omnicrawl"), "实际：{name}");
    }

    #[test]
    fn explicit_kernel_and_approval_are_honoured() {
        let parsed = options_with_approval(
            &[
                "--model",
                "m",
                "--kernel",
                "D:/k/omnicrawl.exe",
                "--approval",
                "auto",
            ],
            // 配置写的是 review，命令行更具体，应以前者为准。
            &|| Ok(ApprovalMode::Review),
        );
        assert_eq!(parsed.kernel, PathBuf::from("D:/k/omnicrawl.exe"));
        assert_eq!(parsed.approval, ApprovalMode::Auto);
        assert_eq!(parsed.approval.label(), "AUTO");
    }

    #[test]
    fn config_supplies_approval_mode_when_no_flag() {
        let parsed = options_with_approval(&["--model", "m"], &|| Ok(ApprovalMode::Manual));
        assert_eq!(parsed.approval, ApprovalMode::Manual);
        assert_eq!(parsed.approval.label(), "MAN");
    }

    #[test]
    fn unreadable_approval_config_is_an_error() {
        let args: Vec<String> = ["--model", "m"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let error = parse_with(
            &args,
            Path::new("C:/tools"),
            &no_configured_model,
            &|| Err("approval.mode 仅支持 manual, auto, review，当前值：x。".to_string()),
        )
        .expect_err("审批模式读不出来时应当报错，不静默降级");
        assert!(error.contains("审批模式"), "实际：{error}");
        assert!(error.contains("approval.mode"), "实际：{error}");
        assert!(error.contains("--approval"), "实际：{error}");
    }

    #[test]
    fn command_timeout_is_clamped_to_the_python_range() {
        let clamped = options(&["--model", "m", "--command-timeout", "9999"]);
        assert_eq!(clamped.command_timeout_seconds, 360);
        let custom = options(&["--model", "m", "--command-timeout", "30"]);
        assert_eq!(custom.command_timeout_seconds, 30);
        let error = options_err(&["--model", "m", "--command-timeout", "abc"]);
        assert!(error.contains("--command-timeout"), "{error}");
    }

    #[test]
    fn tool_timeout_is_clamped_to_the_python_range() {
        let custom = options(&["--model", "m", "--tool-timeout", "30"]);
        assert_eq!(custom.tool_timeout_seconds, 30);
        let clamped = options(&["--model", "m", "--tool-timeout", "9999"]);
        assert_eq!(clamped.tool_timeout_seconds, 3600, "上限与 Python 一致");

        let default = options(&["--model", "m"]);
        assert_eq!(default.tool_timeout_seconds, 600, "没给时用默认值");

        let error = options_err(&["--model", "m", "--tool-timeout", "abc"]);
        assert!(error.contains("--tool-timeout"), "{error}");
    }

    #[test]
    fn invalid_approval_is_rejected() {
        let error = options_err(&["--model", "m", "--approval", "sometimes"]);
        // 文案与 `ApprovalMode::parse` 的三种模式（manual / review / auto）保持一致。
        assert!(error.contains("manual / review / auto"), "{error}");
    }

    fn options_err(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|value| value.to_string()).collect();
        parse(&args, Path::new("C:/tools")).expect_err("应当解析失败")
    }

    #[test]
    fn help_and_version_short_circuit() {
        let help = parse(&["--help".to_string()], Path::new(".")).expect("帮助应可解析");
        assert!(matches!(help, Parsed::Help(text) if text.contains("--kernel")));
        let version = parse(&["-V".to_string()], Path::new(".")).expect("版本应可解析");
        assert!(matches!(version, Parsed::Version(text) if text.starts_with("omnicrawl-tui ")));
    }

    #[test]
    fn native_vision_comes_from_flag_only() {
        assert!(!options(&["--model", "m"]).native_vision);
        assert!(options(&["--model", "m", "--native-vision"]).native_vision);
    }

    #[test]
    fn advisor_and_session_root_come_from_flags_only() {
        let parsed = options(&["--model", "m"]);
        assert!(!parsed.advisor.enabled, "没给 --advisor-model 时不启用");
        assert!(parsed.advisor.model.is_empty());

        let with_advisor = options(&[
            "--model",
            "m",
            "--advisor-model",
            "gpt-5",
            "--advisor-base-url",
            "https://cli.example/v1",
            "--advisor-effort",
            "high",
        ]);
        assert!(with_advisor.advisor.enabled);
        assert_eq!(with_advisor.advisor.model, "gpt-5");
        assert_eq!(with_advisor.advisor.base_url, "https://cli.example/v1");
        assert_eq!(with_advisor.advisor.effort, "high");

        assert!(options(&["--model", "m"]).session_root.is_none());
        assert_eq!(
            options(&["--model", "m", "--session-root", "C:/sessions"])
                .session_root
                .as_deref(),
            Some(Path::new("C:/sessions"))
        );
    }
}
