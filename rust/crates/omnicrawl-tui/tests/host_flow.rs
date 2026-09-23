//! 宿主侧流程：用脚本化的假内核驱动 `App`，钉住握手、回合提交、工具批次与审批应答。
//!
//! 假内核是一对管道：往写端喂帧，宿主回出去的每一帧都记在写端旁边。管道保持打开，
//! 因此不会出现「脚本读完 = 内核退出」的假信号。

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use serde_json::{json, Value};

use omnicrawl_ipc::{Frame, Id};
use omnicrawl_tui::app::App;
use omnicrawl_tui::args::{ApprovalMode, Options};
use omnicrawl_tui::host::{Waiting, DENIED};
use omnicrawl_tui::kernel::KernelClient;
use omnicrawl_tui::state::Record;

/// 等待条件的上限：假内核在本地，正常都在毫秒级完成。
const WAIT: Duration = Duration::from_secs(5);

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

    fn response(&self, id: i64) -> Option<Frame> {
        self.frames()
            .into_iter()
            .find(|frame| frame.is_response() && frame.id() == Some(&Id::Number(id)))
    }
}

impl Write for Recorder {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("记录缓冲未被毒化")
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 假内核的输出流：按脚本吐字节，脚本读完就阻塞等更多输入，直到显式关闭。
///
/// 用自造读取器而不是管道，是为了让「脚本读完」和「内核退出」两件事分开：
/// 前者只是暂时没数据，后者是 `close()` 之后的 EOF。
struct ScriptState {
    script: Vec<u8>,
    position: usize,
    closed: bool,
}

#[derive(Clone)]
struct ScriptHandle {
    state: Arc<(Mutex<ScriptState>, Condvar)>,
}

impl ScriptHandle {
    fn new(script: &str) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ScriptState {
                    script: script.as_bytes().to_vec(),
                    position: 0,
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    /// 追加要吐出的帧。
    fn feed(&self, script: &str) {
        let (lock, condvar) = &*self.state;
        let mut state = lock.lock().expect("脚本锁未被毒化");
        state.script.extend_from_slice(script.as_bytes());
        condvar.notify_all();
    }

    /// 关掉输出：读取端从此拿到 EOF，等价于内核进程退出。
    fn close(&self) {
        let (lock, condvar) = &*self.state;
        let mut state = lock.lock().expect("脚本锁未被毒化");
        state.closed = true;
        condvar.notify_all();
    }
}

impl Read for ScriptHandle {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let (lock, condvar) = &*self.state;
        let mut state = lock.lock().expect("脚本锁未被毒化");
        loop {
            if state.position < state.script.len() {
                let available = &state.script[state.position..];
                let count = available.len().min(buffer.len());
                buffer[..count].copy_from_slice(&available[..count]);
                state.position += count;
                return Ok(count);
            }
            if state.closed {
                return Ok(0);
            }
            state = condvar.wait(state).expect("脚本锁未被毒化");
        }
    }
}

struct Harness {
    app: App,
    recorder: Recorder,
    script: ScriptHandle,
}

impl Harness {
    fn start(script: &str, approval: ApprovalMode) -> Self {
        Self::start_with_timeout(script, approval, 120)
    }

    fn start_with_timeout(script: &str, approval: ApprovalMode, tool_timeout_seconds: i64) -> Self {
        let handle = ScriptHandle::new(script);
        let recorder = Recorder::default();
        let kernel = KernelClient::from_streams(
            Box::new(BufReader::new(handle.clone())),
            Box::new(recorder.clone()),
        );
        let workspace = workspace_dir();
        std::fs::write(
            workspace.join("a.py"),
            "文件内容
第二行
",
        )
        .expect("写测试文件");
        let mut options = options(approval);
        options.tool_timeout_seconds = tool_timeout_seconds;
        let app = App::new(options, kernel, &workspace).expect("工具表应当构建成功");
        Self {
            app,
            recorder,
            script: handle,
        }
    }

