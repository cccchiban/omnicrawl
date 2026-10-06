//! core 配置（workspace / settings / bootstrap）的对照测试。
//!
//! 数据集是冻结的对照契约。

use std::path::{Path, PathBuf};

use omnicrawl_config::core::bootstrap::{self as B, NodeProbe, PluginRow, StartupPorts};
use omnicrawl_config::core::runtime as R;
use omnicrawl_config::core::settings as S;
use omnicrawl_config::core::workspace as W;
use omnicrawl_config::toml::Table;
use omnicrawl_config::value::json_object_to_table;
use serde_json::{json, Value as Json};

const FIXTURE: &str = include_str!("fixtures/config_core_parity.json");

fn fixture() -> Json {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-core-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn case_dirs(root: &Path, index: usize) -> (PathBuf, PathBuf) {
    let case_dir = root.join(format!("case-{index}"));
    let user_dir = case_dir.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&user_dir).expect("建立用户配置目录");
    (case_dir, user_dir)
}

fn env_for(case_dir: &Path) -> R::ConfigEnvironment {
    R::ConfigEnvironment::new(case_dir.to_string_lossy().to_string(), "win32")
}

fn write_file(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).expect("写文件");
}

fn write_config(dir: &Path, name: &str, data: &Json) {
    write_file(
        dir,
        name,
        &omnicrawl_config::toml::dump_document(&json_object_to_table(data)),
    );
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 把用例目录从文案里抹掉，对齐生成器对解析库错误尾巴的处理。
fn redact(message: &str, case_dir: &Path) -> String {
    let needle = case_dir.to_string_lossy().to_string();
    match message.find(&needle) {
        Some(index) => format!("{}<case_dir>", &message[..index]),
        None => message.to_string(),
    }
}

fn check_outcome(actual: Result<Json, String>, case: &Json, name: &str) {
    match (actual, case.get("result"), case.get("error")) {
        (Ok(value), Some(expected), _) => assert_eq!(&value, expected, "{name}"),
        (Err(message), _, Some(expected)) => {
            assert_eq!(message, expected.as_str().unwrap(), "{name}")
        }
        (Ok(value), None, Some(_)) => panic!("{name} 应报错，实际得到 {value}"),
        (Err(message), Some(_), None) => panic!("{name} 不应报错：{message}"),
        _ => panic!("{name} 数据集缺少期望值"),
    }
}

fn check_text(actual: Result<String, String>, case: &Json, name: &str) {
    match (actual, case.get("text"), case.get("error")) {
        (Ok(text), Some(expected), _) => assert_eq!(text, expected.as_str().unwrap(), "{name}"),
        (Err(message), _, Some(expected)) => {
            assert_eq!(message, expected.as_str().unwrap(), "{name}")
        }
        (Ok(text), None, Some(_)) => panic!("{name} 应报错，实际写回 {text}"),
        (Err(message), Some(_), None) => panic!("{name} 不应报错：{message}"),
        _ => panic!("{name} 数据集缺少期望值"),
    }
}

#[test]
fn workspace_load_matches_python() {
    let data = fixture();
    let root = temp_root("ws-load");
    for (index, case) in data["workspace_load"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        write_config(&user_dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let actual = W::load_workspace_root(&env, None)
            .map(|value| value.map(Json::String).unwrap_or(Json::Null))
            .map_err(|error| error.message().to_string());
        check_outcome(actual, case, name);
        let _ = case_dir;
    }
}

#[test]
fn workspace_save_matches_python() {
    let data = fixture();
    let root = temp_root("ws-save");
    let (case_dir, user_dir) = case_dirs(&root, 0);
    write_config(
        &user_dir,
        "config.toml",
        &json!({"llm": {"model": "keep"}, "workspace": {"other": 1}}),
    );
    let env = env_for(&case_dir);
    W::save_workspace_root(&env, Path::new("D:\\proj\\sub\\..\\proj"), None).expect("写回工作区根");
    let expected = data["workspace_save"][0]["text"].as_str().unwrap();
    assert_eq!(read_text(&user_dir.join("config.toml")), expected);
}

#[test]
fn settings_enabled_matches_python() {
    let data = fixture();
    let root = temp_root("set-enabled");
    for (index, case) in data["settings_enabled"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        write_config(&user_dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let actual = S::load_feature_enabled(
            &env,
            case["section"].as_str().unwrap(),
            case["default"].as_bool().unwrap(),
            None,
            None,
        )
        .map(Json::Bool)
        .map_err(|error| error.message().to_string());
        check_outcome(actual, case, name);
    }
}

#[test]
fn settings_window_config_matches_python() {
    let data = fixture();
    let root = temp_root("set-window");
    for (index, case) in data["settings_window_config"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let target = user_dir.join("config.toml");
        write_config(&user_dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let actual = S::save_context_window_tokens(
            &env,
            200_000,
            case["model_source"].as_str().unwrap(),
            case["catalog_key"].as_str().unwrap(),
            None,
            None,
        )
        .map(|_| read_text(&target))
        .map_err(|error| error.message().to_string());
        check_text(actual, case, name);
    }
}

#[test]
fn settings_window_store_matches_python() {
    let data = fixture();
    let root = temp_root("set-window-store");
    for (index, case) in data["settings_window_store"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let target = user_dir.join("models.toml");
        write_config(&user_dir, "models.toml", &case["models"]);
        write_config(&user_dir, "config.toml", &json!({}));
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let actual = S::save_context_window_tokens(
            &env,
            case["tokens"].as_i64().unwrap(),
            "custom",
            case["key"].as_str().unwrap(),
            None,
            None,
        )
        .map(|_| read_text(&target))
        .map_err(|error| error.message().to_string());
        check_text(actual, case, name);
    }
}

#[test]
fn settings_compaction_percent_matches_python() {
    let data = fixture();
    let root = temp_root("set-compaction");
    for (index, case) in data["settings_compaction_percent"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let target = user_dir.join("config.toml");
        write_config(
            &user_dir,
            "config.toml",
            &json!({"context_compaction": {"recent_turns": 6}}),
        );
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let actual = S::save_context_compaction_trigger_percent(
            &env,
            case["percent"].as_i64().unwrap(),
            case["window"].as_i64().unwrap(),
            None,
        )
        .map(|_| read_text(&target))
        .map_err(|error| error.message().to_string());
        check_text(actual, case, name);
    }
}

#[test]
fn settings_show_thinking_matches_python() {
    let data = fixture();
    let root = temp_root("set-thinking");
    for (index, case) in data["settings_show_thinking"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let target = user_dir.join("config.toml");
        write_config(&user_dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (S::load_show_thinking(&env, None), case.get("error")) {
            (Ok(value), None) => {
                assert_eq!(Json::Bool(value), case["result"], "{name} 读取值");
                S::save_show_thinking(&env, false, None).expect("写回 show_thinking");
                assert_eq!(
                    read_text(&target),
                    case["text"].as_str().unwrap(),
                    "{name} 写回文本"
                );
            }
            (Err(error), Some(expected)) => {
                assert_eq!(error.message(), expected.as_str().unwrap(), "{name}")
            }
            (Ok(value), Some(_)) => panic!("{name} 应报错，实际得到 {value}"),
            (Err(error), None) => panic!("{name} 不应报错：{}", error.message()),
        }
    }
}

#[test]
fn settings_subagent_matches_python() {
    let data = fixture();
    let root = temp_root("set-subagent");
    for (index, case) in data["settings_subagent"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let (case_dir, user_dir) = case_dirs(&root, index);
        let target = user_dir.join("subagents.toml");
        write_config(
            &user_dir,
            "subagents.toml",
            &json!({"subagents": {"enabled": true, "max_depth": 1}}),
        );
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let value = omnicrawl_config::value::json_to_toml(&case["value"]);
        let actual = S::save_subagent_setting(&env, case["key"].as_str().unwrap(), &value, None)
            .map(|_| read_text(&target))
            .map_err(|error| error.message().to_string());
        check_text(actual, case, name);
    }
}

#[test]
fn settings_mcp_matches_python() {
    let data = fixture();
    let root = temp_root("set-mcp");
    let (case_dir, user_dir) = case_dirs(&root, 0);
    write_config(&user_dir, "config.toml", &json!({"llm": {"model": "keep"}}));
    let env = env_for(&case_dir);

    let mut env_table = Table::new();
    env_table.insert(
        "A".to_string(),
        omnicrawl_config::toml::Value::String("1".to_string()),
    );
    let mut headers = Table::new();
    headers.insert(
        "X".to_string(),
        omnicrawl_config::toml::Value::String("y".to_string()),
    );
    let config = S::McpConfigData {
        enabled: true,
        default_timeout_seconds: 30,
        servers: vec![(
            "files".to_string(),
            S::McpServerData {
                enabled: true,
                transport: "stdio".to_string(),
                command: "node".to_string(),
                args: vec!["server.js".to_string()],
                url: String::new(),
                env: env_table,
                headers,
                timeout_seconds: 15,
                risk_level: "read".to_string(),
            },
        )],
        policy: S::McpPolicyData {
            require_confirmation_for_write: true,
            require_confirmation_for_command: false,
            allow_external_network_tools: false,
            audit_log_enabled: true,
        },
    };
    S::save_mcp_config(&env, &config, None).expect("写回 MCP 配置");
    let expected = data["settings_mcp"][0]["text"].as_str().unwrap();
    assert_eq!(read_text(&user_dir.join("config.toml")), expected);
}

#[test]
fn bootstrap_node_matches_python() {
    let data = fixture();
    for case in data["bootstrap_node"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let probe_case = &case["case"];
        let probe = match probe_case.get("node") {
            None | Some(Json::Null) => NodeProbe::Missing,
            Some(_) => match probe_case.get("raise") {
                Some(Json::String(error)) => NodeProbe::Failed {
                    error: error.clone(),
                },
                _ => NodeProbe::Version {
                    version: probe_case["output"].as_str().unwrap().trim().to_string(),
                    npm_available: !matches!(probe_case.get("npm"), None | Some(Json::Null)),
                },
            },
        };
        let check = B::check_node(&|| probe.clone());
        let expected = case["check"].as_array().unwrap();
        assert_eq!(check.name, expected[0].as_str().unwrap(), "{name}");
        assert_eq!(check.status, expected[1].as_str().unwrap(), "{name}");
        assert_eq!(check.message, expected[2].as_str().unwrap(), "{name}");
    }
}

#[test]
fn bootstrap_plugin_matches_python() {
    let data = fixture();
    for case in data["bootstrap_plugin"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let config = json_object_to_table(&case["config"]);
        let rows: Vec<PluginRow> = case["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| PluginRow {
                error: row
                    .get("error")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                enabled: row
                    .get("enabled")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            })
            .collect();
        let failing = case["raise"].as_bool().unwrap();
        let check = B::check_plugin_state(&config, &|| {
            if failing {
                Err("注册表不可读".to_string())
            } else {
                Ok(rows.clone())
            }
        });
        let expected = case["check"].as_array().unwrap();
        assert_eq!(check.name, expected[0].as_str().unwrap(), "{name}");
        assert_eq!(check.status, expected[1].as_str().unwrap(), "{name}");
        assert_eq!(check.message, expected[2].as_str().unwrap(), "{name}");
    }
}

#[test]
fn bootstrap_report_matches_python() {
    let data = fixture();
    let setup = |overrides: &Json| -> B::StartupSetup {
        let checks = overrides["checks"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| B::StartupCheck {
                        name: item[0].as_str().unwrap().to_string(),
                        status: item[1].as_str().unwrap().to_string(),
                        message: item[2].as_str().unwrap().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        B::StartupSetup {
            config_dir: PathBuf::from("C:\\cfg"),
            config_path: PathBuf::from("C:\\cfg\\config.toml"),
            models_path: PathBuf::from("C:\\cfg\\models.toml"),
            subagents_path: PathBuf::from("C:\\cfg\\subagents.toml"),
            config_created: overrides["config_created"].as_bool().unwrap_or(false),
            models_created: overrides["models_created"].as_bool().unwrap_or(false),
            subagents_created: overrides["subagents_created"].as_bool().unwrap_or(false),
            api_key_prompted: overrides["api_key_prompted"].as_bool().unwrap_or(false),
            api_key_configured: overrides["api_key_configured"].as_bool().unwrap_or(true),
            checks,
            errors: overrides["errors"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default(),
        }
    };
    // 逐例重放；构造参数见 `report_case_descriptor`。
    for (index, case) in data["bootstrap_report"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let name = case["name"].as_str().unwrap();
        let setup = setup(&report_case_descriptor(index));
        let lines = B::format_startup_report(&setup);
        let expected: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        assert_eq!(lines, expected, "{name}");
    }
}

/// 数据集里逐例的构造参数（与生成器的 `startup_setup` 调用一一对应）。
fn report_case_descriptor(index: usize) -> Json {
    match index {
        0 => json!({}),
        1 => json!({"config_created": true, "models_created": true, "subagents_created": true}),
        2 => json!({"api_key_configured": false}),
        3 => json!({
            "errors": ["运行配置读取失败：坏文件"],
            "checks": [["模型配置", "warning", "配置文件存在错误，请修复后重试。"]],
        }),
        4 => json!({
            "api_key_prompted": true,
            "checks": [
                ["模型配置", "ok", "默认模型：GPT-5 (gpt-5)"],
                ["Node.js", "warning", "未检测到 Node.js；插件功能暂不可用。"],
                ["插件状态", "ok", "插件系统已禁用，已注册 0 个插件，当前启用 0 个。"],
            ],
        }),
        other => panic!("用例 {other} 缺少构造参数"),
    }
}

#[test]
fn bootstrap_initialize_matches_python() {
    let data = fixture();
    let templates = data["templates"].clone();
    let root = temp_root("bs-init");
    for (index, case) in data["bootstrap_initialize"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let name = case["name"].as_str().unwrap();
        let case_dir = root.join(format!("case-{index}"));
        std::fs::create_dir_all(&case_dir).expect("建立用例目录");
        for (file, text) in case["initial_files"].as_object().unwrap() {
            write_file(&case_dir, file, text.as_str().unwrap());
        }
        let mut env = env_for(&case_dir);
        for (key, value) in case["env"].as_object().unwrap() {
            env = env.with_env_value(key, value.as_str().unwrap());
        }

        let template_map = templates.clone();
        let read_template = move |resource: &str| -> Result<String, String> {
            template_map
                .get(resource)
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("缺少模板 {resource}"))
        };
        let channel_setup = case["channel_setup"].as_bool().unwrap();
        let channel_setup_port =
            move |config_path: &Path, _models: &Path| -> Result<bool, String> {
                let text = omnicrawl_config::toml::dump_document(&json_object_to_table(&json!({
                    "llm": {
                        "profiles": {"openai-main": {"provider": "openai", "api_key": "sk-wizard"}},
                        "active_model": {"profile": "openai-main"},
                    }
                })));
                std::fs::write(config_path, text).map_err(|error| error.to_string())?;
                Ok(true)
            };
        let prompt_value = case["prompt"].as_str().map(str::to_string);
        let prompt_port = move |_message: &str| -> Option<String> { prompt_value.clone() };

        let node_probe = || NodeProbe::Version {
            version: "v20.11.0".to_string(),
            npm_available: true,
        };
        let plugin_rows = || -> Result<Vec<PluginRow>, String> { Ok(Vec::new()) };
        let ports = StartupPorts {
            read_template: &read_template,
            channel_setup: if channel_setup {
                Some(&channel_setup_port)
            } else {
                None
            },
            prompt: if case["prompt"].is_null() {
                None
            } else {
                Some(&prompt_port)
            },
            node_probe: &node_probe,
            plugin_rows: &plugin_rows,
        };

        let setup = B::initialize_user_configuration(&env, Some(&case_dir), &ports)
            .expect("初始化用户配置");
        let summary = &case["summary"];
        assert_eq!(
            setup.config_created,
            summary["config_created"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            setup.models_created,
            summary["models_created"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            setup.subagents_created,
            summary["subagents_created"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            setup.api_key_prompted,
            summary["api_key_prompted"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            setup.api_key_configured,
            summary["api_key_configured"].as_bool().unwrap(),
            "{name}"
        );
        assert_eq!(
            setup.first_run(),
            summary["first_run"].as_bool().unwrap(),
            "{name}"
        );

        let expected_errors: Vec<String> = summary["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        let actual_errors: Vec<String> = setup
            .errors
            .iter()
            .map(|error| redact(error, &case_dir))
            .collect();
        assert_eq!(actual_errors, expected_errors, "{name} errors");

        let expected_checks: Vec<(String, String, String)> = summary["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item[0].as_str().unwrap().to_string(),
                    item[1].as_str().unwrap().to_string(),
                    item[2].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let actual_checks: Vec<(String, String, String)> = setup
            .checks
            .iter()
            .map(|check| {
                (
                    check.name.clone(),
                    check.status.clone(),
                    check.message.clone(),
                )
            })
            .collect();
        assert_eq!(actual_checks, expected_checks, "{name} checks");

        let mut actual_files: Vec<(String, String)> = std::fs::read_dir(&case_dir)
            .expect("读用例目录")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    read_text(&entry.path()),
                )
            })
            .collect();
        actual_files.sort();
        let mut expected_files: Vec<(String, String)> = case["final_files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(file, text)| (file.clone(), text.as_str().unwrap().to_string()))
            .collect();
        expected_files.sort();
        assert_eq!(actual_files, expected_files, "{name} files");
    }
}
