//! Skill 额外路径配置（`[skills]` 段）。
//!
//! `extra_paths` 里的目录或 `.md` 文件与内置作用域一起参与发现，优先级最高
//! （在个人级与项目级之后扫描，同名时不覆盖前面已加载的）。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

const CONFIG_SECTION: &str = "skills";

/// `[skills]` 段配置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfig {
    /// 额外 Skill 路径：目录或 `.md` 文件；相对路径按工作区解析。
    pub extra_paths: Vec<String>,
}

/// 读取 `[skills]` 段；缺省为空列表。
pub fn load_skills_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<SkillsConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, CONFIG_SECTION)?;
    Ok(SkillsConfig {
        extra_paths: string_list_field(&section, "extra_paths")?,
    })
}

fn string_list_field(section: &crate::toml::Table, name: &str) -> Result<Vec<String>, ConfigError> {
    match section.get(name) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(text) => list.push(text.clone()),
                    _ => {
                        return Err(ConfigError::new(format!(
                            "配置项 {CONFIG_SECTION}.{name} 必须是字符串数组。"
                        )))
                    }
                }
            }
            Ok(list)
        }
        Some(_) => Err(ConfigError::new(format!(
            "配置项 {CONFIG_SECTION}.{name} 必须是字符串数组。"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::runtime::ConfigEnvironment;
    use crate::toml::Table;

    #[test]
    fn missing_section_yields_default() {
        let env = ConfigEnvironment::new("bundle", "test");
        let config = load_skills_config(&env, Some(Path::new("no-such-config.toml")))
            .expect("缺文件时应回落默认值");
        assert!(config.extra_paths.is_empty());
    }

    #[test]
    fn non_string_items_are_rejected() {
        let mut section = Table::new();
        section.insert(
            "extra_paths".to_string(),
            Value::Array(vec![Value::Integer(1)]),
        );
        let error = string_list_field(&section, "extra_paths").expect_err("非字符串项应报错");
        assert_eq!(
            error.message(),
            "配置项 skills.extra_paths 必须是字符串数组。"
        );
    }
}