    /// 往假内核写帧（脚本或后续追加）。
    fn send(&mut self, script: &str) {
        self.script.feed(script);
    }

    /// 反复收帧直到条件成立；超时即失败并给出当时的界面状态。
    fn expect_ready(&mut self, mut ready: impl FnMut(&App) -> bool, message: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            self.app.drain_frames();
            if ready(&self.app) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{message}（当前记录：{:?}）",
                self.app.state.records
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// 等到宿主回了指定 id 的响应。
    fn expect_response(&mut self, id: i64) -> Frame {
        let deadline = Instant::now() + WAIT;
        loop {
            self.app.drain_frames();
            if let Some(frame) = self.recorder.response(id) {
                return frame;
            }
            assert!(Instant::now() < deadline, "等待 id={id} 的响应超时");
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// 关掉假内核：相当于内核进程退出。
    fn close_kernel(&mut self) {
        self.script.close();
    }

    fn press(&mut self, code: KeyCode) {
        self.app
            .handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn frames(&self) -> Vec<Frame> {
        self.recorder.frames()
    }
}

/// 每个用例一个干净的工作区：工具执行体真的会在这里读写文件。
///
/// 目录名带进程内的自增序号：用例是并行跑的，共用同一个目录会互相删掉
/// 对方的工作区（工具表构建因此随目录消失而失败）。
fn workspace_dir() -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "omnicrawl-tui-host-flow-{}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    root
}

fn options(approval: ApprovalMode) -> Options {
    Options {
        kernel: Path::new("omnicrawl").to_path_buf(),
        model: "stub-model".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key_env: "OMNICRAWL_TUI_TEST_KEY".to_string(),
        system_prompt: "你是测试助手。".to_string(),
        session_root: None,
        context_window_tokens: Some(128_000),
        approval,
        command_timeout_seconds: 360,
        tool_timeout_seconds: 120,
        native_vision: false,
        image_gen: Default::default(),
        advisor: Default::default(),
    }
}

const HANDSHAKE: &str =
    "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocol_version\":\"1.0\"}}\n";

fn tool_batch(id: i64, body: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tool.batch\",\"params\":{body}}}\n")
}

#[test]
fn handshake_submits_turn_and_answers_tool_batch() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        7,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"bash\",\"arguments\":{\"command\":\"echo 审批后执行\"},\"id\":\"c1\",\"function_name\":\"bash\"}]}",
    ));
    harness
        .send("{\"jsonrpc\":\"2.0\",\"method\":\"turn.delta\",\"params\":{\"text\":\"你好\"}}\n");

    // shell 命令在 manual 模式下要先弹审批，宿主还不该回响应。
    harness.expect_ready(|app| app.state.waiting().is_some(), "应停在审批面板");
    assert!(
        harness.recorder.response(7).is_none(),
        "审批未决定前不能回响应"
    );

    // 面板期间键盘归面板：先批准，整批观察按序回给内核。
    harness.press(KeyCode::Char('y'));
    let response = harness.expect_response(7);
    let observations = &response.result.expect("响应应带 result")["observations"];
    assert_eq!(observations.as_array().map(Vec::len), Some(1));
    let output = observations[0]["result"]["output"]
        .as_str()
        .unwrap_or_default();
    if output.contains("未找到可用的 Git Bash") {
        eprintln!("（本机没有 Git Bash：这一步只验证审批后确实派发了执行）");
        assert_eq!(observations[0]["result"]["ok"], false, "{observations}");
    } else {
        assert_eq!(observations[0]["result"]["ok"], true, "{observations}");
        assert!(output.contains("退出码：0"), "{output}");
    }
    assert_eq!(
        observations[0]["message"]["content"].as_str(),
        Some(output),
        "回填给模型的 tool 消息应当与结果一致"
    );
    assert!(
        harness.app.state.records.iter().any(|record| matches!(
            record,
            Record::Tool(card) if card.name == "bash"
        )),
        "应当留下 bash 工具卡：{:?}",
        harness.app.state.records
    );

