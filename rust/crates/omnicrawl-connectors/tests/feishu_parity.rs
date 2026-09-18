//! 飞书连接器的跨语言 parity：期望值来自 Python `omnicrawl/connectors/fsapp.py`。
//!
//! 覆盖文本清理与分段、工具摘要与正文、文件变更预览（含 `SequenceMatcher` 统计）、
//! 执行计划与子任务进度、卡片 JSON、配置解析、去重键，以及时间线条目真正发出的消息序列。
//! 卡片负载按键序逐字节比对（Python 的 `json.dumps` 分隔符也是契约的一部分）。

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use omnicrawl_connectors::feishu::render::{
    clip_line, compact_line, diff_preview_lines, file_change_preview, file_change_result_note,
    file_change_summary, format_line_stats, list_result_summary, operation_of,
    read_result_line_range, sample_output_lines, MAX_SUBAGENT_LINES, REASONING_PREVIEW_LINES,
    TOOL_BODY_MAX_LINES,
};
use omnicrawl_connectors::feishu::ws::pbbp2;
use omnicrawl_connectors::feishu::{
    check_config, classify_filename, clean_text, display_text, file_marker_paths, format_elapsed,
    inbox_dedupe_key, load_feishu_config, mask_secret, normalize_todos, parse_json_object,
    post_text_and_images, reasoning_panel, resolve_final_text, resolve_temp_destination,
    split_segment_for_card, split_text, todos_text, tool_body, tool_summary, ConfigSource,
    MessagePort, PlanMessage, ReasoningMessage, SeenMessages, SubAgentMessage, TextMessage,
    ToolMessage, ToolRecord, DEDUP_MAX_ENTRIES, MAX_TEXT_CHARS, SEGMENT_MAX_CHARS,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/feishu_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

/// 记录型平台端口：与生成器里的 Python stub 一一对应。
#[derive(Default)]
struct RecordingPort {
    calls: Mutex<Vec<Value>>,
    counter: Mutex<usize>,
}

impl RecordingPort {
    fn calls(&self) -> Vec<Value> {
        self.calls.lock().expect("调用记录锁中毒").clone()
    }
}

impl MessagePort for RecordingPort {
    fn send_raw(
        &self,
        receive_id: &str,
        payload: &str,
        msg_type: &str,
        receive_id_type: &str,
    ) -> Option<String> {
        let mut counter = self.counter.lock().expect("计数锁中毒");
        *counter += 1;
        let message_id = format!("om_{counter}");
        self.calls.lock().expect("调用记录锁中毒").push(json!({
            "call": "send_raw",
            "receive_id": receive_id,
            "payload": payload,
            "msg_type": msg_type,
            "receive_id_type": receive_id_type,
        }));
        Some(message_id)
    }

    fn patch_card(&self, message_id: &str, payload: &str) -> bool {
        self.calls.lock().expect("调用记录锁中毒").push(json!({
            "call": "patch_card",
            "message_id": message_id,
            "payload": payload,
        }));
        true
    }

    fn send_text(&self, receive_id: &str, text: &str, receive_id_type: &str) -> bool {
        self.calls.lock().expect("调用记录锁中毒").push(json!({
            "call": "send_text",
            "receive_id": receive_id,
            "text": text,
            "receive_id_type": receive_id_type,
        }));
        true
    }
}

/// 把耗时片段（Python 侧固定 136ms，Rust 侧按真实微秒计）统一成占位，其余仍逐字节比对。
fn normalize_durations(calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .map(|call| {
            let mut call = call.clone();
            if let Some(payload) = call.get("payload").and_then(Value::as_str) {
                let normalized = replace_duration_token(payload);
                call["payload"] = json!(normalized);
            }
            call
        })
        .collect()
}

fn replace_duration_token(text: &str) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(" · ") {
        result.push_str(&rest[..index]);
        let after = &rest[index + " · ".len()..];
        let digits: String = after
            .chars()
            .take_while(|value| value.is_ascii_digit())
            .collect();
        if !digits.is_empty() && after[digits.len()..].starts_with("ms") {
            result.push_str(" · <duration>ms");
            rest = &after[digits.len() + 2..];
            continue;
        }
        result.push_str(" · ");
        rest = after;
    }
    result.push_str(rest);
    result
}

