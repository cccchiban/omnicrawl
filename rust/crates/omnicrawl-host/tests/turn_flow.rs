//! 无头回合运行器的流程测试：用脚本化假内核钉住握手、整批工具定调、取消与事件出口。
//!
//! 假内核是一对内存管道：脚本字节先灌进去，宿主写出的每一帧都记在旁边的记录器里。
//! 管道保持打开（脚本读完只是暂时没数据），因此不会出现「脚本读完 = 内核退出」的假信号。

use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use omnicrawl_host::approval::ApprovalMode;
use omnicrawl_host::host::DENIED;
use omnicrawl_host::kernel::KernelClient;
use omnicrawl_host::tools::RegistryOptions;
use omnicrawl_host::turn::{Interactor, RunnerOptions, TurnControl, TurnError, TurnRunner};
use omnicrawl_ipc::bridge::{HostEvent, KernelModelConfig};
use omnicrawl_ipc::{Frame, Id, ToolBatchResult};
use serde_json::{json, Map, Value};

/// 内核要发的字节：脚本喂完就阻塞等更多输入，直到显式关闭。
#[derive(Clone, Default)]
struct Pipe {
    inner: Arc<(Mutex<PipeInner>, Condvar)>,
}

#[derive(Default)]
struct PipeInner {
    buffer: VecDeque<u8>,
    closed: bool,
}

impl Pipe {
    fn push(&self, line: &str) {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("管道未被毒化");
        inner.buffer.extend(line.as_bytes());
        inner.buffer.push_back(b'\n');
        wake.notify_all();
    }

    fn close(&self) {
        let (lock, wake) = &*self.inner;
        lock.lock().expect("管道未被毒化").closed = true;
        wake.notify_all();
    }
}

impl Read for Pipe {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let (lock, wake) = &*self.inner;
        let mut inner = lock.lock().expect("管道未被毒化");
        loop {
            if !inner.buffer.is_empty() {
                let take = out.len().min(inner.buffer.len());
                for slot in out.iter_mut().take(take) {
                    *slot = inner.buffer.pop_front().unwrap_or_default();
                }
                return Ok(take);
            }
            if inner.closed {
                return Ok(0);
            }
            inner = wake.wait(inner).expect("等待内核脚本");
        }
    }
}

/// 宿主写出的帧记录器；每次 read 都重新解析整段字节，测试里够用。
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Recorder {
    fn frames(&self) -> Vec<Frame> {
        let bytes = self.0.lock().expect("记录缓冲未被毒化").clone();
        String::from_utf8(bytes)
            .expect("帧是 UTF-8")
            .lines()
            .map(|line| Frame::parse(line).expect("宿主写出的必须是合法帧"))
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.frames()
            .into_iter()
            .filter_map(|frame| frame.method().map(str::to_string))
            .collect()
    }

    fn response(&self, id: i64) -> Frame {
        self.frames()
            .into_iter()
            .find(|frame| frame.is_response() && frame.id() == Some(&Id::Number(id)))
            .expect("宿主应当回了这个响应")
    }

    fn result(&self, id: i64) -> Value {
        self.response(id)
            .result
            .clone()
            .expect("响应帧应当带 result")
    }
}

impl Write for Recorder {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("记录缓冲未被毒化")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn kernel() -> (KernelClient, Recorder, Pipe) {
    let pipe = Pipe::default();
    let recorder = Recorder::default();
    let client = KernelClient::from_streams(
        Box::new(BufReader::new(pipe.clone())),
        Box::new(recorder.clone()),
    );
    (client, recorder, pipe)
}

fn workspace(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("oc-host-turn-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&path).expect("建立临时工作区");
    path
}

fn model() -> KernelModelConfig {
    KernelModelConfig {
        model: "test-model".to_string(),
        provider: String::new(),
        protocol: String::new(),
        base_url: String::new(),
        api_key_env: String::new(),
        user_agent: "omnicrawl-test".to_string(),
        system_prompt: "系统提示".to_string(),
        context_messages: Vec::new(),
        tools: Vec::new(),
        options: json!({}),
        request_timeout_seconds: None,
        context_window_tokens: 0,
        prompt_cache_capable: false,
        prompt_cache_identity: Default::default(),
        request_retry_count: 1,
    }
}

fn runner(client: KernelClient, root: &Path) -> TurnRunner {
    let options = RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        native_vision: false,
        client_name: "omnicrawl-host-test".to_string(),
        plugins: None,
        review: None,
    };
    TurnRunner::new(client, options, &RegistryOptions::default()).expect("工具表应当建成")
}

/// 脚本化的交互：固定批准与否、固定作答，同时记下被询问过什么。
struct Scripted {
    approve: bool,
    answer: Option<String>,
    asked: Vec<String>,
}

impl Interactor for Scripted {
    fn decide(&mut self, tool: &str, _arguments: &Map<String, Value>) -> Option<bool> {
        self.asked.push(format!("decide:{tool}"));
        Some(self.approve)
    }

    fn answer(
        &mut self,
        prompt: &str,
        _options: &[String],
        _arguments: &Map<String, Value>,
    ) -> Option<String> {
        self.asked.push(format!("answer:{prompt}"));
        self.answer.clone()
    }
}

fn events_of(events: &[HostEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| event.method().to_string())
        .collect()
}

#[test]
fn handshake_declares_tools_and_reports_rejection() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("handshake");
    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    let mut subject = runner(client, &root);