    // 提交一个回合，验证 turn.submit 帧的形状。
    harness.app.state.composer.insert("问一句");
    harness.press(KeyCode::Enter);
    assert!(harness.app.state.turn.is_running(), "提交后应进入运行态");
    let submitted = harness
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("turn.submit"))
        .expect("应发出 turn.submit");
    let params = submitted.params.clone().unwrap_or(Value::Null);
    assert_eq!(params["user_text"], "问一句");
    assert_eq!(params["turn_id"], "turn-1");

    // 流式增量进了消息流。
    harness.expect_ready(
        |app| {
            app.state
                .records
                .iter()
                .any(|record| matches!(record, Record::Assistant(text) if text.contains("你好")))
        },
        "正文增量应进入消息流",
    );
}

/// manual 模式只对 shell 命令与 git 写操作弹确认：文件与搜索类工具直接执行。
#[test]
fn manual_mode_runs_file_and_search_tools_without_approval() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        12,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"read\",\"arguments\":{\"path\":\"a.py\"},\"id\":\"c1\",\"function_name\":\"read\"}]}",
    ));

    let response = harness.expect_response(12);
    let observations = &response.result.expect("响应应带 result")["observations"];
    assert_eq!(observations[0]["result"]["ok"], true, "{observations}");
    assert!(
        observations[0]["result"]["output"]
            .as_str()
            .unwrap_or_default()
            .starts_with("1: 文件内容"),
        "{observations}"
    );
    assert!(
        harness.app.state.waiting().is_none(),
        "文件类工具不该出现审批面板"
    );
}

#[test]
fn denial_reports_denied_and_kernel_close_ends_session() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        3,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"bash\",\"arguments\":{\"command\":\"rm -rf /\"},\"id\":\"c9\",\"function_name\":\"bash\"}]}",
    ));
    harness.expect_ready(|app| app.state.waiting().is_some(), "应停在审批面板");

    harness.press(KeyCode::Char('n'));
    let response = harness.expect_response(3);
    let observations = &response.result.expect("响应应带 result")["observations"];
    assert_eq!(
        observations[0]["result"]["error_code"].as_str(),
        Some(DENIED)
    );

    // 关掉假内核等于内核退出：宿主收尾并退出。
    harness.close_kernel();
    harness.expect_ready(|app| app.quit, "内核退出后宿主要跟着退出");
    assert!(!harness.app.state.turn.is_running());
    assert!(
        harness.app.state.records.iter().any(
            |record| matches!(record, Record::Notice(text) if text.contains("内核进程已退出"))
        ),
        "应把内核退出写进消息流：{:?}",
        harness.app.state.records
    );
}

#[test]
fn auto_mode_skips_approval_and_free_text_question_uses_composer() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Auto);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        4,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"read\",\"arguments\":{\"path\":\"a.py\"},\"id\":\"c1\",\"function_name\":\"read\"}]}",
    ));

    // auto 模式不弹审批，直接执行并把真实内容回给内核。
    let first = harness.expect_response(4);
    let observations = &first.result.expect("响应应带 result")["observations"];
    assert_eq!(observations[0]["result"]["ok"], true, "{observations}");
    assert!(
        observations[0]["result"]["output"]
            .as_str()
            .unwrap_or_default()
            .starts_with("1: 文件内容"),
        "{observations}"
    );

    // 前一批回复之后内核才会发下一批（协议保证批次串行）。
    harness.send(&tool_batch(
        5,
        "{\"turn_id\":\"t1\",\"step\":2,\"calls\":[{\"name\":\"ask_user\",\"arguments\":{\"question\":\"补充点什么？\"},\"id\":\"c2\",\"function_name\":\"ask_user\"}]}",
    ));

    // 提问没有选项：用输入框回答。
    harness.expect_ready(
        |app| matches!(app.state.waiting(), Some(Waiting::Question(_))),
        "应停在提问面板",
    );
    harness.app.state.composer.insert("补充内容");
    harness.press(KeyCode::Enter);
    let second = harness.expect_response(5);
    let observations = &second.result.expect("响应应带 result")["observations"];
    assert_eq!(observations[0]["result"]["ok"], true);
    assert_eq!(observations[0]["result"]["output"], "补充内容");
}

