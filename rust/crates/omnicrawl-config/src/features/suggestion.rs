//! 提问建议配置（`config.toml` 的 `[suggestion]` 段）。
//!
//! 三类建议（工具、子代理、Skill）在用户发来新提问时，用本地 BM25 从候选里挑出贴合
//! 本次提问的几个名字，拼在 user 尾部当提示。全部默认关闭：建议只是提示，不该默认
//! 改变发出去的文本。
//!
//! 这三项原先挂在 `decision_models.toml` 的 `[features]` 下，走决策模型判定。决策模型
//! 一次往返是 70–500ms 量级，而建议发生在**每一轮提问之前**，这个开销压在最敏感的路径上。
//! 现在改成纯本地 BM25 匹配（毫秒级、不出网、不依赖任何渠道配置），配置也随之独立成段，
//! 不再与决策模型共用一张开关表。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

/// 配置段名。
pub const SUGGESTION_SECTION: &str = "suggestion";

/// 「新提问自动建议工具」的配置键。
pub const SUGGESTION_KEY_TOOLS: &str = "tools";

/// 「新提问自动建议子代理」的配置键。
pub const SUGGESTION_KEY_SUBAGENTS: &str = "subagents";

/// 「新提问自动建议 Skill」的配置键。
pub const SUGGESTION_KEY_SKILLS: &str = "skills";

/// 一个建议开关：配置键、界面文案、默认值、旧配置键。
///
/// `legacy_key` 是它在 `decision_models.toml [features]` 里的旧键名——键名换过（`tools` 原先
/// 叫 `tool_suggestion`），升级时按旧键读一次，免得用户已开启的建议被静默关掉。
pub struct SuggestionSwitchSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub default: bool,
    legacy_key: &'static str,
}

/// 全部建议开关（顺序即界面顺序）。
pub const SUGGESTION_SWITCHES: [SuggestionSwitchSpec; 3] = [
    SuggestionSwitchSpec {
        key: SUGGESTION_KEY_TOOLS,
        label: "建议工具",
        default: false,
        legacy_key: "tool_suggestion",
    },
    SuggestionSwitchSpec {
        key: SUGGESTION_KEY_SUBAGENTS,
        label: "建议子代理",
        default: false,
        legacy_key: "subagent_suggestion",
    },
    SuggestionSwitchSpec {
        key: SUGGESTION_KEY_SKILLS,
        label: "建议 Skill",
        default: false,
        legacy_key: "skill_suggestion",
    },
];

/// 提问建议的三个开关。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuggestionConfig {
    pub tools: bool,
    pub subagents: bool,
    pub skills: bool,
}

impl SuggestionConfig {
    /// 按开关键取值；未知键回落 `false`。
    pub fn get(&self, key: &str) -> bool {
        match key {
            SUGGESTION_KEY_TOOLS => self.tools,
            SUGGESTION_KEY_SUBAGENTS => self.subagents,
            SUGGESTION_KEY_SKILLS => self.skills,
            _ => false,
        }
    }

    /// 按开关键改值；未知键不生效。
    pub fn set(&mut self, key: &str, enabled: bool) {
        match key {
            SUGGESTION_KEY_TOOLS => self.tools = enabled,
            SUGGESTION_KEY_SUBAGENTS => self.subagents = enabled,
            SUGGESTION_KEY_SKILLS => self.skills = enabled,
            _ => {}
        }
    }

    /// 三个开关都关着时无事可做。
    pub fn any_enabled(&self) -> bool {
        self.tools || self.subagents || self.skills
    }
}

impl SuggestionSwitchSpec {
    /// 该键在配置里的取值。
    fn value(&self, config: &SuggestionConfig) -> bool {
        config.get(self.key)
    }
}

/// 读取 `[suggestion]` 段；缺段、缺键时回落到默认值（关闭）。
///
/// 旧配置把这三个开关写在 `decision_models.toml` 的 `[features]` 下，键名相同、段不同。
/// 本段缺键时沿用旧段的同名取值，避免升级后用户已开启的建议被静默改成关闭。
pub fn load_suggestion_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> SuggestionConfig {
    let mut config = SuggestionConfig::default();
    let section = match load_config_data(env, config_path) {
        Ok(data) => get_section(&data, SUGGESTION_SECTION).unwrap_or_default(),
        Err(_) => Table::new(),
    };
    let legacy = legacy_switches(env);
    for spec in SUGGESTION_SWITCHES {
        let enabled = match section.get(spec.key) {
            Some(value) => truthy(value),
            None => legacy
                .get(spec.key)
                .copied()
                .unwrap_or(spec.default),
        };
        config.set(spec.key, enabled);
    }
    config
}

