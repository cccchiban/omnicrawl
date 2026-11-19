//! 运行期装配与回执文案：模型选项、TTS、压缩设置、MCP 管理器。

use super::super::*;

/// 是否是一次 `Ctrl+V` 的**抬起**事件。
///
/// 命中判据必须认抬起、不能认按下：Windows Terminal 默认把 `ctrl+v` 绑成自家粘贴动作，
/// 它会吃掉按下（`vk=0x11` 的 down 记录里没有对应的 `v` down），只把抬起透传进 pty；
/// conhost 下两者都会到达，`short-circuit` 到同一个处理即可。
///
/// 两个细节来自 crossterm 在 Windows 上的解析方式（`KeyCode` 由 `ToUnicodeEx` 反推）：
/// * 大写锁打开时 `v` 会被折成 `'V'`，所以两种大小写都收；
/// * `Ctrl+Shift+V` 也是 `'V'`（终端自己的文本粘贴），必须按 SHIFT 挡掉，
///   否则剪贴板里同时有文本与位图时会「文本粘一遍、图片又插一遍」。
///
/// `pub` 是因为事件循环（`main.rs` 侧）也用它决定「这个抬起要不要放进主事件流」。
pub fn is_ctrl_v_release(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Release {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }
    matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// 生成选项（`GenerationOptions` 的 JSON 形状）：只给配置里真正有值的字段。
///
/// 内核按 `GenerationOptions` 解析，结构体带 `#[serde(default)]`，缺字段取默认值，
/// 因此这里不必把每个字段都填满。
pub(crate) fn generation_options(llm: &LlmConfig) -> Value {
    let mut options = serde_json::Map::new();
    if !llm.reasoning_effort.trim().is_empty() {
        options.insert(
            "reasoning_effort".to_string(),
            Value::String(llm.reasoning_effort.clone()),
        );
    }
    if llm.max_output_tokens > 0 {
        options.insert(
            "max_output_tokens".to_string(),
            json!(llm.max_output_tokens),
        );
    }
    if let Some(temperature) = llm.temperature {
        options.insert("temperature".to_string(), json!(temperature));
    }
    if llm.request_timeout_seconds > 0 {
        options.insert(
            "request_timeout_seconds".to_string(),
            json!(llm.request_timeout_seconds as f64),
        );
    }
    if llm.request_retry_count > 0 {
        options.insert(
            "request_retry_count".to_string(),
            json!(llm.request_retry_count as u32),
        );
    }
    if !llm.provider_options.is_empty() {
        // `provider_options` 在配置侧是 TOML 表（`toml::Table`），而协议只认 JSON。
        // 走 serde 序列化而不手写逐类型转换，避免漏掉日期、数组与嵌套表分支。
        match serde_json::to_value(&llm.provider_options) {
            Ok(value) if value.is_object() => {
                options.insert("provider_options".to_string(), value);
            }
            _ => {}
        }
    }
    Value::Object(options)
}

/// 图像生成运行期选项：命令行只承载显式覆盖，未给的字段回落 `[image_gen]` 配置。
///
/// 凭据只认配置里的明文 `api_key`，因此这里必须一并带上，否则工具每次都会以
/// 「缺少 API Key」失败。
pub(crate) fn image_gen_options_from(
    config: &ImageGenConfiguration,
    cli: &crate::args::ImageGenArgs,
) -> ImageGenOptions {
    ImageGenOptions {
        enabled: cli.enabled.unwrap_or(config.enabled),
        provider: config.provider.clone(),
        base_url: cli
            .base_url
            .clone()
            .unwrap_or_else(|| config.base_url.clone()),
        model: cli.model.clone().unwrap_or_else(|| config.model.clone()),
        api_key: cli
            .api_key
            .clone()
            .unwrap_or_else(|| config.api_key.clone()),
        size: config.size.clone(),
        quality: config.quality.clone(),
        output_format: config.output_format.clone(),
        aspect_ratio: config.aspect_ratio.clone(),
        image_size: config.image_size.clone(),
        n: config.n,
        timeout_seconds: config.timeout_seconds as f64,
        ..ImageGenOptions::default()
    }
}

/// 工具开关行：按 `TOOL_SWITCH_KEYS` 的顺序，注册状态来自当前工具表。
/// MCP 配置 → 设置面板初值。
pub(crate) fn mcp_values_from_config(
    config: &omnicrawl_mcp::config::McpConfig,
) -> McpSettingsValues {
    McpSettingsValues {
        enabled: config.enabled,
        allow_external_network_tools: config.policy.allow_external_network_tools,
        require_confirmation_for_write: config.policy.require_confirmation_for_write,
        require_confirmation_for_command: config.policy.require_confirmation_for_command,
        audit_log_enabled: config.policy.audit_log_enabled,
        timeout_seconds: config.default_timeout_seconds,
        servers: config
            .servers
            .iter()
            .map(|(_, server)| McpServerRow {
                name: server.name.clone(),
                enabled: server.enabled,
                transport: server.transport.clone(),
                risk_level: server.risk_level.clone(),
            })
            .collect(),
    }
}

/// MCP 配置（内核/客户端的形态）→ 可写回配置（`omnicrawl-config` 的形态）。
pub(crate) fn mcp_config_data(config: &omnicrawl_mcp::config::McpConfig) -> McpConfigData {
    McpConfigData {
        enabled: config.enabled,
        default_timeout_seconds: config.default_timeout_seconds,
        servers: config
            .servers
            .iter()
            .map(|(_, server)| {
                let mut env = omnicrawl_config::toml::Table::new();
                for (key, value) in &server.env {
                    env.insert(
                        key.clone(),
                        omnicrawl_config::toml::Value::String(value.clone()),
                    );
                }
                let mut headers = omnicrawl_config::toml::Table::new();
                for (key, value) in &server.headers {
                    headers.insert(
                        key.clone(),
                        omnicrawl_config::toml::Value::String(value.clone()),
                    );
                }
                (
                    server.name.clone(),
                    McpServerData {
                        enabled: server.enabled,
                        transport: server.transport.clone(),
                        command: server.command.clone().unwrap_or_default(),
                        args: server.args.clone(),
                        url: server.url.clone().unwrap_or_default(),
                        env,
                        headers,
                        timeout_seconds: server.timeout_seconds,
                        risk_level: server.risk_level.clone(),
                    },
                )
            })
            .collect(),
        policy: McpPolicyData {
            require_confirmation_for_write: config.policy.require_confirmation_for_write,
            require_confirmation_for_command: config.policy.require_confirmation_for_command,
            allow_external_network_tools: config.policy.allow_external_network_tools,
            audit_log_enabled: config.policy.audit_log_enabled,
        },
    }
}

/// 编辑草稿 → 可写回的 Server 数据。
///
/// 环境变量与请求头在界面上是 `KEY=VALUE;…`：留空表示保持原值（凭据不在界面回显，
/// 因此不能因为一次编辑把它们抹掉）；参数按 `shlex.split(posix=True)` 切分，
/// 与 Python 编辑器同口径。
pub(crate) fn mcp_server_data(
    draft: &McpServerDraft,
    previous: Option<&McpServerData>,
) -> Result<McpServerData, String> {
    let transport = if draft.transport.trim().is_empty() {
        "stdio".to_string()
    } else {
        draft.transport.trim().to_string()
    };
    let args = omnicrawl_controllers::subagents::shlex_split(draft.args.trim(), true)
        .unwrap_or_else(|_| draft.args.split_whitespace().map(str::to_string).collect());
    let env = parse_key_value_list(&draft.env, previous.map(|server| &server.env), "环境变量")?;
    let headers = parse_key_value_list(
        &draft.headers,
        previous.map(|server| &server.headers),
        "请求头",
    )?;
    let risk_level = if draft.risk_level.trim().is_empty() {
        "restricted".to_string()
    } else {
        draft.risk_level.trim().to_string()
    };
    Ok(McpServerData {
        enabled: draft.enabled,
        transport,
        command: draft.command.trim().to_string(),
        args,
        url: draft.url.trim().to_string(),
        env,
        headers,
        timeout_seconds: draft.timeout_seconds,
        risk_level,
    })
}

/// 压缩配置 → 内核协议字段。
///
/// `[context_compaction]` 是压缩策略的唯一真源（与 Python 侧读同一份配置）：命中百分比时
/// 只改触发阈值与窗口，其余字段内核保持握手时的值。
///
/// 历史上握手这里发的是 `None`，内核便退回 `CompactionConfig::default()`，于是
/// `target_summary_tokens` 变成默认的 2000、`recent_turns` 变成 3、推理强度变成 medium：
/// 用户配的 0（无预算上限）被丢掉，摘要随即被 token 预算卡住并被校验拒绝（完整性要求与
/// 预算直接冲突）。现在整段下发，内核侧不会再出现「配置写了却不生效」。
pub(crate) fn compaction_settings(window: i64, percent: i64) -> KernelCompactionConfig {
    KernelCompactionConfig {
        trigger_context_tokens: Some(context_compaction_trigger_tokens(window, percent)),
        context_window_tokens: Some(window),
        ..KernelCompactionConfig::default()
    }
}

/// 压缩成功回执 → 界面消息（与命令层 `handle_compact_command` 的收尾文案一致）。
pub(crate) fn compact_message(result: &Value) -> String {
    let compacted = result["compacted"].as_bool().unwrap_or(true);
    if !compacted {
        return "当前会话不需要压缩（上下文里还没有可压缩的轮次）。".to_string();
    }
    "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。".to_string()
}

/// 把内核的撤销结果渲染成一行界面消息。
pub(crate) fn undo_message(result: &Value) -> String {
    let count = result["message_count"].as_u64().unwrap_or_default();
    let mut message = match result["kind"].as_str().unwrap_or_default() {
        "incomplete" => format!("已撤销最近一轮的未完成部分（回退 {count} 条消息）"),
        _ => format!("已撤销最近一轮（回退 {count} 条消息）"),
    };
    if result["side_effects_reverted"] == Value::Bool(true) {
        message.push_str("，工作区已恢复");
    }
    let unrestorable: Vec<&str> = result["unrestorable"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !unrestorable.is_empty() {
        message.push_str(&format!(
            "；以下文件没有内容副本、未能恢复：{}",
            unrestorable.join("、")
        ));
    }
    message.push('。');
    message
}

/// Computer Use 是否进表：读 `[computer_use] enabled`，与 `[subagents]` 无关。
///
/// 读配置失败时按关闭处理：这是可选的桌面能力，配置坏了不该让整张工具表起不来。
pub(crate) fn computer_use_enabled(environment: &ConfigEnvironment) -> bool {
    load_computer_use_config(environment, None)
        .map(|config| config.active())
        .unwrap_or(false)
}

/// TTS 工具配置：读 config.toml 的 `[tts]` 与 `[tts_cloud]`，未启用时不给这个工具
/// （与 Python 一致）。
pub(crate) fn tts_options(workspace: &Path) -> Option<Arc<TtsOptions>> {
    let environment = ConfigEnvironment::from_process();
    let configuration = match load_tts_configuration(&environment, None) {
        Ok(configuration) => configuration,
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("TTS 配置读取失败，已跳过语音合成：{error}"),
            );
            return None;
        }
    };
    tts_options_from(&environment, &configuration, workspace)
}

