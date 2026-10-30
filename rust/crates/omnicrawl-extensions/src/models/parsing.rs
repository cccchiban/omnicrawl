//! 从 JSON 解析出配置与清单：字段校验在此就地完成。
//!
//! 从 `models.rs` 按职责拆出。

use super::*;

/// 从 config.toml 的 plugins 段构造配置；缺省时默认关闭插件。
pub fn parse_plugins_config(data: Option<&Value>) -> Result<PluginsConfig, PluginError> {
    let value = match data {
        Some(item) if !is_falsy(item) => item,
        _ => return Ok(PluginsConfig::default()),
    };
    let Some(map) = value.as_object() else {
        return Err(PluginError::new("plugins 配置必须是 JSON 对象。"));
    };
    PluginsConfig {
        enabled: bool_of(map.get("enabled"), false),
        default_timeout_ms: int_alias(map, "default_timeout_ms", "defaultTimeoutMs", 1000),
        max_timeout_ms: int_alias(
            map,
            "max_timeout_ms",
            "maxTimeoutMs",
            DEFAULT_MAX_TIMEOUT_MS,
        ),
        failure_threshold: int_alias(
            map,
            "failure_threshold",
            "failureThreshold",
            DEFAULT_FAILURE_THRESHOLD,
        ),
        max_message_bytes: int_alias(
            map,
            "max_message_bytes",
            "maxMessageBytes",
            DEFAULT_MAX_MESSAGE_BYTES,
        ),
        custom_event_max_depth: int_alias(
            map,
            "custom_event_max_depth",
            "customEventMaxDepth",
            DEFAULT_CUSTOM_EVENT_MAX_DEPTH,
        ),
        allow_network_install: bool_alias(
            map,
            "allow_network_install",
            "allowNetworkInstall",
            false,
        ),
        audit_log_enabled: bool_alias(map, "audit_log_enabled", "auditLogEnabled", true),
    }
    .validate()
}

/// 解析一个 Handler 声明。
pub fn parse_handler_registration(raw: &Value) -> Result<HandlerRegistration, PluginManifestError> {
    let Some(map) = raw.as_object() else {
        return Err(PluginManifestError::new("hooks 项必须是对象。"));
    };
    let handler_id = text_of(map.get("id")).trim().to_string();
    let hook = text_of(map.get("hook")).trim().to_string();
    let mode = text_of(map.get("mode")).trim().to_string();
    if !is_valid_handler_id(&handler_id) {
        return Err(PluginManifestError::new(format!(
            "非法 Handler ID：{handler_id}"
        )));
    }
    if !HANDLER_MODES.contains(&mode.as_str()) {
        return Err(PluginManifestError::new(format!(
            "Handler {handler_id} mode 非法：{mode}"
        )));
    }
    if CORE_HOOKS.contains(&hook.as_str()) {
        if !hook_allowed_modes(&hook).contains(&mode.as_str()) {
            return Err(PluginManifestError::new(format!(
                "Hook {hook} 不支持 mode={mode}"
            )));
        }
    } else if hook.starts_with("plugin.") {
        if !is_valid_custom_event(&hook) {
            return Err(PluginManifestError::new(format!(
                "自定义 Hook 名称非法：{hook}"
            )));
        }
    } else {
        return Err(PluginManifestError::new(format!("未知 Hook：{hook}")));
    }

    let priority = int_of(map.get("priority"), 0);
    let replaces = match map.get("replaces") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| text_of(Some(item)).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect(),
        Some(_) => {
            return Err(PluginManifestError::new(format!(
                "Handler {handler_id} replaces 必须是数组。"
            )))
        }
    };
    for target in &replaces {
        if target.starts_with(SEALED_HANDLER_PREFIX) {
            return Err(PluginManifestError::new(format!(
                "不能替换 sealed Handler：{target}"
            )));
        }
        if !target.contains('/') {
            return Err(PluginManifestError::new(format!(
                "replaces 必须是 package/handler 形式：{target}"
            )));
        }
    }

    let timeout_ms = match map.get("timeoutMs").or_else(|| map.get("timeout_ms")) {
        Some(value) if !value.is_null() => Some(int_of(Some(value), 0)),
        _ => None,
    };
    let event_version = match map.get("eventVersion").or_else(|| map.get("event_version")) {
        Some(value) if !value.is_null() => Some(text_of(Some(value))),
        _ => None,
    };
    Ok(HandlerRegistration {
        id: handler_id,
        hook,
        mode,
        priority,
        replaces,
        event_version,
        timeout_ms,
    })
}

