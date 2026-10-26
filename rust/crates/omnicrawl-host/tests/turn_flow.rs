//! 无头回合运行器的流程测试：用脚本化假内核钉住握手、整批工具定调、取消与事件出口。
//!
//! 假内核是一对内存管道：脚本字节先灌进去，宿主写出的每一帧都记在旁边的记录器里。
//! 管道保持打开（脚本读完只是暂时没数据），因此不会出现「脚本读完 = 内核退出」的假信号。

use std::collections::VecDeque;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
        api_key: String::new(),
        user_agent: "omnicrawl-test".to_string(),
        system_prompt: "系统提示".to_string(),
        context_messages: Vec::new(),
        tools: Vec::new(),
        options: json!({}),
        request_timeout_seconds: None,
        context_window_tokens: 0,
        prompt_cache_capable: false,
        prompt_cache_identity: Default::default(),
        native_vision: false,
        request_retry_count: 1,
    }
}

fn runner(client: KernelClient, root: &Path) -> TurnRunner {
    runner_with_custody(client, root, None)
}

fn runner_with_custody(
    client: KernelClient,
    root: &Path,
    custody: Option<omnicrawl_host::tools::CustodyOptions>,
) -> TurnRunner {
    let options = RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        attach_vision_images: false,
        client_name: "omnicrawl-host-test".to_string(),
        plugins: None,
        review: None,
        custody,
        // 不做工具调用淘汰：测试只关心提问托管的分支。
        prune: None,
        // 不做建议：user 原文照发。
        suggest: None,
        subagent_suggest: None,
        prompt: None,
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

/// 固定选某一项的托管桩：不起网络，并记下收到的上下文。
struct FixedChoice {
    index: usize,
    seen: Arc<Mutex<Option<(String, Vec<String>)>>>,
}

impl omnicrawl_host::tools::ChoiceClient for FixedChoice {
    fn choose(
        &self,
        request: &omnicrawl_host::tools::decision_choice::ChoiceRequest<'_>,
    ) -> Result<usize, String> {
        *self.seen.lock().expect("记录未被毒化") = Some((
            request.state.to_string(),
            request
                .criteria
                .iter()
                .map(|(key, _)| key.clone())
                .collect(),
        ));
        Ok(self.index)
    }
}

/// 固定建议名单的桩，并记下送审的候选清单与提问正文。
struct FixedSuggest {
    names: Vec<String>,
    seen: Arc<Mutex<Option<(String, Vec<String>)>>>,
}

impl omnicrawl_host::tools::SuggestClient for FixedSuggest {
    fn suggest(
        &self,
        request: &omnicrawl_host::tools::decision_suggest::SuggestRequest<'_>,
    ) -> Result<Vec<String>, String> {
        // 建议行与送审字段名都按种类取：`state.tools` / `state.subagents`，兜底名同理。
        *self.seen.lock().expect("记录未被毒化") = Some((
            request.user_prompt.to_string(),
            request
                .candidates
                .iter()
                .map(|(name, _)| (*name).to_string())
                .collect(),
        ));
        Ok(self.names.clone())
    }
}

/// 会睡一段时间的桩：记录并发发起时的进入顺序与峰值在飞数。
struct SlowFixedSuggest {
    delay: Duration,
    names: Vec<String>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl omnicrawl_host::tools::SuggestClient for SlowFixedSuggest {
    fn suggest(
        &self,
        _request: &omnicrawl_host::tools::decision_suggest::SuggestRequest<'_>,
    ) -> Result<Vec<String>, String> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        thread::sleep(self.delay);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(self.names.clone())
    }
}

fn runner_with_suggest(
    client: KernelClient,
    root: &Path,
    suggest: Option<omnicrawl_host::tools::SuggestOptions>,
) -> TurnRunner {
    runner_with_both_suggestions(client, root, suggest, None, None)
}

/// 带上两类建议的版本；`roles` 钉住子代理候选，否则会随本机 subagents 配置漂移。
fn runner_with_both_suggestions(
    client: KernelClient,
    root: &Path,
    suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    subagent_suggest: Option<omnicrawl_host::tools::SuggestOptions>,
    roles: Option<Vec<(String, String)>>,
) -> TurnRunner {
    let mut options = RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        attach_vision_images: false,
        client_name: "omnicrawl-host-test".to_string(),
        plugins: None,
        review: None,
        custody: None,
        prune: None,
        suggest: None,
        subagent_suggest: None,
        prompt: None,
    };
    options.suggest = suggest;
    options.subagent_suggest = subagent_suggest;
    let registry = RegistryOptions {
        subagent_roles: roles.unwrap_or_default(),
        ..RegistryOptions::default()
    };
    TurnRunner::new(client, options, &registry).expect("工具表应当建成")
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

    // 稳定前缀身份必须在工具表就位后组装：七字段齐全，且工具表哈希非空。
    let identity = initialize
        .params
        .as_ref()
        .and_then(|params| params.get("model"))
        .and_then(|model| model.get("prompt_cache_identity"))
        .and_then(Value::as_object)
        .expect("握手必须装配 prompt_cache_identity");
    assert_eq!(identity.len(), 7, "身份指纹七字段：{identity:?}");
    assert!(
        identity
            .get("tool_schema_hash")
            .and_then(Value::as_str)
            .map(|hash| !hash.is_empty())
            .unwrap_or(false),
        "工具表哈希不能为空：{identity:?}"
    );

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
fn custody_answers_the_question_without_asking_the_user() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("custody");
    let seen = Arc::new(Mutex::new(None));
    let custody = omnicrawl_host::tools::CustodyOptions {
        enabled: true,
        channel: Some(omnicrawl_host::tools::decision_choice::ChoiceChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "jv_test".to_string(),
            retry_count: 0,
        }),
        masking: None,
        client: Arc::new(FixedChoice {
            index: 1,
            seen: Arc::clone(&seen),
        }),
    };
    let mut subject = runner_with_custody(client, &root, Some(custody));

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

    // 托管生效时交互层不该被问到：主播的 `answer` 一次都没走。
    let mut interactor = Scripted {
        approve: true,
        answer: Some("选项A".to_string()),
        asked: Vec::new(),
    };
    let mut events: Vec<HostEvent> = Vec::new();
    subject
        .submit(
            "问一下",
            &TurnControl::new(),
            &mut interactor,
            &mut |event| events.push(event),
        )
        .expect("回合应当跑完");

    assert!(interactor.asked.is_empty(), "{:?}", interactor.asked);
    let result =
        ToolBatchResult::from_result(&recorder.result(3)).expect("宿主回的应当是工具批次结果");
    assert_eq!(result.observations.len(), 1);
    assert!(result.observations[0].result.ok);
    assert_eq!(
        result.observations[0].result.output, "选项B",
        "取桩选中的那一项"
    );

    // 自动作答在会话流里留痕，并带上问题与选中项。
    let notices: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Notice(payload) => Some(payload.message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("选哪个？"), "{}", notices[0]);
    assert!(notices[0].contains("选项B"), "{}", notices[0]);

    // 请求上下文带上了提问与用户本回合的请求。
    let (state, criteria) = seen
        .lock()
        .expect("记录未被毒化")
        .clone()
        .expect("托管被调用");
    assert!(state.contains("选哪个？"), "{state}");
    assert!(state.contains("问一下"), "{state}");
    assert_eq!(criteria, vec!["o0", "o1", "o2"], "末尾还有自定义作答入口");
    pipe.close();
}