#[test]
fn constants_match_python() {
    let constants = &fixture()["constants"];
    assert_eq!(MAX_TEXT_CHARS as u64, constants["max_text_chars"]);
    assert_eq!(SEGMENT_MAX_CHARS as u64, constants["segment_max_chars"]);
    assert_eq!(TOOL_BODY_MAX_LINES as u64, constants["tool_body_max_lines"]);
    assert_eq!(
        REASONING_PREVIEW_LINES as u64,
        constants["reasoning_preview_lines"]
    );
    assert_eq!(MAX_SUBAGENT_LINES as u64, constants["max_subagent_lines"]);
    assert_eq!(DEDUP_MAX_ENTRIES as u64, constants["dedup_max_entries"]);
}

#[test]
fn text_helpers_match_python() {
    let fixture = fixture();
    let text = &fixture["text"];
    for case in text["clean"].as_array().expect("clean") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(json!(clean_text(input)), case["expected"], "clean {case}");
    }
    for case in text["display"].as_array().expect("display") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            json!(display_text(input)),
            case["expected"],
            "display {case}"
        );
    }
    for case in text["split"].as_array().expect("split") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(json!(split_text(input)), case["expected"], "split {case}");
    }
    for case in text["resolve"].as_array().expect("resolve") {
        assert_eq!(
            json!(resolve_final_text(
                case["streamed"].as_str().expect("streamed"),
                case["reply"].as_str().expect("reply")
            )),
            case["expected"],
            "resolve {case}"
        );
    }
    for case in text["segments"].as_array().expect("segments") {
        let input = case["input"].as_str().expect("input");
        let (head, tail) = split_segment_for_card(input);
        assert_eq!(
            json!([head, tail]),
            case["expected"],
            "segments 长度 {}",
            input.chars().count()
        );
    }
    for case in text["helpers"]["compact"].as_array().expect("compact") {
        let value = &case["value"];
        assert_eq!(
            json!(compact_line(
                Some(value),
                case["max_chars"].as_u64().expect("max_chars") as usize
            )),
            case["expected"],
            "compact {case}"
        );
    }
    for case in text["helpers"]["clip"].as_array().expect("clip") {
        assert_eq!(
            json!(clip_line(case["line"].as_str().expect("line"), 160)),
            case["expected"],
            "clip {case}"
        );
    }
    for case in text["helpers"]["elapsed"].as_array().expect("elapsed") {
        assert_eq!(
            json!(format_elapsed(case["seconds"].as_f64().expect("seconds"))),
            case["expected"],
            "elapsed {case}"
        );
    }
    for case in text["helpers"]["operation"].as_array().expect("operation") {
        assert_eq!(
            json!(operation_of(case["name"].as_str().expect("name"))),
            case["expected"],
            "operation {case}"
        );
    }
    for case in text["helpers"]["stats"].as_array().expect("stats") {
        assert_eq!(
            json!(format_line_stats(
                case["added"].as_u64().expect("added") as usize,
                case["removed"].as_u64().expect("removed") as usize
            )),
            case["expected"],
            "stats {case}"
        );
    }
}

