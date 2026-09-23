//! 线上协议纯函数的对照测试：分帧、拆包、SSE、分页与结果文本化。

mod common;

use omnicrawl_mcp::jsonrpc::{
    capability_declared, list_capability_pages, parse_content_length, parse_sse_json_payloads,
    stringify_prompt_payload, stringify_resource_payload, stringify_tool_result_payload,
    unwrap_json_rpc_response, FrameKind, McpClientError,
};
use serde_json::{json, Map, Value};

#[test]
fn content_length_headers_match_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "headers");
    assert!(!cases.is_empty());
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let kind = match common::field(&case, "kind").as_str().unwrap_or_default() {
            "request" => FrameKind::Request,
            other => {
                assert_eq!(other, "response");
                FrameKind::Response
            }
        };
        let header = common::field(&case, "header").as_str().unwrap_or_default();
        match parse_content_length(header.as_bytes(), kind) {
            Ok(length) => assert_eq!(
                json!(length),
                *common::field(&case, "length"),
                "用例 {name} 的解析长度不一致"
            ),
            Err(error) => {
                if case.get("error_kind").and_then(|value| value.as_str()) == Some("ValueError") {
                    // Python 本地 Server 侧把非法长度直接抛成 ValueError；Rust 降级为协议错误。
                    assert_eq!(error.message(), "MCP Content-Length 不是整数。");
                    continue;
                }
                assert_eq!(
                    json!(error.message()),
                    *common::field(&case, "error"),
                    "用例 {name} 的错误文案不一致"
                );
            }
        }
    }
}

#[test]
fn sse_payloads_match_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "sse");
    assert!(!cases.is_empty());
    for case in cases {
        let text = common::field(&case, "text").as_str().unwrap_or_default();
        let actual: Vec<Value> = parse_sse_json_payloads(text)
            .into_iter()
            .map(Value::Object)
            .collect();
        assert_eq!(
            Value::Array(actual),
            *common::field(&case, "payloads"),
            "SSE 文本 {text:?} 的解析结果不一致"
        );
    }
}

#[test]
fn stringified_payloads_match_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "stringify");
    assert!(cases.len() >= 10);
    for case in cases {
        let kind = common::field(&case, "kind").as_str().unwrap_or_default();
        let payload = common::field(&case, "payload")
            .as_object()
            .cloned()
            .unwrap_or_default();
        let actual = match kind {
            "tool" => stringify_tool_result_payload(&payload),
            "resource" => stringify_resource_payload(&payload),
            _ => stringify_prompt_payload(&payload),
        };
        assert_eq!(
            json!(actual),
            *common::field(&case, "text"),
            "{kind} 用例 {payload:?} 的文本不一致"
        );
    }
}

#[test]
fn json_rpc_unwrap_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "unwrap");
    assert!(cases.len() >= 8);
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let method = common::field(&case, "method").as_str().unwrap_or_default();
        let payload = common::field(&case, "payload")
            .as_object()
            .cloned()
            .unwrap_or_default();
        match unwrap_json_rpc_response(&payload, method) {
            Ok(result) => assert_eq!(
                Value::Object(result),
                *common::field(&case, "result"),
                "用例 {name} 的拆包结果不一致"
            ),
            Err(error) => assert_eq!(
                json!(error.message()),
                *common::field(&case, "error"),
                "用例 {name} 的错误文案不一致"
            ),
        }
    }
}

#[test]
fn capability_declaration_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "capability");
    assert!(!cases.is_empty());
    for case in cases {
        let init = common::field(&case, "init")
            .as_object()
            .cloned()
            .unwrap_or_default();
        let capability = common::field(&case, "capability")
            .as_str()
            .unwrap_or_default();
        assert_eq!(
            json!(capability_declared(&init, capability)),
            *common::field(&case, "declared"),
            "能力声明判定不一致"
        );
    }
}

#[test]
fn pagination_matches_python() {
    let data = common::fixture();
    let cases = common::cases(&data["protocol"], "pagination");
    assert!(!cases.is_empty());
    for case in cases {
        let name = common::field(&case, "name").as_str().unwrap_or_default();
        let script: Vec<Value> = common::field(&case, "pages")
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut calls: Vec<Value> = Vec::new();
        let items = list_capability_pages(
            |method, params| {
                calls.push(json!({"method": method, "params": Value::Object(params.clone())}));
                let index = calls.len() - 1;
                let page = script.get(index).cloned().unwrap_or(Value::Null);
                if page.get("error").is_some() {
                    return Err(McpClientError::new("分页请求失败"));
                }
                let mut payload = Map::new();
                payload.insert(
                    "tools".to_string(),
                    page.get("items").cloned().unwrap_or(json!([])),
                );
                if let Some(next) = page.get("next").and_then(|value| value.as_str()) {
                    payload.insert("nextCursor".to_string(), json!(next));
                }
                Ok(payload)
            },
            "tools/list",
            "tools",
        );
        assert_eq!(
            Value::Array(items.into_iter().map(Value::Object).collect()),
            *common::field(&case, "items"),
            "用例 {name} 的分页结果不一致"
        );
        assert_eq!(
            Value::Array(calls),
            *common::field(&case, "calls"),
            "用例 {name} 的请求序列不一致"
        );
    }
}
