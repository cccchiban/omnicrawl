//! 终端渠道配置向导：首次启动时补齐一个可用渠道（对映 Python 的渠道管理屏）。
//!
//! Python 侧是 Textual 全屏屏（`ui/fullscreen/screens/channel_manager.py` + `channel_setup.py`）；
//! Rust 宿主已经有一套终端渲染，但**首次配置必须能在没有任何界面依赖时也跑起来**——它出现在
//! splash 之前，且可能运行在只有 stdin/stdout 的环境里。这里因此用最小的行式向导：
//! 逐项提问 → 组装 [`ChannelConfig`] → 走 [`save_channel_configuration`] 落盘（复用配置域的
//! 原子写与校验，不重写 TOML 逻辑）。
//!
//! 与 Python 的差异：不做多列宽表单与鼠标交互，也不做远端模型自动发现（那需要模型目录网络层）；
//! 渠道名、Provider、协议、基地址、模型 ID、凭据变量名六项足够启动，其余字段保存后可在
//! 设置面板里改。

use std::io::{BufRead, IsTerminal, Write};

use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::models::channels::{
    protocols_for_provider, provider_options, save_channel_configuration, ChannelConfig,
    ChannelConfiguration, PROVIDER_OPENAI,
};

/// 向导结果：保存成功与否，以及给调用方看的说明行。
pub struct ChannelSetupOutcome {
    pub completed: bool,
    pub lines: Vec<String>,
}

/// 无交互终端时的结果：不询问，直接报告「跳过」。
pub fn unavailable_outcome() -> ChannelSetupOutcome {
    ChannelSetupOutcome {
        completed: false,
        lines: vec!["未完成模型渠道配置：当前终端不可交互，无法运行首次配置向导。".to_string()],
    }
}

/// 在当前终端跑一次渠道向导。
///
/// `config_path` / `models_path` 由调用方给出（`initialize_user_configuration` 已经算好写入位置），
/// 保存后以返回值为准：保存失败不会留半个渠道。
pub fn run_channel_setup(
    env: &ConfigEnvironment,
    config_path: &std::path::Path,
    models_path: &std::path::Path,
) -> ChannelSetupOutcome {
    let mut lines: Vec<String> = Vec::new();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout();
    if !stdin.is_terminal() {
        return unavailable_outcome();
    }

    lines.push("首次配置：请填写一个模型渠道（直接回车使用括号内的默认值）。".to_string());
    let Some(name) = ask(
        &mut input,
        &mut stdout,
        "渠道名（默认 my-channel）：",
        "my-channel",
    ) else {
        return cancelled(lines);
    };

    let providers: Vec<String> = provider_options()
        .iter()
        .map(|item| item.to_string())
        .collect();
    let Some(provider) = ask_choice(
        &mut input,
        &mut stdout,
        "Provider",
        &providers,
        PROVIDER_OPENAI,
    ) else {
        return cancelled(lines);
    };

    let protocols: Vec<String> = protocols_for_provider(&provider)
        .iter()
        .map(|item| item.to_string())
        .collect();
    let default_protocol = protocols.first().cloned().unwrap_or_default();
    let Some(protocol) = ask_choice(
        &mut input,
        &mut stdout,
        "协议",
        &protocols,
        default_protocol.as_str(),
    ) else {
        return cancelled(lines);
    };

    let default_base_url = default_base_url(&provider).to_string();
    let Some(base_url) = ask(
        &mut input,
        &mut stdout,
        "接口基地址",
        default_base_url.as_str(),
    ) else {
        return cancelled(lines);
    };
    let Some(model_id) = ask(&mut input, &mut stdout, "模型 ID", "") else {
        return cancelled(lines);
    };
    if model_id.trim().is_empty() {
        lines.push("模型 ID 不能为空，已取消本次配置。".to_string());
        return ChannelSetupOutcome {
            completed: false,
            lines,
        };
    }
    let default_env = default_api_key_env(&provider).to_string();
    let Some(api_key_env) = ask(
        &mut input,
        &mut stdout,
        "凭据环境变量名",
        default_env.as_str(),
    ) else {
        return cancelled(lines);
    };

    let channel = ChannelConfig {
        key: channel_key(&name),
        name: name.trim().to_string(),
        profile_id: channel_key(&name),
        provider,
        protocol,
        base_url: base_url.trim().to_string(),
        api_key: String::new(),
        model_id: model_id.trim().to_string(),
        enabled: true,
        api_key_env: api_key_env.trim().to_string(),
        user_agent: String::new(),
    };
    let configuration = ChannelConfiguration {
        channels: vec![channel],
        default_key: String::new(),
    };
    match save_channel_configuration(env, &configuration, Some(config_path), Some(models_path)) {
        Ok((saved_config, saved_models)) => {
            lines.push(format!("已保存运行配置：{}", saved_config.display()));
            lines.push(format!("已保存模型配置：{}", saved_models.display()));
            ChannelSetupOutcome {
                completed: true,
                lines,
            }
        }
        Err(error) => {
            lines.push(format!("模型渠道配置保存失败：{}", error.message()));
            ChannelSetupOutcome {
                completed: false,
                lines,
            }
        }
    }
}