/// 用一份已读出的通用配置构造 TTS 工具选项；未启用时不给工具。
///
/// 通道参数在这里现读：切服务商只改 `[tts_cloud]`，重建工具表就会拿到新的一家。
pub(crate) fn tts_options_from(
    environment: &ConfigEnvironment,
    configuration: &TtsConfiguration,
    workspace: &Path,
) -> Option<Arc<TtsOptions>> {
    if !configuration.enabled {
        return None;
    }
    let cloud = match load_tts_cloud_configuration(environment, None) {
        Ok(cloud) => cloud,
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("TTS 服务商配置读取失败，已跳过语音合成：{error}"),
            );
            return None;
        }
    };
    let openai = load_tts_api_configuration(environment, None).unwrap_or_default();
    let gemini = load_tts_gemini_configuration(environment, None).unwrap_or_default();
    let deepseek = load_tts_deepseek_configuration(environment, None).unwrap_or_default();
    Some(Arc::new(TtsOptions::new(
        configuration.clone(),
        tts_backend(&cloud, openai, gemini, deepseek),
        workspace.to_path_buf(),
    )))
}

/// TTS 设置页的初值：通用键 + 三条通道（界面按 `provider` 显示其中一家）。
pub(crate) fn tts_settings_values(environment: &ConfigEnvironment) -> TtsValues {
    let general = load_tts_configuration(environment, None).unwrap_or_default();
    let cloud = load_tts_cloud_configuration(environment, None).unwrap_or_default();
    let openai = load_tts_api_configuration(environment, None).unwrap_or_default();
    let gemini = load_tts_gemini_configuration(environment, None).unwrap_or_default();
    let deepseek = load_tts_deepseek_configuration(environment, None).unwrap_or_default();
    TtsValues {
        enabled: general.enabled,
        auto_play: general.auto_play,
        speak_replies: general.speak_replies,
        provider: cloud.provider,
        openai: TtsChannelValues {
            base_url: openai.base_url,
            model: openai.model,
            voice: openai.voice,
            instructions: openai.instructions,
            speed: format_speed(openai.speed),
            transport: String::new(),
            api_key: openai.api_key,
        },
        gemini: TtsChannelValues {
            base_url: gemini.base_url,
            model: gemini.model,
            voice: gemini.voice,
            instructions: gemini.instructions,
            speed: String::new(),
            transport: String::new(),
            api_key: gemini.api_key,
        },
        deepseek: TtsChannelValues {
            base_url: deepseek.base_url,
            model: String::new(),
            voice: deepseek.voice,
            instructions: String::new(),
            speed: String::new(),
            transport: deepseek.transport,
            api_key: deepseek.api_key,
        },
    }
}

