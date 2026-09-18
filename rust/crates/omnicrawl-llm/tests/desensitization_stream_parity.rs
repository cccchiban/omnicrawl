//! 消息脱敏「流式还原」的跨语言 parity：期望值来自 Python `omnicrawl/llm/desensitization/stream.py`。
//!
//! 每个场景是一串操作；两侧都用注入计数器建注册表（序号因此确定），逐步比对返回值、严格模式
//! 错误文案、三路还原计数，以及最终的生命周期标记。占位符文本一律取自数据集：测试源码里不写
//! 完整占位符字面量（它会被宿主自身的消息脱敏还原成会话原文，详见生成器说明）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omnicrawl_llm::desensitization::{SequenceRegistry, StreamRestorer, TRUNCATED_FINISH_REASONS};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/desensitization_stream_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn counter(start: u64) -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(start))
}

fn source(shared: Arc<AtomicU64>) -> impl Fn() -> u64 + Send + Sync + 'static {
    move || shared.fetch_add(1, Ordering::SeqCst) + 1
}

fn text_of(op: &Value) -> &str {
    op["text"].as_str().expect("text")
}

/// 执行一步操作；返回结果或严格模式下的错误文案。
fn apply(restorer: &mut StreamRestorer<'_>, op: &Value) -> Result<Value, String> {
    let kind = op["op"].as_str().expect("op");
    match kind {
        "feed_text" => restorer
            .feed_text(text_of(op))
            .map(Value::String)
            .map_err(|error| error.message().to_string()),
        "feed_reasoning" => restorer
            .feed_reasoning(text_of(op))
            .map(Value::String)
            .map_err(|error| error.message().to_string()),
        "feed_tool_arguments" => restorer
            .feed_tool_arguments(op["call_id"].as_str().expect("call_id"), text_of(op))
            .map(Value::String)
            .map_err(|error| error.message().to_string()),
        "flush" => restorer
            .flush()
            .map(|(text, reasoning)| json!({ "text": text, "reasoning": reasoning }))
            .map_err(|error| error.message().to_string()),
        "flush_tool_arguments" => restorer
            .flush_tool_arguments()
            .map(|tails| json!(tails))
            .map_err(|error| error.message().to_string()),
        "restore_string" => restorer
            .restore_string(text_of(op))
            .map(Value::String)
            .map_err(|error| error.message().to_string()),
        "restore_arguments" => restorer
            .restore_arguments(&op["value"])
            .map_err(|error| error.message().to_string()),
        "note_text" => {
            restorer.note_text(text_of(op));
            Ok(Value::Null)
        }
        "note_reasoning" => {
            restorer.note_reasoning(text_of(op));
            Ok(Value::Null)
        }
        "note_tool_call" => {
            restorer.note_tool_call();
            Ok(Value::Null)
        }
        "note_completed" => {
            restorer.note_completed(op["finish_reason"].as_str().expect("finish_reason"));
            Ok(Value::Null)
        }
        "reply_usable" => Ok(json!(restorer.reply_usable())),
        "take_warnings" => Ok(json!(restorer.take_warnings())),
        other => panic!("未知操作：{other}"),
    }
}

