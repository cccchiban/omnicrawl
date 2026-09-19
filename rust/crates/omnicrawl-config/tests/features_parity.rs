//! features 配置（approval / tools / context_compaction / run_guard）的对照测试。
//!
//! 期望值来自 Python 真实现（生成器 `rust/tools/gen_config_features_fixture.py`）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use omnicrawl_config::core::runtime as R;
use omnicrawl_config::features::approval;
use omnicrawl_config::features::context_compaction as compaction;
use omnicrawl_config::features::run_guard as guard;
use omnicrawl_config::features::tools;
use omnicrawl_config::value::json_to_toml;
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/config_features_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("解析对照数据集")
}

fn temp_root(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-features-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("建立临时根目录");
    path
}

fn case_root(root: &Path, index: usize) -> PathBuf {
    let dir = root.join(format!("case-{index}"));
    std::fs::create_dir_all(&dir).expect("建立用例目录");
    dir
}

fn user_dir(case_root: &Path) -> PathBuf {
    let dir = case_root.join(R::USER_CONFIG_DIRNAME);
    std::fs::create_dir_all(&dir).expect("建立用户配置目录");
    dir
}

fn env_for(dir: &Path) -> R::ConfigEnvironment {
    R::ConfigEnvironment::new(dir.to_string_lossy().to_string(), "win32")
}

fn write_config(dir: &Path, name: &str, data: &Value) {
    let table = omnicrawl_config::value::json_object_to_table(data);
    std::fs::write(
        dir.join(name),
        omnicrawl_config::toml::dump_document(&table),
    )
    .expect("写配置");
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn approval_normalize_matches_python() {
    let data = fixture();
    for case in data["approval_normalize"].as_array().unwrap() {
        let value = case["value"].as_str().unwrap();
        match (
            approval::normalize_approval_mode(value),
            case.get("expected"),
        ) {
            (Ok(mode), Some(expected)) => {
                assert_eq!(mode, expected.as_str().unwrap(), "输入：{value}")
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "输入：{value}"
                )
            }
            (Ok(mode), None) => panic!("输入 {value} 应报错，实际得到 {mode}"),
        }
    }
}

#[test]
fn approval_labels_match_python() {
    let data = fixture();
    for case in data["approval_labels"].as_array().unwrap() {
        assert_eq!(
            approval::approval_mode_label(case["mode"].as_str().unwrap()),
            case["expected"].as_str().unwrap()
        );
    }
}

