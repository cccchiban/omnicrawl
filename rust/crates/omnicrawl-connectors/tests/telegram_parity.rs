//! Telegram 连接器的跨语言 parity：期望值来自 Python `omnicrawl/connectors/telegram.py`。
//!
//! 覆盖分段与裁剪、流式收尾的消息调用序列、文件提取/分类/落盘命名、配置解析、更新路由、
//! `/thinking` 与 `/workspace` 的判定。长度按**字符**计、`pathlib` 的边界写法与
//! 重名序号的逐轮递进都是语义的一部分，因此都有用例。

use std::collections::{HashMap, HashSet};
use std::fs;

use omnicrawl_connectors::telegram::{
    classify_file_name, extract_telegram_file, load_telegram_config, parse_thinking_command,
    plan_abort, plan_finalize, route_update, split_message, temp_destination, truncate_for_stream,
    workspace_argument, FinalizePlan, ThinkingCommand, UpdateRoute, MAX_MESSAGE_LEN,
    POLLING_TIMEOUT_SECONDS, STREAM_EDIT_INTERVAL_SECONDS,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/telegram_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

#[test]
fn constants_match_python() {
    let constants = &fixture()["constants"];
    assert_eq!(MAX_MESSAGE_LEN as u64, constants["max_message_len"]);
    assert_eq!(POLLING_TIMEOUT_SECONDS, constants["polling_timeout"]);
    assert_eq!(
        STREAM_EDIT_INTERVAL_SECONDS,
        constants["stream_edit_interval"]
    );
}

#[test]
fn split_message_matches_python() {
    for case in fixture()["split"].as_array().expect("split") {
        let text = case["input"].as_str().expect("输入是字符串");
        let limit = case["limit"].as_u64().expect("limit") as usize;
        let expected: Vec<&str> = case["expected"]
            .as_array()
            .expect("expected")
            .iter()
            .map(|part| part.as_str().expect("分段是字符串"))
            .collect();
        assert_eq!(split_message(text, limit), expected, "用例 {case}");
    }
}

#[test]
fn truncate_for_stream_matches_python() {
    for case in fixture()["truncate"].as_array().expect("truncate") {
        let text = case["input"].as_str().expect("输入是字符串");
        assert_eq!(
            json!(truncate_for_stream(text)),
            case["expected"],
            "长度 {} 的用例",
            text.chars().count()
        );
    }
}

/// 把 Rust 的收尾计划翻译成消息调用序列，与 Python 记录的调用逐条比对。
fn abort_calls(plan: &omnicrawl_connectors::telegram::AbortPlan) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    if let Some(head) = &plan.edit_head {
        calls.push(("edit".to_string(), head.clone()));
    }
    calls.push(("send".to_string(), plan.send_text.clone()));
    calls
}

fn finalize_calls(plan: &FinalizePlan) -> Vec<(String, String)> {
    match plan {
        FinalizePlan::EditFull(text) => vec![("edit".to_string(), text.clone())],
        FinalizePlan::EditHeadThenSend { head, full } => vec![
            ("edit".to_string(), head.clone()),
            ("send".to_string(), full.clone()),
        ],
        FinalizePlan::SendFull(text) => vec![("send".to_string(), text.clone())],
    }
}

fn expected_calls(case: &Value) -> Vec<(String, String)> {
    case["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            (
                call["call"].as_str().expect("call").to_string(),
                call["text"].as_str().expect("text").to_string(),
            )
        })
        .collect()
}

#[test]
fn abort_stream_plan_matches_python() {
    let fixture = fixture();
    for case in fixture["stream_tail"]["abort"].as_array().expect("abort") {
        let deltas = case["deltas"].as_str().expect("deltas");
        let has_message = case["has_message"].as_bool().expect("has_message");
        let plan = plan_abort(has_message, deltas, "❌ 出错了");
        assert_eq!(abort_calls(&plan), expected_calls(case), "用例 {case}");
    }
}

