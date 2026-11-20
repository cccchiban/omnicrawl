//! 设置页的行构造：把配置折算成可渲染的行，以及反向把行写回配置。

use super::super::*;

/// 联网搜索页的初始投影：默认引擎 + 各凭据是否已配置（凭据本身不进界面状态）。
pub(crate) fn web_search_row(environment: &ConfigEnvironment) -> WebSearchRow {
    let configuration = load_web_search_configuration(environment, None).unwrap_or_default();
    WebSearchRow {
        engine: configuration.engine,
        tavily_configured: !configuration.tavily_api_key.trim().is_empty(),
        exa_configured: !configuration.exa_api_key.trim().is_empty(),
        firecrawl_configured: !configuration.firecrawl_api_key.trim().is_empty(),
        anysearch_configured: !configuration.anysearch_api_key.trim().is_empty(),
        tinyfish_configured: !configuration.tinyfish_api_key.trim().is_empty(),
        github_configured: !configuration.github_token.trim().is_empty(),
        deepseek_search_url: configuration.deepseek_search_url,
        deepseek_configured: !configuration.deepseek_api_key.trim().is_empty(),
        grok_search_url: configuration.grok_search_url,
        grok_configured: !configuration.grok_api_key.trim().is_empty(),
        grok_model: configuration.grok_model,
        keenable_configured: !configuration.keenable_api_key.trim().is_empty(),
    }
}

/// 抓取页的初始投影：`[fetcher] engine` 与失败回落开关。
pub(crate) fn fetcher_row(environment: &ConfigEnvironment) -> FetcherRow {
    let configuration =
        omnicrawl_config::features::fetcher::load_fetcher_configuration(environment, None)
            .unwrap_or_default();
    FetcherRow {
        engine: configuration.engine,
        fallback_local: configuration.fallback_local,
    }
}

/// 子任务设置页的行：总开关 + 六个高级参数（对映 Python 的 `_SubagentsPane.sections`）。
pub(crate) fn subagent_rows(config: &SubAgentConfig) -> Vec<SubagentRow> {
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
pub(crate) fn feature_label(key: &str) -> String {
    match key {
        "memory" => "记忆功能".to_string(),
        "knowledge" => "知识库".to_string(),
        other => other.to_string(),
    }
}

/// 决策模型功能开关的界面行：标签与顺序取自配置域的开关表，值取配置。
///
/// 只收「决策模型」页自己的开关（`SwitchPage::Decision`）；画在向量模型页的开关由
/// 向量页自己取用，不进这张表。
pub(crate) fn decision_switch_rows(switches: &DecisionSwitches) -> Vec<DecisionSwitchRow> {
    DECISION_SWITCHES
        .iter()
        .filter(|spec| spec.page == SwitchPage::Decision)
        .map(|spec| DecisionSwitchRow {
            key: spec.key.to_string(),
            label: spec.label.to_string(),
            enabled: switches
                .get(spec.key)
                .copied()
                .unwrap_or_else(|| decision_switch_default(spec.key)),
        })
        .collect()
}

/// 提问建议页下方列表的 Skill 行（`list_all()` 已按名称排好序）。
pub(crate) fn suggestion_skill_rows(
    metas: &[omnicrawl_extensions::skill::SkillMeta],
) -> Vec<SuggestionSkillRow> {
    metas
        .iter()
        .map(|meta| SuggestionSkillRow {
            name: meta.name.clone(),
            scope: meta.scope.clone(),
            description: meta.description.trim().to_string(),
        })
        .collect()
}

/// 配置决策渠道 → 决策页的界面行。
pub(crate) fn decision_row_from_config(channel: &DecisionChannelConfig) -> DecisionRow {
    DecisionRow {
        key: channel.key.clone(),
        name: channel.name.clone(),
        mode: channel.mode.clone(),
        base_url: channel.base_url.clone(),
        account_id: channel.account_id.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        model: channel.model.clone(),
        retry_count: channel.retry_count,
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置决策渠道（`api_key` 留空表示「不改动」，由调用方按 key 继承）。
pub(crate) fn embedding_row_from_config(channel: &EmbeddingChannelConfig) -> EmbeddingRow {
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
pub(crate) fn embedding_config_from_row(row: &EmbeddingRow) -> EmbeddingChannelConfig {
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

/// 配置重排渠道 → 界面行。
pub(crate) fn rerank_row_from_config(channel: &RerankChannelConfig) -> EmbeddingRow {
    EmbeddingRow {
        key: channel.key.clone(),
        name: channel.name.clone(),
        base_url: channel.base_url.clone(),
        // 密钥只随行传递（供掩码显示与保存时写回），渲染层只会拿到掩码后的文本。
        api_key: channel.api_key.clone(),
        model: channel.model.clone(),
        dimensions: 0,
        timeout_seconds: channel.timeout_seconds,
        enabled: channel.enabled,
    }
}

/// 界面行 → 配置重排渠道（`api_key` 留空表示「不改动」，由调用方按 key 继承）。
pub(crate) fn rerank_config_from_row(row: &EmbeddingRow) -> RerankChannelConfig {
    RerankChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model: row.model.trim().to_string(),
        timeout_seconds: row.timeout_seconds,
        enabled: row.enabled,
    }
}

pub(crate) fn decision_config_from_row(row: &DecisionRow) -> DecisionChannelConfig {
    DecisionChannelConfig {
        key: row.key.trim().to_string(),
        name: row.name.trim().to_string(),
        mode: row.mode.trim().to_string(),
        base_url: row.base_url.trim().to_string(),
        account_id: row.account_id.trim().to_string(),
        api_key: row.api_key.trim().to_string(),
        model: row.model.trim().to_string(),
        retry_count: row.retry_count,
        enabled: row.enabled,
    }
}

pub(crate) fn channel_row_from_config(channel: &ChannelConfig) -> ChannelRow {
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
pub(crate) fn channel_config_from_row(row: &ChannelRow) -> ChannelConfig {
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

pub(crate) fn tool_switch_rows(registry: &ToolRegistry) -> Vec<ToolSwitchRow> {
    let environment = ConfigEnvironment::from_process();
    let switches = load_tool_switches(&environment, None).unwrap_or_default();
    let registered = registered_tool_names(registry);
    TOOL_SWITCH_KEYS
        .iter()
        .map(|name| ToolSwitchRow {
            name: (*name).to_string(),
            enabled: switches.get(*name).copied().unwrap_or(true),
            registered: registered.contains(*name),
        })
        .collect()
}

/// 已注册进工具表的工具名（声明里的 `function.name`）。
pub(crate) fn registered_tool_names(registry: &ToolRegistry) -> std::collections::BTreeSet<String> {
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

/// 可用角色（名称 + 定义里的一句话描述）：只有内核侧 SubAgent 启用时才有；宿主据此把
/// `subagent` 声明给模型。
///
/// 内置层按 [`AgentDefinitionRegistry::with_located_builtin`] 自行定位（与内核侧同一口径），
/// 否则内置角色一个都扫不到，`subagent` 的参数枚举会是空的。
pub(crate) fn subagent_roles() -> Vec<(String, String)> {
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
    registry.discover(&workspace);
    registry
        .list_all()
        .iter()
        .map(|item| (item.name.clone(), item.description.clone()))
        .collect()
}