#[test]
fn approval_load_matches_python() {
    let data = fixture();
    let root = temp_root("approval-load");
    for (index, case) in data["approval_load"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (approval::load_approval_mode(&env, None), case.get("mode")) {
            (Ok(mode), Some(expected)) => {
                assert_eq!(mode, expected.as_str().unwrap(), "模式：{name}")
            }
            (Err(error), _) => assert_eq!(
                error.message(),
                case["mode_error"].as_str().unwrap(),
                "模式：{name}"
            ),
            (Ok(mode), None) => panic!("模式用例 {name} 应报错，实际得到 {mode}"),
        }
        match (
            approval::load_approval_review_model(&env, None),
            case.get("review_model"),
        ) {
            (Ok(model), Some(expected)) => {
                assert_eq!(model, expected.as_str().unwrap(), "审查模型：{name}")
            }
            (Err(error), _) => assert_eq!(
                error.message(),
                case["review_model_error"].as_str().unwrap(),
                "审查模型：{name}"
            ),
            (Ok(model), None) => panic!("审查模型用例 {name} 应报错，实际得到 {model}"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn approval_save_matches_python() {
    let data = fixture();
    let root = temp_root("approval-save");
    for (index, case) in data["approval_save"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        let target = dir.join("config.toml");
        write_config(
            &dir,
            "config.toml",
            &json!({"approval": {"review_model": "m"}, "ui": {"show_thinking": true}}),
        );
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (
            approval::save_approval_mode(&env, case["mode"].as_str().unwrap(), None),
            case.get("text"),
        ) {
            (Ok(_), Some(expected)) => {
                assert_eq!(
                    read_text(&target),
                    expected.as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tools_validate_matches_python() {
    let data = fixture();
    for case in data["tools_validate"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        match (tools::validate_tool_switch_name(name), case.get("expected")) {
            (Ok(value), Some(expected)) => {
                assert_eq!(value, expected.as_str().unwrap(), "输入：{name}")
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "输入：{name}"
                )
            }
            (Ok(value), None) => panic!("输入 {name} 应报错，实际得到 {value}"),
        }
    }
}

#[test]
fn tools_catalog_matches_python() {
    let data = fixture();
    let keys: Vec<String> = data["tools_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        tools::TOOL_SWITCH_KEYS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        keys
    );
    let defaults: BTreeMap<String, bool> = data["tools_defaults"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_bool().unwrap()))
        .collect();
    let ours: BTreeMap<String, bool> = tools::default_tool_switches()
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
    assert_eq!(ours, defaults);
    let expected = data["tools_labels"].as_object().unwrap();
    let ours: Value = Value::Object(
        tools::TOOL_SWITCH_LABELS
            .iter()
            .map(|(key, label)| (key.to_string(), Value::String(label.to_string())))
            .collect(),
    );
    assert_eq!(ours, Value::Object(expected.clone()));
    // 键序也要一致（Python dict 的插入序 = TOOL_SWITCH_KEYS 的顺序）。
    let expected_keys: Vec<&String> = expected.keys().collect();
    let ours_keys: Vec<&String> = ours.as_object().unwrap().keys().collect();
    assert_eq!(ours_keys, expected_keys);
}

#[test]
fn tools_load_matches_python() {
    let data = fixture();
    let root = temp_root("tools-load");
    for (index, case) in data["tools_load"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (tools::load_tool_switches(&env, None), case.get("switches")) {
            (Ok(switches), Some(expected)) => {
                let actual: Value = Value::Object(
                    switches
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::Bool(*value)))
                        .collect(),
                );
                assert_eq!(actual, *expected, "开关表：{name}");
                let mut disabled = tools::load_disabled_tools(&env, None).expect("禁用表");
                disabled.sort();
                let expected: Vec<String> = case["disabled"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|item| item.as_str().unwrap().to_string())
                    .collect();
                assert_eq!(disabled, expected, "禁用表：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tools_save_matches_python() {
    let data = fixture();
    let root = temp_root("tools-save");
    for (index, case) in data["tools_save"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        let target = dir.join("config.toml");
        write_config(&dir, "config.toml", &json!({"tools": {"list": true}}));
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        let mut switches: Vec<(String, bool)> = Vec::new();
        for (key, value) in case["switches"].as_object().unwrap() {
            switches.push((key.clone(), value.as_bool().unwrap_or_default()));
        }
        match (
            tools::save_tool_switches(&env, &switches, None),
            case.get("text"),
        ) {
            (Ok(_), Some(expected)) => {
                assert_eq!(
                    read_text(&target),
                    expected.as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                );
                assert_eq!(
                    read_text(&target),
                    case["text_after"].as_str().unwrap(),
                    "用例 {name} 失败时不应写盘"
                );
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn context_compaction_matches_python() {
    let data = fixture();
    let root = temp_root("compaction");
    for (index, case) in data["context_compaction"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (
            compaction::load_context_compaction_config(&env, None),
            case.get("expected"),
        ) {
            (Ok(config), Some(expected)) => {
                let actual = json!({
                    "trigger_context_percent": config.trigger_context_percent,
                    "trigger_context_tokens": config.trigger_context_tokens,
                    "next_user_reserve_tokens": config.next_user_reserve_tokens,
                    "emergency_context_ratio": config.emergency_context_ratio,
                    "summary_profile": config.summary_profile,
                    "reasoning_effort": config.reasoning_effort,
                    "recent_turns": config.recent_turns,
                    "target_summary_tokens": config.target_summary_tokens,
                    "preserve_exact_evidence": config.preserve_exact_evidence,
                    "archive_compacted_events": config.archive_compacted_events,
                    "auto_memory_recall": config.auto_memory_recall,
                    "allow_cross_provider": config.allow_cross_provider,
                    "failure_fallback": config.failure_fallback,
                });
                assert_eq!(actual, *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_guard_matches_python() {
    let data = fixture();
    let root = temp_root("run-guard");
    for (index, case) in data["run_guard"].as_array().unwrap().iter().enumerate() {
        let case_dir = case_root(&root, index);
        let dir = user_dir(&case_dir);
        write_config(&dir, "config.toml", &case["config"]);
        let env = env_for(&case_dir);
        let name = case["name"].as_str().unwrap();
        match (
            guard::load_run_guard_config(&env, None),
            case.get("expected"),
        ) {
            (Ok(config), Some(expected)) => {
                let actual = json!({
                    "enabled": config.enabled,
                    "guard": {
                        "enabled": config.guard.enabled,
                        "window_chars": config.guard.window_chars,
                        "substr_len": config.guard.substr_len,
                        "repeat_ratio": config.guard.repeat_ratio,
                        "check_every": config.guard.check_every,
                        "max_blocks": config.guard.max_blocks,
                        "max_chars": config.guard.max_chars,
                        "max_guard_retries": config.guard.max_guard_retries,
                        "auto_retry_errors": config.guard.auto_retry_errors,
                    },
                    "continue": {
                        "enabled": config.continuation.enabled,
                        "max_auto_followups": config.continuation.max_auto_followups,
                    },
                });
                assert_eq!(actual, *expected, "用例：{name}");
            }
            (Err(error), _) => {
                assert_eq!(
                    error.message(),
                    case["error"].as_str().unwrap(),
                    "用例：{name}"
                )
            }
            (Ok(_), None) => panic!("用例 {name} 应报错"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_guard_save_matches_python() {
    let data = fixture();
    let root = temp_root("run-guard-save");
    for case in data["run_guard_save"].as_array().unwrap() {
        let dir = user_dir(&root);
        let target = dir.join("config.toml");
        let mut ui = toml::map::Map::new();
        ui.insert(
            "show_thinking".to_string(),
            json_to_toml(&Value::Bool(true)),
        );
        let mut data_table = toml::map::Map::new();
        data_table.insert("ui".to_string(), toml::Value::Table(ui));
        std::fs::write(&target, omnicrawl_config::toml::dump_document(&data_table))
            .expect("写配置");
        let env = env_for(&root);
        let config = guard::RunGuardConfig {
            enabled: false,
            guard: guard::ReasoningGuardConfig {
                window_chars: 3000,
                auto_retry_errors: vec!["RATE_LIMIT".to_string()],
                ..guard::ReasoningGuardConfig::default()
            },
            continuation: guard::ContinueConfig {
                max_auto_followups: 7,
                ..guard::ContinueConfig::default()
            },
        };
        guard::save_run_guard_config(&env, &config, None).expect("写回护栏配置");
        assert_eq!(
            read_text(&target),
            case["text"].as_str().unwrap(),
            "用例：{}",
            case["name"]
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