    subject.handshake(&mut |_| {}).expect("握手应当成功");
    let initialize = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("initialize"))
        .expect("应当发过 initialize");
    let declared = initialize
        .params
        .as_ref()
        .and_then(|params| params.get("model"))
        .and_then(|model| model.get("tools"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    assert!(declared > 0, "握手必须声明工具表");

    let (client, _, pipe) = kernel();
    pipe.push(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"负载字段不符"}}"#);
    let mut rejected = runner(client, &root);
    let message = rejected
        .handshake(&mut |_| {})
        .expect_err("内核拒绝时应当报错");
    assert!(message.contains("负载字段不符"), "实际：{message}");
    pipe.close();
}

#[test]
fn submit_streams_events_and_answers_tool_batch() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("submit");
    let mut subject = runner(client, &root);

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");

    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(r#"{"jsonrpc":"2.0","method":"turn.delta","params":{"text":"你好"}}"#);
    pipe.push(concat!(
        r#"{"jsonrpc":"2.0","id":3,"method":"tool.batch","params":{"turn_id":"turn-1","step":1,"calls":["#,
        r#"{"name":"update_todos","arguments":{"todos":[{"id":"1","step":"写测试","completed":false}]},"id":"c1","function_name":"update_todos"},"#,
        r#"{"name":"bash","arguments":{"command":"echo hi"},"id":"c2","function_name":"bash"}]}}"#,
    ));
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"完成","reasoning":"","model_turns":1,"tool_calls":2,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: false,
        answer: None,
        asked: Vec::new(),
    };
    let mut events: Vec<HostEvent> = Vec::new();
    let outcome = subject
        .submit(
            "你好",
            &TurnControl::new(),
            &mut interactor,
            &mut |event| events.push(event),
        )
        .expect("回合应当跑完");

    assert_eq!(outcome.final_text, "完成");
    assert_eq!(outcome.tool_calls, 2);
    assert_eq!(interactor.asked, vec!["decide:bash".to_string()]);
    let expected = vec![
        "turn.delta",
        "tool.started",
        "tool.finished",
        "todo.update",
        "turn.finished",
    ];
    assert_eq!(events_of(&events), expected);

    let result =
        ToolBatchResult::from_result(&recorder.result(3)).expect("宿主回的应当是工具批次结果");
    assert_eq!(result.observations.len(), 2);
    assert!(result.observations[0].result.ok, "清单工具应当就地成功");
    assert_eq!(
        result.observations[1].result.error_code.as_deref(),
        Some(DENIED),
        "拒绝执行的调用必须回 denied"
    );
    assert_eq!(events[2].method(), "tool.finished");
    pipe.close();
}

#[test]
fn ask_user_is_answered_through_interactor() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("ask");
    let mut subject = runner(client, &root);

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(concat!(
        r#"{"jsonrpc":"2.0","id":3,"method":"tool.batch","params":{"turn_id":"turn-1","step":1,"calls":["#,
        r#"{"name":"ask_user","arguments":{"question":"选哪个？","options":["选项A","选项B"],"kind":"select"},"id":"c1","function_name":"ask_user"}]}}"#,
    ));
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":1,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: Some("选项A".to_string()),
        asked: Vec::new(),
    };
    subject
        .submit("问一下", &TurnControl::new(), &mut interactor, &mut |_| {})
        .expect("回合应当跑完");

    assert_eq!(interactor.asked, vec!["answer:选哪个？".to_string()]);
    let result =
        ToolBatchResult::from_result(&recorder.result(3)).expect("宿主回的应当是工具批次结果");
    assert_eq!(result.observations.len(), 1);
    assert!(result.observations[0].result.ok);
    assert_eq!(result.observations[0].result.output, "选项A");
    pipe.close();
}

#[test]
fn cancel_sends_turn_cancel_and_reports_cancelled() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("cancel");
    let mut subject = runner(client, &root);

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);

    let control = TurnControl::new();
    let cancel_from = control.clone();
    let worker = thread::spawn(move || {
        let mut interactor = Scripted {
            approve: true,
            answer: None,
            asked: Vec::new(),
        };
        subject.submit("长任务", &control, &mut interactor, &mut |_| {})
    });
    thread::sleep(Duration::from_millis(150));
    cancel_from.request_cancel();
    let outcome = worker.join().expect("回合线程应当正常结束");

    assert_eq!(outcome.expect_err("取消时应当报错"), TurnError::Cancelled);
    assert!(
        recorder.methods().contains(&"turn.cancel".to_string()),
        "取消必须请内核停止：{:?}",
        recorder.methods()
    );
    pipe.close();
}

#[test]
fn kernel_exit_fails_the_turn() {
    let (client, _, pipe) = kernel();
    let root = workspace("exit");
    let mut subject = runner(client, &root);

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.close();

    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    let error = subject
        .submit("你好", &TurnControl::new(), &mut interactor, &mut |_| {})
        .expect_err("内核退出后回合必须失败");
    assert_eq!(error, TurnError::KernelExited);
}