/// 语速的显示文本：整数不带小数点（`1` 而不是 `1.0`），便于界面里来回编辑。
pub(crate) fn format_speed(speed: f64) -> String {
    if (speed - speed.round()).abs() < f64::EPSILON {
        format!("{}", speed.round() as i64)
    } else {
        format!("{speed}")
    }
}

/// 草稿里的当前服务商字段 → 两家的完整配置。
///
/// 只有被编辑的那一家用草稿覆盖，另一家保持磁盘原值：界面上看不到的段不该被顺手改写。
/// 草稿里的密钥为空串时表示「不改动」——保留磁盘原值（对映渠道表单的 API Key 行）。
pub(crate) fn tts_channel_configurations(
    draft: &TtsDraft,
    previous_api: &TtsApiConfiguration,
    previous_gemini: &GeminiTtsConfiguration,
    previous_deepseek: &DeepSeekTtsConfiguration,
) -> Result<
    (
        TtsApiConfiguration,
        GeminiTtsConfiguration,
        DeepSeekTtsConfiguration,
    ),
    String,
> {
    let mut api = previous_api.clone();
    let mut gemini = previous_gemini.clone();
    let mut deepseek = previous_deepseek.clone();
    match &draft.channel {
        TtsChannelDraft::OpenAi {
            base_url,
            model,
            voice,
            instructions,
            speed,
            api_key,
        } => {
            api.base_url = base_url.trim().to_string();
            api.model = model.trim().to_string();
            api.voice = voice.trim().to_string();
            api.instructions = instructions.trim().to_string();
            api.speed = speed
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("语速无法解析为数值：{}", speed.trim()))?;
            if let Some(key) = keep_or_replace(api_key) {
                api.api_key = key;
            }
        }
        TtsChannelDraft::Gemini {
            base_url,
            model,
            voice,
            instructions,
            api_key,
        } => {
            gemini.base_url = base_url.trim().to_string();
            gemini.model = model.trim().to_string();
            gemini.voice = voice.trim().to_string();
            gemini.instructions = instructions.trim().to_string();
            if let Some(key) = keep_or_replace(api_key) {
                gemini.api_key = key;
            }
        }
        TtsChannelDraft::DeepSeek {
            base_url,
            voice,
            transport,
            api_key,
        } => {
            deepseek.base_url = base_url.trim().to_string();
            deepseek.voice = voice.trim().to_string();
            deepseek.transport = transport.trim().to_string();
            if let Some(key) = keep_or_replace(api_key) {
                deepseek.api_key = key;
            }
        }
    }
    Ok((api, gemini, deepseek))
}

