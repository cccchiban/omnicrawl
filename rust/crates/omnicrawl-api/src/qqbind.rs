//! `connector qqbot --bind`：扫码把 AppID / AppSecret 写进 `config.toml`。
//!
//! 对齐 QQ 官方「第三方 Agent 接入」的扫码流程：控制台打印二维码，手机 QQ 扫码后平台
//! 直接把凭据交给本机（AES-256-GCM 加密，密钥只在本地）。写配置时**保留**其它段，
//! 白名单留空即公开访问模式，首次绑定时会把扫码者自己的 openid 写进白名单。

use std::path::PathBuf;

use omnicrawl_config::core::runtime::{load_config_data, save_config_data, ConfigEnvironment};
use omnicrawl_config::toml::Value;

/// 扫码页上显示的接入方名称。
pub const SOURCE_NAME: &str = "OmniCrawl";

/// 绑定成功后的落地结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindOutcome {
    pub app_id: String,
    pub config_path: PathBuf,
    /// 是否把扫码者写进了白名单。
    pub allowlisted: bool,
}

/// 把凭据写进 `[qqbot]` 段（其它段原样保留）。
pub fn write_credentials(
    env: &ConfigEnvironment,
    app_id: &str,
    app_secret: &str,
    allowed_user_ids: &[String],
) -> Result<PathBuf, String> {
    let mut data = load_config_data(env, None).map_err(|error| error.message().to_string())?;
    let mut section = match data.get("qqbot") {
        Some(Value::Table(table)) => table.clone(),
        Some(_) => return Err("config.toml 的 [qqbot] 必须是对象。".to_string()),
        None => omnicrawl_config::toml::Table::new(),
    };
    section.insert("app_id".to_string(), Value::String(app_id.to_string()));
    section.insert(
        "app_secret".to_string(),
        Value::String(app_secret.to_string()),
    );
    if !allowed_user_ids.is_empty() {
        section.insert(
            "allowed_user_ids".to_string(),
            Value::Array(
                allowed_user_ids
                    .iter()
                    .map(|item| Value::String(item.clone()))
                    .collect(),
            ),
        );
    }
    data.insert("qqbot".to_string(), Value::Table(section));
    save_config_data(env, &data, None).map_err(|error| error.message().to_string())
}

/// 执行扫码绑定：打印二维码、轮询结果、写配置。
pub fn run_bind() -> Result<BindOutcome, String> {
    let env = ConfigEnvironment::from_process();
    let timeout = omnicrawl_connectors::qqbot::DEFAULT_BIND_TIMEOUT;
    println!("正在创建 QQ 机器人绑定任务…");
    let credentials = omnicrawl_connectors::qqbot::bind_with_console(timeout, SOURCE_NAME)
        .map_err(|error| error.0)?;
    let allowlist: Vec<String> = if credentials.user_openid.trim().is_empty() {
        Vec::new()
    } else {
        vec![credentials.user_openid.trim().to_string()]
    };
    let path = write_credentials(
        &env,
        &credentials.app_id,
        &credentials.app_secret,
        &allowlist,
    )?;
    println!("✅ 绑定成功：AppID {}", credentials.app_id);
    if allowlist.is_empty() {
        println!("提示：平台没有返回扫码者 openid，allowed_user_ids 留空（公开访问模式）。");
    } else {
        println!("已把扫码者 {} 写入 allowed_user_ids。", allowlist[0]);
    }
    println!("凭据已写入：{}", path.display());
    println!("启动接入：omnicrawl-host connector qqbot");
    Ok(BindOutcome {
        app_id: credentials.app_id,
        config_path: path,
        allowlisted: !allowlist.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_env(name: &str) -> ConfigEnvironment {
        let root = std::env::temp_dir().join(format!("oc-qq-bind-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".OmniCrawl")).expect("建目录");
        ConfigEnvironment::new(root, "windows")
    }

    #[test]
    fn credentials_are_written_and_other_sections_survive() {
        let env = temp_env("write");
        let dir = omnicrawl_config::core::runtime::user_config_dir(&env);
        std::fs::write(
            dir.join("config.toml"),
            "[ui]\nshow_thinking = true\n\n[qqbot]\nconfirmation_timeout_seconds = 60\n",
        )
        .expect("写初始配置");
        let path =
            write_credentials(&env, "102012345", "secret", &["F6A8".to_string()]).expect("写配置");
        let text = std::fs::read_to_string(&path).expect("读配置");
        assert!(text.contains("app_id = \"102012345\""), "{text}");
        assert!(text.contains("app_secret = \"secret\""), "{text}");
        assert!(text.contains("F6A8"), "{text}");
        assert!(
            text.contains("show_thinking = true"),
            "其它段必须保留：{text}"
        );
        assert!(
            text.contains("confirmation_timeout_seconds = 60"),
            "既有键必须保留：{text}"
        );
    }

    #[test]
    fn empty_allowlist_does_not_clear_an_existing_one() {
        let env = temp_env("keep-allow");
        let dir = omnicrawl_config::core::runtime::user_config_dir(&env);
        std::fs::write(
            dir.join("config.toml"),
            "[qqbot]\nallowed_user_ids = [\"kept\"]\n",
        )
        .expect("写初始配置");
        let path = write_credentials(&env, "1", "s", &[]).expect("写配置");
        let text = std::fs::read_to_string(&path).expect("读配置");
        assert!(text.contains("kept"), "{text}");
    }

    #[test]
    fn non_table_section_is_rejected() {
        let env = temp_env("bad-section");
        let dir = omnicrawl_config::core::runtime::user_config_dir(&env);
        std::fs::write(dir.join("config.toml"), "qqbot = 1\n").expect("写初始配置");
        let error = write_credentials(&env, "1", "s", &[]).expect_err("应报错");
        assert!(error.contains("必须是对象"), "{error}");
    }
}
