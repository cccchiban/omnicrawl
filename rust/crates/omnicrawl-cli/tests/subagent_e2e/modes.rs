use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Condvar;
use std::time::Instant;

struct GatedServer {
    server: StubServer,
    child_started: Receiver<()>,
    parent_requests: Arc<AtomicUsize>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl GatedServer {
    fn start(background: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let parent_requests = Arc::new(AtomicUsize::new(0));
        let parent_count = Arc::clone(&parent_requests);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let waiting = Arc::clone(&gate);
        let (started, child_started) = mpsc::channel();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let requests = Arc::clone(&recorded);
                let parent_count = Arc::clone(&parent_count);
                let gate = Arc::clone(&waiting);
                let started = started.clone();
                thread::spawn(move || {
                    let raw = read_request(&mut stream);
                    let request: Value = serde_json::from_str(&raw).unwrap();
                    requests.lock().unwrap().push(request.clone());
                    let child = raw.contains("subagent_task");
                    let has_tool = request["messages"].as_array().unwrap().iter()
                        .any(|message| message["role"] == "tool");
                    let body = if child && !has_tool {
                        let _ = started.send(());
                        let (lock, signal) = &*gate;
                        let guard = lock.lock().unwrap();
                        let (guard, _) = signal.wait_timeout_while(guard, WAIT, |open| !*open).unwrap();
                        assert!(*guard, "子代理闸门必须由测试释放");
                        tool_call_stream("read", "{\"path\":\"a.py\"}")
                    } else if child {
                        text_stream(CHILD_TEXT)
                    } else {
                        match parent_count.fetch_add(1, Ordering::SeqCst) {
                            0 => tool_call_stream("subagent_run", &json!({
                                "background": background,
                                "tasks": [{"description": "受控子任务", "prompt": "读 a.py", "subagent_type": "review"}],
                            }).to_string()),
                            1 => tool_call_stream("read", "{\"path\":\"parent.txt\"}"),
                            _ => text_stream(PARENT_TEXT),
                        }
                    };
                    stream.write_all(sse_response(&body).as_bytes()).unwrap();
                });
            }
        });
        Self {
            server: StubServer { addr: format!("http://{addr}/v1"), requests },
            child_started, parent_requests, gate,
        }
    }

    fn release(&self) {
        let (lock, signal) = &*self.gate;
        *lock.lock().unwrap() = true;
        signal.notify_all();
    }
}

impl Drop for GatedServer {
    fn drop(&mut self) { self.release(); }
}

