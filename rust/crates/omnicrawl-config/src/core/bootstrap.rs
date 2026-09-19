//! 首次启动时创建用户配置并输出运行环境诊断（对应 `omnicrawl/config/core/bootstrap.py`）。
//!
//! 与 Python 的差别集中在端口化：模板资源、交互式输入、渠道向导、Node 探测与插件注册表
//! 都由 [`StartupPorts`] 注入，内核只保留编排与判定。

use std::path::{Path, PathBuf};

use crate::core::runtime::{
    absolute_path, expand_user, load_config_data, migrate_legacy_user_config,
    resolve_config_write_path, resolve_models_write_path, resolve_subagents_write_path,
    save_config_data, ConfigEnvironment, DEFAULT_CONFIG_FILENAME, DEFAULT_MODELS_FILENAME,
    DEFAULT_SUBAGENTS_FILENAME,
};
use crate::error::ConfigError;
use crate::models::model_store::{load_model_store, ModelStore};
use crate::toml::{Table, Value};
use crate::value::python_str;

/// Node.js 运行时探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeProbe {
    /// 未检测到 `node`。
    Missing,
    /// `node --version` 的输出（已 strip）与 npm 是否可用。
    Version {
        version: String,
        npm_available: bool,
    },
    /// 版本检查本身失败。
    Failed { error: String },
}

/// 插件注册表的一行（`extensions.plugin_install.list_plugins` 的投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    pub error: Option<String>,
    pub enabled: bool,
}

/// 渠道配置向导端口：写入渠道配置并返回是否完成。
pub type ChannelSetupFn<'a> = &'a dyn Fn(&Path, &Path) -> Result<bool, String>;

/// 交互式 API Key 输入端口。
pub type ApiKeyPromptFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 启动层需要的外部能力。
pub struct StartupPorts<'a> {
    /// 读取包内模板资源（`config/templates/<name>`）。
    pub read_template: &'a dyn Fn(&str) -> Result<String, String>,
    /// 渠道配置向导；`None` 表示该入口不支持。
    pub channel_setup: Option<ChannelSetupFn<'a>>,
    /// 交互式索取 API Key；`None` 表示无交互终端，按「直接跳过」处理。
    pub prompt: Option<ApiKeyPromptFn<'a>>,
    pub node_probe: &'a dyn Fn() -> NodeProbe,
    pub plugin_rows: &'a dyn Fn() -> Result<Vec<PluginRow>, String>,
}

/// 一次启动检查的可展示结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupCheck {
    pub name: String,
    pub status: String,
    pub message: String,
}

impl StartupCheck {
    fn new(name: &str, status: &str, message: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status: status.to_string(),
            message: message.into(),
        }
    }
}

/// 首次启动初始化与诊断的完整结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSetup {
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub models_path: PathBuf,
    pub subagents_path: PathBuf,
    pub config_created: bool,
    pub models_created: bool,
    pub subagents_created: bool,
    pub api_key_prompted: bool,
    pub api_key_configured: bool,
    pub checks: Vec<StartupCheck>,
    pub errors: Vec<String>,
}

impl StartupSetup {
    /// 本次是否发生了「首次运行」动作。
    pub fn first_run(&self) -> bool {
        self.config_created
            || self.models_created
            || self.subagents_created
            || self.api_key_prompted
    }
}

