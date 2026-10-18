//! 上下文计量的分层：投影后计价、逐事件账、锚点增量与统一口径。
//!
//! 这一套只验行为边界，不复述实现细节：图片不按 Base64 计价、压缩后的窗口能从逐事件账
//! 里选出来、供应商数字不能把压力压低、宿主回报的大小与实际测量同源。

use omnicrawl_controllers::context_compaction::meter::{
    conversation_tokens, measure_surface, message_tokens, text_tokens, MeasurementBaseline,
    TokenNode,
};
use omnicrawl_controllers::context_compaction::{
    DefaultContextBudgetManager, MeasureInput, SourceEvent, TokenCountInput,
};
use serde_json::{json, Value};

const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn image_message(payload_repeat: usize) -> Value {
    let data = format!("data:image/png;base64,{}", PNG_BASE64.repeat(payload_repeat));
    json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "看图"},
            {"type": "image_url", "image_url": {"url": data, "detail": "high"}},
        ],
    })
}

fn node(event_id: &str, event_type: &str, tokens: i64) -> TokenNode {
    TokenNode {
        event_id: event_id.to_string(),
        event_type: event_type.to_string(),
        tokens,
    }
}

#[test]
fn pasted_images_do_not_inflate_the_measurement() {
    let small = message_tokens(&image_message(1), &text_tokens);
    let huge = message_tokens(&image_message(200), &text_tokens);
    assert!(
        huge - small <= 6,
        "图片体积放大 200 倍，价格只应随句柄字节数微增：{small} → {huge}"
    );
    assert!(huge < 40, "一张图不该顶到几十 Token 以上：{huge}");
}

#[test]
fn image_free_messages_keep_the_text_price() {
    let plain = json!({"role": "user", "content": "你好"});
    // 4（角色框架）+ 1（"user" 四字符折 1）+ 2（CJK 两字）
    assert_eq!(message_tokens(&plain, &text_tokens), 7);
}

#[test]
fn the_same_estimate_is_used_for_display_and_for_measurement() {
    // 宿主回报「压缩后大小」用的入口，与预算测量用的是同一份：两者必须相等。
    let system = "系统提示词";
    let context = vec![json!({"role": "user", "content": "项目规范"})];
    let tools = vec![json!({"type": "function", "function": {"name": "bash"}})];
    let history = vec![
        json!({"role": "user", "content": "你好"}),
        image_message(3),
    ];
    let display = conversation_tokens(system, &context, &tools, &history, &text_tokens);

    let manager = DefaultContextBudgetManager;
    let snapshot = manager
        .measure(&MeasureInput {
            system_prompt: system.to_string(),
            context_messages: context,
            history_messages: history,
            tool_schemas: tools,
            recent_turns: 1,
            target_summary_tokens: 100,
            next_user_reserve_tokens: 0,
            trigger_context_tokens: 1_000,
            context_window_tokens: 10_000,
            usage: Default::default(),
            provider_input_tokens: 0,
            emergency_context_ratio: 0.9,
            nodes: Vec::new(),
        })
        .expect("测量应当成功");
    // 预算快照会把首条摘要消息单独摘出、并按最近回合切分历史，所以这里比对的是
    // 「两段之和」而不是字段一一对应；口径同源意味着差额只来自切分，不来自规则。
    let measured = snapshot.stable_context_tokens
        + snapshot.existing_summary_tokens
        + snapshot.cold_history_tokens
        + snapshot.recent_history_tokens;
    assert_eq!(measured, display, "宿主显示与预算测量必须同源");
}

#[test]
fn provider_usage_never_lowers_the_pressure() {
    let manager = DefaultContextBudgetManager;
    let snapshot = manager
        .measure_from_token_counts(&TokenCountInput {
            stable_context_tokens: 1_000,
            cold_history_tokens: 4_000,
            recent_history_tokens: 2_000,
            trigger_context_tokens: 5_000,
            context_window_tokens: 100_000,
            emergency_context_ratio: 0.9,
            // 供应商数字明显偏小（摘要调用未归或缓存折算）：不能因此判定「还没到阈值」。
            provider_input_tokens: 500,
            ..TokenCountInput::default()
        })
        .expect("测量应当成功");
    assert_eq!(
        snapshot.baseline,
        Some(MeasurementBaseline::Estimated(7_000)),
        "偏小的供应商数字不应被采信"
    );
    assert!(snapshot.trigger_reached, "真实压力已过阈值，不能被低估成未触发");
    assert_eq!(snapshot.post_turn_context_tokens, 7_000);
}

