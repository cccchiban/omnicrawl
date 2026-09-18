//! 摘要模型适配器：复用主请求前缀与工具面、按 `tool_choice=none` 发一次请求。
//!
//! 用一个假运行时盯住内核真正发出去的东西：模型、系统提示词、逐字前缀、工具面、tool_choice、
//! 用量累计，以及没有前缀时的降级。

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use omnicrawl_compaction::{SummaryAdapterSettings, SummaryModelAdapter};
use omnicrawl_controllers::context_compaction::{SummaryModelCall, TokenUsageSample};
use omnicrawl_llm::{ChatRequestInput, ModelRuntime, RuntimeError, TurnSink};
use omnicrawl_protocol::{
    ConversationMessage, GenerationOptions, ModelReply, ModelStreamEvent, Role, UsageReported,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
struct SeenRequest {
    model: String,
    system_prompt: String,
    message_count: usize,
    tool_count: usize,
    tool_choice: String,
}

#[derive(Default)]
struct FakeRuntime {
    seen: Rc<RefCell<Vec<SeenRequest>>>,
}

impl ModelRuntime for FakeRuntime {
    fn run_turn(
        &self,
        input: &ChatRequestInput<'_>,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelReply, RuntimeError> {
        self.seen.borrow_mut().push(SeenRequest {
            model: input.model.to_string(),
            system_prompt: input.system_prompt.to_string(),
            message_count: input.messages.len(),
            tool_count: input.tools.len(),
            tool_choice: input.options.tool_choice.clone(),
        });
        sink.on_event(ModelStreamEvent::UsageReported(UsageReported {
            input_tokens: 1000,
            output_tokens: 100,
            cached_input_tokens: 200,
            reasoning_tokens: 0,
        }));
        Ok(ModelReply {
            assistant_message: ConversationMessage::new(Role::Assistant),
            content: "摘要正文".to_string(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            usage: None,
            finish_reason: "stop".to_string(),
            content_streamed: true,
            warnings: Vec::new(),
        })
    }
}

fn build_adapter(
    seen: Rc<RefCell<Vec<SeenRequest>>>,
    prefix: Vec<Value>,
    tools: Vec<Value>,
    context_window_tokens: i64,
) -> SummaryModelAdapter {
    SummaryModelAdapter::new(SummaryAdapterSettings {
        runtime: Box::new(FakeRuntime { seen }),
        model: "gpt-cheap".to_string(),
        provider: "openai".to_string(),
        system_prompt: "系统提示词".to_string(),
        prefix,
        tools,
        options: GenerationOptions {
            tool_choice: "auto".to_string(),
            ..GenerationOptions::default()
        },
        prompt_cache_identity: BTreeMap::from([("context".to_string(), "summary".to_string())]),
        context_window_tokens,
    })
}

#[test]
fn summary_request_reuses_prefix_and_forbids_tools() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let prefix = vec![
        json!({"role": "system", "content": "系统提示词"}),
        json!({"role": "user", "content": "用户原文"}),
    ];
    let tools = vec![json!({"type": "function", "function": {"name": "bash"}})];
    let adapter = build_adapter(Rc::clone(&seen), prefix.clone(), tools.clone(), 128_000);

    let response = adapter
        .call(&[json!({"role": "user", "content": "摘要提示词"})])
        .expect("摘要请求成功");

    assert_eq!(
        response.usage,
        TokenUsageSample::new(1000, 100, 200).expect("用量非负")
    );
    assert_eq!(response.profile, "gpt-cheap");
    assert_eq!(response.provider, "openai");
    assert_eq!(response.tool_calls, 0);

    let recorded = seen.borrow();
    let request = recorded.first().expect("发出过一次请求");
    assert_eq!(request.model, "gpt-cheap");
    assert_eq!(request.system_prompt, "系统提示词");
    assert_eq!(request.tool_choice, "none", "摘要请求禁止调用工具");
    assert_eq!(request.tool_count, tools.len(), "工具面与主请求一致");
    assert_eq!(
        request.message_count,
        prefix.len() + 1,
        "请求 = 复用前缀 + 摘要提示词"
    );
}

#[test]
fn requests_without_prefix_degrade() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let adapter = build_adapter(Rc::clone(&seen), Vec::new(), Vec::new(), 128_000);
    let error = adapter
        .call(&[json!({"role": "user", "content": "摘要提示词"})])
        .expect_err("没有前缀必须降级");
    assert!(error.message().starts_with("没有可复用的主请求前缀"));
    assert!(seen.borrow().is_empty(), "降级时不发请求");
}

#[test]
fn index_chunk_budget_follows_window() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let adapter = build_adapter(Rc::clone(&seen), Vec::new(), Vec::new(), 128_000);
    assert!(adapter.prefix_token_estimate() > 0);
    assert!(adapter.index_chunk_budget_tokens().is_some());

    let small = build_adapter(Rc::clone(&seen), Vec::new(), Vec::new(), 1_000);
    assert!(
        small.index_chunk_budget_tokens().is_none(),
        "窗口过小即没有余额"
    );
    let missing = build_adapter(Rc::clone(&seen), Vec::new(), Vec::new(), 0);
    assert!(
        missing.index_chunk_budget_tokens().is_none(),
        "窗口缺失即没有余额"
    );
}