/// 解析一个自定义事件声明。
pub fn parse_custom_event(
    raw: &Value,
    package_name: &str,
) -> Result<CustomEventDeclaration, PluginManifestError> {
    let Some(map) = raw.as_object() else {
        return Err(PluginManifestError::new("customEvents 项必须是对象。"));
    };
    let name = text_of(map.get("name")).trim().to_string();
    let namespace = normalize_plugin_namespace(package_name);
    let expected_prefix = format!("plugin.{namespace}.");
    if !name.starts_with(&expected_prefix) {
        return Err(PluginManifestError::new(format!(
            "自定义事件必须位于本插件命名空间 {expected_prefix}*，当前：{name}"
        )));
    }
    if !is_valid_custom_event(&name) {
        return Err(PluginManifestError::new(format!(
            "自定义事件名称非法：{name}"
        )));
    }
    let version = int_of(map.get("version"), 1);
    if version < 1 {
        return Err(PluginManifestError::new(format!(
            "自定义事件版本必须 >= 1：{name}"
        )));
    }
    let visibility = match map.get("visibility") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => "private".to_string(),
    };
    if visibility != "private" && visibility != "public" {
        return Err(PluginManifestError::new(format!(
            "自定义事件 visibility 非法：{visibility}"
        )));
    }
    let schema = match map.get("schema") {
        Some(Value::Object(value)) => value.clone(),
        None | Some(Value::Null) => {
            let mut default_schema = Map::new();
            default_schema.insert("type".to_string(), Value::from("object"));
            default_schema
        }
        Some(_) => {
            return Err(PluginManifestError::new(format!(
                "自定义事件 schema 必须是对象：{name}"
            )))
        }
    };
    Ok(CustomEventDeclaration {
        name,
        version,
        visibility,
        schema,
    })
}