/// 创建配置模板、收集 API Key，并完成首次启动诊断。
pub fn initialize_user_configuration(
    env: &ConfigEnvironment,
    config_dir: Option<&Path>,
    ports: &StartupPorts<'_>,
) -> Result<StartupSetup, ConfigError> {
    let mut errors: Vec<String> = Vec::new();
    let (resolved_dir, config_path, models_path, subagents_path) = match config_dir {
        None => {
            if let Err(error) = migrate_legacy_user_config(env, None) {
                errors.push(error.message().to_string());
            }
            let config_path = resolve_config_write_path(env, None)?;
            let models_path = resolve_models_write_path(env, None)?;
            let subagents_path = resolve_subagents_write_path(env, None)?;
            let resolved_dir = config_path
                .parent()
                .map(|parent| parent.to_path_buf())
                .unwrap_or_default();
            (resolved_dir, config_path, models_path, subagents_path)
        }
        Some(dir) => {
            let resolved_dir = absolute_path(&expand_user(env, &dir.to_string_lossy()));
            (
                resolved_dir.clone(),
                resolved_dir.join(DEFAULT_CONFIG_FILENAME),
                resolved_dir.join(DEFAULT_MODELS_FILENAME),
                resolved_dir.join(DEFAULT_SUBAGENTS_FILENAME),
            )
        }
    };

    for path in [&config_path, &models_path, &subagents_path] {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ConfigError::new(format!("创建配置目录失败：{}，{error}", parent.display()))
            })?;
        }
    }

    let config_created = ensure_template(env, ports, &config_path, "config.example.toml")?;
    let models_created = ensure_template(env, ports, &models_path, "models.example.toml")?;
    let subagents_created = ensure_template(env, ports, &subagents_path, "subagents.example.toml")?;

    let mut config_data = Table::new();
    let mut model_store = ModelStore {
        version: 1,
        models: Vec::new(),
        path: Some(models_path.clone()),
    };
    match load_config_data(env, Some(&config_path)) {
        Ok(data) => config_data = data,
        Err(error) => errors.push(format!("运行配置读取失败：{}", error.message())),
    }
    match load_model_store(env, Some(&models_path)) {
        Ok(store) => model_store = store,
        Err(error) => errors.push(format!("模型配置读取失败：{}", error.message())),
    }

    let mut api_key_configured = false;
    let mut api_key_prompted = false;
    if errors.is_empty() {
        api_key_configured = active_api_key_configured(env, &config_data, &model_store);
        if !api_key_configured && ports.channel_setup.is_some() && ports.prompt.is_none() {
            api_key_prompted = true;
            let mut completed = false;
            if let Some(channel_setup) = ports.channel_setup {
                match channel_setup(&config_path, &models_path) {
                    Ok(flag) => completed = flag,
                    Err(error) => errors.push(format!("模型渠道配置失败：{error}")),
                }
            }
            if completed && errors.is_empty() {
                let mut config_ok = true;
                match load_config_data(env, Some(&config_path)) {
                    Ok(data) => config_data = data,
                    Err(error) => {
                        config_ok = false;
                        errors.push(format!("模型渠道配置读取失败：{}", error.message()));
                    }
                }
                if config_ok {
                    match load_model_store(env, Some(&models_path)) {
                        Ok(store) => {
                            model_store = store;
                            api_key_configured =
                                active_api_key_configured(env, &config_data, &model_store);
                        }
                        Err(error) => {
                            errors.push(format!("模型渠道配置读取失败：{}", error.message()))
                        }
                    }
                }
            }
        } else if !api_key_configured {
            let (configured, prompted, key_error) = ensure_api_key(
                env,
                &mut config_data,
                &model_store,
                &config_path,
                ports.prompt,
            );
            api_key_configured = configured;
            api_key_prompted = prompted;
            if let Some(error) = key_error {
                errors.push(error);
            }
        }
    }

    let checks = vec![
        check_model_config(&config_data, &model_store, &errors),
        check_node(ports.node_probe),
        check_plugin_state(&config_data, ports.plugin_rows),
    ];
    Ok(StartupSetup {
        config_dir: resolved_dir,
        config_path,
        models_path,
        subagents_path,
        config_created,
        models_created,
        subagents_created,
        api_key_prompted,
        api_key_configured,
        checks,
        errors,
    })
}

/// 把初始化结果转换为用户可直接执行的启动提示。
pub fn format_startup_report(setup: &StartupSetup) -> Vec<String> {
    if !setup.first_run() && setup.api_key_configured && setup.errors.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    if setup.config_created || setup.models_created || setup.subagents_created {
        lines.push(format!(
            "已准备用户配置目录：{}",
            setup.config_dir.display()
        ));
    }
    if setup.config_created {
        lines.push(format!("已生成运行配置：{}", setup.config_path.display()));
    }
    if setup.models_created {
        lines.push(format!("已生成模型配置：{}", setup.models_path.display()));
    }
    if setup.subagents_created {
        lines.push(format!(
            "已生成子代理设置：{}",
            setup.subagents_path.display()
        ));
    }

    for error in &setup.errors {
        lines.push(format!("[错误] {error}"));
    }
    for check in &setup.checks {
        let prefix = if check.status == "ok" {
            "通过"
        } else {
            "警告"
        };
        lines.push(format!("[{prefix}] {}：{}", check.name, check.message));
    }

    if !setup.api_key_configured {
        lines.push(
            "未完成模型渠道配置，本次不启动 TUI。请重新运行并保存至少一个可用渠道。".to_string(),
        );
    } else if setup.api_key_prompted {
        lines.push("模型渠道和 API Key 已保存到本机配置目录。".to_string());
    }
    lines
}

fn ensure_template(
    env: &ConfigEnvironment,
    ports: &StartupPorts<'_>,
    path: &Path,
    resource_name: &str,
) -> Result<bool, ConfigError> {
    if path.exists() {
        return Ok(false);
    }
    let text = (ports.read_template)(resource_name)
        .map_err(|error| ConfigError::new(format!("读取模板 {resource_name} 失败：{error}")))?;
    std::fs::write(path, text)
        .map_err(|error| ConfigError::new(format!("写入 {} 失败：{error}", path.display())))?;
    restrict_config_permissions(env, path);
    Ok(true)
}