/// 决策模型选「我有自己的想法...」时，提问退回给用户自己作答（面板与输入框照旧工作）。
#[test]
fn custody_falls_back_to_the_user_when_the_custom_option_wins() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("custody-custom");
    let seen = Arc::new(Mutex::new(None));
    let custody = omnicrawl_host::tools::CustodyOptions {
        enabled: true,
        channel: Some(omnicrawl_host::tools::decision_choice::ChoiceChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "jv_test".to_string(),
            retry_count: 0,
        }),
        masking: None,
        // 三个候选 = 两个模型选项 + 末尾的自定义作答入口。
        client: Arc::new(FixedChoice {
            index: 2,
            seen: Arc::clone(&seen),
        }),
    };
    let mut subject = runner_with_custody(client, &root, Some(custody));

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(concat!(
        r#"{"jsonrpc":"2.0","id":3,"method":"tool.batch","params":{"turn_id":"turn-1","step":1,"calls":["#,
        r#"{"name":"ask_user","arguments":{"question":"给这个分支起个名字？","options":["feature-a","feature-b"],"kind":"select"},"id":"c1","function_name":"ask_user"}]}}"#,
    ));
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":1,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: Some("我自己起的名字".to_string()),
        asked: Vec::new(),
    };
    let mut events: Vec<HostEvent> = Vec::new();
    subject
        .submit(
            "加个分支",
            &TurnControl::new(),
            &mut interactor,
            &mut |event| events.push(event),
        )
        .expect("回合应当跑完");

    // 托管先被问过一次，它选了自定义作答入口，于是退回人工提问。
    let (state, criteria) = seen
        .lock()
        .expect("记录未被毒化")
        .clone()
        .expect("托管被调用");
    assert!(state.contains("给这个分支起个名字？"), "{state}");
    assert_eq!(criteria, vec!["o0", "o1", "o2"]);
    assert_eq!(
        interactor.asked,
        vec!["answer:给这个分支起个名字？".to_string()]
    );
    let result =
        ToolBatchResult::from_result(&recorder.result(3)).expect("宿主回的应当是工具批次结果");
    assert_eq!(result.observations[0].result.output, "我自己起的名字");
    // 放回用户作答就不该再报「已自动作答」。
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HostEvent::Notice(_))),
        "{:?}",
        events_of(&events)
    );
    pipe.close();
}

