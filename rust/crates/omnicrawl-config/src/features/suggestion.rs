//! 提问建议配置（`config.toml` 的 `[suggestion]` 段）。
//!
//! 用户发来新提问时，先由主模型把提问识别成「技能词」，再用本地 BM25 从本机 Skill 索引里
//! 挑出贴合本次提问的几个名字，拼在 user 尾部当提示。默认关闭：建议只是提示，不该默认
//! 改变发出去的文本。
//!
//! 这一项原先挂在 `decision_models.toml` 的 `[features]` 下，走决策模型判定。决策模型一次
//! 往返是 70–500ms 量级，而建议发生在**每一轮提问之前**，这个开销压在最敏感的路径上。
//! 现在关键词识别走主模型渠道（那条渠道本来就要收到这条提问），检索纯本地（毫秒级），
//! 配置也随之独立成段，不再与决策模型共用一张开关表。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::{Table, Value};

/// 配置段名。
pub const SUGGESTION_SECTION: &str = "suggestion";

/// 「新提问自动建议 Skill」的配置键。
pub const SUGGESTION_KEY_SKILLS: &str = "skills";

/// 该开关在界面上的文案。
pub const SUGGESTION_SKILLS_LABEL: &str = "建议 Skill";

/// 该键在旧位置（`decision_models.toml [features]`）里的名字。
const LEGACY_KEY_SKILLS: &str = "skill_suggestion";

/// 提问建议的开关。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuggestionConfig {
    pub skills: bool,
}

impl SuggestionConfig {
    /// 是否有建议需要算（唯一的开关关着时无事可做）。
    pub fn enabled(&self) -> bool {
        self.skills
    }
}

/// 读取 `[suggestion]` 段；缺段、缺键时回落到默认值（关闭）。
///
/// 旧配置把这个开关写在 `decision_models.toml` 的 `[features]` 下，键名不同（`skill_suggestion`）。
/// 本段缺键时沿用旧段的取值，避免升级后用户已开启的建议被静默改成关闭。
pub fn load_suggestion_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> SuggestionConfig {
    let section = match load_config_data(env, config_path) {
        Ok(data) => get_section(&data, SUGGESTION_SECTION).unwrap_or_default(),
        Err(_) => Table::new(),
    };
    let skills = match section.get(SUGGESTION_KEY_SKILLS) {
        Some(value) => truthy(value),
        None => legacy_switch(env).unwrap_or(false),
    };
    SuggestionConfig { skills }
}

/// 旧位置（`decision_models.toml [features]`）里该开关的取值。
///
/// 旧文件不存在或没有这个键时返回 `None`，与默认行为（关闭）一致。
fn legacy_switch(env: &ConfigEnvironment) -> Option<bool> {
    use crate::core::runtime::resolve_decision_models_path;
    use crate::features::decision_model::DECISION_FEATURES_SECTION;
    let path = resolve_decision_models_path(env, None).ok()?;
    let data = load_config_data(env, Some(&path)).ok()?;
    let section = get_section(&data, DECISION_FEATURES_SECTION).ok()?;
    section.get(LEGACY_KEY_SKILLS).map(truthy)
}

/// 把开关写回 `config.toml`（保留其他段与键）。
pub fn save_suggestion_config(
    env: &ConfigEnvironment,
    config: &SuggestionConfig,
    config_path: Option<&Path>,
) -> Result<std::path::PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = Table::new();
    section.insert(
        SUGGESTION_KEY_SKILLS.to_string(),
        Value::Boolean(config.skills),
    );
    data.insert(SUGGESTION_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
}

/// 写回开关（读一次现有配置再落盘，未涉及的键原样保留）。
pub fn save_suggestion_switch(
    env: &ConfigEnvironment,
    enabled: bool,
    config_path: Option<&Path>,
) -> Result<std::path::PathBuf, ConfigError> {
    let config = SuggestionConfig { skills: enabled };
    save_suggestion_config(env, &config, config_path)
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
    fn defaults_to_disabled() {
        let (root, env) = temp_env("defaults");
        let config = load_suggestion_config(&env, Some(&root.join("config.toml")));
        assert!(!config.enabled());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_switch_survives_a_round_trip() {
        let (root, env) = temp_env("roundtrip");
        let path = root.join("config.toml");
        save_suggestion_config(&env, &SuggestionConfig { skills: true }, Some(&path))
            .expect("写盘");

        let reloaded = load_suggestion_config(&env, Some(&path));
        assert!(reloaded.skills, "Skill 建议应保持开启");
        let _ = fs::remove_dir_all(&root);
    }

    /// 旧配置写在 decision_models 的 `[features]` 下：升级后不该被静默关掉。
    #[test]
    fn the_legacy_decision_switch_is_carried_over() {
        let (root, env) = temp_env("legacy");
        // 直接按旧版形状写盘：旧键已从决策开关表移除，只能用原始 TOML 造这条迁移用例。
        let legacy_path =
            crate::core::runtime::resolve_decision_models_path(&env, None).expect("旧文件路径");
        if let Some(parent) = legacy_path.parent() {
            fs::create_dir_all(parent).expect("建配置目录");
        }
        fs::write(&legacy_path, "[features]\nskill_suggestion = true\n").expect("写旧开关");

        let config = load_suggestion_config(&env, None);
        assert!(config.skills, "旧位置的开启状态应沿用到新段");
        let _ = fs::remove_dir_all(&root);
    }
}