#[test]
fn tool_rendering_matches_python() {
    let fixture = fixture();
    let tools = &fixture["tools"];
    for case in tools["summaries"].as_array().expect("summaries") {
        let name = case["name"].as_str().expect("name");
        let arguments = &case["arguments"];
        let result = case["result"].as_str().expect("result");
        assert_eq!(
            json!(tool_summary(name, arguments, result)),
            case["expected"],
            "摘要 {case}"
        );
    }
    for case in tools["bodies"].as_array().expect("bodies") {
        let name = case["name"].as_str().expect("name");
        let arguments = &case["arguments"];
        let result = case["result"].as_str().expect("result");
        assert_eq!(
            json!(tool_body(name, arguments, result)),
            case["expected"],
            "正文 {case}"
        );
    }
    for case in tools["sample_lines"].as_array().expect("sample_lines") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(
            json!(sample_output_lines(input)),
            case["expected"],
            "采样 {case}"
        );
    }
    for case in tools["read_range"].as_array().expect("read_range") {
        let input = case["input"].as_str().expect("input");
        let expected = match &case["expected"] {
            Value::Null => Value::Null,
            other => json!([other[0], other[1]]),
        };
        let actual = match read_result_line_range(input) {
            Some((first, last)) => json!([first, last]),
            None => Value::Null,
        };
        assert_eq!(actual, expected, "行号 {case}");
    }
    for case in tools["list_summary"].as_array().expect("list_summary") {
        let input = case["input"].as_str().expect("input");
        let expected = match &case["expected"] {
            Value::Null => Value::Null,
            other => other.clone(),
        };
        let actual = match list_result_summary(input) {
            Some(value) => json!(value),
            None => Value::Null,
        };
        assert_eq!(actual, expected, "目录摘要 {case}");
    }
    for case in tools["diff"].as_array().expect("diff") {
        let old_text = case["old_text"].as_str().expect("old_text");
        let new_text = case["new_text"].as_str().expect("new_text");
        let (preview, added, removed) = diff_preview_lines(old_text, new_text);
        assert_eq!(json!(preview), case["preview"], "diff 预览 {case}");
        assert_eq!(json!(added), case["added"], "diff 新增 {case}");
        assert_eq!(json!(removed), case["removed"], "diff 删除 {case}");
    }
    for case in tools["file_change"].as_array().expect("file_change") {
        let operation = case["operation"].as_str().expect("operation");
        let arguments = &case["arguments"];
        assert_eq!(
            json!(file_change_summary(operation, arguments)),
            case["summary"],
            "变更统计 {case}"
        );
        assert_eq!(
            json!(file_change_preview(operation, arguments)),
            case["preview"],
            "变更预览 {case}"
        );
    }
    for case in tools["file_change_note"]
        .as_array()
        .expect("file_change_note")
    {
        let result = case["result"].as_str().expect("result");
        assert_eq!(
            json!(file_change_result_note("Edit_file", result)),
            case["expected"],
            "结果摘要 {case}"
        );
    }
}

#[test]
fn plan_and_reasoning_match_python() {
    let fixture = fixture();
    let plan = &fixture["plan"];
    for case in plan["normalize"].as_array().expect("normalize") {
        let items = if case["input"].is_null() {
            None
        } else {
            Some(&case["input"])
        };
        assert_eq!(
            json!(normalize_todos(items)),
            case["expected"],
            "计划规范化 {case}"
        );
    }
    for case in plan["text"].as_array().expect("text") {
        let todos: Vec<(String, bool)> = case["todos"]
            .as_array()
            .expect("todos")
            .iter()
            .map(|item| {
                (
                    item[0].as_str().expect("步骤").to_string(),
                    item[1].as_bool().expect("完成"),
                )
            })
            .collect();
        assert_eq!(
            json!(todos_text(&todos)),
            case["expected"],
            "计划文本 {case}"
        );
    }
    for case in plan["reasoning"].as_array().expect("reasoning") {
        let input = case["input"].as_str().expect("input");
        let streaming = case["streaming"].as_bool().expect("streaming");
        assert_eq!(
            json!(reasoning_panel(input, streaming)),
            case["expected"],
            "思考面板"
        );
    }
}

#[test]
fn subagent_timeline_matches_python() {
    for case in fixture()["subagents"].as_array().expect("subagents") {
        let port = Arc::new(RecordingPort::default());
        let mut message = SubAgentMessage::new(port.clone(), "oc_1", "chat_id");
        for event in case["scenario"].as_array().expect("scenario") {
            message.update(event["event"].as_str().expect("event"), &event["payload"]);
        }
        assert_eq!(json!(port.calls()), case["calls"], "子任务事件序列 {case}");
    }
}

#[test]
fn timeline_messages_match_python() {
    let fixture = fixture();
    let timeline = &fixture["timeline"];
    for case in timeline["text"].as_array().expect("text") {
        let port = Arc::new(RecordingPort::default());
        let mut message = TextMessage::new(port.clone(), "oc_1", "chat_id");
        let text = case["text"].as_str().expect("text");
        let suffix = case["suffix"].as_str().expect("suffix");
        let streamed = message.stream(text);
        let remaining = message.seal(text, suffix);
        assert_eq!(json!(streamed), case["streamed"], "正文流式 {case}");
        assert_eq!(json!(remaining), case["remaining"], "正文封口 {case}");
        assert_eq!(json!(port.calls()), case["calls"], "正文消息序列");
    }
    for case in timeline["tool"].as_array().expect("tool") {
        let port = Arc::new(RecordingPort::default());
        let name = case["name"].as_str().expect("name");
        let arguments = case["arguments"].clone();
        let record = ToolRecord::new("c1", name, arguments);
        let mut message = ToolMessage::new(port.clone(), "oc_1", "chat_id", record);
        message.start();
        message.finish(
            case["ok"].as_bool().expect("ok"),
            case["output"].as_str().expect("output"),
        );
        assert_eq!(
            json!(normalize_durations(&port.calls())),
            json!(normalize_durations(
                case["calls"].as_array().expect("calls").as_slice()
            )),
            "工具消息序列 {case}"
        );
    }

    let port = Arc::new(RecordingPort::default());
    let mut reasoning = ReasoningMessage::new(port.clone(), "oc_1", "chat_id");
    reasoning.stream("思考中");
    reasoning.seal("完整思考");
    assert_eq!(
        json!(port.calls()),
        timeline["reasoning"]["calls"],
        "思考面板序列"
    );

    let port = Arc::new(RecordingPort::default());
    let mut plan = PlanMessage::new(port.clone(), "oc_1", "chat_id");
    plan.update(Some(&json!([{"step": "第一步", "completed": true}])));
    plan.update(Some(&json!([{"step": "第一步", "completed": true}])));
    plan.update(Some(&json!([{"step": "第二步"}])));
    assert_eq!(
        json!(port.calls()),
        timeline["plan"]["calls"],
        "执行计划序列"
    );
}

