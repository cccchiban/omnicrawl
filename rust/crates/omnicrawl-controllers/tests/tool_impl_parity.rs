//! `controllers/tools/implementations.py` 判定面的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `tool_impl` 段后，本套件用同一批入参
//! 重放 Rust 的投影与解析，逐字段比对（含清单输出文本与提问信封的字节形状）。

use omnicrawl_controllers::tool_impl::{
    ask_user_answer_output, memory_store_missing_error, parse_ask_user, parse_memory_scope,
    project_todos, todos_output, MemoryScope, ASK_USER_UNANSWERED, TODOS_NOT_ARRAY,
};
use serde_json::{Map, Value};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["tool_impl"].clone()
}

fn arguments(value: &Value) -> Map<String, Value> {
    value.as_object().expect("入参应当是对象").clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

#[test]
fn todos_projection_matches_python() {
    let data = section();
    for case in data["todos"].as_array().expect("todos") {
        let label = case["label"].as_str().expect("label");
        let input = &case["input"];
        let output = case["output"].as_str().expect("output");

        if !input.is_array() {
            assert_eq!(output, TODOS_NOT_ARRAY, "非数组拒绝文案（{label}）");
            continue;
        }

        let items = project_todos(input.as_array().expect("数组"));
        assert_eq!(todos_output(&items), output, "清单输出（{label}）");

        let active = case["active"].as_array().expect("active");
        assert_eq!(active.len(), items.len(), "活跃清单条数（{label}）");
        for (item, expected) in items.iter().zip(active.iter()) {
            assert_eq!(
                item.id,
                expected["id"].as_str().expect("id"),
                "条目 ID（{label}）"
            );
            assert_eq!(
                item.step,
                expected["step"].as_str().expect("step"),
                "条目步骤（{label}）"
            );
            assert_eq!(
                item.completed,
                expected["completed"].as_bool().expect("completed"),
                "完成标记（{label}）"
            );
        }

        let notified = case["notified"].as_array().expect("notified");
        assert_eq!(notified.len(), 1, "通知载荷只发一次（{label}）");
        assert_eq!(
            notified[0]["todos"].as_array().expect("todos").len(),
            items.len(),
            "通知载荷条数（{label}）"
        );
    }
}

#[test]
fn ask_user_parsing_matches_python() {
    let data = section();
    for case in data["ask_user"].as_array().expect("ask_user") {
        let label = case["label"].as_str().expect("label");
        let args = arguments(&case["arguments"]);
        let output = case["output"].as_str().expect("output");
        let parsed = parse_ask_user(&args);

        let Some(expected) = case["request"].as_object() else {
            let error = parsed.expect_err("入参非法时应当拒绝");
            assert_eq!(error, output, "拒绝文案（{label}）");
            continue;
        };

        let request = parsed.expect("入参合法时应当通过");
        assert_eq!(
            request.kind,
            expected["kind"].as_str().expect("kind"),
            "归一化后的 kind（{label}）"
        );
        assert_eq!(
            request.question,
            expected["question"].as_str().expect("question"),
            "问题正文（{label}）"
        );
        assert_eq!(
            request.options,
            strings(&expected["options"]),
            "选项列表（{label}）"
        );
        assert_eq!(
            request.request_id,
            expected["request_id"].as_str().expect("request_id"),
            "请求标识（{label}）"
        );

        if output == ASK_USER_UNANSWERED {
            assert!(case["answer"].is_null(), "未回答用例不应带答案（{label}）");
        } else {
            let answer = case["answer"].as_str().expect("answer");
            assert_eq!(
                ask_user_answer_output(&request, answer),
                output,
                "回答信封（{label}）"
            );
        }
    }
}

#[test]
fn memory_scope_matches_python() {
    let data = section();
    for case in data["memory_scope"].as_array().expect("memory_scope") {
        let label = case["label"].as_str().expect("label");
        let args = arguments(&case["arguments"]);
        let expected_error = case["error"].as_str();

        match parse_memory_scope(args.get("scope")) {
            Err(error) => {
                assert_eq!(
                    Some(error.message()),
                    expected_error,
                    "非法 scope 文案（{label}）"
                );
            }
            Ok(scope) => {
                // 宿主按作用域取存储：这里与生成器用同一套桩，未注入存储的作用域应当拒绝。
                let store = match scope {
                    MemoryScope::Project => Some("project-store"),
                    MemoryScope::User => Some("user-store"),
                    MemoryScope::Session => None,
                };
                match store {
                    Some(name) => {
                        assert_eq!(case["store"].as_str(), Some(name), "选中的存储（{label}）");
                        assert!(expected_error.is_none(), "启用时不报错（{label}）");
                    }
                    None => assert_eq!(
                        Some(memory_store_missing_error(scope).message()),
                        expected_error,
                        "未启用时的文案（{label}）"
                    ),
                }
            }
        }
    }
}