/// 装配 MCP 管理器：读配置、发现能力，失败只警告不阻断启动。
///
/// 协议 v1 的工具声明固定在 `initialize`，所以能力发现必须发生在握手之前——
/// Python 侧是「首次使用时懒加载」，Rust 宿主把它提前到启动期（见 crate README）。
pub(crate) fn mcp_manager(workspace: &Path) -> Option<Arc<McpClientManager>> {
    let config = match load_mcp_config(&ConfigEnvironment::from_process(), None) {
        Ok(config) => config,
        Err(error) => {
            report_startup_log(
                LogLevel::Warning,
                &format!("MCP 配置读取失败，已跳过 MCP：{error}"),
            );
            return None;
        }
    };
    if !config.enabled {
        return None;
    }
    let manager = Arc::new(McpClientManager::new(config, workspace));
    // 能力清单已缓存时只声明、不连接；模型第一次调用某个 Server 的工具时，
    // `McpClientManager::ensure_server_connected` 再拉起对应进程。
    if !manager.has_cached_capabilities() {
        manager.discover();
    }
    for diagnostic in manager.diagnostics() {
        let prefix = diagnostic
            .server_name
            .as_deref()
            .map(|name| format!("{name}: "))
            .unwrap_or_default();
        report_startup_log(
            LogLevel::Warning,
            &format!(
                "MCP [{}] {prefix}{}",
                diagnostic.severity, diagnostic.message
            ),
        );
    }
    Some(manager)
}

/// 失败的回合错误是否值得提供「重试」按钮。
///
/// 判据是「原样再发一次有没有机会成功」：网络中断、超时、上游 5xx、限流、以及自动重试
/// 用尽都算；上下文超限（要先压缩）、参数非法、鉴权失败这类确定性错误不给按钮，
/// 免得用户点出一个必然再次失败的结果。
pub(crate) fn is_retryable_failure(message: &str) -> bool {
    const NOT_RETRYABLE: &[&str] = &[
        "上下文长度",
        "context length",
        "超过上下文",
        "鉴权",
        "unauthorized",
        "401",
        "invalid api key",
        "参数",
        "invalid params",
        "工具状态",
        "重复提交",
    ];
    let lower = message.to_lowercase();
    if NOT_RETRYABLE
        .iter()
        .any(|marker| lower.contains(&marker.to_lowercase()))
    {
        return false;
    }
    // 默认给按钮：内核的错误都是「这一轮没跑成」，重发是用户最自然的补救动作。
    true
}
