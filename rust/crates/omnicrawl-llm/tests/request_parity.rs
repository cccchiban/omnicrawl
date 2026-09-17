//! 跨语言 parity：用 Python 真实现产出的期望值校验 Rust 请求构建层。
//!
//! fixture 由 `rust/tools/gen_llm_request_fixture.py` 生成，覆盖六组：请求体组装
//!（messages 转换、工具声明、生成选项、prompt_cache_key）、provider_options 校验、
//! GPT 系列判定、prompt_cache_key 计算、工具调用参数串的书写形式、浮点写法。

use std::collections::BTreeMap;

use omnicrawl_llm::{
    build_chat_request, build_prompt_cache_key, is_openai_gpt_model, sanitize_provider_options,
    to_openai_messages, ChatRequest, ChatRequestInput, RequestError,
};
use omnicrawl_protocol::{
    ConversationMessage, GenerationOptions, MessageBlock, Role, ToolCallBlock, ToolSpec,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("fixtures/openai_chat_request_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 与 `ChatRequestInput` 同形；借由它把 fixture 里的输入反序列化成内核类型。
#[derive(Debug, Deserialize)]
struct CaseInput {
    model: String,
    system_prompt: String,
    #[serde(default)]
    messages: Vec<ConversationMessage>,
    #[serde(default)]
    tools: Vec<ToolSpec>,
    #[serde(default)]
    options: GenerationOptions,
    profile_request_timeout_seconds: f64,
    #[serde(default)]
    prompt_cache_capable: bool,
    #[serde(default)]
    prompt_cache_identity: BTreeMap<String, String>,
}

fn build(input: &CaseInput) -> Result<ChatRequest, RequestError> {
    build_chat_request(&ChatRequestInput {
        model: &input.model,
        system_prompt: &input.system_prompt,
        messages: &input.messages,
        tools: &input.tools,
        options: &input.options,
        profile_request_timeout_seconds: input.profile_request_timeout_seconds,
        prompt_cache_capable: input.prompt_cache_capable,
        prompt_cache_identity: &input.prompt_cache_identity,
    })
}

/// 把一条 assistant 工具调用消息交给 messages 转换，取回参数串。
fn arguments_string(arguments: Value) -> String {
    let parsed: Map<String, Value> = serde_json::from_value(arguments).expect("参数样本必须是对象");
    let mut message = ConversationMessage::new(Role::Assistant);
    message
        .blocks
        .push(MessageBlock::ToolCall(ToolCallBlock::new(
            "call_1", "probe", parsed,
        )));
    let messages = to_openai_messages("", &[message]);
    messages[0]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .expect("参数串必须是字符串")
        .to_string()
}

/// 参数串的键序在两侧必然不同：Python 保留 dict 插入序，Rust 的 `serde_json::Map` 是字典序。
/// 这里把两侧都规范成紧凑字典序后比对语义；书写形式由 `argument_string` 组逐字节钉住。
fn canonicalize_arguments(body: &Value) -> Value {
    let mut body = body.clone();
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return body;
    };
    for message in messages {
        let Some(tool_calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for call in tool_calls {
            let Some(arguments) = call.pointer_mut("/function/arguments") else {
                continue;
            };
            let Some(raw) = arguments.as_str() else {
                continue;
            };
            let parsed: Value = serde_json::from_str(raw).expect("参数串必须是合法 JSON");
            *arguments = Value::String(serde_json::to_string(&parsed).expect("参数可序列化"));
        }
    }
    body
}

#[test]
fn chat_request_matches_python() {
    for case in fixture()["requests"].as_array().expect("缺少 requests") {
        let input: CaseInput =
            serde_json::from_value(case["input"].clone()).expect("用例输入无法解析成内核类型");
        let name = &case["name"];
        match build(&input) {
            Ok(request) => {
                assert_eq!(
                    case["error"],
                    Value::Null,
                    "用例 {name} 期望组装失败，Rust 却成功"
                );
                assert_eq!(
                    canonicalize_arguments(&request.body),
                    canonicalize_arguments(&case["expected"]["body"]),
                    "用例 {name} 请求体不一致"
                );
                assert_eq!(
                    json!(request.timeout_seconds),
                    case["expected"]["timeout_seconds"],
                    "用例 {name} 超时不一致"
                );
            }
            Err(error) => {
                assert_eq!(
                    json!(error.message),
                    case["error"],
                    "用例 {name} 错误文案不一致"
                );
            }
        }
    }
}

#[test]
fn provider_options_sanitize_matches_python() {
    for case in fixture()["provider_options"]
        .as_array()
        .expect("缺少 provider_options")
    {
        let options: Map<String, Value> =
            serde_json::from_value(case["options"].clone()).expect("选项无法解析");
        let expected_ok = case["ok"].as_bool().unwrap_or(false);
        match sanitize_provider_options(&options) {
            Ok(sanitized) => {
                assert!(expected_ok, "用例 {:?} 期望被拒绝", case["options"]);
                assert_eq!(
                    Value::Object(sanitized),
                    case["sanitized"],
                    "用例 {:?} 收敛结果不一致",
                    case["options"]
                );
            }
            Err(error) => {
                assert!(!expected_ok, "用例 {:?} 期望通过", case["options"]);
                assert_eq!(
                    json!(error.message),
                    case["message"],
                    "用例 {:?} 错误文案不一致",
                    case["options"]
                );
            }
        }
    }
}

#[test]
fn gpt_model_detection_matches_python() {
    for case in fixture()["gpt_model"].as_array().expect("缺少 gpt_model") {
        let model = case["model"].as_str().unwrap_or_default();
        assert_eq!(
            json!(is_openai_gpt_model(model)),
            case["expected"],
            "GPT 判定用例 {model:?} 不一致"
        );
    }
}

#[test]
fn prompt_cache_key_matches_python() {
    for case in fixture()["prompt_cache_key"]
        .as_array()
        .expect("缺少 prompt_cache_key")
    {
        let identity: BTreeMap<String, String> =
            serde_json::from_value(case["identity"].clone()).expect("身份映射无法解析");
        let model = case["model"].as_str().unwrap_or_default();
        assert_eq!(
            json!(build_prompt_cache_key(&identity, model)),
            case["expected"],
            "prompt_cache_key 用例 {model:?} 不一致"
        );
    }
}

#[test]
fn argument_string_matches_python() {
    for case in fixture()["argument_string"]
        .as_array()
        .expect("缺少 argument_string")
    {
        assert_eq!(
            json!(arguments_string(case["arguments"].clone())),
            case["expected"],
            "参数串用例 {} 书写形式不一致",
            case["arguments"]
        );
    }
}

#[test]
fn float_notation_stays_semantically_equal() {
    for case in fixture()["float_notation"]
        .as_array()
        .expect("缺少 float_notation")
    {
        let actual = arguments_string(json!({"v": case["value"]}));
        let parsed: Value = serde_json::from_str(&actual).expect("参数串必须是合法 JSON");
        let expected: f64 = case["python"]
            .as_str()
            .expect("缺少 Python 写法")
            .parse()
            .expect("Python 写法必须是浮点");
        assert_eq!(
            parsed["v"].as_f64(),
            Some(expected),
            "浮点用例 {} 两侧语义不等价（Rust 写法 {actual}）",
            case["value"]
        );
    }
}
