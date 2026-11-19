//! 字段读取小工具与「表单值 ↔ 配置」互转。改设置项字段顺序时只动这里。

use super::super::*;

/// 取字符串字段；缺失或不是字符串时为空串（避免把 JSON 拼进文案）。
pub(crate) fn result_text(result: &Value, key: &str) -> String {
    result
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 表单字段的文本取值；字段缺失时为空串（保存时由校验兜住）。
pub(crate) fn field_text(values: &[FieldValue], index: usize) -> String {
    values
        .get(index)
        .map(|value| value.text().to_string())
        .unwrap_or_default()
}

/// 表单字段的正整数取值（对映 Python 的 `_read_positive_int`）。
pub(crate) fn positive_int(
    values: &[FieldValue],
    index: usize,
    label: &str,
) -> Result<i64, String> {
    let raw = field_text(values, index);
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是正整数。"))?;
    if value <= 0 {
        return Err(format!("设置未完成：{label}必须是正整数。"));
    }
    Ok(value)
}

/// 表单字段的非负整数取值（0 合法，如「NER 缓存容量 0 关闭」）。
pub(crate) fn non_negative_int(
    values: &[FieldValue],
    index: usize,
    label: &str,
) -> Result<i64, String> {
    let raw = field_text(values, index);
    let value: i64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是整数。"))?;
    if value < 0 {
        return Err(format!("设置未完成：{label}不能为负数。"));
    }
    Ok(value)
}

/// 表单字段的小数取值（重复率阈值、熵兜底阈值这类 0～1 / 0～8 的比例）。
pub(crate) fn field_float(values: &[FieldValue], index: usize, label: &str) -> Result<f64, String> {
    field_text(values, index)
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是数字。"))
}

/// 表单字段的开关取值。
pub(crate) fn field_flag(values: &[FieldValue], index: usize) -> bool {
    values.get(index).map(|value| value.flag()).unwrap_or(false)
}

/// 表单字段的文本取值；空串回落到默认值（对映 Python 的 `_read_input(fallback=...)`）。
pub(crate) fn field_text_or(values: &[FieldValue], index: usize, fallback: &str) -> String {
    let text = field_text(values, index).trim().to_string();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

/// 凭据字段的写回取值：草稿留空表示「不改动」→ `None`；否则用新值。
///
/// 用于界面掩码已折成空串的页（TTS 渠道表单）：那套口径下空串即「不改动」，
/// 因此无法删除已存凭据；需要删除通道的页用 [`secret_write`]。
pub(crate) fn keep_or_replace(draft: &str) -> Option<String> {
    let trimmed = draft.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// 联网搜索页凭据字段的写回取值：`None` = 不动磁盘原值，`Some("")` = 删掉这把凭据。
///
/// 与 [`keep_or_replace`] 的差别就在空串：那套口径下空串表示「不改动」，凭据一旦写入
/// 就再也删不掉，而 `mixed` 只按「凭据非空」判定渠道可用，于是清空地址与模型后搜索
/// 仍会请求该渠道。这里把「不动」的唯一标记收窄成界面掩码本身。
pub(crate) fn secret_write(draft: &str) -> Option<String> {
    let trimmed = draft.trim();
    if trimmed == MASKED_SECRET {
        return None;
    }
    Some(trimmed.to_string())
}

/// 表单字段的分隔列表取值：切分后去掉空白与空项（对映 Python 的 `_read_keys`）。
pub(crate) fn field_list(values: &[FieldValue], index: usize, separator: char) -> Vec<String> {
    field_text(values, index)
        .split(separator)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 按渠道 key 取渠道记录；配置里没有这条渠道时返回 `None`。
pub(crate) fn channel_for_key(environment: &ConfigEnvironment, key: &str) -> Option<ChannelConfig> {
    if key.trim().is_empty() {
        return None;
    }
    load_channel_configuration(environment, None, None)
        .ok()?
        .channels
        .into_iter()
        .find(|channel| channel.key == key)
}

/// 顾问设置页的字段初值；顺序与 `FormKind::Advisor` 的字段表一致。
///
/// 「顾问渠道 + 顾问模型」：把配置里的 `model_key` 拆成渠道与模型两段
/// （与 `/advisor` 的命令行解析同一套口径，见 [`split_token`]）。
pub(crate) fn advisor_form_values(
    config: &AdvisorConfig,
    channels: &[ChannelRow],
) -> Vec<FieldValue> {
    let (channel, model) = split_token(&config.model_key, channels);
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.display_effort()),
        FieldValue::Text(channel),
        FieldValue::Text(model),
    ]
}

/// 工具输出压缩页的字段初值；顺序与 `FormKind::ToolOutputCompression` 的字段表一致。
pub(crate) fn compression_form_values(
    config: &ToolOutputCompressionConfig,
    channels: &[ChannelRow],
) -> Vec<FieldValue> {
    let (channel, model) = split_token(&config.model_key, channels);
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.thinking_enabled),
        FieldValue::Text(config.reasoning_effort.clone()),
        FieldValue::Text(config.min_chars.to_string()),
        FieldValue::Text(config.max_input_chars.to_string()),
        FieldValue::Text(config.max_output_chars.to_string()),
        FieldValue::Text(config.timeout_seconds.to_string()),
        FieldValue::Text(channel),
        FieldValue::Text(model),
    ]
}