/// 工具建议：进内核的 `user_text` 已带上建议行，送审的清单直接来自工具表。
#[test]
fn the_suggestion_is_appended_to_the_user_text_sent_to_the_kernel() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("suggest");
    let seen = Arc::new(Mutex::new(None));
    let suggest = omnicrawl_host::tools::SuggestOptions {
        enabled: true,
        channel: Some(omnicrawl_host::tools::decision_suggest::SuggestChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "jv_test".to_string(),
            retry_count: 0,
        }),
        masking: None,
        client: Arc::new(FixedSuggest {
            names: vec!["web_search".to_string()],
            seen: Arc::clone(&seen),
        }),
    };
    let mut subject = runner_with_suggest(client, &root, Some(suggest));

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    subject
        .submit(
            "帮我把这个任务拆成几路",
            &TurnControl::new(),
            &mut interactor,
            &mut |_| {},
        )
        .expect("回合应当跑完");

    // 送审的清单直接来自工具表：进表的工具（含泛化工具）都在其中，说明取描述首句。
    let (prompt, candidates) = seen
        .lock()
        .expect("记录未被毒化")
        .clone()
        .expect("建议被调用");
    assert_eq!(prompt, "帮我把这个任务拆成几路");
    assert!(
        candidates.contains(&"web_search".to_string()),
        "{candidates:?}"
    );
    assert!(candidates.contains(&"bash".to_string()), "{candidates:?}");
    assert!(candidates.contains(&"read".to_string()), "{candidates:?}");
    assert!(
        candidates.contains(&"kb_search".to_string()),
        "{candidates:?}"
    );
    // 决定送审范围的是工具表本身：记忆组默认不进表，因此也不在清单里。
    assert!(
        !candidates.contains(&"memory_search".to_string()),
        "{candidates:?}"
    );

    // 进内核的 user_text 尾部带上了建议行。
    let submitted = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("turn.submit"))
        .and_then(|frame| frame.params)
        .and_then(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("应当发过 turn.submit");
    assert_eq!(
        submitted,
        "帮我把这个任务拆成几路\n\n当前请求建议调用tool：web_search。"
    );
    pipe.close();
}