#[test]
fn finalize_stream_plan_matches_python() {
    let fixture = fixture();
    for case in fixture["stream_tail"]["finalize"]
        .as_array()
        .expect("finalize")
    {
        let text = case["text"].as_str().expect("text");
        let has_message = case["has_message"].as_bool().expect("has_message");
        let plan = plan_finalize(has_message, text);
        assert_eq!(
            finalize_calls(&plan),
            expected_calls(case),
            "用例长度 {}",
            text.chars().count()
        );
    }
}

#[test]
fn file_extraction_matches_python() {
    for case in fixture()["files"]["extract"].as_array().expect("extract") {
        let message = &case["message"];
        let expected = &case["expected"];
        match extract_telegram_file(message) {
            Some(file) => {
                assert_eq!(json!(file.file_id), expected["file_id"], "用例 {message}");
                assert_eq!(
                    json!(file.file_name),
                    expected["file_name"],
                    "用例 {message}"
                );
            }
            None => assert!(expected.is_null(), "用例 {message} 应当没有文件"),
        }
    }
}

#[test]
fn file_classification_matches_python() {
    for case in fixture()["files"]["classify"].as_array().expect("classify") {
        let name = case["input"].as_str().expect("input");
        assert_eq!(
            classify_file_name(name),
            case["expected"].as_str().unwrap(),
            "用例 {name}"
        );
    }
}

#[test]
fn temp_destination_matches_python() {
    let case_root = std::env::temp_dir().join(format!("ocl-tg-parity-{}", std::process::id()));
    let root = case_root.join(".omnicrawl").join(".agent_tmp");
    let _ = fs::remove_dir_all(&case_root);
    for case in fixture()["destination"].as_array().expect("destination") {
        let subdir = case["subdir"].as_str().expect("subdir");
        let name = case["file_name"].as_str().expect("file_name");
        for existing in case["existing"].as_array().expect("existing") {
            let target = root.join(subdir).join(existing.as_str().expect("existing"));
            fs::create_dir_all(target.parent().expect("父目录")).expect("建目录");
            fs::write(&target, "x").expect("写占位文件");
        }
        let resolved = temp_destination(&root, subdir, name).expect("计算目标路径");
        let relative = resolved
            .strip_prefix(&root)
            .expect("在 temp 根内")
            .to_string_lossy()
            .replace('\\', "/");
        let expected = case["expected"].as_str().expect("expected");
        // 无文件名时回落成 telegram_<时间戳>，两侧时间戳必然不同：只比对形状。
        if expected.starts_with("files/telegram_") {
            let stamp = expected.trim_start_matches("files/telegram_");
            assert!(
                relative.starts_with("files/telegram_")
                    && relative
                        .trim_start_matches("files/telegram_")
                        .chars()
                        .all(|value| value.is_ascii_digit()),
                "回落名形状不符：{relative}（对照 {stamp}）"
            );
            continue;
        }
        assert_eq!(relative, expected, "用例 {case}");
    }
    let _ = fs::remove_dir_all(&case_root);
}

#[test]
fn config_loading_matches_python() {
    for case in fixture()["config"].as_array().expect("config") {
        let environment: HashMap<String, String> = case["environment"]
            .as_object()
            .expect("environment")
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value.as_str().expect("env 值是字符串").to_string(),
                )
            })
            .collect();
        let section = case["section"].clone();
        let lookup = move |name: &str| environment.get(name).cloned();
        let result = load_telegram_config(&lookup, Some(&section));
        match &case["expected"] {
            Value::Object(expected) if expected.contains_key("error") => {
                let error = result.expect_err("应解析失败");
                assert_eq!(json!(error), expected["error"], "用例 {case}");
            }
            Value::Object(expected) => {
                let config = result.unwrap_or_else(|error| panic!("应解析成功，实际报错：{error}"));
                assert_eq!(
                    json!(config.bot_token),
                    expected["bot_token"],
                    "用例 {case}"
                );
                assert_eq!(
                    json!(config.allowed_user_ids),
                    expected["allowed_user_ids"],
                    "用例 {case}"
                );
                assert_eq!(
                    config.confirm_timeout_seconds,
                    expected["confirm_timeout_seconds"]
                        .as_f64()
                        .expect("超时是数字"),
                    "用例 {case}"
                );
            }
            other => panic!("fixture 形状意外：{other}"),
        }
    }
}

