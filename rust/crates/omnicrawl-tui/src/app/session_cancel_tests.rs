use super::*;

fn app() -> App {
    let options = Options {
        kernel: "omnicrawl".into(), model: "stub".into(), base_url: "http://127.0.0.1:1/v1".into(),
        api_key: String::new(), system_prompt: Some("test".into()), session_root: None,
        context_window_tokens: Some(128_000), approval: ApprovalMode::Auto, approval_from_cli: true,
        command_timeout_seconds: 1, tool_timeout_seconds: 1, native_vision: false,
        image_gen: Default::default(), advisor: Default::default(),
    };
    let kernel = KernelClient::from_streams(
        Box::new(std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()))),
        Box::new(std::io::sink()),
    );
    App::new(options, kernel, &std::env::temp_dir()).unwrap()
}

#[test]
fn new_session_does_not_wait_for_the_mcp_capability_lock() {
    let mut app = app();
    let manager = Arc::new(omnicrawl_mcp::client::McpClientManager::new(
        omnicrawl_mcp::config::McpConfig::default(), std::env::temp_dir(),
    ).without_capability_cache());
    app.registry_options.mcp = Some(manager.clone());
    app.rebuild_registry().unwrap();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _guard = manager.registry();
        sender.send(()).unwrap();
        thread::sleep(Duration::from_secs(1));
    });
    receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    let started = Instant::now();
    app.apply_new_session(&json!({"session_id":"20260102-000000-abcdef"})).unwrap();
    let elapsed = started.elapsed();
    println!("new_session_receipt_elapsed={elapsed:?}");
    worker.join().unwrap();
    assert!(elapsed < Duration::from_millis(250), "切会话不能重建能力表而等待旧任务：{elapsed:?}");
    assert_eq!(app.registry.memory().session_id.as_deref(), Some("20260102-000000-abcdef"));
}

#[test]
fn late_completion_is_not_recorded_in_a_different_batch() {
    let mut app = app();
    let old = ToolCall { id: "old".into(), name: "read".into(), function_name: String::new(), arguments: Default::default() };
    let new = ToolCall { id: "new".into(), name: "read".into(), function_name: String::new(), arguments: Default::default() };
    app.state.start_batch(Id::Number(701), vec![new]);
    app.completion_sender.send(ToolCompletion {
        batch_id: Id::Number(700), index: 0, call: old,
        result: ToolResult { ok: true, output: "旧结果".into(), full_output: String::new(), error_code: None, retryable: false },
        vision: None,
    }).unwrap();
    app.drain_completions();
    assert_eq!(app.state.batch_request_id(), Some(Id::Number(701)), "旧结果不能完成新批次");
}
