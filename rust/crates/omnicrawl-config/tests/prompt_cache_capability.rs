//! 自定义模型条目的 `capabilities.prompt_cache` 必须进入 `LlmConfig`。
//!
//! 这是 Rust 侧新增的字段（Python 把它放在 `ModelCapabilities` / 描述符里），
//! 不参与 `config_models_parity.json` 的逐字段对照，因此单独在此固定契约：
//! 宿主握手时据此决定 `initialize.model.prompt_cache_capable`。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::models::llm::load_llm_config;

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-prompt-cache-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn write_text(path: &Path, text: &str) {
    std::fs::write(path, text).expect("写配置");
}

/// 声明了 prompt 缓存的自定义模型：能力声明要随配置解析出来。
#[test]
fn custom_model_prompt_cache_capability_flows_into_llm_config() {
    let root = temp_root("enabled");
    let user = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    write_text(
        &user.join("config.toml"),
        r#"
[llm.profiles.ch1]
provider = "openai"
base_url = "https://api.ch1/v1"
api_key = "plain-key"
default_protocol = "openai_chat_completions"

[llm.active_model]
source = "custom"
key = "cached-model"
"#,
    );
    write_text(
        &user.join("models.toml"),
        r#"
version = 1

[models.cached-model]
display_name = "网关缓存模型"
profile = "ch1"
model_id = "gateway-cache-model"
protocol = "openai_chat_completions"

[models.cached-model.capabilities]
prompt_cache = true
"#,
    );

    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_llm_config(&env).expect("加载多模型配置");
    assert_eq!(config.catalog_key, "cached-model");
    assert_eq!(
        config.prompt_cache,
        Some(true),
        "自定义模型声明的 prompt_cache 要带进 LlmConfig"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// detected / legacy 路径没有能力声明来源，保持 `None`（仅 GPT 系列回退尝试）。
#[test]
fn detected_model_has_no_prompt_cache_capability() {
    let root = temp_root("detected");
    let user = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user).expect("建立用户配置目录");
    write_text(
        &user.join("config.toml"),
        r#"
[llm.profiles.ch1]
provider = "openai"
base_url = "https://api.ch1/v1"
api_key = "plain-key"
default_protocol = "openai_chat_completions"

[llm.active_model]
source = "detected"
profile = "ch1"
model_id = "gpt-5.2"
"#,
    );

    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_llm_config(&env).expect("加载多模型配置");
    assert_eq!(config.model_source, "detected");
    assert_eq!(config.prompt_cache, None);

    let _ = std::fs::remove_dir_all(&root);
}