#[test]
fn file_helpers_match_python() {
    let fixture = fixture();
    let files = &fixture["files"];
    for case in files["classify"].as_array().expect("classify") {
        let name = case["input"].as_str().expect("input");
        assert_eq!(
            json!(classify_filename(name)),
            case["expected"],
            "分类 {case}"
        );
    }
    let root = std::env::temp_dir().join(format!("ocl-feishu-parity-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for case in files["destination"].as_array().expect("destination") {
        let filename = case["filename"].as_str().expect("filename");
        for existing in case["existing"].as_array().expect("existing") {
            let name = existing.as_str().expect("existing");
            let category = if name.ends_with(".png") {
                "images"
            } else {
                "files"
            };
            let target = root.join(category).join(name);
            fs::create_dir_all(target.parent().expect("父目录")).expect("建目录");
            fs::write(&target, "x").expect("写占位文件");
        }
        let resolved = resolve_temp_destination(&root, filename).expect("落盘路径");
        let relative = resolved
            .strip_prefix(&root)
            .expect("在根内")
            .to_string_lossy()
            .replace('\\', "/");
        assert_eq!(json!(relative), case["expected"], "落盘命名 {case}");
    }
    let _ = fs::remove_dir_all(&root);

    for case in files["markers"].as_array().expect("markers") {
        let input = case["input"].as_str().expect("input");
        let expected: Vec<String> = case["expected"]
            .as_array()
            .expect("expected")
            .iter()
            .map(|item| item.as_str().expect("path").trim().to_string())
            .collect();
        assert_eq!(
            json!(file_marker_paths(input)),
            json!(expected),
            "文件标记 {case}"
        );
    }
    for case in files["post"].as_array().expect("post") {
        let (text, images) = post_text_and_images(&case["input"]);
        assert_eq!(json!([text, images]), case["expected"], "富文本 {case}");
    }
    assert_eq!(
        classic_file_types(),
        files["file_type_map"],
        "飞书文件类型映射"
    );
    assert_eq!(json!(resource_types()), files["resource_types"]);
}

fn classic_file_types() -> Value {
    let mut map = serde_json::Map::new();
    for (extension, file_type) in omnicrawl_connectors::feishu::FILE_TYPE_MAP {
        map.insert(extension.to_string(), json!(file_type));
    }
    Value::Object(map)
}

fn resource_types() -> Vec<String> {
    let mut values: Vec<String> = omnicrawl_connectors::feishu::MESSAGE_RESOURCE_TYPES
        .iter()
        .map(|value| value.to_string())
        .collect();
    values.sort();
    values
}

#[test]
fn config_loading_matches_python() {
    for case in fixture()["config"].as_array().expect("config") {
        let environment: BTreeMap<String, String> = case["environment"]
            .as_object()
            .expect("environment")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("env 值").to_string()))
            .collect();
        let lookup = move |name: &str| environment.get(name).cloned();
        let data = case["data"].clone();
        let result = load_feishu_config(ConfigSource {
            environment: &lookup,
            data: &data,
        });
        match &case["expected"] {
            Value::Object(expected) if expected.contains_key("error") => {
                let error = result.expect_err("应报错");
                assert_eq!(json!(error), expected["error"], "用例 {case}");
            }
            Value::Object(expected) => {
                let config = result.unwrap_or_else(|error| panic!("应解析成功：{error}"));
                assert_eq!(json!(config.app_id), expected["app_id"], "用例 {case}");
                assert_eq!(
                    json!(config.app_secret),
                    expected["app_secret"],
                    "用例 {case}"
                );
                assert_eq!(
                    json!(config
                        .allowed_user_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<String>>()),
                    expected["allowed_user_ids"],
                    "用例 {case}"
                );
                assert_eq!(
                    json!(config.public_access()),
                    expected["public_access"],
                    "用例 {case}"
                );
                assert_eq!(
                    config.confirmation_timeout_seconds,
                    expected["confirmation_timeout_seconds"]
                        .as_f64()
                        .expect("超时"),
                    "用例 {case}"
                );
                let diagnostics = check_config(&config);
                assert!(diagnostics["ready"].is_boolean());
            }
            other => panic!("fixture 形状意外：{other}"),
        }
    }
    for case in fixture()["mask"].as_array().expect("mask") {
        let input = case["input"].as_str().expect("input");
        assert_eq!(json!(mask_secret(input)), case["expected"], "掩码 {case}");
    }
}

