//! `[fetcher]` 段（抓取引擎选择）的读取、校验与写回契约。
//!
//! 这段没有 Python 对映实现，因此不进 parity 数据集，自带本套件固定默认值、归一化
//! 与写回隔离。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::fetcher::{
    load_fetcher_configuration, save_fetcher_configuration, FetcherConfiguration,
    DEFAULT_FETCH_ENGINE, FETCH_ENGINES,
};

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-fetcher-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn user_dir(root: &Path) -> PathBuf {
    let dir = root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&dir).expect("建立用户配置目录");
    dir
}

/// 没有 `fetcher` 段时：走本地抓取，且允许远程失败回落（两个默认值都是「不改变既有行为」）。
#[test]
fn missing_section_yields_local_with_fallback() {
    let root = temp_root("empty");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let config = load_fetcher_configuration(&env, None).expect("缺段时用默认值");
    assert_eq!(config, FetcherConfiguration::default());
    assert_eq!(config.engine, DEFAULT_FETCH_ENGINE);
    assert!(config.fallback_local);
    assert!(!config.uses_remote(), "local 不该触发远程抓取");
}

/// 引擎名大小写不敏感，末尾空白被去掉。
#[test]
fn engine_names_are_normalized() {
    let root = temp_root("normalize");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let dir = user_dir(&root);
    std::fs::write(
        dir.join("config.toml"),
        "[fetcher]\nengine = \"  Tavily  \"\n",
    )
    .expect("写配置");
    let config = load_fetcher_configuration(&env, None).expect("读取配置");
    assert_eq!(config.engine, "tavily");
    assert!(config.uses_remote());
    // 没写 fallback_local 时仍然取 true：缺省就该有兜底。
    assert!(config.fallback_local);
}

#[test]
fn unknown_engine_is_rejected() {
    let root = temp_root("reject");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let dir = user_dir(&root);
    std::fs::write(dir.join("config.toml"), "[fetcher]\nengine = \"bing\"\n").expect("写配置");
    let error = load_fetcher_configuration(&env, None).expect_err("未知引擎应当被拒");
    assert!(error.message().contains("fetcher.engine"), "{}", error.message());
    // 报错里列出全部合法取值，用户不用去翻源码。
    for engine in FETCH_ENGINES {
        assert!(error.message().contains(engine), "缺 {engine}：{}", error.message());
    }
}

/// `fallback_local = false` 是显式要求「要么用接口要么失败」，要能读回来。
#[test]
fn explicit_no_fallback_is_honored() {
    let root = temp_root("no-fallback");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let dir = user_dir(&root);
    std::fs::write(
        dir.join("config.toml"),
        "[fetcher]\nengine = \"balanced\"\nfallback_local = false\n",
    )
    .expect("写配置");
    let config = load_fetcher_configuration(&env, None).expect("读取配置");
    assert_eq!(config.engine, "balanced");
    assert!(!config.fallback_local);
}

/// 写回只动 `[fetcher]` 段，其它段原样保留。
#[test]
fn save_keeps_other_sections() {
    let root = temp_root("save");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let dir = user_dir(&root);
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "[web_search]\nengine = \"tavily\"\ntavily_api_key = \"tvly-x\"\n\n[fetcher]\nengine = \"local\"\n",
    )
    .expect("写配置");

    let saved = save_fetcher_configuration(
        &env,
        &FetcherConfiguration {
            engine: "exa".to_string(),
            fallback_local: false,
        },
        None,
    )
    .expect("写回成功");
    let text = std::fs::read_to_string(&saved).expect("读回配置");
    assert!(text.contains("[web_search]"), "其它段被抹掉了：{text}");
    assert!(text.contains("tvly-x"), "搜索凭据被抹掉了：{text}");
    let config = load_fetcher_configuration(&env, None).expect("重新读出");
    assert_eq!(config.engine, "exa");
    assert!(!config.fallback_local);
}

/// `fetcher` 段不是对象时直接报错，而不是悄悄退回默认值。
#[test]
fn non_table_section_is_rejected() {
    let root = temp_root("shape");
    let env = R::ConfigEnvironment::new(root.to_string_lossy().to_string(), "win32");
    let dir = user_dir(&root);
    std::fs::write(dir.join("config.toml"), "fetcher = \"tavily\"\n").expect("写配置");
    let error = load_fetcher_configuration(&env, None).expect_err("形状错误应当报错");
    assert!(error.message().contains("fetcher"), "{}", error.message());
}