/// 没有建议（名单为空）时 user 原文照发：不留下任何插入痕迹。
#[test]
fn an_empty_shortlist_sends_the_prompt_untouched() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("suggest-empty");
    let suggest = omnicrawl_host::tools::SuggestOptions {
        enabled: true,
        channel: Some(omnicrawl_host::tools::decision_suggest::SuggestChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "jv_test".to_string(),
            retry_count: 0,
        }),
        masking: None,
        client: Arc::new(FixedSuggest {
            names: Vec::new(),
            seen: Arc::new(Mutex::new(None)),
        }),
    };
    let mut subject = runner_with_suggest(client, &root, Some(suggest));

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    subject
        .submit("你好", &TurnControl::new(), &mut interactor, &mut |_| {})
        .expect("回合应当跑完");

    let submitted = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("turn.submit"))
        .and_then(|frame| frame.params)
        .and_then(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("应当发过 turn.submit");
    assert_eq!(submitted, "你好", "无建议时提问原文照发");
    pipe.close();
}

/// 两类建议同时开启：两条请求并发在飞，且都拼进 user 尾部（工具行在前，子代理行在后）。
#[test]
fn both_suggestion_kinds_run_concurrently_and_both_land_in_the_user_text() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("suggest-both");
    let delay = Duration::from_millis(400);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let channel = || omnicrawl_host::tools::decision_suggest::SuggestChannel {
        mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
        model: "jev-latest".to_string(),
        base_url: "http://127.0.0.1:1".to_string(),
        api_key: "jv_test".to_string(),
        retry_count: 0,
    };
    let options = |names: Vec<String>| omnicrawl_host::tools::SuggestOptions {
        enabled: true,
        channel: Some(channel()),
        masking: None,
        client: Arc::new(SlowFixedSuggest {
            delay,
            names,
            in_flight: Arc::clone(&in_flight),
            peak: Arc::clone(&peak),
        }),
    };
    let roles = vec![
        ("explore".to_string(), "快速只读探索代码".to_string()),
        ("plan".to_string(), "制定只读实施计划".to_string()),
    ];
    let mut subject = runner_with_both_suggestions(
        client,
        &root,
        Some(options(vec!["web_search".to_string()])),
        Some(options(vec!["explore".to_string()])),
        Some(roles),
    );

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    let started = std::time::Instant::now();
    subject
        .submit(
            "先探索代码再定计划",
            &TurnControl::new(),
            &mut interactor,
            &mut |_| {},
        )
        .expect("回合应当跑完");
    let elapsed = started.elapsed();

    assert!(
        elapsed < delay * 2,
        "两条请求应当并发：耗时 {elapsed:?} 不该接近两倍延迟"
    );
    assert_eq!(peak.load(Ordering::SeqCst), 2, "两条建议请求必须同时在飞");

    let submitted = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("turn.submit"))
        .and_then(|frame| frame.params)
        .and_then(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("应当发过 turn.submit");
    assert_eq!(
        submitted,
        "先探索代码再定计划

当前请求建议调用tool：web_search。
当前请求建议使用子代理：explore。"
    );
    pipe.close();
}

