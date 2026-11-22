//! Skill 额外路径与常驻选择配置（`[skills]` 段）。
//!
//! `extra_paths` 里的目录或 `.md` 文件与内置作用域一起参与发现，优先级最高
//! （在个人级与项目级之后扫描，同名时不覆盖前面已加载的）。

use std::path::Path;

use crate::core::runtime::{get_section, load_config_data, save_config_data, ConfigEnvironment};
use crate::error::ConfigError;
use crate::toml::Value;

const CONFIG_SECTION: &str = "skills";

/// `[skills]` 段配置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfig {
    /// 额外 Skill 路径：目录或 `.md` 文件；相对路径按工作区解析。
    pub extra_paths: Vec<String>,
    pub resident_skill: Option<String>,
}

/// 读取 `[skills]` 段；缺省无额外路径、无常驻 Skill。
pub fn load_skills_config(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
) -> Result<SkillsConfig, ConfigError> {
    let data = load_config_data(env, config_path)?;
    let section = get_section(&data, CONFIG_SECTION)?;
    Ok(SkillsConfig {
        extra_paths: string_list_field(&section, "extra_paths")?,
        resident_skill: match section.get("resident_skill") {
            None => None,
            Some(Value::String(name)) => Some(name.trim().to_string()).filter(|name| !name.is_empty()),
            Some(_) => return Err(ConfigError::new("配置项 skills.resident_skill 必须是字符串。")),
        },
    })
}

pub fn save_resident_skill(
    env: &ConfigEnvironment,
    config_path: Option<&Path>,
    name: Option<&str>,
) -> Result<std::path::PathBuf, ConfigError> {
    let mut data = load_config_data(env, config_path)?;
    let mut section = get_section(&data, CONFIG_SECTION)?;
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => {
            section.insert("resident_skill".to_string(), Value::String(name.to_string()));
        }
        None => {
            section.remove("resident_skill");
        }
    }
    data.insert(CONFIG_SECTION.to_string(), Value::Table(section));
    save_config_data(env, &data, config_path)
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

    #[test]
    fn resident_skill_defaults_and_rejects_multiple_values() {
        let root = std::env::temp_dir().join(format!("oc-resident-config-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("validation.toml");
        let env = ConfigEnvironment::new(&root, "test");
        for text in ["", r#"[skills]
resident_skill = "   "
"#] {
            std::fs::write(&path, text).unwrap();
            assert_eq!(load_skills_config(&env, Some(&path)).unwrap().resident_skill, None);
        }
        for value in ["[]", r#"["one", "two"]"#, "true", "1"] {
            std::fs::write(&path, format!("[skills]
resident_skill = {value}
")).unwrap();
            assert!(load_skills_config(&env, Some(&path)).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn resident_skill_save_replaces_or_clears_only_its_key() {
        let root = std::env::temp_dir().join(format!("oc-resident-config-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("save.toml");
        let env = ConfigEnvironment::new(&root, "test");
        std::fs::write(&path, r#"[skills]
extra_paths = ["./custom"]
other = true
[suggestion]
skills = true
"#).unwrap();
        for name in [Some(" first "), Some("second"), None] {
            save_resident_skill(&env, Some(&path), name).unwrap();
            let config = load_skills_config(&env, Some(&path)).unwrap();
            assert_eq!(config.resident_skill.as_deref(), name.map(str::trim));
            assert_eq!(config.extra_paths, vec!["./custom".to_string()]);
            let data = load_config_data(&env, Some(&path)).unwrap();
            assert_eq!(get_section(&data, "skills").unwrap()["other"], Value::Boolean(true));
            assert_eq!(get_section(&data, "suggestion").unwrap()["skills"], Value::Boolean(true));
        }
        std::fs::remove_file(path).unwrap();
    }
}
