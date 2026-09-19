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
