//! 帧层行为：单行不变式、形状校验、id 保真与错误码。

use omnicrawl_ipc::frame::{error_code, ErrorObject, Frame, FrameError, Id};
use serde_json::json;

#[test]
fn request_round_trip_keeps_id_and_params() {
    let frame = Frame::request(
        Id::Number(7),
        "initialize",
        json!({"protocol_version": "1.0"}),
    );
    assert!(frame.is_request());
    let parsed = Frame::parse(&frame.to_line()).expect("回读帧");
    assert_eq!(parsed, frame);
    assert_eq!(parsed.id(), Some(&Id::Number(7)));
    assert_eq!(parsed.method(), Some("initialize"));
}

#[test]
fn notification_carries_no_id() {
    let frame = Frame::notification("turn.delta", json!({"text": "增量"}));
    assert!(frame.is_notification());
    assert!(!frame.is_request());
    assert_eq!(frame.id(), None);
    assert_eq!(Frame::parse(&frame.to_line()).expect("回读帧"), frame);
}

#[test]
fn response_and_error_response_round_trip() {
    let ok = Frame::response(Id::Text("r-1".into()), json!({"ok": true}));
    assert!(ok.is_response());
    assert_eq!(Frame::parse(&ok.to_line()).expect("回读帧"), ok);

    let failed = Frame::error_response(
        Id::Text("r-1".into()),
        ErrorObject::with_data(
            error_code::INVALID_PARAMS,
            "字段不符",
            json!({"field": "calls"}),
        ),
    );
    assert_eq!(Frame::parse(&failed.to_line()).expect("回读帧"), failed);
}

#[test]
fn lines_stay_single_line_even_with_newlines_in_payload() {
    let frame = Frame::notification("turn.delta", json!({"text": "第一行\n第二行\r\n\t结束"}));
    let line = frame.to_line();
    assert!(!line.contains('\n'), "帧必须是单行：{line}");
    assert!(!line.contains('\r'), "帧必须是单行：{line}");
    assert_eq!(Frame::parse(&line).expect("回读帧"), frame);
}

#[test]
fn id_kinds_are_preserved() {
    let numeric = Frame::request(Id::Number(-3), "shutdown", json!({}));
    let text = Frame::request(Id::Text("turn-1".into()), "shutdown", json!({}));
    assert!(numeric.to_line().contains("\"id\":-3"));
    assert!(text.to_line().contains("\"id\":\"turn-1\""));
    assert_eq!(
        Frame::parse(&numeric.to_line()).expect("回读帧").id(),
        Some(&Id::Number(-3))
    );
    assert_eq!(
        Frame::parse(&text.to_line()).expect("回读帧").id(),
        Some(&Id::Text("turn-1".into()))
    );
}

#[test]
fn malformed_lines_are_rejected() {
    assert_eq!(Frame::parse(""), Err(FrameError::Empty));
    assert_eq!(Frame::parse("  \t "), Err(FrameError::Empty));
    assert!(matches!(Frame::parse("{oops"), Err(FrameError::Json(_))));
    // id 只允许整数与字符串。
    assert!(matches!(
        Frame::parse(r#"{"jsonrpc":"2.0","id":true,"method":"x","params":{}}"#),
        Err(FrameError::Json(_))
    ));
    assert!(matches!(
        Frame::parse(r#"{"jsonrpc":"1.0","id":1,"method":"x","params":{}}"#),
        Err(FrameError::Invalid(_))
    ));
    // 请求不得携带 result。
    assert!(matches!(
        Frame::parse(r#"{"jsonrpc":"2.0","id":1,"method":"x","result":{}}"#),
        Err(FrameError::Invalid(_))
    ));
    // 既没有 method 也没有 id。
    assert!(matches!(
        Frame::parse(r#"{"jsonrpc":"2.0"}"#),
        Err(FrameError::Invalid(_))
    ));
    // 响应缺 result 与 error。
    assert!(matches!(
        Frame::parse(r#"{"jsonrpc":"2.0","id":1}"#),
        Err(FrameError::Invalid(_))
    ));
    // 响应同时带 result 与 error。
    assert!(matches!(
        Frame::parse(
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"x"}}"#
        ),
        Err(FrameError::Invalid(_))
    ));
}

#[test]
fn error_codes_match_jsonrpc_standard_and_protocol_extension() {
    assert_eq!(error_code::PARSE_ERROR, -32700);
    assert_eq!(error_code::INVALID_REQUEST, -32600);
    assert_eq!(error_code::METHOD_NOT_FOUND, -32601);
    assert_eq!(error_code::INVALID_PARAMS, -32602);
    assert_eq!(error_code::INTERNAL_ERROR, -32603);
    assert_eq!(error_code::UNSUPPORTED_PROTOCOL_VERSION, -32001);
    assert_eq!(error_code::TURN_BUSY, -32002);
}