/// 只有子代理建议开着时，工具建议的开关不影响它；插入行只有子代理那一行。
#[test]
fn subagent_suggestion_works_without_tool_suggestion() {
    let (client, recorder, pipe) = kernel();
    let root = workspace("suggest-subagents-only");
    let subagent_suggest = omnicrawl_host::tools::SuggestOptions {
        enabled: true,
        channel: Some(omnicrawl_host::tools::decision_suggest::SuggestChannel {
            mode: omnicrawl_config::features::decision_model::DECISION_MODE_JEV.to_string(),
            model: "jev-latest".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: "jv_test".to_string(),
            retry_count: 0,
        }),
        masking: None,
        client: Arc::new(FixedSuggest {
            names: vec!["explore".to_string(), "plan".to_string()],
            seen: Arc::new(Mutex::new(None)),
        }),
    };
    let mut subject = runner_with_both_suggestions(
        client,
        &root,
        None,
        Some(subagent_suggest),
        Some(vec![
            ("explore".to_string(), "快速只读探索代码".to_string()),
            ("plan".to_string(), "制定只读实施计划".to_string()),
        ]),
    );

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );

    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    subject
        .submit(
            "先探索再计划",
            &TurnControl::new(),
            &mut interactor,
            &mut |_| {},
        )
        .expect("回合应当跑完");

    let submitted = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("turn.submit"))
        .and_then(|frame| frame.params)
        .and_then(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("应当发过 turn.submit");
    assert_eq!(
        submitted,
        "先探索再计划

当前请求建议使用子代理：explore、plan。"
    );
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
fn a_turn_after_cancel_runs_shell_commands_again() {
    // 回归：Esc 取消一个回合后继续对话，`bash` 不能再被令牌判成「命令已取消」。
    let has_bash =
        omnicrawl_host::tools::command::find_bash_executable(&|name| std::env::var(name).ok())
            .is_some();
    let (client, recorder, pipe) = kernel();
    let root = workspace("cancel-resume");
    let mut subject = runner(client, &root);

    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");

    // 回合 1：提交后由另一条线程置位取消开关。
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    let control = TurnControl::new();
    let cancel_from = control.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel_from.request_cancel();
    });
    let mut interactor = Scripted {
        approve: true,
        answer: None,
        asked: Vec::new(),
    };
    let cancelled = subject
        .submit("长任务", &control, &mut interactor, &mut |_| {})
        .expect_err("取消时应当报错");
    canceller.join().expect("取消线程应当结束");
    assert_eq!(cancelled, TurnError::Cancelled);
    assert!(
        subject.registry().cancel_token().is_cancelled(),
        "取消后令牌应当置位"
    );

    // 回合 2：未取消的新回合，必须重新拉起命令并拿到真实退出码。
    pipe.push(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#);
    pipe.push(concat!(
        r#"{"jsonrpc":"2.0","id":4,"method":"tool.batch","params":{"turn_id":"turn-2","step":1,"calls":["#,
        r#"{"name":"bash","arguments":{"command":"echo hi"},"id":"c1","function_name":"bash"}]}}"#,
    ));
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-2","final_text":"好","reasoning":"","model_turns":1,"tool_calls":1,"paused":false}}"#,
    );
    let outcome = subject
        .submit("继续", &TurnControl::new(), &mut interactor, &mut |_| {})
        .expect("取消后的新回合应当跑完");
    assert_eq!(outcome.turn_id, "turn-2");
    assert!(
        !subject.registry().cancel_token().is_cancelled(),
        "新回合开始必须复位取消标记"
    );

    let result =
        ToolBatchResult::from_result(&recorder.result(4)).expect("宿主回的应当是工具批次结果");
    assert_eq!(result.observations.len(), 1);
    let output = &result.observations[0].result.output;
    assert!(
        !output.contains("命令已取消"),
        "取消后的新回合不能再报已取消：{output}"
    );
    if has_bash {
        assert!(
            result.observations[0].result.ok,
            "新回合的命令必须真的执行：{output}"
        );
        assert!(output.contains("退出码：0"), "{output}");
    } else {
        // 没装 Git Bash 的机器上只能验证「不是被取消」；具体报缺 shell 是可接受的。
        assert!(output.contains("Git Bash"), "{output}");
    }
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

