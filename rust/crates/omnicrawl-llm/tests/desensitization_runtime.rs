//! 脱敏运行时装饰器的验收：出站屏蔽、入站还原、未注册序号告警与开关语义。
//!
//! 替身内层运行时把「内层到底收到了什么」钉下来（不发网络请求），并支持**回显**：
//! 把收到的（已屏蔽的）用户文本原样当成回复或工具参数回放，这样还原断言不必猜序号。
//! 行为对齐 Python `DesensitizationRuntime.stream_turn` 的语义。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use omnicrawl_llm::desensitization::{DesensitizationOptions, DesensitizationRuntime};
use omnicrawl_llm::{ChatRequestInput, ModelRuntime, RuntimeError, SinkFlow, TurnSink};
use omnicrawl_protocol::{
    aggregate_stream_events, ConversationMessage, GenerationOptions, MessageBlock, ModelReply,
    ModelStreamEvent, Role, TextBlock, TextDelta, ToolCallCompleted,
};
use serde_json::{json, Map};

const SECRET: &str = "sk-live-abcdefghijklmnopqrstuvwxyz123456";

/// 未注册的占位符序号：取一个远大于任何真实分配值的数字。
const UNREGISTERED_SEQ: u64 = 4_242_424;

fn placeholder(seq: u64) -> String {
    // 拼接构造：本仓库自己就是宿主，写完整占位符字面量会被还原成原文。
    format!("\u{ff5b}Desensitized:{seq}\u{ff5d}")
}

enum Behavior {
    /// 回放预设事件。
    Events(Vec<ModelStreamEvent>),
    /// 把收到的用户文本（屏蔽态）当成文本回复回放。
    EchoText,
    /// 把收到的用户文本（屏蔽态）放进工具参数的 `token` 字段。
    EchoToolArgument,
}

struct StubRuntime {
    seen: Arc<Mutex<Vec<String>>>,
    behavior: Behavior,
}

impl StubRuntime {
    fn new(seen: &Arc<Mutex<Vec<String>>>, behavior: Behavior) -> Self {
        Self {
            seen: Arc::clone(seen),
            behavior,
        }
    }

    fn user_text(input: &ChatRequestInput<'_>) -> String {
        input
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                MessageBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

impl ModelRuntime for StubRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        let user_text = Self::user_text(input);
        self.seen
            .lock()
            .expect("记录锁")
            .push(input.system_prompt.to_string());
        self.seen.lock().expect("记录锁").push(user_text.clone());

        let events: Vec<ModelStreamEvent> = match &self.behavior {
            Behavior::Events(events) => events.clone(),
            Behavior::EchoText => vec![
                ModelStreamEvent::TextDelta(TextDelta::new(&user_text)),
                ModelStreamEvent::Finished {
                    finish_reason: "stop".to_string(),
                },
            ],
            Behavior::EchoToolArgument => {
                let mut arguments = Map::new();
                arguments.insert("token".to_string(), json!(user_text));
                vec![
                    ModelStreamEvent::ToolCallCompleted(ToolCallCompleted::new(
                        "call_a",
                        "read_file",
                        arguments,
                    )),
                    ModelStreamEvent::Finished {
                        finish_reason: "tool_calls".to_string(),
                    },
                ]
            }
        };
        for event in &events {
            sink.on_event(event.clone());
        }
        Ok(aggregate_stream_events(events))
    }
}

struct RecordingSink {
    events: Vec<ModelStreamEvent>,
}

impl TurnSink for RecordingSink {
    fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
        self.events.push(event);
        SinkFlow::Continue
    }
}

struct Fixture {
    messages: Vec<ConversationMessage>,
    options: GenerationOptions,
    identity: BTreeMap<String, String>,
}

impl Fixture {
    fn with_text(text: &str) -> Self {
        Self {
            messages: vec![ConversationMessage {
                role: Role::User,
                blocks: vec![MessageBlock::Text(TextBlock::new(text))],
                reasoning: String::new(),
                tools: Vec::new(),
            }],
            options: GenerationOptions::default(),
            identity: BTreeMap::new(),
        }
    }

    fn input(&self) -> ChatRequestInput<'_> {
        ChatRequestInput {
            model: "gpt-5.2",
            system_prompt: "系统提示",
            messages: &self.messages,
            tools: &[],
            options: &self.options,
            profile_request_timeout_seconds: 10.0,
            prompt_cache_capable: false,
            prompt_cache_identity: &self.identity,
        }
    }
}