/// 检查当前模型 Profile 是否已有直接 Key 或环境变量 Key。
fn active_api_key_configured(
    env: &ConfigEnvironment,
    config_data: &Table,
    model_store: &ModelStore,
) -> bool {
    let Some(llm) = config_data.get("llm").and_then(|value| value.as_table()) else {
        return false;
    };
    let Some(profiles) = llm.get("profiles").and_then(|value| value.as_table()) else {
        return false;
    };
    let profile_id = active_profile_id(llm, model_store);
    let Some(profile) = profiles.get(&profile_id).and_then(|value| value.as_table()) else {
        return false;
    };
    profile_has_key(env, profile)
}

fn profile_has_key(env: &ConfigEnvironment, profile: &Table) -> bool {
    let provider = profile_provider(profile);
    let env_name = profile_api_key_env(profile, &provider);
    let direct_key = python_str(profile.get("api_key")).trim().to_string();
    if !direct_key.is_empty() {
        return true;
    }
    !env_name.is_empty() && !env.get_trimmed(&env_name).is_empty()
}

fn profile_provider(profile: &Table) -> String {
    let provider = python_str(profile.get("provider")).trim().to_lowercase();
    if provider.is_empty() {
        "openai".to_string()
    } else {
        provider
    }
}

fn profile_api_key_env(profile: &Table, provider: &str) -> String {
    let name = python_str(profile.get("api_key_env")).trim().to_string();
    if name.is_empty() {
        default_api_key_env(provider)
    } else {
        name
    }
}

fn default_api_key_env(provider: &str) -> String {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY".to_string(),
        "gemini" => "GEMINI_API_KEY".to_string(),
        _ => "OPENAI_API_KEY".to_string(),
    }
}

/// 索取并写回 API Key，返回 `(是否已配置, 是否提示过, 错误)`。
fn ensure_api_key(
    env: &ConfigEnvironment,
    config_data: &mut Table,
    model_store: &ModelStore,
    config_path: &Path,
    prompt: Option<ApiKeyPromptFn<'_>>,
) -> (bool, bool, Option<String>) {
    let missing = |message: &str| (false, false, Some(message.to_string()));
    let Some(llm_value) = config_data.get("llm") else {
        return missing("配置缺少 llm 对象，无法确定 API Key 所属 Profile。");
    };
    let Some(llm) = llm_value.as_table() else {
        return missing("配置缺少 llm 对象，无法确定 API Key 所属 Profile。");
    };
    let profiles = match llm.get("profiles") {
        Some(Value::Table(profiles)) => profiles.clone(),
        _ => {
            return missing("配置缺少 llm.profiles，无法确定 API Key 所属 Profile。");
        }
    };

    let profile_id = active_profile_id(llm, model_store);
    let Some(profile) = profiles.get(&profile_id).and_then(|value| value.as_table()) else {
        return (
            false,
            false,
            Some(format!(
                "当前模型引用的 Profile 不存在：{}。",
                if profile_id.is_empty() {
                    "(空)".to_string()
                } else {
                    profile_id
                }
            )),
        );
    };
    if profile_has_key(env, profile) {
        return (true, false, None);
    }

    let provider = profile_provider(profile);
    let env_name = profile_api_key_env(profile, &provider);
    let label = if env_name.is_empty() {
        "当前模型 API Key"
    } else {
        env_name.as_str()
    };
    let value = match prompt {
        Some(prompt) => prompt(&format!("请输入 {label}（输入不会回显，直接回车跳过）：")),
        None => None,
    };
    let value = value.unwrap_or_default().trim().to_string();
    if value.is_empty() {
        return (false, true, None);
    }

    let Some(Value::Table(profiles_mut)) = config_data
        .get_mut("llm")
        .and_then(|value| value.as_table_mut())
        .and_then(|llm| llm.get_mut("profiles"))
    else {
        return missing("配置缺少 llm.profiles，无法确定 API Key 所属 Profile。");
    };
    let Some(Value::Table(profile_mut)) = profiles_mut.get_mut(&profile_id) else {
        return missing("配置缺少 llm.profiles，无法确定 API Key 所属 Profile。");
    };
    profile_mut.insert("api_key".to_string(), Value::String(value));
    if let Err(error) = save_config_data(env, config_data, Some(config_path)) {
        return (
            false,
            true,
            Some(format!("API Key 保存失败：{}", error.message())),
        );
    }
    restrict_config_permissions(env, config_path);
    (true, true, None)
}