/// 解析并校验 package.json 中的 omnicrawl 扩展元数据。
pub fn parse_plugin_manifest(
    package_data: &Value,
    source_path: &str,
) -> Result<PluginManifest, PluginManifestError> {
    let Some(data) = package_data.as_object() else {
        return Err(PluginManifestError::new("package.json 顶层必须是对象。"));
    };
    let name = text_of(data.get("name")).trim().to_string();
    let version = text_of(data.get("version")).trim().to_string();
    if !is_valid_npm_name(&name) {
        return Err(PluginManifestError::new(format!("非法 NPM 包名：{name}")));
    }
    if !is_valid_semver(&version) {
        return Err(PluginManifestError::new(format!("非法 SemVer：{version}")));
    }

    let Some(omnicrawl) = data.get("omnicrawl").and_then(Value::as_object) else {
        return Err(PluginManifestError::new("缺少 omnicrawl 扩展字段。"));
    };

    let api_version = alias_text(omnicrawl, "apiVersion", "api_version")
        .trim()
        .to_string();
    if api_version != HOOK_API_VERSION {
        return Err(PluginManifestError::new(format!(
            "不支持的 apiVersion：{api_version}，当前仅接受 {HOOK_API_VERSION}。"
        )));
    }

    let engines = match omnicrawl.get("engines") {
        Some(Value::Object(value)) => value.clone(),
        None | Some(Value::Null) => Map::new(),
        Some(_) => {
            return Err(PluginManifestError::new("omnicrawl.engines 必须是对象。"));
        }
    };
    let engines_omnicrawl = text_of(engines.get("omnicrawl")).trim().to_string();
    let engines_node = text_of(engines.get("node")).trim().to_string();
    if engines_omnicrawl.is_empty() {
        return Err(PluginManifestError::new(
            "缺少 omnicrawl.engines.omnicrawl。",
        ));
    }
    if engines_node.is_empty() {
        return Err(PluginManifestError::new("缺少 omnicrawl.engines.node。"));
    }

    let entry = match omnicrawl.get("entry") {
        Some(value) => text_of(Some(value)).trim().to_string(),
        None => text_of(data.get("main")).trim().to_string(),
    };
    if entry.is_empty() {
        return Err(PluginManifestError::new("缺少 omnicrawl.entry。"));
    }
    let normalized_entry = entry.replace('\\', "/");
    if entry.starts_with('/')
        || entry.starts_with('\\')
        || normalized_entry.split('/').any(|part| part == "..")
    {
        return Err(PluginManifestError::new(format!(
            "entry 路径非法或不允许逃逸：{entry}"
        )));
    }

    let permissions = match omnicrawl.get("permissions") {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| text_of(Some(item)).trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<String>>(),
        _ => {
            return Err(PluginManifestError::new(
                "omnicrawl.permissions 必须是非空数组。",
            ));
        }
    };

    let agents_raw = match omnicrawl.get("agents") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(PluginManifestError::new(
                "omnicrawl.agents 必须是包内 Markdown 相对路径数组。",
            ));
        }
    };
    if !agents_raw.iter().all(Value::is_string) {
        return Err(PluginManifestError::new(
            "omnicrawl.agents 必须是包内 Markdown 相对路径数组。",
        ));
    }
    let mut agents: Vec<String> = Vec::new();
    for raw_path in &agents_raw {
        let raw_text = raw_path.as_str().unwrap_or_default();
        let normalized = raw_text.trim().replace('\\', "/");
        let parts: Vec<&str> = normalized.split('/').collect();
        let invalid = normalized.is_empty()
            || normalized.starts_with('/')
            || parts[0].contains(':')
            || parts.contains(&"..")
            || !normalized.to_lowercase().ends_with(".md");
        if invalid {
            return Err(PluginManifestError::new(format!(
                "omnicrawl.agents 路径非法或不允许逃逸：{raw_text}"
            )));
        }
        if agents.contains(&normalized) {
            return Err(PluginManifestError::new(format!(
                "omnicrawl.agents 不允许重复路径：{normalized}"
            )));
        }
        agents.push(normalized);
    }
    if !agents.is_empty() && !permissions.iter().any(|item| item == "agent:definitions") {
        return Err(PluginManifestError::new(
            "声明 omnicrawl.agents 需要 agent:definitions 权限。",
        ));
    }

    let hooks_raw = match omnicrawl.get("hooks") {
        Some(Value::Array(items)) => items.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            return Err(PluginManifestError::new("omnicrawl.hooks 必须是数组。"));
        }
    };
    if hooks_raw.is_empty() && agents.is_empty() {
        return Err(PluginManifestError::new(
            "omnicrawl.hooks 必须是非空数组，或声明 omnicrawl.agents。",
        ));
    }
    let mut hooks: Vec<HandlerRegistration> = Vec::new();
    for item in &hooks_raw {
        hooks.push(parse_handler_registration(item)?);
    }
    let mut handler_ids: Vec<&str> = hooks.iter().map(|item| item.id.as_str()).collect();
    handler_ids.sort_unstable();
    for pair in handler_ids.windows(2) {
        if pair[0] == pair[1] {
            return Err(PluginManifestError::new("Handler ID 在包内必须唯一。"));
        }
    }

    for handler in &hooks {
        let required_perm = format!("hook:{}", handler.hook);
        if handler.hook.starts_with("plugin.") {
            // 订阅自定义 Hook 使用 hook:custom-subscribe；发布权限另计。
            if !permissions
                .iter()
                .any(|item| item == "hook:custom-subscribe" || *item == required_perm)
            {
                return Err(PluginManifestError::new(format!(
                    "Handler {} 订阅自定义 Hook 需要 hook:custom-subscribe 权限。",
                    handler.id
                )));
            }
        } else if !permissions.contains(&required_perm) {
            return Err(PluginManifestError::new(format!(
                "Handler {} 注册 {} 需要权限 {required_perm}。",
                handler.id, handler.hook
            )));
        }
    }

    let custom_events_raw = match omnicrawl
        .get("customEvents")
        .or_else(|| omnicrawl.get("custom_events"))
    {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err(PluginManifestError::new(
                "omnicrawl.customEvents 必须是数组。",
            ));
        }
    };
    let mut custom_events: Vec<CustomEventDeclaration> = Vec::new();
    if !custom_events_raw.is_empty() {
        if !permissions.iter().any(|item| item == "hook:custom-emit") {
            return Err(PluginManifestError::new(
                "声明 customEvents 需要 hook:custom-emit 权限。",
            ));
        }
        for item in &custom_events_raw {
            custom_events.push(parse_custom_event(item, &name)?);
        }
    }

    let timeout_ms = match omnicrawl
        .get("timeoutMs")
        .or_else(|| omnicrawl.get("timeout_ms"))
    {
        Some(value) if !value.is_null() => Some(int_of(Some(value), 0)),
        _ => None,
    };
    let package_type = {
        let text = text_of(data.get("type")).trim().to_string();
        if text.is_empty() {
            "module".to_string()
        } else {
            text
        }
    };

    Ok(PluginManifest {
        name,
        version,
        api_version,
        entry: normalized_entry,
        permissions,
        hooks,
        engines_omnicrawl,
        engines_node,
        timeout_ms,
        custom_events,
        agents,
        package_type,
        source_path: source_path.to_string(),
    })
}
