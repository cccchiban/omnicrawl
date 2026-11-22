use super::*;

#[test]
fn cancelled_turn_is_drained_before_creating_a_new_session() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().unwrap();
    disable_suggestions(&mut harness);
    type_text(&mut harness, "旧任务");
    harness.press(KeyCode::Enter);
    let submit = harness.frames().into_iter().find(|frame| frame.method() == Some("turn.submit")).unwrap();
    harness.press(KeyCode::Esc);
    let count = harness.app.state.records.len();
    type_text(&mut harness, "/new");
    let started = Instant::now();
    harness.press(KeyCode::Enter);
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(!harness.frames().iter().any(|frame| frame.method() == Some("session.new")), "旧回合尚未收口时不能新建");
    harness.send("{\"jsonrpc\":\"2.0\",\"method\":\"turn.reasoning_delta\",\"params\":{\"text\":\"旧回合迟到的思考\"}}\n");
    harness.send(&format!("{}\n", Frame::error_response(submit.id().unwrap().clone(), omnicrawl_ipc::ErrorObject::new(-32001, "回合已取消。")).to_line()));
    let deadline = Instant::now() + WAIT;
    let request = loop {
        harness.app.drain_frames();
        if let Some(request) = harness.frames().into_iter().find(|frame| frame.method() == Some("session.new")) {
            break request;
        }
        assert!(Instant::now() < deadline, "取消收口后应推进 /new");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(harness.app.state.records.len(), count, "取消后的增量不能复活旧视图");
    type_text(&mut harness, "新任务");
    harness.press(KeyCode::Enter);
    let new_id = "20260102-000000-abcdef";
    harness.send(&format!("{}\n", Frame::response(request.id().unwrap().clone(), json!({"session_id": new_id})).to_line()));
    harness.expect_ready(|app| app.current_session_id() == new_id && app.state.turn.is_running(), "等待新会话并提交新任务");
    assert_eq!(harness.frames().iter().filter(|frame| frame.method() == Some("turn.submit")).count(), 2);
}

#[test]
fn a_tool_batch_arriving_after_cancel_is_not_executed() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Auto);
    harness.app.handshake().unwrap();
    disable_suggestions(&mut harness);
    type_text(&mut harness, "旧任务");
    harness.press(KeyCode::Enter);
    harness.press(KeyCode::Esc);
    harness.send(&tool_batch(702, r#"{"turn_id":"turn-1","step":1,"calls":[{"name":"write_file","arguments":{"path":"late.txt","content":"不应写入"},"id":"late","function_name":"write_file"}]}"#));
    let response = harness.expect_response(702);
    assert_eq!(response.error.expect("应取消迟到批次").code, omnicrawl_ipc::error_code::TURN_CANCELLED);
    assert!(!harness.workspace.join("late.txt").exists());
}

#[test]
fn frame_drain_yields_with_a_notification_backlog() {
    let mut harness = Harness::start(HANDSHAKE, ApprovalMode::Manual);
    harness.app.handshake().unwrap();
    let frame = "{\"jsonrpc\":\"2.0\",\"method\":\"turn.notice\",\"params\":{\"message\":\"积压通知\"}}\n";
    harness.send(&frame.repeat(2_000));
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    harness.app.drain_frames();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(harness.app.state.records.len() <= 64, "每帧处理必须有界，给输入和绘制让路");
}
