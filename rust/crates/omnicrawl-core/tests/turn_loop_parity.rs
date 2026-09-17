//! 跨语言 parity：同一份脚本化输入分别跑 Python 循环与 Rust 循环，逐字段比对。
//!
//! fixture 由 `rust/tools/gen_core_parity_fixture.py` 用 Python 真实实现产出；
//! 输入部分（回复、工具批次、取消/停止触发点、时钟序列）是两侧共用的 wire 形状。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use omnicrawl_core::{
    AgentLoopLimits, AgentLoopObservation, AgentLoopRunner, AgentModelReply, Clock, LoopError,
    LoopGuards, ReplySource, ToolBatchHost, ToolCall,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/turn_loop_parity.json");

/// 按给定序列返回时刻的单调时钟，并记录取时刻次数。
struct ScriptedClock {
    values: RefCell<VecDeque<f64>>,
    last: Cell<f64>,
    calls: Rc<Cell<usize>>,
}

impl ScriptedClock {
    fn new(values: Vec<f64>, calls: Rc<Cell<usize>>) -> Self {
        let last = values.first().copied().unwrap_or(0.0);
        Self {
            values: RefCell::new(values.into_iter().collect()),
            last: Cell::new(last),
            calls,
        }
    }
}

impl Clock for ScriptedClock {
    fn now(&self) -> f64 {
        self.calls.set(self.calls.get() + 1);
        if let Some(value) = self.values.borrow_mut().pop_front() {
            self.last.set(value);
        }
        self.last.get()
    }
}

/// 脚本化模型回复来源。
struct ReplyScript {
    replies: Vec<AgentModelReply>,
    error_at: Option<usize>,
    served: Cell<usize>,
    log: RefCell<Vec<usize>>,
}

impl ReplySource for ReplyScript {
    fn request_reply(&mut self, messages: &mut Vec<Value>) -> Result<AgentModelReply, LoopError> {
        self.log.borrow_mut().push(messages.len());
        let index = self.served.get() + 1;
        self.served.set(index);
        if self.error_at == Some(index) {
            return Err(LoopError::ReplySource("模型请求失败".to_string()));
        }
        self.replies
            .get(index - 1)
            .cloned()
            .ok_or_else(|| LoopError::ReplySource("脚本化回复不足".to_string()))
    }
}

/// 脚本化工具批次宿主。
struct BatchScript {
    batches: Vec<Vec<AgentLoopObservation>>,
    error_at: Option<usize>,
    consumed: Cell<usize>,
    entered: Rc<Cell<usize>>,
    log: RefCell<Vec<Value>>,
}

impl ToolBatchHost for BatchScript {
    fn execute_tool_batch(
        &mut self,
        calls: &[ToolCall],
        first_step: usize,
    ) -> Result<Vec<AgentLoopObservation>, LoopError> {
        let index = self.consumed.get() + 1;
        self.consumed.set(index);
        self.entered.set(index);
        self.log.borrow_mut().push(json!({
            "first_step": first_step,
            "calls": calls.iter().map(|call| call.name.clone()).collect::<Vec<_>>(),
        }));
        if self.error_at == Some(index) {
            return Err(LoopError::ToolBatch("工具批次失败".to_string()));
        }
        self.batches
            .get(index - 1)
            .cloned()
            .ok_or_else(|| LoopError::ToolBatch("脚本化工具批次不足".to_string()))
    }
}