fn text_of(events: &[ModelStreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            ModelStreamEvent::TextDelta(delta) => Some(delta.text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn masks_outbound_and_restores_inbound() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let runtime = DesensitizationRuntime::new(
        Box::new(StubRuntime::new(&seen, Behavior::EchoText)),
        DesensitizationOptions::default(),
    );

    let fixture = Fixture::with_text(&format!("api_key={SECRET}"));
    let mut sink = RecordingSink { events: Vec::new() };
    let reply = runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("回合应当成功");

    let sent = seen.lock().expect("记录锁").clone();
    assert!(
        sent.iter().all(|text| !text.contains(SECRET)),
        "原文不得发给模型：{sent:?}"
    );
    assert!(
        sent.iter().any(|text| text.contains("Desensitized")),
        "出站请求应被屏蔽成占位符：{sent:?}"
    );
    assert_eq!(
        text_of(&sink.events),
        format!("api_key={SECRET}"),
        "入站的占位符应还原成原文"
    );
    assert_eq!(
        reply.content,
        format!("api_key={SECRET}"),
        "归并后也是还原态"
    );
    assert!(runtime.stats().values_masked > 0, "屏蔽计数应增加");
}

#[test]
fn disabled_options_leave_runtime_untouched() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let options = DesensitizationOptions::default();
    let runtime = DesensitizationRuntime::maybe_wrap(
        Box::new(StubRuntime::new(&seen, Behavior::Events(Vec::new()))),
        &options,
    );

    let fixture = Fixture::with_text(&format!("api_key={SECRET}"));
    let mut sink = RecordingSink { events: Vec::new() };
    runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("回合应当成功");

    let sent = seen.lock().expect("记录锁").clone();
    assert!(
        sent.iter().any(|text| text.contains(SECRET)),
        "未启用时必须原样发送：{sent:?}"
    );
}

#[test]
fn unregistered_placeholder_keeps_text_and_warns() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let leaked = format!("未知序号 {}", placeholder(UNREGISTERED_SEQ));
    let runtime = DesensitizationRuntime::new(
        Box::new(StubRuntime::new(
            &seen,
            Behavior::Events(vec![
                ModelStreamEvent::TextDelta(TextDelta::new(&leaked)),
                ModelStreamEvent::Finished {
                    finish_reason: "stop".to_string(),
                },
            ]),
        )),
        DesensitizationOptions::default(),
    );

    let fixture = Fixture::with_text("普通提问");
    let mut sink = RecordingSink { events: Vec::new() };
    runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("非严格模式下不应失败");

    assert!(
        text_of(&sink.events).contains(&placeholder(UNREGISTERED_SEQ)),
        "未注册序号按原样保留：{:?}",
        sink.events
    );
    assert!(
        sink.events
            .iter()
            .any(|event| matches!(event, ModelStreamEvent::ProviderWarning(_))),
        "应补发一条还原告警：{:?}",
        sink.events
    );
}

/// NER 兜底层按配置装载：不填路径时用随包权重（`data/ner_bilstm_crf.bin`）。
///
/// 这里只验「装载与否」——权重推理的逐位对照在 `ner_parity.rs`，兜底层接入 `mask_text`
/// 的语义在 `desensitization_ner_stage.rs`。
#[test]
fn ner_layer_loads_from_packaged_weights_when_enabled() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut options = DesensitizationOptions::default();
    options.ner.enabled = true;
    let runtime = DesensitizationRuntime::new(
        Box::new(StubRuntime::new(&seen, Behavior::Events(Vec::new()))),
        options,
    );

    let stats = runtime
        .ner_stats()
        .expect("启用后应当装载随包权重（否则路径解析链断了）");
    assert_eq!(stats.inferred_chunks, 0, "还未跑过任何文本");
}

/// 权重缺失 / 读取失败时静默降级：不报错、不阻断回合，只是少一层软兑底。
#[test]
fn ner_layer_degrades_silently_when_weights_are_missing() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut options = DesensitizationOptions::default();
    options.ner.enabled = true;
    options.ner.model_path = "不存在的目录/ner.bin".to_string();
    let runtime = DesensitizationRuntime::new(
        Box::new(StubRuntime::new(&seen, Behavior::EchoText)),
        options,
    );
    assert!(
        runtime.ner_stats().is_none(),
        "权重不可用时应当降级为「没有兜底层」"
    );

    let fixture = Fixture::with_text(&format!("api_key={SECRET}"));
    let mut sink = RecordingSink { events: Vec::new() };
    runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("降级后回合仍应跑通");
    assert_eq!(text_of(&sink.events), format!("api_key={SECRET}"));
}

