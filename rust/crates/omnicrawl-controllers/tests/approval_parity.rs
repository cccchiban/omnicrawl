//! 审批判定层的跨语言对照（parity）。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `approval` 段后，本套件用同一份语料重放
//! Rust 的手写匹配器逐条比对。语料是新旧两侧共同的语义基准，改规则必须同时重跑生成器。

use omnicrawl_controllers::approval::{self, ApprovalDecision, ApprovalMode};
use serde_json::{Map, Value};

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn approval_section() -> Value {
    fixture()["approval"].clone()
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().expect("对象参数")
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

#[test]
fn approval_constants_match_python() {
    let data = approval_section();
    let expected = &data["constants"];
    assert_eq!(
        approval::TOOL_REVIEW_SYSTEM_PROMPT,
        expected["tool_review_system_prompt"]
            .as_str()
            .expect("text")
    );
    assert_eq!(
        approval::GIT_TOOL_NAME,
        expected["git_tool_name"].as_str().expect("text")
    );
    assert_eq!(
        approval::GIT_TIER_READONLY,
        expected["git_tier_readonly"].as_str().expect("text")
    );
    assert_eq!(
        approval::GIT_TIER_LOCAL,
        expected["git_tier_local"].as_str().expect("text")
    );
    assert_eq!(
        approval::GIT_TIER_HIGH,
        expected["git_tier_high"].as_str().expect("text")
    );
    assert_eq!(
        approval::SHELL_RISK_REVIEW,
        expected["shell_risk_review"].as_str().expect("text")
    );
    assert_eq!(
        approval::SHELL_RISK_SAFE,
        expected["shell_risk_safe"].as_str().expect("text")
    );
    assert_eq!(
        approval::GIT_SUPPORTED_ACTIONS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["git_supported_actions"])
    );
    assert_eq!(
        sorted(&approval::GIT_READ_ONLY_SUBCOMMANDS),
        strings(&expected["git_read_only_subcommands"])
    );
    assert_eq!(
        sorted(&approval::GIT_HIGH_RISK_ACTIONS),
        strings(&expected["git_high_risk_actions"])
    );
    assert_eq!(
        sorted(&approval::GIT_MIXED_ACTIONS),
        strings(&expected["git_mixed_actions"])
    );
    assert_eq!(
        sorted(&approval::GIT_INTENT_KEYS),
        strings(&expected["git_intent_keys"])
    );
    assert_eq!(
        sorted(&approval::DELETE_INTENT_KEYS),
        strings(&expected["delete_intent_keys"])
    );
    assert_eq!(
        approval::DELETE_LOCALIZED_TERMS
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>(),
        strings(&expected["delete_localized_terms"])
    );
    assert_eq!(
        approval::REVIEW_USER_SUMMARY_MAX_CHARS as u64,
        expected["review_user_summary_max_chars"]
            .as_u64()
            .expect("int")
    );
    assert_eq!(
        approval::REVIEW_ASK_USER_QA_MAX_CHARS as u64,
        expected["review_ask_user_qa_max_chars"]
            .as_u64()
            .expect("int")
    );

    for (key, mode) in [
        ("approval_mode_auto", ApprovalMode::Auto),
        ("approval_mode_review", ApprovalMode::Review),
        ("approval_mode_manual", ApprovalMode::Manual),
    ] {
        let text = expected[key].as_str().expect("mode");
        assert_eq!(mode.as_str(), text, "审批模式取值（{key}）");
        assert_eq!(
            ApprovalMode::from_config(text),
            Some(mode),
            "审批模式解析（{key}）"
        );
    }
}

#[test]
fn approval_names_match_python() {
    let data = approval_section();
    for case in data["names"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            approval::is_git_tool_call(name),
            case["is_git"].as_bool().expect("is_git"),
            "git 工具识别（{label}）"
        );
        assert_eq!(
            approval::is_shell_command_tool_call(name),
            case["is_shell"].as_bool().expect("is_shell"),
            "shell 工具识别（{label}）"
        );
    }
}

#[test]
fn approval_schema_detection_matches_python() {
    let data = approval_section();
    for case in data["schemas"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            approval::tool_accepts_shell_command(case["schema"].as_str().expect("schema")),
            case["expected"].as_bool().expect("expected"),
            "schema 判定（{label}）"
        );
    }
}

#[test]
fn approval_git_tier_matches_python() {
    let data = approval_section();
    for case in data["git_tier"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            approval::git_action_tier(&object(&case["arguments"])),
            case["expected"].as_str().expect("expected"),
            "git 风险档（{label}）"
        );
    }
}

#[test]
fn approval_git_mutation_matches_python() {
    let data = approval_section();
    for case in data["git_mutation"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            approval::is_git_mutation_tool_call(
                case["name"].as_str().expect("name"),
                &object(&case["arguments"])
            ),
            case["expected"].as_bool().expect("expected"),
            "git 变更判定（{label}）"
        );
    }
}

#[test]
fn approval_command_git_intent_matches_python() {
    let data = approval_section();
    for case in data["command_git_intent"].as_array().expect("cases") {
        let command = case["command"].as_str().expect("command");
        assert_eq!(
            approval::command_has_git_mutation_intent(command),
            case["expected"].as_bool().expect("expected"),
            "命令内 git 变更意图（{command}）"
        );
    }
}