/// 表单页里的「渠道 + 模型」两段值拼回配置里的模型引用。
///
/// 与 `/advisor`、`/model` 的解析口径同源：模型非空时写 `profile/model_id`（渠道取该
/// 渠道记录里的 `profile_id`，与渠道页/模型发现同一份口径），只选了渠道时写渠道 key
/// （`apply_model_selection` 会把渠道 key 解析成它自带的模型）。
pub(crate) fn form_model_token(
    values: &[FieldValue],
    channel_index: usize,
    model_index: usize,
    channels: &[ChannelRow],
) -> String {
    let channel = field_text(values, channel_index).trim().to_string();
    let model = field_text(values, model_index).trim().to_string();
    if model.is_empty() {
        return channel;
    }
    let profile = channels
        .iter()
        .find(|row| row.key == channel)
        .map(|row| row.profile_id.trim().to_string())
        .unwrap_or_default();
    if profile.is_empty() {
        return model;
    }
    format!("{profile}/{model}")
}

/// 消息脱敏页的字段初值；顺序与 `FormKind::Desensitization` 的字段表一致。
pub(crate) fn desensitization_form_values(config: &DesensitizationConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.fail_closed),
        FieldValue::Flag(config.strict_restore),
        FieldValue::Flag(config.entropy_enabled),
        FieldValue::Flag(config.entropy_pure_letters),
        FieldValue::Flag(config.entropy_pure_digits),
        FieldValue::Text(config.entropy_min_length.to_string()),
        FieldValue::Text(config.entropy_min_bits.to_string()),
        FieldValue::Text(config.extra_sensitive_keys.join(", ")),
        FieldValue::Text(config.exempt_keys.join(", ")),
        FieldValue::Flag(config.detect_pem_private_key),
        FieldValue::Flag(config.detect_db_connection_string),
        FieldValue::Flag(config.detect_email),
        FieldValue::Flag(config.detect_bank_card),
        FieldValue::Flag(config.detect_internal_ip),
        FieldValue::Flag(config.detect_external_ip),
        FieldValue::Flag(config.detect_url),
        FieldValue::Flag(config.detect_mac_address),
        FieldValue::Flag(config.detect_license_plate),
        FieldValue::Flag(config.gitleaks_enabled),
        FieldValue::Text(config.gitleaks_config_path.clone()),
    ]
}

/// 持续运转页的字段初值；顺序与 `FormKind::RunGuard` 的字段表一致。
pub(crate) fn run_guard_form_values(config: &RunGuardConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Flag(config.guard.enabled),
        FieldValue::Text(config.guard.window_chars.to_string()),
        FieldValue::Text(config.guard.substr_len.to_string()),
        FieldValue::Text(config.guard.repeat_ratio.to_string()),
        FieldValue::Text(config.guard.check_every.to_string()),
        FieldValue::Text(config.guard.max_blocks.to_string()),
        FieldValue::Text(config.guard.max_chars.to_string()),
        FieldValue::Text(config.guard.max_guard_retries.to_string()),
        FieldValue::Text(config.guard.auto_retry_errors.join(", ")),
        FieldValue::Flag(config.continuation.enabled),
        FieldValue::Text(config.continuation.max_auto_followups.to_string()),
    ]
}