/// 旧位置（`decision_models.toml [features]`）里这三个键的取值。
///
/// 本段缺键时用它补上，避免升级后用户已开启的建议被静默改成关闭；旧文件不存在时
/// 拿到空表，与默认行为（全关）一致。旧键已从决策开关表里移除，因此这里直接读旧文件的
/// `[features]` 段。
fn legacy_switches(env: &ConfigEnvironment) -> std::collections::BTreeMap<String, bool> {
    use crate::core::runtime::resolve_decision_models_path;
    use crate::features::decision_model::DECISION_FEATURES_SECTION;
    let Ok(path) = resolve_decision_models_path(env, None) else {
        return Default::default();
    };
    let Ok(data) = load_config_data(env, Some(&path)) else {
        return Default::default();
    };
    let Ok(section) = get_section(&data, DECISION_FEATURES_SECTION) else {
        return Default::default();
    };
    SUGGESTION_SWITCHES
        .iter()
        .filter_map(|spec| {
            section
                .get(spec.legacy_key)
                .map(|value| (spec.key.to_string(), truthy(value)))
        })
        .collect()
}

/// 把整段建议开关写回 `config.toml`（保留其他段与键）。
pub fn save_suggestion_config(
    env: &ConfigEnvironment,
    config: &SuggestionConfig,
    config_path: Option<&Path>,
) -> Result<std::path::PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    for spec in SUGGESTION_SWITCHES {
        section.insert(spec.key.to_string(), Value::Boolean(spec.value(config)));
    }
    data.insert(SUGGESTION_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 写回单个开关（其余两项取传入配置里的值，保持不变）。
pub fn save_suggestion_switch(
    env: &ConfigEnvironment,
    config: &mut SuggestionConfig,
    key: &str,
    enabled: bool,
    config_path: Option<&Path>,
) -> Result<std::path::PathBuf, ConfigError> {
    if !SUGGESTION_SWITCHES.iter().any(|spec| spec.key == key) {
        return Err(ConfigError::new(format!("未知的提问建议开关：{key}。")));
    }
    config.set(key, enabled);
    save_suggestion_config(env, config, config_path)
}

/// Python `bool(...)` 的取值口径（与决策开关一致）。
fn truthy(value: &Value) -> bool {
    crate::value::truthy(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_env(name: &str) -> (std::path::PathBuf, ConfigEnvironment) {
        let home = std::env::temp_dir().join(format!(
            "omnicrawl-suggestion-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("建临时目录");
        let env = ConfigEnvironment::new(&home, "windows");
        (home, env)
    }

    #[test]
    fn defaults_to_everything_off() {
        let (root, env) = temp_env("defaults");
        let config = load_suggestion_config(&env, Some(&root.join("config.toml")));
        assert!(!config.any_enabled());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_switch_survives_a_round_trip() {
        let (root, env) = temp_env("roundtrip");
        let path = root.join("config.toml");
        let mut config = SuggestionConfig::default();
        config.set(SUGGESTION_KEY_TOOLS, true);
        config.set(SUGGESTION_KEY_SKILLS, true);
        save_suggestion_config(&env, &config, Some(&path)).expect("写盘");

        let reloaded = load_suggestion_config(&env, Some(&path));
        assert!(reloaded.tools, "工具建议应保持开启");
        assert!(!reloaded.subagents);
        assert!(reloaded.skills, "Skill 建议应保持开启");
        let _ = fs::remove_dir_all(&root);
    }

    /// 旧配置写在 decision_models 的 `[features]` 下：升级后不该被静默关掉。
    #[test]
    fn legacy_decision_switches_are_carried_over() {
        let (root, env) = temp_env("legacy");
        // 直接按旧版形状写盘：旧键已从决策开关表移除，只能用原始 TOML 造这条迁移用例。
        let legacy_path =
            crate::core::runtime::resolve_decision_models_path(&env, None).expect("旧文件路径");
        if let Some(parent) = legacy_path.parent() {
            fs::create_dir_all(parent).expect("建配置目录");
        }
        fs::write(
            &legacy_path,
            "[features]\ntool_suggestion = true\nsubagent_suggestion = false\n",
        )
        .expect("写旧开关");

        let config = load_suggestion_config(&env, None);
        assert!(config.tools, "旧位置的开启状态应沿用到新段");
        assert!(!config.subagents);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unknown_key_is_rejected_on_write() {
        let (root, env) = temp_env("unknown");
        let mut config = SuggestionConfig::default();
        let error = save_suggestion_switch(&env, &mut config, "nope", true, None)
            .expect_err("未知键应被拒绝");
        assert!(error.message().contains("nope"), "{}", error.message());
        let _ = fs::remove_dir_all(&root);
    }
}