/// 当前模型引用的 Profile id。
fn active_profile_id(llm: &Table, model_store: &ModelStore) -> String {
    if let Some(active) = llm.get("active_model").and_then(|value| value.as_table()) {
        let explicit_profile = python_str(active.get("profile")).trim().to_string();
        if !explicit_profile.is_empty() {
            return explicit_profile;
        }
        let key = python_str(active.get("key")).trim().to_string();
        if !key.is_empty() {
            if let Ok(Some(record)) = model_store.resolve_alias(&key) {
                return record.profile.clone();
            }
        }
    }

    if let Some(profiles) = llm.get("profiles").and_then(|value| value.as_table()) {
        for (profile_id, profile) in profiles {
            let enabled = match profile.as_table().and_then(|table| table.get("enabled")) {
                Some(Value::Boolean(flag)) => *flag,
                _ => true,
            };
            if profile.as_table().is_some() && enabled {
                return profile_id.clone();
            }
        }
    }
    String::new()
}

fn check_model_config(
    config_data: &Table,
    model_store: &ModelStore,
    errors: &[String],
) -> StartupCheck {
    if !errors.is_empty() {
        return StartupCheck::new("模型配置", "warning", "配置文件存在错误，请修复后重试。");
    }
    if model_store.models.is_empty() {
        return StartupCheck::new("模型配置", "warning", "models.toml 中没有可用模型。");
    }

    let key = config_data
        .get("llm")
        .and_then(|value| value.as_table())
        .and_then(|llm| llm.get("active_model"))
        .and_then(|value| value.as_table())
        .map(|active| python_str(active.get("key")))
        .unwrap_or_default();
    let record = match model_store.resolve_alias(&key) {
        Ok(record) => record,
        Err(error) => return StartupCheck::new("模型配置", "warning", error.message()),
    };
    let Some(record) = record else {
        return StartupCheck::new(
            "模型配置",
            "warning",
            format!(
                "找不到默认模型：{}。",
                if key.is_empty() {
                    "(空)"
                } else {
                    key.as_str()
                }
            ),
        );
    };
    StartupCheck::new(
        "模型配置",
        "ok",
        format!("默认模型：{} ({})", record.display_name, record.model_id),
    )
}

/// Node.js 可用性判定（探测结果由端口给出，便于启动诊断与测试复用）。
pub fn check_node(probe: &dyn Fn() -> NodeProbe) -> StartupCheck {
    match probe() {
        NodeProbe::Missing => {
            StartupCheck::new("Node.js", "warning", "未检测到 Node.js；插件功能暂不可用。")
        }
        NodeProbe::Failed { error } => {
            StartupCheck::new("Node.js", "warning", format!("版本检查失败：{error}"))
        }
        NodeProbe::Version {
            version,
            npm_available,
        } => {
            let major = version
                .trim_start_matches('v')
                .split('.')
                .next()
                .unwrap_or_default()
                .parse::<i64>();
            let Ok(major) = major else {
                let head = version
                    .trim_start_matches('v')
                    .split('.')
                    .next()
                    .unwrap_or_default();
                return StartupCheck::new(
                    "Node.js",
                    "warning",
                    format!("版本检查失败：invalid literal for int() with base 10: '{head}'"),
                );
            };
            if major < 20 {
                return StartupCheck::new(
                    "Node.js",
                    "warning",
                    format!("需要 Node.js >=20，当前为 {version}。"),
                );
            }
            if !npm_available {
                return StartupCheck::new(
                    "Node.js",
                    "warning",
                    format!("Node.js {version} 可用，但未找到 npm。"),
                );
            }
            StartupCheck::new("Node.js", "ok", format!("Node.js {version} / npm 可用。"))
        }
    }
}

/// 插件注册表状态判定（注册表读取由端口给出，避免启动诊断被外部依赖阻塞）。
pub fn check_plugin_state(
    config_data: &Table,
    plugin_rows: &dyn Fn() -> Result<Vec<PluginRow>, String>,
) -> StartupCheck {
    let enabled = config_data
        .get("plugins")
        .and_then(|value| value.as_table())
        .and_then(|plugins| plugins.get("enabled"))
        .map(crate::value::truthy)
        .unwrap_or(false);
    let rows = match plugin_rows() {
        Ok(rows) => rows,
        Err(error) => {
            return StartupCheck::new(
                "插件状态",
                "warning",
                format!("读取插件注册表失败：{error}"),
            )
        }
    };

    let errors: Vec<String> = rows.iter().filter_map(|row| row.error.clone()).collect();
    let enabled_count = rows.iter().filter(|row| row.enabled).count();
    let state = if enabled { "已启用" } else { "已禁用" };
    if !errors.is_empty() {
        return StartupCheck::new("插件状态", "warning", errors.join("; "));
    }
    StartupCheck::new(
        "插件状态",
        "ok",
        format!(
            "插件系统{state}，已注册 {} 个插件，当前启用 {enabled_count} 个。",
            rows.len()
        ),
    )
}

/// 在支持 POSIX 权限的系统上限制配置文件读取权限。
fn restrict_config_permissions(env: &ConfigEnvironment, path: &Path) {
    #[cfg(unix)]
    {
        if env.platform().starts_with("win") {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = (env, path);
    }
}
