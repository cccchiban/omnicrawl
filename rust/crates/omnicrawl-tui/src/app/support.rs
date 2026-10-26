//! `App` 之外的自由函数：设置表单值 ↔ 配置、工具开关行、回执文案、运行期装配。
//!
//! 这些函数不碰 `App` 状态，因此从 `app.rs` 里独立出来；`use super::*` 让它们继续
//! 复用同一批导入，不必在新文件里重复一遍依赖清单。

use super::*;

/// 取字符串字段；缺失或不是字符串时为空串（避免把 JSON 拼进文案）。
pub(super) fn result_text(result: &Value, key: &str) -> String {
    result
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 表单字段的文本取值；字段缺失时为空串（保存时由校验兜住）。
pub(super) fn field_text(values: &[FieldValue], index: usize) -> String {
    values
        .get(index)
        .map(|value| value.text().to_string())
        .unwrap_or_default()
}

/// 表单字段的正整数取值（对映 Python 的 `_read_positive_int`）。
pub(super) fn positive_int(values: &[FieldValue], index: usize, label: &str) -> Result<i64, String> {
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
pub(super) fn non_negative_int(values: &[FieldValue], index: usize, label: &str) -> Result<i64, String> {
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
pub(super) fn field_float(values: &[FieldValue], index: usize, label: &str) -> Result<f64, String> {
    field_text(values, index)
        .trim()
        .parse()
        .map_err(|_| format!("设置未完成：{label}必须是数字。"))
}

/// 表单字段的开关取值。
pub(super) fn field_flag(values: &[FieldValue], index: usize) -> bool {
    values.get(index).map(|value| value.flag()).unwrap_or(false)
}

/// 表单字段的文本取值；空串回落到默认值（对映 Python 的 `_read_input(fallback=...)`）。
pub(super) fn field_text_or(values: &[FieldValue], index: usize, fallback: &str) -> String {
    let text = field_text(values, index).trim().to_string();
    if text.is_empty() {
        fallback.to_string()
    } else {
        text
    }
}

/// 凭据字段的写回取值：草稿留空表示「不改动」→ `None`；否则用新值。
///
/// 界面上的掩码已在状态机里折成空串，所以这里只需判空。
pub(super) fn keep_or_replace(draft: &str) -> Option<String> {
    let trimmed = draft.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// 表单字段的分隔列表取值：切分后去掉空白与空项（对映 Python 的 `_read_keys`）。
pub(super) fn field_list(values: &[FieldValue], index: usize, separator: char) -> Vec<String> {
    field_text(values, index)
        .split(separator)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 按渠道 key 取渠道记录；配置里没有这条渠道时返回 `None`。
pub(super) fn channel_for_key(environment: &ConfigEnvironment, key: &str) -> Option<ChannelConfig> {
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
pub(super) fn advisor_form_values(config: &AdvisorConfig, channels: &[ChannelRow]) -> Vec<FieldValue> {
    let (channel, model) = split_token(&config.model_key, channels);
    vec![
        FieldValue::Flag(config.enabled),
        FieldValue::Text(config.display_effort()),
        FieldValue::Text(channel),
        FieldValue::Text(model),
    ]
}

/// 工具输出压缩页的字段初值；顺序与 `FormKind::ToolOutputCompression` 的字段表一致。
pub(super) fn compression_form_values(
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
pub(super) fn form_model_token(
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
pub(super) fn desensitization_form_values(config: &DesensitizationConfig) -> Vec<FieldValue> {
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
        FieldValue::Flag(config.ner_enabled),
        FieldValue::Text(config.ner_device.clone()),
        FieldValue::Text(config.ner_model_path.clone()),
        FieldValue::Text(config.ner_entity_types.join(", ")),
        FieldValue::Text(config.ner_min_entity_chars.to_string()),
        FieldValue::Text(config.ner_cache_size.to_string()),
    ]
}

/// 持续运转页的字段初值；顺序与 `FormKind::RunGuard` 的字段表一致。
pub(super) fn run_guard_form_values(config: &RunGuardConfig) -> Vec<FieldValue> {
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

/// 隔离工作区页的字段初值；顺序与 `FormKind::AgentWorkspace` 的字段表一致。
pub(super) fn agent_workspace_form_values(config: &AgentWorkspaceConfig) -> Vec<FieldValue> {
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

/// 联网搜索在工具页上的投影：默认引擎 + 各凭据是否已配置（凭据本身不进界面状态）。
pub(super) fn web_search_row(environment: &ConfigEnvironment) -> WebSearchRow {
    let configuration = load_web_search_configuration(environment, None).unwrap_or_default();
    WebSearchRow {
        engine: configuration.engine,
        tavily_configured: !configuration.tavily_api_key.trim().is_empty(),
        exa_configured: !configuration.exa_api_key.trim().is_empty(),
        firecrawl_configured: !configuration.firecrawl_api_key.trim().is_empty(),
        anysearch_configured: !configuration.anysearch_api_key.trim().is_empty(),
        tinyfish_configured: !configuration.tinyfish_api_key.trim().is_empty(),
        github_configured: !configuration.github_token.trim().is_empty(),
    }
}

/// Computer Use 页的字段初值；顺序与 `COMPUTER_USE_FIELDS` 一致。
pub(super) fn computer_use_form_values(config: &ComputerUseConfig) -> Vec<FieldValue> {
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
pub(super) fn image_gen_form_values(config: &ImageGenConfiguration) -> Vec<FieldValue> {
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
pub(super) fn vision_ref_from_config(reference: &ActiveModelRef) -> VisionModelRef {
    VisionModelRef {
        source: reference.source.clone(),
        key: reference.key.clone(),
        profile: reference.profile.clone(),
        model_id: reference.model_id.clone(),
        protocol: reference.protocol.clone(),
    }
}

/// 界面投影 → 配置侧的模型引用。
pub(super) fn vision_ref_to_config(reference: &VisionModelRef) -> ActiveModelRef {
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
pub(super) fn vision_native_target(llm: &LlmConfig) -> (&'static str, String) {
    let catalog_key = llm.catalog_key.trim();
    if !catalog_key.is_empty() {
        return ("model", catalog_key.to_string());
    }
    ("channel", llm.profile_id.trim().to_string())
}

/// 子任务设置页的行：总开关 + 六个高级参数（对映 Python 的 `_SubagentsPane.sections`）。
pub(super) fn subagent_rows(config: &SubAgentConfig) -> Vec<SubagentRow> {
    let mut rows = vec![SubagentRow {
        key: "enabled".to_string(),
        label: "功能总开关".to_string(),
        value: if config.enabled {
            "已开启"
        } else {
            "已关闭"
        }
        .to_string(),
        toggle: true,
        active: config.enabled,
        section: "子任务功能",
    }];
    let current: [(&str, i64); 4] = [
        (
            "default_timeout_seconds",
            config.default_timeout_seconds as i64,
        ),
        (
            "model_request_concurrency",
            config.model_request_concurrency,
        ),
        (
            "verify_command_timeout_seconds",
            config.verify_command_timeout_seconds,
        ),
        ("task_retention_minutes", config.task_retention_minutes),
    ];
    for (key, value) in current {
        let Some(spec) = SUBAGENT_ADVANCED_SPECS.iter().find(|spec| spec.key == key) else {
            continue;
        };
        rows.push(SubagentRow {
            key: key.to_string(),
            label: spec.label.to_string(),
            value: value.to_string(),
            toggle: false,
            active: false,
            section: "高级参数",
        });
    }
    rows
}

/// 功能开关的中文名（对映 Python 的 `_FEATURES` 表）。
pub(super) fn feature_label(key: &str) -> String {
    match key {
        "memory" => "记忆功能".to_string(),
        "plugins" => "插件功能".to_string(),
        other => other.to_string(),
    }
}

/// 图像生成运行期选项：命令行只承载显式覆盖，未给的字段回落 `[image_gen]` 配置。
///
/// 凭据只认配置里的明文 `api_key`，因此这里必须一并带上，否则工具每次都会以
/// 「缺少 API Key」失败。
pub(super) fn image_gen_options_from(
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

/// 决策模型功能开关的界面行：标签与顺序取自配置域的开关表，值取配置。
///
/// 只收「决策模型」页自己的开关（`SwitchPage::Decision`）；画在向量模型页的开关由
/// 向量页自己取用，不进这张表。
pub(super) fn decision_switch_rows(switches: &DecisionSwitches) -> Vec<DecisionSwitchRow> {
    DECISION_SWITCHES
        .iter()
        .filter(|spec| spec.page == SwitchPage::Decision)
        .map(|spec| DecisionSwitchRow {
            key: spec.key.to_string(),
            label: spec.label.to_string(),
            enabled: switches.get(spec.key).copied().unwrap_or(spec.default),
        })
        .collect()
}

/// 配置决策渠道 → 决策页的界面行。
pub(super) fn decision_row_from_config(channel: &DecisionChannelConfig) -> DecisionRow {
    DecisionRow {
        key: channel.key.clone(),
        name: channel.name.clone(),
        mode: channel.mode.clone(),
        base_url: channel.base_url.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        model: channel.model.clone(),
        retry_count: channel.retry_count,
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置决策渠道（`api_key` 留空表示「不改动」，由调用方按 key 继承）。
pub(super) fn embedding_row_from_config(channel: &EmbeddingChannelConfig) -> EmbeddingRow {
    EmbeddingRow {
        key: channel.key.clone(),
        name: channel.name.clone(),
        base_url: channel.base_url.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        model: channel.model.clone(),
        dimensions: channel.dimensions,
        timeout_seconds: channel.timeout_seconds,
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置向量渠道（`api_key` 留空表示「不改动」，由调用方按 key 继承）。
pub(super) fn embedding_config_from_row(row: &EmbeddingRow) -> EmbeddingChannelConfig {
    EmbeddingChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model: row.model.trim().to_string(),
        dimensions: row.dimensions,
        batch_size: EmbeddingChannelConfig::default().batch_size,
        timeout_seconds: row.timeout_seconds,
        enabled: row.enabled,
    }
}

pub(super) fn decision_config_from_row(row: &DecisionRow) -> DecisionChannelConfig {
    DecisionChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        mode: row.mode.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model: row.model.trim().to_string(),
        retry_count: row.retry_count,
        enabled: row.enabled,
    }
}

pub(super) fn channel_row_from_config(channel: &ChannelConfig) -> ChannelRow {
    ChannelRow {
        key: channel.key.clone(),
        profile_id: channel.profile_id.clone(),
        name: channel.name.clone(),
        provider: channel.provider.clone(),
        protocol: channel.protocol.clone(),
        base_url: channel.base_url.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        model_id: channel.model_id.clone(),
        user_agent: channel.user_agent.clone(),
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置渠道。
///
/// `api_key` 只在面板里被改过（非空）时才是新值；留空表示「不改动」，由调用方按 key
/// 从磁盘上的同一条渠道继承（否则会把已配置的密钥抹掉）。
pub(super) fn channel_config_from_row(row: &ChannelRow) -> ChannelConfig {
    ChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        profile_id: row.profile_id.trim().to_string(),
        provider: row.provider.trim().to_string(),
        protocol: row.protocol.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model_id: row.model_id.trim().to_string(),
        enabled: row.enabled,
        user_agent: row.user_agent.trim().to_string(),
    }
}

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

/// 当前生效的上下文窗口：显式 CLI/环境值优先，其次配置文件。
pub(super) fn effective_context_window(explicit: Option<u64>, llm: &LlmConfig) -> i64 {
    match explicit {
        Some(value) if value > 0 => value as i64,
        _ => llm.context_window_tokens,
    }
}

/// 当前生效的原生视觉开关：命令行/环境显式开启优先，其次配置里当前模型的覆盖。
///
/// 配置里的值由 `load_llm_config` 按「模型条目覆盖 > 渠道 Profile 覆盖 > 未配置」解出，
/// 与 Python `_native_vision_enabled` 同口径；未配置时 Python 会回落运行时模型能力，
/// 而 TUI 没有能力表（见 crate README），因此按关闭处理。
///
/// `--base-url` 指向外部渠道时不再叠加配置：那份 `native_vision` 描述的是配置里选中的
/// 模型，而外部渠道跑的是命令行给的那个模型（与 Provider / 协议 / 生成选项同一口径）。
pub(super) fn effective_native_vision(external_channel: bool, explicit: bool, llm: &LlmConfig) -> bool {
    if explicit {
        return true;
    }
    if external_channel {
        return false;
    }
    llm.native_vision.unwrap_or(false)
}

/// 生成选项（`GenerationOptions` 的 JSON 形状）：只给配置里真正有值的字段。
///
/// 内核按 `GenerationOptions` 解析，结构体带 `#[serde(default)]`，缺字段取默认值，
/// 因此这里不必把每个字段都填满。
pub(super) fn generation_options(llm: &LlmConfig) -> Value {
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

/// 工具开关行：按 `TOOL_SWITCH_KEYS` 的顺序，注册状态来自当前工具表。
/// MCP 配置 → 设置面板初值。
pub(super) fn mcp_values_from_config(config: &omnicrawl_mcp::config::McpConfig) -> McpSettingsValues {
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
pub(super) fn mcp_config_data(config: &omnicrawl_mcp::config::McpConfig) -> McpConfigData {
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

/// 按写前快照还原配置文件；`None` 表示写之前文件不存在，那就把它删掉。
///
/// 还原失败只告警：调用方已经把「配置非法」这个真正的原因报给用户了，这里再抛错只会
/// 把它盖掉，而且磁盘上留着的也只是读取器拒绝的内容（加载时仍会报同一句错）。
pub(super) fn restore_config_file(path: &Path, snapshot: Option<&[u8]>) {
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

/// 编辑草稿 → 可写回的 Server 数据。
///
/// 环境变量与请求头在界面上是 `KEY=VALUE;…`：留空表示保持原值（凭据不在界面回显，
/// 因此不能因为一次编辑把它们抹掉）；参数按 `shlex.split(posix=True)` 切分，
/// 与 Python 编辑器同口径。
pub(super) fn mcp_server_data(
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

/// 解析 `KEY=VALUE;KEY2=VALUE` 形式的表；留空保持原值，格式错时拒绝保存。
pub(super) fn parse_key_value_list(
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

pub(super) fn tool_switch_rows(registry: &ToolRegistry) -> Vec<ToolSwitchRow> {
    let environment = ConfigEnvironment::from_process();
    let switches = load_tool_switches(&environment, None).unwrap_or_default();
    let registered = registered_tool_names(registry);
    TOOL_SWITCH_KEYS
        .iter()
        .map(|name| ToolSwitchRow {
            name: (*name).to_string(),
            label: tool_label(name),
            enabled: switches.get(*name).copied().unwrap_or(true),
            registered: registered.contains(*name),
        })
        .collect()
}

/// 已注册进工具表的工具名（声明里的 `function.name`）。
pub(super) fn registered_tool_names(registry: &ToolRegistry) -> std::collections::BTreeSet<String> {
    registry
        .declarations()
        .iter()
        .filter_map(|declaration| {
            declaration
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// 工具开关的中文名；未知名字回落为工具名本身。
pub(super) fn tool_label(name: &str) -> String {
    TOOL_SWITCH_LABELS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| name.to_string())
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
pub(super) fn compaction_settings(window: i64, percent: i64) -> KernelCompactionConfig {
    KernelCompactionConfig {
        trigger_context_tokens: Some(context_compaction_trigger_tokens(window, percent)),
        context_window_tokens: Some(window),
        ..KernelCompactionConfig::default()
    }
}

/// 压缩成功回执 → 界面消息（与命令层 `handle_compact_command` 的收尾文案一致）。
pub(super) fn compact_message(result: &Value) -> String {
    let compacted = result["compacted"].as_bool().unwrap_or(true);
    if !compacted {
        return "当前会话不需要压缩（上下文里还没有可压缩的轮次）。".to_string();
    }
    "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。".to_string()
}

/// 把内核的撤销结果渲染成一行界面消息。
pub(super) fn undo_message(result: &Value) -> String {
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

/// 可用角色（名称 + 定义里的一句话描述）：只有内核侧 SubAgent 启用时才有；宿主据此把
/// `subagent` 声明给模型，子代理建议也直接拿这份清单当候选。
///
/// 内置层按 [`AgentDefinitionRegistry::with_located_builtin`] 自行定位（与内核侧同一口径），
/// 否则内置角色一个都扫不到，`subagent` 的参数枚举与子代理建议的候选都会是空的。
pub(super) fn subagent_roles() -> Vec<(String, String)> {
    let env = ConfigEnvironment::from_process();
    let Ok(config) = load_subagent_config(&env, None) else {
        return Vec::new();
    };
    if !config.enabled {
        return Vec::new();
    }
    let mut registry =
        AgentDefinitionRegistry::with_located_builtin(Some(env.home().to_path_buf()));
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    registry.discover(&workspace, &[]);
    registry
        .list_all()
        .iter()
        .map(|item| (item.name.clone(), item.description.clone()))
        .collect()
}

/// Computer Use 是否进表：读 `[computer_use] enabled`，与 `[subagents]` 无关。
///
/// 读配置失败时按关闭处理：这是可选的桌面能力，配置坏了不该让整张工具表起不来。
pub(super) fn computer_use_enabled(environment: &ConfigEnvironment) -> bool {
    load_computer_use_config(environment, None)
        .map(|config| config.active())
        .unwrap_or(false)
}

/// TTS 工具配置：读 config.toml 的 `[tts]` 与 `[tts_cloud]`，未启用时不给这个工具
/// （与 Python 一致）。
pub(super) fn tts_options(workspace: &Path) -> Option<Arc<TtsOptions>> {
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
pub(super) fn tts_options_from(
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
    Some(Arc::new(TtsOptions::new(
        configuration.clone(),
        tts_backend(&cloud, openai, gemini),
        workspace.to_path_buf(),
    )))
}

/// TTS 设置页的初值：通用键 + 两条通道（界面按 `provider` 显示其中一家）。
pub(super) fn tts_settings_values(environment: &ConfigEnvironment) -> TtsValues {
    let general = load_tts_configuration(environment, None).unwrap_or_default();
    let cloud = load_tts_cloud_configuration(environment, None).unwrap_or_default();
    let openai = load_tts_api_configuration(environment, None).unwrap_or_default();
    let gemini = load_tts_gemini_configuration(environment, None).unwrap_or_default();
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
            api_key: openai.api_key,
        },
        gemini: TtsChannelValues {
            base_url: gemini.base_url,
            model: gemini.model,
            voice: gemini.voice,
            instructions: gemini.instructions,
            speed: String::new(),
            api_key: gemini.api_key,
        },
    }
}

/// 语速的显示文本：整数不带小数点（`1` 而不是 `1.0`），便于界面里来回编辑。
pub(super) fn format_speed(speed: f64) -> String {
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
pub(super) fn tts_channel_configurations(
    draft: &TtsDraft,
    previous_api: &TtsApiConfiguration,
    previous_gemini: &GeminiTtsConfiguration,
) -> Result<(TtsApiConfiguration, GeminiTtsConfiguration), String> {
    let mut api = previous_api.clone();
    let mut gemini = previous_gemini.clone();
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
    }
    Ok((api, gemini))
}

/// 装配 MCP 管理器：读配置、发现能力，失败只警告不阻断启动。
///
/// 协议 v1 的工具声明固定在 `initialize`，所以能力发现必须发生在握手之前——
/// Python 侧是「首次使用时懒加载」，Rust 宿主把它提前到启动期（见 crate README）。
pub(super) fn mcp_manager(workspace: &Path) -> Option<Arc<McpClientManager>> {
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
    manager.discover();
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
pub(super) fn is_retryable_failure(message: &str) -> bool {
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
