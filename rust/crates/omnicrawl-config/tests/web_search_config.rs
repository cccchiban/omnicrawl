//! `[web_search]` 段（联网搜索的默认引擎与接口型引擎凭据）的读取、校验与写回契约。
//!
//! 这段没有 Python 对映实现，因此不进 parity 数据集，自带本套件固定默认值、归一化、
//! 凭据解析与写回隔离。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::web_search::{
    load_web_search_configuration, save_web_search_configuration, WebSearchConfiguration,
    DEFAULT_WEB_SEARCH_ENGINE, WEB_SEARCH_ENGINES,
};

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-web-search-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn user_dir(root: &Path) -> PathBuf {
    let dir = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&dir).expect("建立用户配置目录");
    dir
}

fn write_text(path: &Path, text: &str) {
    std::fs::write(path, text).expect("写配置");
}

/// 没有 `web_search` 段时：默认引擎是无需要凭据的 bing，三把密钥都为空。
#[test]
fn missing_section_yields_defaults() {
    let root = temp_root("empty");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_web_search_configuration(&env, None).expect("缺段时用默认值");
    assert_eq!(config, WebSearchConfiguration::default());
    assert_eq!(config.engine, DEFAULT_WEB_SEARCH_ENGINE);
    assert!(config.tavily_api_key.is_empty());
    assert!(config.exa_api_key.is_empty());
    assert!(config.firecrawl_api_key.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// 全字段读取：引擎名大小写与空白都归一，凭据去首尾空白。
#[test]
fn values_are_read_and_normalized() {
    let root = temp_root("read");
    let user = user_dir(&root);
    write_text(
        &user.join("config.toml"),
        r#"
[web_search]
engine = "  Exa "
tavily_api_key = "  tvly-key  "
exa_api_key = " exa-key "
firecrawl_api_key = " fc-key "
"#,
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_web_search_configuration(&env, None).expect("读取联网搜索配置");
    assert_eq!(config.engine, "exa");
    assert_eq!(config.tavily_api_key, "tvly-key");
    assert_eq!(config.exa_api_key, "exa-key");
    assert_eq!(config.firecrawl_api_key, "fc-key");
    let _ = std::fs::remove_dir_all(&root);
}

/// 空串按「没填」处理：引擎回落默认值，而不是把配置判为非法。
#[test]
fn blank_values_fall_back_to_defaults() {
    let root = temp_root("blank");
    let user = user_dir(&root);
    write_text(
        &user.join("config.toml"),
        "[web_search]\nengine = \"\"\ntavily_api_key = \"\"\n",
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_web_search_configuration(&env, None).expect("空串用默认值");
    assert_eq!(config.engine, DEFAULT_WEB_SEARCH_ENGINE);
    let _ = std::fs::remove_dir_all(&root);
}

/// 引擎清单是唯一来源；非法引擎报错并列出全部候选。
#[test]
fn unknown_engine_is_rejected_with_the_full_list() {
    let root = temp_root("bad-engine");
    let user = user_dir(&root);
    write_text(
        &user.join("config.toml"),
        "[web_search]\nengine = \"not_an_engine\"\n",
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let error = load_web_search_configuration(&env, None).expect_err("未知引擎应当被拒绝");
    assert_eq!(
        error.message(),
        format!(
            "web_search.engine 仅支持 {}。",
            WEB_SEARCH_ENGINES.join("、")
        )
    );
    assert_eq!(WEB_SEARCH_ENGINES.len(), 9);
    // 雅虎已下线；混合搜索与站点检索引擎都是合法值。
    assert!(!WEB_SEARCH_ENGINES.contains(&"yahoo"));
    // Google 与 Semantic Scholar 已下线：拿不到结果，不再作为引擎名保留。
    assert!(!WEB_SEARCH_ENGINES.contains(&"google"));
    assert!(!WEB_SEARCH_ENGINES.contains(&"semantic_scholar"));
    assert!(WEB_SEARCH_ENGINES.contains(&"mixed"));
    assert!(WEB_SEARCH_ENGINES.contains(&"firecrawl"));
    for engine in ["github", "arxiv", "huggingface"] {
        assert!(WEB_SEARCH_ENGINES.contains(&engine), "{engine} 应当合法");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// 每把密钥只归自己的引擎：抓页面型引擎没有凭据。
#[test]
fn api_key_resolution_is_per_engine() {
    let config = WebSearchConfiguration {
        engine: "tavily".to_string(),
        tavily_api_key: "tvly-1".to_string(),
        exa_api_key: "exa-1".to_string(),
        firecrawl_api_key: "fc-1".to_string(),
        github_token: "ghp-1".to_string(),
    };
    assert_eq!(config.api_key_for("tavily"), "tvly-1");
    assert_eq!(config.api_key_for("Exa"), "exa-1", "大小写不敏感");
    assert_eq!(config.api_key_for("Firecrawl"), "fc-1");
    assert_eq!(config.api_key_for("bing"), "");
    assert_eq!(config.api_key_for("duckduckgo"), "");
    assert_eq!(config.api_key_for("mixed"), "", "混合搜索本身没有凭据");
    // GitHub Token 不参与「缺凭据即报错」的判定：没有它引擎也照常可用。
    assert_eq!(config.api_key_for("github"), "");
}

/// 写回只动 `web_search` 段，其他段保持原样。
#[test]
fn save_keeps_other_sections_and_round_trips() {
    let root = temp_root("save");
    let user = user_dir(&root);
    let config_path = user.join("config.toml");
    write_text(
        &config_path,
        "[llm]\nmodel = \"keep-me\"\n\n[tools]\npowershell = false\n",
    );
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let configuration = WebSearchConfiguration {
        engine: "tavily".to_string(),
        tavily_api_key: "tvly-key".to_string(),
        exa_api_key: String::new(),
        firecrawl_api_key: "fc-key".to_string(),
        github_token: "ghp-key".to_string(),
    };
    save_web_search_configuration(&env, &configuration, None).expect("写回联网搜索配置");

    let text = std::fs::read_to_string(&config_path).expect("读回配置");
    assert!(
        text.contains("[llm]\nmodel = \"keep-me\""),
        "其他段保持原样：{text}"
    );
    assert!(
        text.contains("[tools]\npowershell = false"),
        "`[tools]` 不得被改写：{text}"
    );

    let reloaded = load_web_search_configuration(&env, None).expect("重新读取");
    assert_eq!(reloaded, configuration);
    let _ = std::fs::remove_dir_all(&root);
}

/// 写回前先归一化：非法引擎不会被写进磁盘。
#[test]
fn save_rejects_invalid_engine_before_writing() {
    let root = temp_root("save-bad");
    let user = user_dir(&root);
    let config_path = user.join("config.toml");
    write_text(&config_path, "[llm]\nmodel = \"keep-me\"\n");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let configuration = WebSearchConfiguration {
        engine: "not_an_engine".to_string(),
        ..WebSearchConfiguration::default()
    };
    let error =
        save_web_search_configuration(&env, &configuration, None).expect_err("非法引擎应当被拒绝");
    assert!(error.message().contains("仅支持"), "{}", error.message());
    let text = std::fs::read_to_string(&config_path).expect("读回配置");
    assert_eq!(text, "[llm]\nmodel = \"keep-me\"\n", "拒绝时不得产生写入");
    let _ = std::fs::remove_dir_all(&root);
}