#[test]
fn select_question_uses_arrow_keys_and_answers_with_option() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        6,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"ask_user\",\"arguments\":{\"question\":\"选哪个？\",\"kind\":\"select\",\"options\":[\"A\",\"B\"]},\"id\":\"c3\",\"function_name\":\"ask_user\"}]}",
    ));
    harness.expect_ready(
        |app| matches!(app.state.waiting(), Some(Waiting::Question(_))),
        "应停在提问面板",
    );

    let selected = |app: &App| match app.state.waiting() {
        Some(Waiting::Question(panel)) => panel.selected,
        other => panic!("应停在提问面板，实际：{other:?}"),
    };
    assert_eq!(selected(&harness.app), 0);
    harness.press(KeyCode::Down);
    assert_eq!(selected(&harness.app), 1, "下键应移动选择");

    harness.press(KeyCode::Enter);
    let response = harness.expect_response(6);
    let observations = &response.result.expect("响应应带 result")["observations"];
    assert_eq!(observations[0]["result"]["output"], "B");
}

/// 慢工具不能把回合挂死：超过批次截止时间后宿主必须按超时回观察。
#[test]
fn slow_tool_is_reaped_by_the_batch_deadline() {
    // 需要真的能起 bash；没有 Git Bash 的机器上换一种可观测的失败，不静默跳过。
    if omnicrawl_tui::tools::command::invocation("true", omnicrawl_tui::tools::command::Shell::Bash)
        .is_err()
    {
        eprintln!("跳过：本机没有可用的 Git Bash");
        return;
    }

    let mut harness = Harness::start_with_timeout(HANDSHAKE, ApprovalMode::Auto, 1);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&tool_batch(
        8,
        "{\"turn_id\":\"t1\",\"step\":1,\"calls\":[{\"name\":\"bash\",\"arguments\":{\"command\":\"sleep 5\"},\"id\":\"c1\",\"function_name\":\"bash\"}]}",
    ));

    let response = harness.expect_response(8);
    let observations = &response.result.expect("响应应带 result")["observations"];
    let output = observations[0]["result"]["output"]
        .as_str()
        .unwrap_or_default();
    assert!(
        output.contains("工具执行超时"),
        "超时后应当回超时观察，而不是一直等：{output}"
    );
    assert!(output.contains("1"), "超时文案应带上设置的秒数：{output}");
    assert_eq!(observations[0]["result"]["ok"], false);
}

#[test]
fn unsupported_model_reply_is_refused() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Auto);
    harness.app.handshake().expect("握手应当成功");
    harness.send(
        "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"model.reply\",\"params\":{\"turn_id\":\"t1\",\"messages\":[]}}\n",
    );
    let response = harness.expect_response(9);
    let error = response.error.expect("应回错误响应");
    assert_eq!(
        error.code, -32601,
        "内核自带 provider runtime 时宿主不代答模型"
    );
    assert!(error.message.contains("model.reply"), "{}", error.message);
}

