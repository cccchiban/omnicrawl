//! `agent/controllers/session/`（settings / control）判定层的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `settings` / `control` 段后，本套件用同一批
//! 输入重放 Rust 侧校验、归一化与阈值换算逐条比对。setter 的事务（重建工具表 / Runtime /
//! MCP Manager、失败回滚）与资源关闭属于宿主，不在对照范围。

use omnicrawl_controllers::control::SessionCloseAction;
use omnicrawl_controllers::{control, settings};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn section(name: &str) -> Value {
    fixture()[name].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn sorted(values: &[&str]) -> Vec<String> {
    let mut owned: Vec<String> = values.iter().map(|item| (*item).to_string()).collect();
    owned.sort();
    owned
}

fn expect_error<T>(
    result: Result<T, omnicrawl_controllers::AgentError>,
    case: &Value,
    label: &str,
) {
    match result {
        Ok(_) => panic!("本该失败却成功（{label}）"),
        Err(error) => assert_eq!(
            error.message(),
            case["error"].as_str().expect("error"),
            "错误文案（{label}）"
        ),
    }
}

#[test]
fn settings_constants_match_python() {
    let data = section("settings");
    let expected = &data["constants"];
    assert_eq!(
        sorted(&settings::VALID_APPROVAL_MODES),
        strings(&expected["approval_modes"])
    );
    assert_eq!(
        sorted(&settings::VALID_REASONING_EFFORTS),
        strings(&expected["reasoning_efforts"])
    );
    assert_eq!(
        settings::TOOL_SWITCH_KEYS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["tool_switch_keys"])
    );
    assert_eq!(
        settings::SUBAGENT_ADVANCED_SETTING_KEYS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["subagent_advanced_keys"])
    );
    assert_eq!(
        settings::DEFAULT_CONTEXT_WINDOW_TOKENS,
        expected["default_context_window_tokens"]
            .as_i64()
            .expect("int")
    );
}

#[test]
fn settings_approval_mode_matches_python() {
    let data = section("settings");
    for case in data["approval_mode"].as_array().expect("cases") {
        let value = case["value"].as_str().expect("value");
        let result = settings::normalize_approval_mode(value);
        if case["ok"].as_bool().expect("ok") {
            assert_eq!(
                result.expect("应当成功"),
                case["value_out"].as_str().expect("value_out"),
                "审批模式归一化（{value}）"
            );
        } else {
            expect_error(result, case, value);
        }
    }
}

#[test]
fn settings_reasoning_effort_matches_python() {
    let data = section("settings");
    for case in data["reasoning_effort"].as_array().expect("cases") {
        let value = case["value"].as_str().expect("value");
        let result = settings::normalize_reasoning_effort(value);
        if case["ok"].as_bool().expect("ok") {
            let normalized = result.expect("应当成功");
            assert_eq!(
                normalized,
                case["value_out"].as_str().expect("value_out"),
                "推理强度归一化（{value}）"
            );
            assert_eq!(
                settings::thinking_type_for(normalized),
                case["thinking_type"].as_str().expect("thinking_type"),
                "思考类型（{value}）"
            );
        } else {
            expect_error(result, case, value);
        }
    }
}

#[test]
fn settings_trigger_tokens_match_python() {
    let data = section("settings");
    for case in data["trigger_tokens"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            settings::context_compaction_trigger_tokens(
                case["window"].as_i64().expect("window"),
                case["percent"].as_i64().expect("percent")
            ),
            case["expected"].as_i64().expect("expected"),
            "阈值换算（{label}）"
        );
    }
}

#[test]
fn settings_compaction_percent_matches_python() {
    let data = section("settings");
    for case in data["compaction_percent"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let percent = case["percent"].as_i64().expect("percent");
        let current_tokens = case["current_tokens"].as_i64().expect("current_tokens");
        let current_percent = case["current_percent"].as_i64();
        let target = settings::percent_change_target(
            case["window"].as_i64().expect("window"),
            percent,
            current_tokens,
            current_percent,
        );
        assert_eq!(
            target.unwrap_or(current_tokens),
            case["tokens_after"].as_i64().expect("tokens_after"),
            "触发阈值（{label}）"
        );
        let percent_after = if target.is_some() {
            Some(percent)
        } else {
            current_percent
        };
        assert_eq!(
            percent_after,
            case["percent_after"].as_i64(),
            "百分比字段（{label}）"
        );
    }
}

