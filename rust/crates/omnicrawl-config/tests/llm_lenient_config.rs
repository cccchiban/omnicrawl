//! 设置面板读取模型视图时允许缺 API Key，运行期仍保持严格校验。
//!
//! 面板本身就是补写凭据的地方：缺 Key 就打不开会形成死锁。这里固定两条契约：
//! 严格加载仍然报「缺少 API Key」，而宽松加载保留模型/渠道/窗口等字段照常回填。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::models::llm::{load_llm_config, load_llm_config_allow_missing_api_key};

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-llm-lenient-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn write_text(path: &Path, text: &str) {
    std::fs::write(path, text).expect("写配置");
}

/// 多模型配置：当前渠道的 api_key 留空。
fn multi_model_config(root: &Path) {
    let user = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    write_text(
        &user.join("config.toml"),
        r#"
[llm.profiles.openai-main]
provider = "openai"
base_url = "https://api.openai.com/v1"
api_key = ""
default_protocol = "openai_chat_completions"
default_context_window_tokens = 200000

[llm.active_model]
source = "custom"
key = "default-chat"
"#,
    );
    write_text(
        &user.join("models.toml"),
        r#"
version = 1

[models.default-chat]
display_name = "默认对话"
profile = "openai-main"
model_id = "gpt-test"
protocol = "openai_chat_completions"
context_window_tokens = 200000
"#,
    );
}

#[test]
fn strict_load_still_rejects_a_missing_api_key() {
    let root = temp_root("strict");
    multi_model_config(&root);
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "windows");

    let error = load_llm_config(&env).expect_err("运行期必须要求凭据");
    assert!(
        error.message().contains("缺少 API Key"),
        "应报告缺少 API Key：{}",
        error.message()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn settings_view_can_load_without_api_key() {
    let root = temp_root("lenient");
    multi_model_config(&root);
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "windows");

    let config = load_llm_config_allow_missing_api_key(&env).expect("面板应能读取配置");
    assert_eq!(config.model, "gpt-test");
    assert_eq!(config.profile_id, "openai-main");
    assert_eq!(config.catalog_key, "default-chat");
    assert_eq!(config.base_url, "https://api.openai.com/v1");
    assert_eq!(config.context_window_tokens, 200000, "窗口仍从模型条目解析");
    assert!(config.api_key.is_empty(), "缺 Key 如实回填为空");

    let _ = std::fs::remove_dir_all(&root);
}

/// legacy 配置同样不能因为缺 Key 就挡住面板。
#[test]
fn settings_view_can_load_legacy_config_without_api_key() {
    let root = temp_root("legacy");
    let user = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    write_text(
        &user.join("config.toml"),
        r#"
[llm]
model = "gpt-legacy"
base_url = "https://api.example.com/v1"
api_key = ""
"#,
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "windows");

    assert!(load_llm_config(&env).is_err(), "严格加载仍应报错");
    let config = load_llm_config_allow_missing_api_key(&env).expect("面板应能读取 legacy 配置");
    assert_eq!(config.model, "gpt-legacy");
    assert_eq!(config.base_url, "https://api.example.com/v1");
    assert!(config.api_key.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}