fn cancelled(lines: Vec<String>) -> ChannelSetupOutcome {
    let mut lines = lines;
    lines.push("已取消首次配置；未保存任何渠道。".to_string());
    ChannelSetupOutcome {
        completed: false,
        lines,
    }
}

/// 读一行；EOF 时返回 `None` 表示用户放弃；空行表示「用默认值」。
fn ask(
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
    default: &str,
) -> Option<String> {
    let _ = write!(stdout, "{label} ");
    let _ = stdout.flush();
    let mut line = String::new();
    if input.read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let value = line.trim().to_string();
    if value.is_empty() {
        if default.is_empty() {
            return Some(String::new());
        }
        return Some(default.to_string());
    }
    Some(value)
}

/// 从候选里选一项：输入序号或取值本身，直接回车用默认值。
fn ask_choice(
    input: &mut impl BufRead,
    stdout: &mut impl Write,
    label: &str,
    options: &[String],
    default: &str,
) -> Option<String> {
    let _ = write!(stdout, "{label}：");
    for (index, option) in options.iter().enumerate() {
        let _ = write!(stdout, " {}.{option}", index + 1);
    }
    let _ = writeln!(stdout, "（默认 {default}）");
    let _ = stdout.flush();
    let mut line = String::new();
    if input.read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let value = line.trim();
    if value.is_empty() {
        return Some(default.to_string());
    }
    if let Ok(index) = value.parse::<usize>() {
        if index >= 1 && index <= options.len() {
            return Some(options[index - 1].clone());
        }
    }
    if options.iter().any(|option| option == value) {
        return Some(value.to_string());
    }
    // 取值不在候选里：按用户输入原样接受，保存阶段的校验会给出准确文案。
    Some(value.to_string())
}

/// Provider 的默认基地址（与 Python 渠道屏的默认值同源）。
pub fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "https://api.anthropic.com",
        "gemini" => "https://generativelanguage.googleapis.com",
        _ => "https://api.openai.com/v1",
    }
}

/// Provider 的默认凭据环境变量名。
pub fn default_api_key_env(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

/// 把渠道名折成合法 key：非字母数字折成连字符，去首尾连字符。
pub fn channel_key(name: &str) -> String {
    let mut key = String::new();
    let mut last_dash = false;
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            key.push(character.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            key.push('-');
            last_dash = true;
        }
    }
    let key = key.trim_matches('-').to_string();
    if key.is_empty() {
        "my-channel".to_string()
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_folded_and_never_empty() {
        assert_eq!(channel_key("My Channel 1"), "my-channel-1");
        assert_eq!(channel_key("  "), "my-channel");
        assert_eq!(channel_key("---"), "my-channel");
    }

    #[test]
    fn defaults_follow_the_provider() {
        assert_eq!(default_base_url("anthropic"), "https://api.anthropic.com");
        assert_eq!(default_api_key_env("gemini"), "GEMINI_API_KEY");
        assert_eq!(default_api_key_env("unknown"), "OPENAI_API_KEY");
    }

    #[test]
    fn choices_accept_index_name_and_default() {
        let options = vec!["a".to_string(), "b".to_string()];
        let mut input = std::io::Cursor::new(b"2\n");
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(
            ask_choice(&mut input, &mut out, "协议", &options, "a").as_deref(),
            Some("b")
        );
        let mut input = std::io::Cursor::new(b"\n");
        assert_eq!(
            ask_choice(&mut input, &mut out, "协议", &options, "a").as_deref(),
            Some("a")
        );
        let mut input = std::io::Cursor::new(b"custom\n");
        assert_eq!(
            ask_choice(&mut input, &mut out, "协议", &options, "a").as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn eof_cancels_the_wizard() {
        let mut input = std::io::Cursor::new(b"");
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(ask(&mut input, &mut out, "渠道名", "x"), None);
    }
}