#[test]
fn background_returns_before_child_completion_and_remains_queryable() {
    let gated = GatedServer::start(true);
    let (root, workspace, agents, config) = prepare_root("background", "[subagents]\nenabled = true\nallow_background = true\n");
    let mut host = Host::start(&workspace, &agents, &config);
    handshake(&mut host, &gated.server, json!([
        {"type": "function", "function": {"name": "read"}},
        {"type": "function", "function": {"name": "subagent_run"}},
    ]), None);
    let finished = host.run_turn("启动后台任务后继续读文件");
    assert_eq!(finished[0]["params"]["final_text"], PARENT_TEXT);
    gated.child_started.recv_timeout(WAIT).unwrap();
    assert_eq!(gated.parent_requests.load(Ordering::SeqCst), 3);
    assert!(host.batches.iter().any(|(turn, _, _)| turn == "turn-1"));
    let tasks = host.request("subagent.query", json!({"action": "list"}));
    let task_id = tasks["result"]["tasks"][0]["task_id"].as_str().unwrap().to_string();
    assert_eq!(tasks["result"]["tasks"][0]["status"], "running");
    assert!(host.notifications("subagent.event", Some("subagent.task.completed")).is_empty());
    gated.release();
    let deadline = Instant::now() + WAIT;
    loop {
        let task = host.request("subagent.query", json!({"action": "get", "task_id": task_id}));
        if task["result"]["task"]["status"] == "completed" { break; }
        assert!(Instant::now() < deadline, "后台任务应在空闲时完成：{task}");
        thread::sleep(Duration::from_millis(5));
    }
    let tasks = host.request("subagent.query", json!({"action": "list"}));
    assert_eq!(tasks["result"]["tasks"].as_array().unwrap().len(), 1);
    assert_eq!(tasks["result"]["tasks"][0]["result"]["summary"], CHILD_TEXT);
    assert!(host.batches.iter().any(|(turn, _, _)| turn == &task_id));
    host.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn foreground_waits_for_child_but_serves_task_queries() {
    let gated = GatedServer::start(false);
    let (root, workspace, agents, config) = prepare_root("foreground", "[subagents]\nenabled = true\n");
    let mut host = Host::start(&workspace, &agents, &config);
    handshake(&mut host, &gated.server, json!([
        {"type": "function", "function": {"name": "read"}},
        {"type": "function", "function": {"name": "subagent_run"}},
    ]), None);
    let turn_id = host.next_id;
    host.next_id += 1;
    host.send(json!({"jsonrpc": "2.0", "id": turn_id, "method": "turn.submit",
        "params": {"turn_id": "turn-foreground", "user_text": "等子任务完成"}}));
    gated.child_started.recv_timeout(WAIT).unwrap();
    let tasks = host.request("subagent.query", json!({"action": "list"}));
    assert_eq!(tasks["result"]["tasks"][0]["status"], "running");
    assert_eq!(gated.parent_requests.load(Ordering::SeqCst), 1);
    assert!(host.notifications("turn.finished", None).is_empty());
    gated.release();
    let deadline = Instant::now() + WAIT;
    loop {
        let line = host.lines.recv_timeout(WAIT).unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        let done = frame["method"] == "turn.finished";
        host.handle_side_frame(&frame);
        if done { break; }
        assert!(Instant::now() < deadline);
    }
    assert_eq!(gated.parent_requests.load(Ordering::SeqCst), 3);
    let tasks = host.request("subagent.query", json!({"action": "list"}));
    assert_eq!(tasks["result"]["tasks"][0]["status"], "completed");
    host.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn missing_git_uses_local_isolation_and_keeps_the_original_workspace_untouched() {
    let server = StubServer::spawn(vec![
        ok_stream(tool_call_stream("subagent_run", &json!({
            "tasks": [{"description": "无 Git 写入任务", "prompt": "在隔离目录中读文件", "subagent_type": "writer"}],
        }).to_string())),
        ok_stream(tool_call_stream("read", "{\"path\":\"a.txt\"}")),
        ok_stream(text_stream(CHILD_TEXT)),
        ok_stream(text_stream(PARENT_TEXT)),
    ]);
    let (root, workspace, agents, config) = prepare_root("missing-git", "[subagents]\nenabled = true\nallow_worktree = true\nallow_standard_agent = true\n");
    std::fs::write(workspace.join("a.txt"), "original").unwrap();
    std::fs::write(agents.join("writer.md"), "---\nname: writer\ndescription: 本地隔离写入\npermissionMode: standard\nisolation: worktree\ntools:\n  - read\n---\n实现指定任务。\n").unwrap();
    let mut host = Host::start_with_path(&workspace, Some(""));
    handshake(&mut host, &server, json!([
        {"type": "function", "function": {"name": "read"}},
        {"type": "function", "function": {"name": "subagent_run"}},
    ]), None);
    assert!(host.request("subagent.query", json!({"action": "list"}))["result"]["tasks"].as_array().unwrap().is_empty());
    let finished = host.run_turn("无 Git 项目执行隔离任务");
    assert_eq!(finished[0]["params"]["final_text"], PARENT_TEXT);
    let tasks = host.request("subagent.query", json!({"action": "list"}));
    assert_eq!(tasks["result"]["tasks"][0]["status"], "completed");
    let isolation = host.batches.iter().find_map(|(_, _, root)| root.as_ref()).unwrap();
    assert_ne!(PathBuf::from(isolation), workspace);
    assert_eq!(std::fs::read_to_string(PathBuf::from(isolation).join("a.txt")).unwrap(), "original");
    assert!(!PathBuf::from(isolation).join(".git").exists());
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "original");
    let sessions = host.request("subagent.query", json!({"action": "list_worktrees"}));
    assert_eq!(sessions["result"]["worktrees"][0]["isolation"], "local");
    host.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}