#[test]
fn approval_download_exec_matches_python() {
    let data = approval_section();
    for case in data["download_exec"].as_array().expect("cases") {
        let command = case["command"].as_str().expect("command");
        assert_eq!(
            approval::command_has_download_exec_intent(command),
            case["expected"].as_bool().expect("expected"),
            "下载即执行判定（{command}）"
        );
    }
}

#[test]
fn approval_classify_shell_matches_python() {
    let data = approval_section();
    for case in data["classify"].as_array().expect("cases") {
        let command = case["command"].as_str().expect("command");
        let expected = case["expected"].as_str().expect("expected");
        assert_eq!(
            approval::classify_shell_command(command),
            expected,
            "命令分流（{command}）"
        );
        assert!(
            expected == approval::SHELL_RISK_REVIEW || expected == approval::SHELL_RISK_SAFE,
            "分流取值（{command}）"
        );
    }
}

#[test]
fn approval_command_delete_intent_matches_python() {
    let data = approval_section();
    for case in data["command_delete"].as_array().expect("cases") {
        let command = case["command"].as_str().expect("command");
        assert_eq!(
            approval::command_has_delete_intent(command),
            case["expected"].as_bool().expect("expected"),
            "命令删除意图（{command}）"
        );
    }
}

#[test]
fn approval_text_delete_intent_matches_python() {
    let data = approval_section();
    for case in data["text_delete"].as_array().expect("cases") {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            approval::text_has_delete_intent(text),
            case["expected"].as_bool().expect("expected"),
            "文本删除意图（{text}）"
        );
    }
}

#[test]
fn approval_description_delete_intent_matches_python() {
    let data = approval_section();
    for case in data["description_delete"].as_array().expect("cases") {
        let text = case["text"].as_str().expect("text");
        assert_eq!(
            approval::description_has_delete_intent(text),
            case["expected"].as_bool().expect("expected"),
            "描述删除意图（{text}）"
        );
    }
}

#[test]
fn approval_arguments_delete_intent_matches_python() {
    let data = approval_section();
    let mcp_keys: Vec<&str> = approval::DELETE_INTENT_KEYS.to_vec();
    for case in data["arguments_delete"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let value = &case["value"];
        assert_eq!(
            approval::arguments_have_delete_intent(value),
            case["default_keys"].as_bool().expect("default_keys"),
            "默认意图字段（{label}）"
        );
        assert_eq!(
            approval::arguments_have_delete_intent_with_keys(value, &mcp_keys),
            case["mcp_keys"].as_bool().expect("mcp_keys"),
            "MCP 意图字段（{label}）"
        );
    }
}

#[test]
fn approval_delete_behavior_matches_python() {
    let data = approval_section();
    for case in data["delete_behavior"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            approval::is_delete_behavior_tool_call(
                case["name"].as_str().expect("name"),
                case["description"].as_str().expect("description"),
                case["schema"].as_str().expect("schema"),
                &object(&case["arguments"])
            ),
            case["expected"].as_bool().expect("expected"),
            "删除类调用判定（{label}）"
        );
    }
}

#[test]
fn approval_review_response_matches_python() {
    let data = approval_section();
    for case in data["review_response"].as_array().expect("cases") {
        let text = case["text"].as_str().expect("text");
        let (approved, reason) = approval::parse_tool_review_response(text);
        assert_eq!(
            approved,
            case["approved"].as_bool().expect("approved"),
            "审查结论（{text}）"
        );
        assert_eq!(
            reason,
            case["reason"].as_str().expect("reason"),
            "审查结论理由（{text}）"
        );
    }
}

#[test]
fn approval_decisions_match_python() {
    let data = approval_section();
    for case in data["decisions"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let mode =
            ApprovalMode::from_config(case["mode"].as_str().expect("mode")).expect("审批模式取值");
        let decision = approval::decide(
            case["name"].as_str().expect("name"),
            case["description"].as_str().expect("description"),
            case["schema"].as_str().expect("schema"),
            &object(&case["arguments"]),
            mode,
        );
        let expected = match case["decision"].as_str().expect("decision") {
            "approve" => ApprovalDecision::Approve,
            "review" => ApprovalDecision::Review,
            "confirm" => ApprovalDecision::Confirm,
            other => panic!("未知结论：{other}"),
        };
        assert_eq!(decision, expected, "审批归属（{label}）");
    }
}

#[test]
fn approval_message_extraction_matches_python() {
    let data = approval_section();
    for case in data["messages"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let messages: Vec<Value> = case["messages"].as_array().expect("messages").to_vec();
        assert_eq!(
            approval::review_user_intent_summary(
                &messages,
                approval::REVIEW_USER_SUMMARY_MAX_CHARS
            ),
            case["user_summary"].as_str().expect("user_summary"),
            "用户意图摘要（{label}）"
        );
        assert_eq!(
            approval::review_ask_user_qa(&messages, approval::REVIEW_ASK_USER_QA_MAX_CHARS),
            case["ask_user_qa"].as_str().expect("ask_user_qa"),
            "ask_user 问答（{label}）"
        );
        if let Some(message) = messages.first() {
            assert_eq!(
                approval::message_plain_text(message),
                case["first_plain_text"].as_str().expect("first_plain_text"),
                "消息纯文本（{label}）"
            );
        }
    }
}