/// 后台命令监控：`monitor` 在 manual 模式下不弹确认，批次间能拿到同一个任务并收尾。
#[test]
fn monitor_batches_start_poll_and_stop_a_background_task() {
    // 后台任务由真实解释器承载；两种 Shell 都不可用时明确跳过，不做静默通过。
    let shell = {
        let bash =
            omnicrawl_tui::tools::command::find_bash_executable(&|name| std::env::var(name).ok())
                .is_some();
        let powershell = omnicrawl_tui::tools::command::find_powershell_executable(&|name| {
            std::env::var(name).ok()
        })
        .is_some();
        match (bash, powershell) {
            (true, _) => "bash",
            (false, true) => "powershell",
            (false, false) => {
                eprintln!("跳过：本机既没有 Git Bash 也没有 PowerShell");
                return;
            }
        }
    };
    let monitor_batch = |id: i64, arguments: Value| {
        tool_batch(
            id,
            &json!({
                "turn_id": "t1",
                "step": 1,
                "calls": [{
                    "name": "monitor",
                    "arguments": arguments,
                    "id": "c1",
                    "function_name": "monitor",
                }],
            })
            .to_string(),
        )
    };
    let observation_output = |frame: &Frame| -> String {
        frame.result.as_ref().expect("响应应带 result")["observations"][0]["result"]["output"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");

    // manual 模式只为 shell 命令与写类 git 弹确认：monitor 直接执行，界面不该停在面板上。
    harness.send(&monitor_batch(
        11,
        json!({"action": "start", "command": "echo monitor-e2e", "shell": shell}),
    ));
    let started = harness.expect_response(11);
    assert!(
        harness.app.state.waiting().is_none(),
        "monitor 不该弹审批面板"
    );
    let start_output = observation_output(&started);
    assert!(
        start_output.starts_with("已启动后台任务：monitor-"),
        "{start_output}"
    );
    let monitor_id = start_output
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches("已启动后台任务：")
        .to_string();

    // 轮询直到终态事件出现：进程输出与「任务已完成」落在同一条观察里。
    let mut batch_id = 12;
    let mut poll_output;
    let deadline = Instant::now() + WAIT;
    loop {
        harness.send(&monitor_batch(
            batch_id,
            json!({"action": "poll", "monitor_id": monitor_id, "cursor": 0}),
        ));
        poll_output = observation_output(&harness.expect_response(batch_id));
        if poll_output.contains("任务已完成") || Instant::now() >= deadline {
            break;
        }
        batch_id += 1;
        thread::sleep(Duration::from_millis(20));
    }
    assert!(poll_output.contains("状态：completed"), "{poll_output}");
    assert!(poll_output.contains("stdout: monitor-e2e"), "{poll_output}");
    assert!(
        poll_output.contains("system: 已启动，shell="),
        "{poll_output}"
    );

    // 已结束的任务再收到 stop 是幂等的，仍然回当前状态。
    batch_id += 1;
    harness.send(&monitor_batch(
        batch_id,
        json!({"action": "stop", "monitor_id": monitor_id}),
    ));
    let stopped = observation_output(&harness.expect_response(batch_id));
    assert!(stopped.contains("状态：completed"), "{stopped}");

    assert!(
        harness.app.state.records.iter().any(|record| matches!(
            record,
            Record::Tool(card) if card.name == "monitor"
        )),
        "应当留下 monitor 工具卡：{:?}",
        harness.app.state.records
    );
}
// ---------- 斜杠命令接线 ----------

/// 内核对 `subagent.query`（list）的响应：一条运行中的后台任务。
const SUBAGENT_LIST_REPLY: &str = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"unavailable\":false,\
\"action\":\"list\",\"tasks\":[{\"task_id\":\"task-1\",\"agent_type\":\"reviewer\",\"status\":\"running\",\
\"description\":\"审查改动\"}],\"task\":null,\"result\":{}}}\n";

/// 内核对 `turn.undo` 的响应。
const UNDO_REPLY: &str = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"kind\":\"complete\",\
\"message_count\":2,\"side_effects_reverted\":true,\"unrestorable\":[]}}\n";

fn notices(harness: &Harness) -> Vec<String> {
    harness
        .app
        .state
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn type_text(harness: &mut Harness, text: &str) {
    for character in text.chars() {
        harness.press(KeyCode::Char(character));
    }
}

#[test]
fn slash_prefix_opens_menu_and_tab_completes() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    assert!(
        !harness.app.state.composer.menu().is_open(),
        "空输入时菜单不应弹出"
    );

    // 输入 `/set`：候选来自统一命令源，只有 `/settings` 以它开头。
    type_text(&mut harness, "/set");
    let names: Vec<&str> = harness
        .app
        .state
        .composer
        .menu()
        .matches()
        .iter()
        .map(|option| option.command.as_str())
        .collect();
    assert_eq!(names, vec!["/settings"], "菜单候选应当来自注册表");

    // Tab 只补全、不提交；补全后完整命令会带上参数提示候选。
    harness.press(KeyCode::Tab);
    assert_eq!(harness.app.state.composer.text(), "/settings");
    let names: Vec<&str> = harness
        .app
        .state
        .composer
        .menu()
        .matches()
        .iter()
        .map(|option| option.command.as_str())
        .collect();
    assert_eq!(names, vec!["/settings", "--chat"], "{names:?}");

    // 输入已是完整命令：Enter 放行提交流程，命令层给出「打开设置面板」。
    harness.press(KeyCode::Enter);
    assert!(
        notices(&harness).iter().any(|text| text == "打开设置面板"),
        "Enter 应当执行 /settings：{:?}",
        notices(&harness)
    );
    assert!(
        harness
            .frames()
            .iter()
            .all(|frame| frame.method() != Some("turn.submit")),
        "斜杠命令不应变成一轮对话"
    );
}

#[test]
fn quit_command_asks_the_host_to_exit() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    type_text(&mut harness, "/quit");
    harness.press(KeyCode::Enter);
    assert!(harness.app.quit, "/quit 应当请求退出");
    assert!(
        harness
            .frames()
            .iter()
            .any(|frame| frame.method() == Some("shutdown")),
        "退出前应当请内核收工"
    );
}

#[test]
fn unsupported_command_reports_reason_instead_of_talking_to_the_model() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    // 切换工作区要重建内核会话、MCP 与工具表，协议仍没有入口：给出原因，而不是当成提示词发出去。
    type_text(&mut harness, "/workspace D:/other");
    harness.press(KeyCode::Enter);
    let messages = notices(&harness);
    assert!(
        messages.iter().any(|text| text.contains("/workspace")),
        "{messages:?}"
    );
    assert!(
        harness
            .frames()
            .iter()
            .all(|frame| frame.method() != Some("turn.submit")),
        "未支持的命令不能退化成模型对话"
    );
}