/// 沙盒页的字段初值；顺序与 `FormKind::Sandbox` 的字段表一致。
///
/// 两个域名列表与两个路径列表都用逗号分隔：与「持续运转」页的多值字段同口径，
/// 切分与去空在 `apply_sandbox_form` 里做。
pub(crate) fn sandbox_form_values(config: &SandboxConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.mode.as_str().to_string()),
        FieldValue::Text(config.writable.join(", ")),
        FieldValue::Text(config.readable.join(", ")),
        FieldValue::Text(config.net_policy.as_str().to_string()),
        FieldValue::Text(config.net_allow.join(", ")),
        FieldValue::Text(config.net_deny.join(", ")),
        FieldValue::Text(config.net_approval_timeout_seconds.to_string()),
        FieldValue::Flag(config.net_log_all),
        FieldValue::Text(config.limits.max_processes.to_string()),
        FieldValue::Text(config.limits.max_memory_mb.to_string()),
        FieldValue::Text(config.limits.cpu_rate_percent.to_string()),
    ]
}

/// 隔离工作区页的字段初值；顺序与 `FormKind::AgentWorkspace` 的字段表一致。
pub(crate) fn agent_workspace_form_values(config: &AgentWorkspaceConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.mode.clone()),
        FieldValue::Text(config.base_ref.clone()),
        FieldValue::Flag(config.detached),
        FieldValue::Flag(config.sync_uncommitted),
        FieldValue::Flag(config.apply_on_exit),
        FieldValue::Text(config.cleanup_on_exit.clone()),
        FieldValue::Text(config.copy_dirs.join(", ")),
        FieldValue::Text(config.env_scripts.join("; ")),
    ]
}

/// Computer Use 页的字段初值；顺序与 `COMPUTER_USE_FIELDS` 一致。
pub(crate) fn computer_use_form_values(config: &ComputerUseConfig) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.timeout_seconds.to_string()),
        FieldValue::Text(config.screenshot_max_dimension.to_string()),
        FieldValue::Text(config.thinking_effort.clone()),
        FieldValue::Text(config.model.clone()),
    ]
}

/// 图像生成页的字段初值；顺序与 `FormKind::ImageGen` 的字段表一致。
///
/// `api_key` 只渲染掩码，输入态从空开始（留空＝不改动），与渠道页同口径。
pub(crate) fn image_gen_form_values(config: &ImageGenConfiguration) -> Vec<FieldValue> {
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.provider.clone()),
        FieldValue::Text(config.base_url.clone()),
        FieldValue::Text(crate::ui::settings::state::mask_api_key(&config.api_key)),
        FieldValue::Text(config.model.clone()),
        FieldValue::Text(config.size.clone()),
        FieldValue::Text(config.quality.clone()),
        FieldValue::Text(config.output_format.clone()),
        FieldValue::Text(config.aspect_ratio.clone()),
        FieldValue::Text(config.image_size.clone()),
        FieldValue::Text(config.n.to_string()),
        FieldValue::Text(config.timeout_seconds.to_string()),
    ]
}

/// 配置侧的模型引用 → 视觉设置页的投影。
pub(crate) fn vision_ref_from_config(reference: &ActiveModelRef) -> VisionModelRef {
    VisionModelRef {
        source: reference.source.clone(),
        key: reference.key.clone(),
        profile: reference.profile.clone(),
        model_id: reference.model_id.clone(),
        protocol: reference.protocol.clone(),
    }
}

/// 界面投影 → 配置侧的模型引用。
pub(crate) fn vision_ref_to_config(reference: &VisionModelRef) -> ActiveModelRef {
    ActiveModelRef {
        source: reference.source.clone(),
        key: reference.key.clone(),
        profile: reference.profile.clone(),
        model_id: reference.model_id.clone(),
        protocol: reference.protocol.clone(),
    }
}

