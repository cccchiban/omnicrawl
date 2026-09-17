//! 跨语言 parity：同一份请求输入与同一份 SSE，Python 真 runtime 与 Rust 内核各跑一遍，
//! 比对事件序列、请求体、归并结果与失败文案。
//!
//! fixture 由 `rust/tools/gen_llm_runtime_fixture.py` 生成：它起一个本机回环服务端，
//! 把固定 SSE 喂给 `OpenAIChatCompletionsRuntime`（真 SDK 客户端 + 仓库的 httpx 客户端工厂），
//! 记录真实现收到的事件、实际发出的请求体与失败时的错误文案。

mod common;

use common::{event_to_json, CaseInput, RecordingSink, Reply, StubServer};
use omnicrawl_llm::{ChatEndpoint, OpenAiChatRuntime};
use omnicrawl_protocol::{ModelReply, TokenUsage};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/openai_chat_runtime_parity.json");

#[test]
fn runtime_matches_python() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    for case in fixture["cases"].as_array().expect("缺少 cases") {
        let name = case["name"].clone();
        let input: CaseInput =
            serde_json::from_value(case["input"].clone()).expect("用例输入无法解析");

        let replies: Vec<Reply> = case["replies"]
            .as_array()
            .expect("缺少 replies")
            .iter()
            .map(|reply| {
                Reply::sse(
                    reply["status"].as_u64().unwrap_or(200) as u16,
                    reply["body"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        let server = StubServer::spawn(replies);
        let runtime = OpenAiChatRuntime::new(ChatEndpoint {
            base_url: server.base_url(),
            api_key: "test-key".to_string(),
            user_agent: "omnicrawl-test".to_string(),
        });

        let mut sink = RecordingSink::default();
        let outcome = runtime.run_turn(&input.chat_input(), &mut sink);
        let expected = &case["expected"];

        let actual_events: Vec<Value> = sink.events.iter().map(event_to_json).collect();
        assert_eq!(
            Value::Array(actual_events),
            expected["events"],
            "用例 {name} 的事件序列不一致"
        );

        match (&outcome, expected["error"].as_str()) {
            (Ok(reply), None) => compare_reply(&name, reply, &expected["reply"]),
            (Err(error), Some(_expected_message)) => {
                assert_eq!(
                    json!(error.message),
                    expected["error"],
                    "用例 {name} 的错误文案不一致"
                );
                if let Some(retryable) = expected["retryable"].as_bool() {
                    assert_eq!(error.retryable, retryable, "用例 {name} 的可重试标记不一致");
                }
            }
            (Ok(_), Some(expected_message)) => {
                panic!("用例 {name} 期望失败（{expected_message}），Rust 却返回成功")
            }
            (Err(error), None) => {
                panic!("用例 {name} 期望成功，Rust 却失败：{}", error.message)
            }
        }

        // 请求体：用例消息里没有工具调用历史，因此不需要键序规范化，可直接逐字段比对。
        // 重试位置不同——Python 的 SDK 自己会重试 5xx/超时（内置 2 次），内核不内置重试，
        // 只保留 prompt_cache_key 的摘字段重发，所以这里只比首个请求体、次数另行断言。
        let rust_bodies = server.bodies();
        let python_bodies = expected["requests"].as_array().expect("缺少 requests");
        let expected_count = if name == "prompt_cache_rejected" {
            2
        } else {
            1
        };
        assert_eq!(
            rust_bodies.len(),
            expected_count,
            "用例 {name} 的请求次数不一致（Python 侧 {python_bodies:?}）"
        );
        assert_eq!(
            rust_bodies[0], python_bodies[0],
            "用例 {name} 的首个请求体不一致"
        );
    }
}

fn compare_reply(name: &Value, reply: &ModelReply, expected: &Value) {
    assert_eq!(
        json!(reply.content),
        expected["content"],
        "用例 {name} 正文不一致"
    );
    assert_eq!(
        json!(reply.reasoning),
        expected["reasoning"],
        "用例 {name} 推理不一致"
    );
    assert_eq!(
        json!(reply.finish_reason),
        expected["finish_reason"],
        "用例 {name} 结束原因不一致"
    );
    assert_eq!(
        json!(reply.content_streamed),
        expected["content_streamed"],
        "用例 {name} 流式标记不一致"
    );

    match (&reply.usage, &expected["usage"]) {
        (None, Value::Null) => {}
        (Some(rust_usage), expected_usage) => {
            assert_eq!(
                usage_json(*rust_usage),
                *expected_usage,
                "用例 {name} 用量不一致"
            );
        }
        (rust_usage, expected_usage) => {
            panic!("用例 {name} 用量有无不一致：Rust {rust_usage:?}，Python {expected_usage}")
        }
    }

    let calls: Vec<Value> = reply
        .tool_calls
        .iter()
        .map(|call| {
            json!({
                "call_id": call.call_id,
                "name": call.name,
                "arguments": call.arguments,
            })
        })
        .collect();
    assert_eq!(
        Value::Array(calls),
        expected["tool_calls"],
        "用例 {name} 工具调用不一致"
    );
}

fn usage_json(usage: TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "reasoning_tokens": usage.reasoning_tokens,
    })
}