/// 测试用会话 id：与内核同格式（`YYYYMMDD-HHMMSS-<hex>`），否则条目校验会拒掉。
const SESSION_ID: &str = "20260101-000000-abcdef";

/// 一条会话索引条目的内核回包形状（与 `SessionIndexEntry::to_dict` 同字段）。
fn session_entry_json() -> String {
    format!(
        "{{\"session_id\":\"{SESSION_ID}\",\"title\":\"旧会话\",\"workspace_root\":\"D:/w\",\
         \"path\":\"sessions/{SESSION_ID}.jsonl\",\"created_at\":\"2026-01-01T00:00:00Z\",\
         \"updated_at\":\"2026-01-02T00:00:00Z\",\"event_count\":4,\"message_count\":2,\
         \"last_event_type\":\"assistant_message\",\"archived_at\":null}}"
    )
}

/// `/resume`：宿主把整条命令下发给内核，并把回给的历史重放进消息流。
///
/// 命令层是同步接口，宿主在按 Enter 的那一帧里等响应，所以回帧必须先备好。
#[test]
fn resume_command_asks_the_kernel_and_replays_history() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(&format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"session_id\":\"{SESSION_ID}\",\
         \"session\":{},\"history\":[{{\"role\":\"user\",\"content\":\"第一问\"}},\
         {{\"role\":\"assistant\",\"content\":\"第一答\"}}]}}}}\n",
        session_entry_json()
    ));

    type_text(&mut harness, &format!("/resume {SESSION_ID}"));
    harness.press(KeyCode::Enter);

    let request = harness
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("session.resume"))
        .expect("应当下发 session.resume");
    assert_eq!(
        request
            .params
            .as_ref()
            .and_then(|params| params.get("session_id")),
        Some(&json!(SESSION_ID)),
        "会话 id 原样传给内核：{request:?}"
    );
    let messages = notices(&harness);
    assert!(
        messages
            .iter()
            .any(|text| text.contains(&format!("已恢复会话：{SESSION_ID}"))
                && text.contains("2 条上下文消息")),
        "{messages:?}"
    );
    // 历史重放进消息流：撤回/切换后旧的对话不再留在视图里，是「重放」而不是「追加提示」。
    let texts: Vec<String> = harness
        .app
        .state
        .records
        .iter()
        .filter_map(|record| match record {
            Record::User(text) | Record::Assistant(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["第一问", "第一答"], "历史应当成为对话视图");
    assert!(
        harness
            .frames()
            .iter()
            .all(|frame| frame.method() != Some("turn.submit")),
        "会话命令不能退化成模型对话"
    );
}

/// 内核拒绝会话命令时如实透出原因（不假装成功，也不退化成模型对话）。
#[test]
fn session_command_failure_is_reported() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32600,\"message\":\"当前会话不受内核持有，无法执行该会话操作。\"}}\n",
    );

    type_text(&mut harness, "/sessions");
    harness.press(KeyCode::Enter);

    let messages = notices(&harness);
    assert!(
        messages.iter().any(|text| {
            text.contains("会话列表读取失败") && text.contains("不受内核持有")
        }),
        "{messages:?}"
    );
    assert!(
        harness
            .frames()
            .iter()
            .all(|frame| frame.method() != Some("turn.submit")),
        "失败也不能退化成模型对话"
    );
}