/// 模型原生视觉的写入范围：自定义模型写 models.toml，其余写渠道 Profile
/// （对映 Python 的 `_native_write_target`）。
pub(crate) fn vision_native_target(llm: &LlmConfig) -> (&'static str, String) {
    let catalog_key = llm.catalog_key.trim();
    if !catalog_key.is_empty() {
        return ("model", catalog_key.to_string());
    }
    ("channel", llm.profile_id.trim().to_string())
}

/// 当前生效的上下文窗口：显式 CLI/环境值优先，其次配置文件。
pub(crate) fn effective_context_window(explicit: Option<u64>, llm: &LlmConfig) -> i64 {
    match explicit {
        Some(value) if value > 0 => value as i64,
        _ => llm.context_window_tokens,
    }
}

/// 当前生效的原生视觉开关：命令行/环境显式开启优先，其次配置里当前模型的覆盖。
///
/// 配置里的值由 `load_llm_config` 按「模型条目覆盖 > 渠道 Profile 覆盖 > 未配置」解出，
/// 与 Python `_native_vision_enabled` 同口径；未配置时回落模型自身声明的
/// `capabilities.vision`（见 `resolve_native_vision_switch`），而不是直接当关闭。
///
/// `--base-url` 指向外部渠道时不再叠加配置：那份 `native_vision` 描述的是配置里选中的
/// 模型，而外部渠道跑的是命令行给的那个模型（与 Provider / 协议 / 生成选项同一口径）。
pub(crate) fn effective_native_vision(
    external_channel: bool,
    explicit: bool,
    llm: &LlmConfig,
) -> bool {
    if explicit {
        return true;
    }
    if external_channel {
        return false;
    }
    resolve_native_vision_switch(llm.native_vision, llm.vision_capability)
}

/// 原生视觉三态解析：显式开关优先，未配置时按模型自身能力回落。
///
/// 这是「原生视觉优先于视觉代理」的判据起点——`Some(true)` 直送主模型，`Some(false)` 交给
/// 代理，`None`（模型也没声明能力）同样交给代理。宿主与内核共用同一口径，
/// 免得一边「未配置」被压成 `false`、另一边按能力放出图片。
pub fn resolve_native_vision_switch(
    configured: Option<bool>,
    vision_capability: Option<bool>,
) -> bool {
    omnicrawl_controllers::turn::turn_loop::native_vision_enabled(
        configured,
        omnicrawl_controllers::turn::turn_loop::model_supports_vision(vision_capability),
    )
}

/// 按写前快照还原配置文件；`None` 表示写之前文件不存在，那就把它删掉。
///
/// 还原失败只告警：调用方已经把「配置非法」这个真正的原因报给用户了，这里再抛错只会
/// 把它盖掉，而且磁盘上留着的也只是读取器拒绝的内容（加载时仍会报同一句错）。
pub(crate) fn restore_config_file(path: &Path, snapshot: Option<&[u8]>) {
    let result = match snapshot {
        Some(bytes) => std::fs::write(path, bytes),
        None => std::fs::remove_file(path),
    };
    if let Err(error) = result {
        diagnostics::warn(format!(
            "[tui] MCP 配置回滚失败（{}）：{error}",
            path.display()
        ));
    }
}

/// 解析 `KEY=VALUE;KEY2=VALUE` 形式的表；留空保持原值，格式错时拒绝保存。
pub(crate) fn parse_key_value_list(
    text: &str,
    previous: Option<&omnicrawl_config::toml::Table>,
    label: &str,
) -> Result<omnicrawl_config::toml::Table, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(previous.cloned().unwrap_or_default());
    }
    let mut table = omnicrawl_config::toml::Table::new();
    for item in trimmed.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((key, value)) = item.split_once('=') else {
            return Err(format!("设置未完成：{label}格式必须是 KEY=VALUE。"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("设置未完成：{label}格式必须是 KEY=VALUE。"));
        }
        table.insert(
            key.to_string(),
            omnicrawl_config::toml::Value::String(value.to_string()),
        );
    }
    Ok(table)
}