fn optional_usize(case: &Value, key: &str) -> Option<usize> {
    case.get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

fn build_limits(case: &Value) -> Result<AgentLoopLimits, LoopError> {
    match case.get("limits") {
        None | Some(Value::Null) => AgentLoopLimits::new(None, None, None),
        Some(spec) => AgentLoopLimits::new(
            spec.get("max_model_turns")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            spec.get("max_tool_calls")
                .and_then(Value::as_u64)
                .map(|value| value as usize),
            spec.get("timeout_seconds").and_then(Value::as_f64),
        ),
    }
}

fn run_case(case: &Value) -> Value {
    let name = case["name"].as_str().unwrap_or_default();
    let mut messages: Vec<Value> = serde_json::from_value(case["seed_messages"].clone())
        .unwrap_or_else(|error| panic!("{name}: seed_messages 反序列化失败：{error}"));
    let replies: Vec<AgentModelReply> = serde_json::from_value(case["replies"].clone())
        .unwrap_or_else(|error| panic!("{name}: replies 反序列化失败：{error}"));
    let batches: Vec<Vec<AgentLoopObservation>> = serde_json::from_value(case["batches"].clone())
        .unwrap_or_else(|error| panic!("{name}: batches 反序列化失败：{error}"));
    let clock_values: Vec<f64> = case["clock"]
        .as_array()
        .map(|values| values.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();

    let boundary_calls = Cell::new(0usize);
    let clock_calls = Rc::new(Cell::new(0usize));
    let batch_entered = Rc::new(Cell::new(0usize));
    let cancel_at = optional_usize(case, "cancel_at");
    let stop_after_batch = optional_usize(case, "stop_after_batch");

    let limits = match build_limits(case) {
        Ok(limits) => limits,
        Err(error) => {
            return json!({
                "ok": false,
                "error": {"tag": error.tag(), "message": error.message()},
                "messages": messages,
                "request_log": [],
                "batch_log": [],
                "clock_calls": clock_calls.get(),
                "boundary_calls": boundary_calls.get(),
            });
        }
    };

    let mut reply_source = ReplyScript {
        replies,
        error_at: optional_usize(case, "reply_error_at"),
        served: Cell::new(0),
        log: RefCell::new(Vec::new()),
    };
    let mut tool_batch = BatchScript {
        batches,
        error_at: optional_usize(case, "tool_error_at"),
        consumed: Cell::new(0),
        entered: Rc::clone(&batch_entered),
        log: RefCell::new(Vec::new()),
    };

    let mut cancel_check = || {
        boundary_calls.set(boundary_calls.get() + 1);
        if cancel_at == Some(boundary_calls.get()) {
            return Err(LoopError::Cancelled("宿主取消".to_string()));
        }
        Ok(())
    };
    let mut stop_check = || stop_after_batch.is_some_and(|limit| batch_entered.get() >= limit);
    let runner = AgentLoopRunner::new(Box::new(ScriptedClock::new(
        clock_values,
        Rc::clone(&clock_calls),
    )));
    let outcome = runner.run(
        &mut messages,
        &mut reply_source,
        &mut tool_batch,
        limits,
        LoopGuards {
            cancel_check: Some(&mut cancel_check),
            stop_check: Some(&mut stop_check),
        },
    );

    let request_log: Vec<Value> = reply_source
        .log
        .borrow()
        .iter()
        .map(|length| json!({"messages_len": length}))
        .collect();
    let batch_log = tool_batch.log.borrow().clone();
    let clock_calls = clock_calls.get();
    let boundary_calls = boundary_calls.get();

    match outcome {
        Ok(result) => json!({
            "ok": true,
            "final_text": result.final_text,
            "reasoning": result.reasoning,
            "content_streamed": result.content_streamed,
            "model_turns": result.model_turns,
            "tool_calls": result.tool_calls,
            "paused": result.paused,
            "has_last_reply": result.last_reply.is_some(),
            "messages": messages,
            "request_log": request_log,
            "batch_log": batch_log,
            "clock_calls": clock_calls,
            "boundary_calls": boundary_calls,
        }),
        Err(error) => json!({
            "ok": false,
            "error": {"tag": error.tag(), "message": error.message()},
            "messages": messages,
            "request_log": request_log,
            "batch_log": batch_log,
            "clock_calls": clock_calls,
            "boundary_calls": boundary_calls,
        }),
    }
}

#[test]
fn turn_loop_matches_python_expectations() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    let cases = fixture["cases"].as_array().expect("fixture 缺少 cases");
    assert!(!cases.is_empty(), "fixture 用例为空");

    for case in cases {
        let name = case["name"].as_str().unwrap_or_default();
        let actual = run_case(case);
        assert_eq!(
            actual, case["expected"],
            "用例 {name} 与 Python 期望值不一致"
        );
    }
}