/// `/review`：宿主先做 git 预检，再异步下发 `subagent.run`，回执渲染成报告、随后注入上下文。
#[test]
fn review_command_runs_precheck_then_subagent_and_injects_report() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    // 工作区是空的 git 仓库（`Harness::start` 只建了目录）：预检会先失败，不会拉起子 Agent。
    type_text(&mut harness, "/review");
    harness.press(KeyCode::Enter);
    assert!(
        harness
            .frames()
            .iter()
            .all(|frame| frame.method() != Some("subagent.run")),
        "预检失败时不应派生评审子 Agent"
    );
    let messages = notices(&harness);
    assert!(
        messages
            .iter()
            .any(|text| text.contains("git") || text.contains("仓库")),
        "应当给出预检原因：{messages:?}"
    );
}

#[test]
fn tasks_command_queries_the_kernel_and_prints_the_snapshot() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    // 响应要在 Enter 之前就位：命令分派会在事件处理里同步等它。
    harness.send(SUBAGENT_LIST_REPLY);
    type_text(&mut harness, "/tasks");
    harness.press(KeyCode::Enter);

    let query = harness
        .frames()
        .into_iter()
        .find(|frame| frame.method() == Some("subagent.query"))
        .expect("应当发出 subagent.query");
    assert_eq!(
        query
            .params
            .as_ref()
            .and_then(|params| params["action"].as_str()),
        Some("list")
    );
    let messages = notices(&harness);
    assert!(
        messages
            .iter()
            .any(|text| text.contains("task-1") && text.contains("running")),
        "{messages:?}"
    );
}

#[test]
fn undo_is_dispatched_to_the_kernel_and_reported_back() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().expect("握手应当成功");
    harness.send(UNDO_REPLY);
    type_text(&mut harness, "/undo");
    harness.press(KeyCode::Enter);

    assert!(
        harness
            .frames()
            .iter()
            .any(|frame| frame.method() == Some("turn.undo")),
        "/undo 应当异步下发给内核"
    );
    harness.expect_ready(
        |app| {
            app.state.records.iter().any(
                |record| matches!(record, Record::Notice(text) if text.contains("已撤销最近一轮")),
            )
        },
        "撤销结果应当回填到消息流",
    );
    assert_eq!(harness.app.state.status, None, "回执到达后状态行应当收起");
}