#[test]
fn a_trustworthy_provider_number_becomes_the_baseline() {
    let manager = DefaultContextBudgetManager;
    let snapshot = manager
        .measure_from_token_counts(&TokenCountInput {
            stable_context_tokens: 1_000,
            cold_history_tokens: 100,
            recent_history_tokens: 100,
            trigger_context_tokens: 5_000,
            context_window_tokens: 100_000,
            emergency_context_ratio: 0.9,
            provider_input_tokens: 9_000,
            ..TokenCountInput::default()
        })
        .expect("测量应当成功");
    assert_eq!(snapshot.baseline, Some(MeasurementBaseline::Usage(9_000)));
    // 启发式表面（1200）比供应商采样（9000）小，差值记为负：压缩后再减一次不会重复计数。
    assert_eq!(snapshot.surface_delta_tokens, -7_800);
    assert_eq!(snapshot.post_turn_context_tokens, 9_000);
}

#[test]
fn compression_range_is_chosen_from_the_tail() {
    let nodes = vec![
        node("e1", "user_message", 100),
        node("e2", "assistant_message", 100),
        node("e3", "user_message", 100),
        node("e4", "tool_call_requested", 100),
        node("e5", "tool_result", 100),
        node("e6", "assistant_message", 100),
    ];
    // 保留最后 250：从尾部累加 e6+e5+e4（300 ≥ 250）即停，窗口是 e1..e3。
    let compacted = DefaultContextBudgetManager::select_range_by_tokens(&nodes, 250);
    assert_eq!(compacted, vec!["e1", "e2", "e3"]);
}

#[test]
fn the_range_never_starts_with_an_orphan_tool_result() {
    let nodes = vec![
        node("e1", "user_message", 100),
        node("e2", "tool_call_requested", 100),
        node("e3", "tool_result", 100),
        node("e4", "assistant_message", 100),
    ];
    // 保留 150 时边界落在 e3（tool_result）：必须前移到它的调用请求之前，
    // 整个工具对留进保留窗口，否则摘要模型会看到孤立结果。
    // 保留 120 时边界正好落在 e3（tool_result，100 < 120 后会再加 e2）：
    // 前移把 e3 与 e2 一起留在保留窗口里，压缩窗口只剩 e1。
    let compacted = DefaultContextBudgetManager::select_range_by_tokens(&nodes, 120);
    assert_eq!(compacted, vec!["e1"]);
}

#[test]
fn an_empty_surface_compacts_nothing() {
    assert!(DefaultContextBudgetManager::select_range_by_tokens(&[], 100).is_empty());
}

#[test]
fn measurement_carries_the_per_event_ledger() {
    let nodes = vec![
        node("e1", "user_message", 30),
        node("e2", "assistant_message", 20),
    ];
    let measured = measure_surface(None, nodes, None);
    assert_eq!(measured.total_tokens, 50, "没有锚点时当前表面就是总量");
    assert_eq!(measured.baseline, Some(MeasurementBaseline::Estimated(50)));
    assert_eq!(measured.surface_delta_tokens, 0, "没有锚点时不存在增量");
    assert_eq!(measured.nodes.len(), 2);
}

#[test]
fn a_session_with_a_huge_image_still_reports_a_sane_pressure() {
    // 端到端：把一张 1 MB 量级的图放进会话历史，压力必须与同文无图的会话接近。
    let huge = "A".repeat(1_400_000);
    let with_image = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "看这张图"},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{huge}")}},
        ],
    })];
    let without_image = vec![json!({"role": "user", "content": "看这张图"})];

    let manager = DefaultContextBudgetManager;
    let measure = |history: Vec<Value>| {
        manager
            .measure(&MeasureInput {
                system_prompt: String::new(),
                context_messages: Vec::new(),
                history_messages: history,
                tool_schemas: Vec::new(),
                recent_turns: 1,
                target_summary_tokens: 0,
                next_user_reserve_tokens: 0,
                trigger_context_tokens: 100_000,
                context_window_tokens: 128_000,
                usage: Default::default(),
                provider_input_tokens: 0,
                emergency_context_ratio: 0.9,
                nodes: Vec::new(),
            })
            .expect("测量应当成功")
    };
    let image_tokens = measure(with_image).recent_history_tokens;
    let text_tokens_only = measure(without_image).recent_history_tokens;
    assert!(
        image_tokens - text_tokens_only < 20,
        "1 MB 图片不该让压力涨 20 Token 以上：{text_tokens_only} → {image_tokens}"
    );
    assert!(
        image_tokens < 1_000,
        "带图会话不该被判成上万 Token：{image_tokens}"
    );
}

#[test]
fn event_nodes_price_images_by_handle_too() {
    // 事件载荷里的图片是原始附件形状（media_type + data_base64），投影后同样只算句柄。
    let event = SourceEvent {
        event_id: "e1".to_string(),
        event_type: "user_message".to_string(),
        payload: json!({
            "content": "看图",
            "images": [{"media_type": "image/png", "data_base64": PNG_BASE64, "detail": "high"}],
        }),
    };
    let tokens = omnicrawl_controllers::context_compaction::event_tokens(&event, &text_tokens);
    assert!(tokens < 40, "事件投影也不该把 Base64 算进来：{tokens}");
}