#[test]
fn restores_tool_call_arguments() {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let runtime = DesensitizationRuntime::new(
        Box::new(StubRuntime::new(&seen, Behavior::EchoToolArgument)),
        DesensitizationOptions::default(),
    );

    let fixture = Fixture::with_text(&format!("api_key={SECRET}"));
    let mut sink = RecordingSink { events: Vec::new() };
    runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("回合应当成功");

    let restored = sink
        .events
        .iter()
        .find_map(|event| match event {
            ModelStreamEvent::ToolCallCompleted(call) => Some(call.arguments.clone()),
            _ => None,
        })
        .expect("应当有工具调用完成事件");
    assert_eq!(
        restored.get("token").and_then(|value| value.as_str()),
        Some(format!("api_key={SECRET}").as_str()),
        "工具参数里的占位符应还原成原文：{restored:?}"
    );
}

/// 流式语义回归：增量必须**在流跑完之前**就到达外层 sink。
///
/// 曾经的实现把内层事件全收进 `CollectSink`，等 `inner.run_turn` 返回后才统一还原外发，
/// 于是整段回复被压成一次输出（界面上表现为「思考内容一口气全出来」）。这里用一个
/// 「发完每个分片就回调一次」的内层替身，在回调里检查外层 sink 已经收到了对应增量：
/// 若又退回攒批实现，回调时外层 sink 仍是空的，断言会直接失败。
#[test]
fn deltas_reach_outer_sink_before_the_stream_ends() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个分片发完后调用 `probe`：此时外层 sink 应已收到该分片。
    struct StreamingStub {
        chunks: Vec<String>,
        probe: Arc<dyn Fn(usize) + Send + Sync>,
    }

    impl ModelRuntime for StreamingStub {
        fn run_turn(
            &self,
            _input: &ChatRequestInput<'_>,
            sink: &mut dyn TurnSink,
        ) -> Result<ModelReply, RuntimeError> {
            let mut events: Vec<ModelStreamEvent> = Vec::new();
            for (index, chunk) in self.chunks.iter().enumerate() {
                let event = ModelStreamEvent::TextDelta(TextDelta::new(chunk));
                if sink.on_event(event.clone()) == SinkFlow::Cancel {
                    return Err(RuntimeError::cancelled());
                }
                events.push(event);
                (self.probe)(index);
            }
            events.push(ModelStreamEvent::Finished {
                finish_reason: "stop".to_string(),
            });
            Ok(aggregate_stream_events(events))
        }
    }

    let seen_count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen_count);
    let probe = Arc::new(move |_index: usize| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let runtime = DesensitizationRuntime::new(
        Box::new(StreamingStub {
            chunks: vec!["第一".to_string(), "第二".to_string(), "第三".to_string()],
            probe,
        }),
        DesensitizationOptions::default(),
    );

    // sink 自己记录「到达时机」：每条增量到达时记下当前已经发过几条内层分片。
    struct TimingSink {
        counter: Arc<AtomicUsize>,
        arrivals: Vec<(usize, String)>,
    }
    impl TurnSink for TimingSink {
        fn on_event(&mut self, event: ModelStreamEvent) -> SinkFlow {
            if let ModelStreamEvent::TextDelta(delta) = event {
                self.arrivals
                    .push((self.counter.load(Ordering::SeqCst), delta.text));
            }
            SinkFlow::Continue
        }
    }

    let mut sink = TimingSink {
        counter: Arc::clone(&seen_count),
        arrivals: Vec::new(),
    };
    let fixture = Fixture::with_text("普通提问");
    runtime
        .run_turn(&fixture.input(), &mut sink)
        .expect("回合应当成功");

    assert_eq!(
        sink.arrivals.len(),
        3,
        "三条增量都应外发：{:?}",
        sink.arrivals
    );
    // 每条增量都在**自己那一片**发完时就已到达（到达时内层刚好发过 index 片）。
    // 攒批实现下三条会全部等到流跑完才出现，计数一律是 3。
    assert_eq!(
        sink.arrivals,
        vec![
            (0, "第一".to_string()),
            (1, "第二".to_string()),
            (2, "第三".to_string()),
        ],
        "增量应逐条即时外发，而不是等到流结束：{:?}",
        sink.arrivals
    );
}