#[test]
fn dedupe_matches_python() {
    let fixture = fixture();
    for case in fixture["dedupe"]["claim"].as_array().expect("claim") {
        let mut seen = SeenMessages::new();
        let actual: Vec<bool> = case["message_ids"]
            .as_array()
            .expect("message_ids")
            .iter()
            .map(|id| seen.claim(id.as_str().expect("id"), 1_000.0))
            .collect();
        assert_eq!(json!(actual), case["results"], "去重序列 {case}");
    }
    for case in fixture["dedupe"]["keys"].as_array().expect("keys") {
        let key = inbox_dedupe_key(
            case["message_type"].as_str().expect("type"),
            case["message_id"].as_str().expect("message_id"),
            case["create_time"].as_str().expect("create_time"),
            case["chat_id"].as_str().expect("chat_id"),
            case["open_id"].as_str().expect("open_id"),
            case["user_text"].as_str().expect("user_text"),
        );
        assert_eq!(json!(key), case["expected"], "去重键 {case}");
    }
}

#[test]
fn ws_frames_match_python_protobuf() {
    for case in fixture()["ws_frames"].as_array().expect("ws_frames") {
        let encoded = hex_to_bytes(case["encoded_hex"].as_str().expect("encoded_hex"));
        let frame = pbbp2::decode(&encoded).expect("可解码");
        assert_eq!(json!(frame.seq_id), case["seq_id"], "seq {case}");
        assert_eq!(json!(frame.log_id), case["log_id"], "log_id {case}");
        assert_eq!(json!(frame.service), case["service"], "service {case}");
        assert_eq!(json!(frame.method), case["method"], "method {case}");
        assert_eq!(
            json!(frame.payload_encoding),
            case["payload_encoding"],
            "payload_encoding {case}"
        );
        assert_eq!(
            json!(frame.payload_type),
            case["payload_type"],
            "payload_type {case}"
        );
        assert_eq!(
            json!(frame.log_id_new),
            case["log_id_new"],
            "log_id_new {case}"
        );
        assert_eq!(
            json!(hex_of(&frame.payload)),
            case["payload_hex"],
            "payload {case}"
        );
        let headers: Vec<Value> = case["headers"].as_array().expect("headers").to_vec();
        assert_eq!(
            json!(frame
                .headers
                .iter()
                .map(|header| json!({"key": header.key, "value": header.value}))
                .collect::<Vec<Value>>()),
            json!(headers),
            "帧头 {case}"
        );
        // 重编码与 Python 的**规范形态**逐字节一致：required 字段一律写出、可选项
        // 只在非默认值时写出（proto2 的存在性信息在解码后已经丢失）。
        assert_eq!(
            hex_of(&pbbp2::encode(&frame)),
            case["canonical_hex"],
            "重编码 {case}"
        );
    }
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("十六进制"), 16).expect("字节")
        })
        .collect()
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn json_object_parsing_is_defensive() {
    assert_eq!(parse_json_object(&json!("{\"a\": 1}")), json!({"a": 1}));
    assert_eq!(parse_json_object(&json!("不是 JSON")), json!({}));
    assert_eq!(parse_json_object(&json!([1, 2])), json!({}));
    assert_eq!(parse_json_object(&json!("")), json!({}));
    assert_eq!(
        parse_json_object(&json!({"content": "x"})),
        json!({"content": "x"})
    );
}