#[test]
fn update_routing_matches_python() {
    let allowed: HashSet<i64> = HashSet::from([7_i64]);
    for case in fixture()["updates"].as_array().expect("updates") {
        let update = &case["update"];
        let expected = case["calls"].as_array().expect("calls");
        match route_update(update, &allowed) {
            UpdateRoute::Text {
                chat_id,
                user_id,
                text,
            } => {
                assert_eq!(expected.len(), 1, "用例 {update}");
                let call = &expected[0];
                assert_eq!(call["kind"], "text", "用例 {update}");
                assert_eq!(json!(chat_id), call["chat_id"], "用例 {update}");
                assert_eq!(json!(user_id), call["user_id"], "用例 {update}");
                assert_eq!(json!(text), call["text"], "用例 {update}");
            }
            UpdateRoute::File {
                chat_id,
                user_id,
                file,
                caption,
            } => {
                assert_eq!(expected.len(), 1, "用例 {update}");
                let call = &expected[0];
                let message = &call["message"];
                assert_eq!(call["kind"], "file", "用例 {update}");
                assert_eq!(json!(chat_id), call["chat_id"], "用例 {update}");
                assert_eq!(json!(user_id), call["user_id"], "用例 {update}");
                let python_file = extract_telegram_file(message).expect("Python 侧提取到文件");
                assert_eq!(file.file_id, python_file.file_id, "用例 {update}");
                assert_eq!(
                    caption,
                    message["caption"].as_str().unwrap_or("").trim(),
                    "用例 {update}"
                );
            }
            UpdateRoute::Unauthorized { user_id, .. } => {
                assert!(expected.is_empty(), "未授权用户不应触发调用：{update}");
                assert!(!case["warnings"].as_array().expect("warnings").is_empty());
                assert_eq!(user_id, 9);
            }
            UpdateRoute::MissingIdentifiers | UpdateRoute::Unsupported => {
                assert!(expected.is_empty(), "用例 {update} 不应触发调用");
            }
        }
    }
}

#[test]
fn thinking_route_matches_python() {
    for case in fixture()["thinking"].as_array().expect("thinking") {
        let text = case["text"].as_str().expect("text");
        let calls = case["calls"].as_array().expect("calls");
        let message = &calls[0]["text"];
        let expected_show = case["show_thinking"].as_bool().expect("show_thinking");
        match parse_thinking_command(text) {
            ThinkingCommand::Query => {
                assert!(
                    message
                        .as_str()
                        .expect("文本")
                        .starts_with("思考内容显示："),
                    "用例 {text}"
                );
                assert!(!expected_show, "查询不改变状态：{text}");
            }
            ThinkingCommand::Enable => {
                assert_eq!(
                    message, "已开启思考内容显示（🧠 独立消息）。",
                    "用例 {text}"
                );
                assert!(expected_show, "用例 {text}");
            }
            ThinkingCommand::Disable => {
                assert_eq!(message, "已关闭思考内容显示。", "用例 {text}");
                assert!(!expected_show, "用例 {text}");
            }
            ThinkingCommand::Usage => {
                assert_eq!(
                    message, "用法：/thinking on 或 /thinking off。",
                    "用例 {text}"
                );
            }
        }
    }
}

#[test]
fn workspace_argument_matches_python() {
    for case in fixture()["workspace"].as_array().expect("workspace") {
        let text = case["text"].as_str().expect("text");
        let message = case["calls"][0]["text"].as_str().expect("文本");
        let argument = workspace_argument(text);
        if message.starts_with("当前工作区：") {
            assert!(argument.is_none(), "用例 {text} 只应查看工作区");
            continue;
        }
        let path = argument.unwrap_or_else(|| panic!("用例 {text} 应解析出路径"));
        if message.starts_with("❌ 切换工作区失败：") {
            assert!(
                case["switched"].as_array().expect("switched").is_empty(),
                "切换失败时工作区不变：{text}"
            );
            return;
        }
        assert_eq!(
            path,
            case["switched"][0].as_str().expect("切换路径"),
            "用例 {text}"
        );
        assert!(
            message.starts_with(&format!("✅ 已切换工作区：{path}")),
            "用例 {text} 的提示应是成功文案：{message}"
        );
    }
}