#[test]
fn settings_compaction_tokens_matches_python() {
    let data = section("settings");
    for case in data["compaction_tokens"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let tokens = case["tokens"].as_i64().expect("tokens");
        let current_tokens = case["current_tokens"].as_i64().expect("current_tokens");
        let current_percent = case["current_percent"].as_i64();
        match settings::validate_compaction_tokens(tokens) {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(_) => {
                let target = settings::token_change_target(tokens, current_tokens, current_percent);
                assert_eq!(
                    target.unwrap_or(current_tokens),
                    case["tokens_after"].as_i64().expect("tokens_after"),
                    "触发阈值（{label}）"
                );
                let percent_after = if target.is_some() {
                    None
                } else {
                    current_percent
                };
                assert_eq!(
                    percent_after,
                    case["percent_after"].as_i64(),
                    "百分比字段（{label}）"
                );
            }
        }
    }
}

#[test]
fn settings_window_tokens_matches_python() {
    let data = section("settings");
    for case in data["window_tokens"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let tokens = case["tokens"].as_i64();
        match tokens {
            None => assert!(!case["ok"].as_bool().expect("ok"), "本该失败（{label}）"),
            Some(value) => match settings::validate_positive_tokens(value) {
                Ok(checked) => {
                    assert!(case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                    assert_eq!(checked, value);
                    assert_eq!(
                        checked,
                        case["window_after"].as_i64().expect("window_after"),
                        "窗口取值（{label}）"
                    );
                }
                Err(error) => {
                    assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                    assert_eq!(
                        error.message(),
                        case["error"].as_str().expect("error"),
                        "错误文案（{label}）"
                    );
                }
            },
        }
    }
}

#[test]
fn settings_tool_switch_name_matches_python() {
    let data = section("settings");
    for case in data["tool_switch_name"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("name");
        let result = settings::validate_tool_switch_name(name);
        if case["ok"].as_bool().expect("ok") {
            assert_eq!(
                result.expect("应当成功"),
                case["value_out"].as_str().expect("value_out"),
                "开关名归一化（{name}）"
            );
        } else {
            expect_error(result, case, name);
        }
    }
}

#[test]
fn settings_tool_toggle_matches_python() {
    let data = section("settings");
    for case in data["tool_toggle"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let disabled = strings(&case["disabled"]);
        let name = case["name"].as_str().expect("name");
        let enabled = case["enabled"].as_bool().expect("enabled");
        let result = settings::apply_tool_switch(&disabled, name, enabled);
        let tools_built = case["tools_built"].as_i64().expect("tools_built");
        match result {
            Err(error) => {
                assert!(!case["ok"].as_bool().expect("ok"), "本该成功（{label}）");
                assert_eq!(
                    error.message(),
                    case["error"].as_str().expect("error"),
                    "错误文案（{label}）"
                );
            }
            Ok(target) => {
                if let Some(next) = target {
                    let mut sorted_next = next.clone();
                    sorted_next.sort();
                    assert_eq!(
                        sorted_next,
                        strings(&case["disabled_after"]),
                        "禁用集合（{label}）"
                    );
                    assert_eq!(tools_built, 1, "重建工具表（{label}）");
                } else {
                    let mut sorted_disabled = disabled.clone();
                    sorted_disabled.sort();
                    assert_eq!(
                        sorted_disabled,
                        strings(&case["disabled_after"]),
                        "禁用集合不变（{label}）"
                    );
                    assert_eq!(tools_built, 0, "不重建工具表（{label}）");
                }
            }
        }
    }
}

#[test]
fn settings_subagent_advanced_matches_python() {
    let data = section("settings");
    for case in data["subagent_advanced"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let name = case["name"].as_str().expect("name");
        let value = case["value"].as_f64().expect("value");
        let is_int = case["value"].is_i64() || case["value"].is_u64();
        let result = settings::validate_subagent_advanced_setting(name, value, is_int);
        if case["ok"].as_bool().expect("ok") {
            let normalized = result.expect("应当成功");
            assert_eq!(
                normalized,
                case["value_out"].as_f64().expect("value_out"),
                "归一化取值（{label}）"
            );
        } else {
            expect_error(result, case, label);
        }
    }
}

#[test]
fn control_plugins_status_matches_python() {
    let data = section("control");
    for case in data["plugins_status"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let expected = case["expected"].as_str().expect("expected");
        let actual = if case["manager_present"].as_bool().expect("manager_present") {
            let rows: Vec<control::PluginWorkerRow> = case["rows"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| control::PluginWorkerRow {
                    name: row["name"].as_str().unwrap_or_default().to_string(),
                    version: row["version"].as_str().unwrap_or_default().to_string(),
                    scope: row["scope"].as_str().unwrap_or_default().to_string(),
                    active: row["active"].as_bool().unwrap_or(false),
                    circuit_open: row["circuitOpen"].as_bool().unwrap_or(false),
                    dev_mode: row["devMode"].as_bool().unwrap_or(false),
                    handlers: row["handlers"]
                        .as_array()
                        .map(|items| {
                            items
                                .iter()
                                .map(|item| item.as_str().unwrap_or_default().to_string())
                                .collect()
                        })
                        .unwrap_or_default(),
                    last_error: row["lastError"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            let enabled = case["enabled"].as_bool().expect("enabled");
            control::plugins_status_text(Some(&(enabled, rows)))
        } else {
            control::plugins_status_text(None)
        };
        assert_eq!(actual, expected, "插件状态文案（{label}）");
    }
}

#[test]
fn control_session_closed_matches_python() {
    let data = section("control");
    for case in data["session_closed"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let last_event_type = case["last_event_type"].as_str();
        let expected = match case["action"].as_str().expect("action") {
            "none" => SessionCloseAction::None,
            "discard" => SessionCloseAction::Discard,
            "close_and_discard" => SessionCloseAction::AppendAndDiscard,
            other => panic!("未知收尾动作：{other}"),
        };
        assert_eq!(
            control::session_closed_action(last_event_type),
            expected,
            "退出收尾动作（{label}）"
        );
    }
}

#[test]
fn control_close_matches_python() {
    let data = section("control");
    for case in data["close"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let has_coordinator = case["has_coordinator"].as_bool().expect("has_coordinator");
        let drained = case["drained"].as_bool().expect("drained");
        let step = control::close_step(has_coordinator, drained);
        let expected_defer = case["deferred"].as_bool().expect("deferred");
        assert_eq!(
            step == control::CloseStep::Defer,
            expected_defer,
            "是否推迟关闭（{label}）"
        );
        assert_eq!(
            step == control::CloseStep::Proceed,
            case["closed"].as_bool().expect("closed"),
            "是否继续关闭（{label}）"
        );
        for call in case["calls"].as_array().expect("calls") {
            assert_eq!(
                call["reason"].as_str().expect("reason"),
                control::subagent_cancel_reason(true),
                "取消原因（{label}）"
            );
            assert_eq!(
                call["timeout_seconds"].as_f64().expect("timeout"),
                control::subagent_cancel_timeout(),
                "等待上限（{label}）"
            );
            assert!(
                call["permanent"].as_bool().expect("permanent"),
                "永久取消（{label}）"
            );
        }
    }
}

#[test]
fn control_subagents_disable_matches_python() {
    let data = section("control");
    for case in data["subagents_disable"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let drained = case["drained"].as_bool().expect("drained");
        let has_coordinator = !case["calls"].as_array().expect("calls").is_empty();
        let result = control::subagents_disable_step(has_coordinator, drained);
        let expected_ok = case["ok"].as_bool().expect("ok");
        assert_eq!(result.is_ok(), expected_ok, "停用结论（{label}）");
        if let Err(error) = result {
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "错误文案（{label}）"
            );
        }
        for call in case["calls"].as_array().expect("calls") {
            assert_eq!(
                call["reason"].as_str().expect("reason"),
                control::subagent_cancel_reason(false),
                "取消原因（{label}）"
            );
        }
    }
}