/// `/skill:<名>`：命中时先下发活动技能上下文，再把余下任务当本轮文本；
/// 未命中时把带可用列表的说明当本轮文本，两者都不带 `/skill:` 原文。
#[test]
fn skill_command_swaps_context_and_submits_the_task() {
    use omnicrawl_host::prompt::{PromptOptions, PromptRuntime};

    let root = workspace("skill-command");
    let skill_dir = root.join(".omnicrawl").join("skills").join("turn-skill");
    std::fs::create_dir_all(&skill_dir).expect("建 skill 目录");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: turn-skill\ndescription: 回合测试技能\n---\n\n正文标记 TURN-SKILL-BODY。\n",
    )
    .expect("写 SKILL.md");

    let env = omnicrawl_config::core::runtime::ConfigEnvironment::new("bundle", "test");
    let load_prompt =
        || PromptRuntime::load(&env, PromptOptions::new(root.clone())).expect("提示词装配应当成功");

    // ── 命中：上下文换成技能正文，提交的是余下任务 ──
    let (client, recorder, pipe) = kernel();
    let options = skill_runner_options(&root, load_prompt());
    let mut subject =
        TurnRunner::new(client, options, &RegistryOptions::default()).expect("工具表应当建成");
    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");

    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(r#"{"jsonrpc":"2.0","id":3,"result":{"applied":["model.context_messages"]}}"#);
    pipe.push(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-1","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );
    let mut interactor = Scripted {
        approve: false,
        answer: None,
        asked: Vec::new(),
    };
    subject
        .submit(
            "/skill:turn-skill 做点事",
            &TurnControl::new(),
            &mut interactor,
            &mut |_| {},
        )
        .expect("回合应当跑完");

    let methods = recorder.methods();
    assert!(
        methods.iter().any(|name| name == "session.settings"),
        "命中时应当先下发活动技能上下文：{methods:?}"
    );
    let submitted: Vec<String> = recorder
        .frames()
        .into_iter()
        .filter(|frame| frame.method() == Some("turn.submit"))
        .filter_map(|frame| frame.params)
        .filter_map(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert_eq!(submitted, vec!["做点事".to_string()]);
    let settings = recorder
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("session.settings"))
        .and_then(|frame| frame.params)
        .expect("设置帧带参数");
    let rendered = settings.to_string();
    assert!(
        rendered.contains("TURN-SKILL-BODY"),
        "应下发技能正文：{rendered}"
    );
    assert!(
        rendered.contains("<active_skill_instructions"),
        "应使用活动技能块：{rendered}"
    );

    // ── 未命中：把说明当本轮文本，且不下发活动技能上下文 ──
    let (client, recorder, pipe) = kernel();
    let options = skill_runner_options(&root, load_prompt());
    let mut subject =
        TurnRunner::new(client, options, &RegistryOptions::default()).expect("工具表应当建成");
    pipe.push(r#"{"jsonrpc":"2.0","id":1,"result":{"protocol_version":"1.0"}}"#);
    subject.handshake(&mut |_| {}).expect("握手应当成功");
    pipe.push(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#);
    pipe.push(
        r#"{"jsonrpc":"2.0","method":"turn.finished","params":{"turn_id":"turn-2","final_text":"好","reasoning":"","model_turns":1,"tool_calls":0,"paused":false}}"#,
    );
    let mut interactor = Scripted {
        approve: false,
        answer: None,
        asked: Vec::new(),
    };
    subject
        .submit(
            "/skill:no-such-skill 做点事",
            &TurnControl::new(),
            &mut interactor,
            &mut |_| {},
        )
        .expect("回合应当跑完");

    let mut methods = recorder.methods();
    methods.retain(|name| !name.starts_with("turn."));
    assert!(
        !methods.iter().any(|name| name == "session.settings"),
        "未命中不该下发活动技能上下文：{methods:?}"
    );
    let submitted: Vec<String> = recorder
        .frames()
        .into_iter()
        .filter(|frame| frame.method() == Some("turn.submit"))
        .filter_map(|frame| frame.params)
        .filter_map(|params| {
            params
                .get("user_text")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert_eq!(submitted.len(), 1, "{submitted:?}");
    assert!(
        submitted[0].contains("Skill「no-such-skill」不存在"),
        "未命中应回可用列表：{submitted:?}"
    );
    assert!(
        submitted[0].contains("turn-skill"),
        "可用列表应含已发现的 Skill：{submitted:?}"
    );
}

/// 带提示词装配的运行器选项（`/skill:` 需要它来加载正文）。
fn skill_runner_options(
    root: &Path,
    prompt: omnicrawl_host::prompt::PromptRuntime,
) -> RunnerOptions {
    RunnerOptions {
        workspace_root: root.to_path_buf(),
        model: model(),
        session: None,
        approval: ApprovalMode::Manual,
        command_timeout_seconds: 5,
        tool_timeout_seconds: 5,
        attach_vision_images: false,
        client_name: "omnicrawl-host-test".to_string(),
        plugins: None,
        review: None,
        custody: None,
        prune: None,
        suggest: None,
        subagent_suggest: None,
        prompt: Some(Arc::new(prompt)),
    }
}