/// 重放一个场景：注册值 → 逐步执行 → 每步比对结果与计数 → 收尾比对生命周期标记。
fn replay(scenario: &Value) {
    let name = scenario["name"].as_str().expect("name");
    let strict = scenario["strict"].as_bool().expect("strict");

    let mut registry = SequenceRegistry::with_sequence_source(source(counter(0)));
    let (mut cycle, reused) = registry.begin_cycle("流式还原对照");
    assert!(!reused, "场景 {name} 的周期不应是复用");
    for entry in scenario["values"].as_array().expect("values") {
        let value = entry[0].as_str().expect("value");
        let expected = entry[1].as_u64().expect("seq");
        let (seq, first) = cycle.seq_for_value(value);
        assert_eq!(seq, expected, "场景 {name} 的序号（{value}）");
        assert!(first, "场景 {name} 的首次登记（{value}）");
    }

    let mut restorer = StreamRestorer::new(&cycle, &mut registry.stats, strict);
    for op in scenario["ops"].as_array().expect("ops") {
        let kind = op["op"].as_str().expect("op");
        let outcome = apply(&mut restorer, op);
        match (&outcome, op.get("error").and_then(Value::as_str)) {
            (Ok(value), None) => assert_eq!(value, &op["result"], "场景 {name} 的 {kind} 结果"),
            (Err(message), Some(expected)) => {
                assert_eq!(message, expected, "场景 {name} 的 {kind} 错误");
            }
            (Ok(value), Some(expected)) => {
                panic!("场景 {name} 的 {kind} 应当报错（{expected}），实际返回 {value}");
            }
            (Err(message), None) => panic!("场景 {name} 的 {kind} 意外报错：{message}"),
        }

        let stats = restorer.stats();
        let expected = &op["stats"];
        assert_eq!(
            json!(stats.restore_hits),
            expected["hits"],
            "场景 {name} 的 {kind} 还原命中数"
        );
        assert_eq!(
            json!(stats.restore_unresolved),
            expected["unresolved"],
            "场景 {name} 的 {kind} 未注册数"
        );
        assert_eq!(
            json!(stats.restore_malformed),
            expected["malformed"],
            "场景 {name} 的 {kind} 畸形数"
        );
    }

    assert_eq!(
        json!(restorer.finish_reason),
        scenario["finish_reason"],
        "场景 {name} 的 finish_reason"
    );
    assert_eq!(
        json!(restorer.saw_text),
        scenario["saw_text"],
        "场景 {name} 的文本标记"
    );
    assert_eq!(
        json!(restorer.saw_reasoning),
        scenario["saw_reasoning"],
        "场景 {name} 的推理标记"
    );
    assert_eq!(
        json!(restorer.saw_tool_call),
        scenario["saw_tool_call"],
        "场景 {name} 的工具调用标记"
    );
}

fn scenario_names() -> Vec<String> {
    fixture()["scenarios"]
        .as_array()
        .expect("scenarios")
        .iter()
        .map(|scenario| scenario["name"].as_str().expect("name").to_string())
        .collect()
}

fn replay_named(names: &[&str]) {
    let fixture = fixture();
    let mut replayed = 0;
    for scenario in fixture["scenarios"].as_array().expect("scenarios") {
        if names.contains(&scenario["name"].as_str().expect("name")) {
            replay(scenario);
            replayed += 1;
        }
    }
    assert_eq!(replayed, names.len(), "数据集里缺少场景：{names:?}");
}

#[test]
fn truncated_finish_reasons_match_python() {
    let mut reasons: Vec<&str> = TRUNCATED_FINISH_REASONS.to_vec();
    reasons.sort_unstable();
    assert_eq!(
        json!(reasons),
        fixture()["truncated_finish_reasons"],
        "截断 finish_reason 清单"
    );
}

#[test]
fn text_channels_match_python() {
    replay_named(&[
        "跨分片占位符",
        "还原变体与多个占位符",
        "未注册序号与告警去重",
        "畸形与半截前缀",
        "挂起上限",
        "文本与推理通道隔离",
    ]);
}

#[test]
fn tool_argument_channels_match_python() {
    replay_named(&["工具参数通道"]);
}

#[test]
fn structured_restore_matches_python() {
    replay_named(&["结构化还原"]);
}

#[test]
fn lifecycle_matches_python() {
    replay_named(&["生命周期与可用性", "截断 finish_reason 全表"]);
}

#[test]
fn strict_mode_matches_python() {
    replay_named(&["严格模式未注册序号", "严格模式畸形前缀", "严格模式正常路径"]);
}

#[test]
fn every_scenario_is_covered_by_a_test() {
    let mut covered = [
        "跨分片占位符",
        "还原变体与多个占位符",
        "未注册序号与告警去重",
        "畸形与半截前缀",
        "挂起上限",
        "文本与推理通道隔离",
        "工具参数通道",
        "结构化还原",
        "生命周期与可用性",
        "截断 finish_reason 全表",
        "严格模式未注册序号",
        "严格模式畸形前缀",
        "严格模式正常路径",
    ];
    covered.sort_unstable();
    let mut names = scenario_names();
    names.sort();
    assert_eq!(covered.to_vec(), names, "数据集里的场景都应被某个测试重放");
}
