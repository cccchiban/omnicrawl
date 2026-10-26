//! 由 `tools.rs` 按职责拆出，语义未变。

use super::*;


/// 内核自持的 `computer_use`：把桌面目标交给专职子代理，按时间收敛并回报结果。
///
/// 与 `subagent` 的三个差异都落在这里：桌面独占（全程持锁）、不设步数上限（靠
/// `timeout_seconds`）、子代理可主动停下（`stop_check` 以 `Ok` 结束而非报错）。
pub(crate) fn run_computer_use(
    conn: &Rc<RefCell<Conn>>,
    arguments: &Map<String, Value>,
    call_id: &str,
) -> (bool, String) {
    let runtime = match crate::computer_use::ComputerUseRuntime::load() {
        Ok(runtime) => runtime,
        Err(detail) => return (false, detail),
    };
    let (prompt, timeout_seconds) = match runtime.read_prompt(arguments) {
        Ok(value) => value,
        Err(detail) => return (false, detail),
    };
    let (model_config, parent_tools) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return (
                false,
                json_result_text(&top_level_error(
                    "COMPUTER_USE_MODEL_ERROR",
                    "内核未持有模型配置，无法执行桌面任务。",
                )),
            );
        };
        let tools = SubAgentRuntime::parent_tool_names(&model);
        (model, tools)
    };
    let tool_names = crate::computer_use::child_tool_names(&parent_tools);
    if tool_names.is_empty() {
        return (
            false,
            json_result_text(&top_level_error(
                "COMPUTER_USE_UNAVAILABLE",
                "当前工具表没有可用的桌面工具，Computer Use 无法执行。",
            )),
        );
    }
    // 桌面独占：排队等前一个任务结束，等待时间同样计入本次的时间预算。
    let started = std::time::Instant::now();
    let _desktop = crate::computer_use::acquire_desktop();
    let remaining = timeout_seconds - started.elapsed().as_secs_f64();
    if remaining <= 1.0 {
        return (
            false,
            json_result_text(&top_level_error(
                "COMPUTER_USE_TIMEOUT",
                "等待桌面独占超时，本次任务未开始。",
            )),
        );
    }

    let batch_id = next_subagent_batch_id();
    let task_id = format!("computer-use-{}", batch_id);
    // 先登记批次与发起它的工具调用：桌面任务不发 `subagent.task.*`，宿主只能靠这条
    // 事件把「查看会话」挂到那张 `computer_use` 卡上（不带它会被当成宿主自发批次直接开窗）。
    emit_subagent_event(
        conn,
        "subagent.batch.created",
        json!({
            "batch_id": batch_id,
            "status": "queued",
            "task_count": 1,
            "origin_call_id": call_id,
        }),
    );
    let messages = vec![runtime.task_message(&prompt)];
    let cancel = CancelToken::new();
    let child_model = child_config(&model_config, &runtime, &tool_names);
    let runtime = std::sync::Arc::new(runtime);

    // 子回合就地跑：工具批次直接借连接执行，模型请求直连，因此不再需要独立线程。
    let outcome = run_computer_use_loop(
        &runtime,
        child_model,
        messages,
        Rc::clone(conn),
        &cancel,
        &task_id,
        &batch_id,
        remaining,
    );
    let cancelled_by_parent = cancel.is_set() && outcome.status == "cancelled";
    let mut screenshots = outcome.screenshots;
    for path in crate::computer_use::collect_screenshots(&outcome.tool_outputs) {
        if !screenshots.iter().any(|item| item == &path) {
            screenshots.push(path);
        }
    }
    let status = if cancelled_by_parent && outcome.status == "cancelled" {
        "cancelled".to_string()
    } else {
        outcome.status.clone()
    };
    let payload = runtime.result_payload(
        &status,
        &outcome.summary,
        &outcome.obstacle,
        &screenshots,
        outcome.model_turns,
        outcome.tool_calls,
    );
    // `timeout` / `blocked` 是可用的业务结果，`ok` 只用来表达「调用本身没出错」。
    let ok = !matches!(status.as_str(), "failed" | "cancelled");
    // 桌面任务不发 `subagent.task.*`：窗口靠收尾事件关闭，否则会一直挂着。
    emit_batch_finished(conn, &batch_id);
    (ok, json_result_text(&payload))
}

/// 跑一个 Computer Use 子回合：与主回合同在一条线程，模型请求直连、工具批次直接借连接。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_computer_use_loop(
    runtime: &crate::computer_use::ComputerUseRuntime,
    config: KernelModelConfig,
    mut messages: Vec<Value>,
    conn: Rc<RefCell<Conn>>,
    cancel: &CancelToken,
    task_id: &str,
    batch_id: &str,
    timeout_seconds: f64,
) -> crate::computer_use::ComputerUseOutcome {
    let mut model = BackgroundModelPort {
        config,
        conn: Rc::clone(&conn),
        cancel: cancel.clone(),
        task_id: task_id.to_string(),
        batch_id: batch_id.to_string(),
        agent_type: crate::computer_use::COMPUTER_USE_ROLE.to_string(),
        stream: true,
        runtime: None,
        model_turns: 0,
    };
    let mut tools = BackgroundTools {
        conn: Rc::clone(&conn),
        turn_id: task_id.to_string(),
        workspace_root: None,
        batch_id: batch_id.to_string(),
        agent_type: crate::computer_use::COMPUTER_USE_ROLE.to_string(),
        stream: true,
        tool_calls: 0,
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));
    let cancel_for_check = cancel.clone();
    let mut cancel_check = move || {
        if cancel_for_check.is_set() {
            Err(LoopError::Cancelled("桌面任务已取消。".to_string()))
        } else {
            Ok(())
        }
    };
    let guards = LoopGuards {
        cancel_check: Some(&mut cancel_check),
        // 子代理判定受阻时以「不再请求工具」自然收尾，回报里带 `status=blocked`；
        // 额外加停止信号会变成一个没人能触发的死钩子。
        stop_check: None,
    };
    let limits = AgentLoopLimits {
        // 不设步数上限：单步耗时差异太大，按时间收敛才是有意义的成本边界。
        max_model_turns: None,
        max_tool_calls: None,
        timeout_seconds: Some(timeout_seconds),
    };
    let outcome = runner.run(&mut messages, &mut model, &mut tools, limits, guards);
    let _ = runtime;
    let tool_outputs = tool_outputs(&messages);
    // 用量取自模型端口，不取自 `AgentLoopResult`：预算超限与取消走的是 `Err` 分支，
    // 那里没有结果结构体，若沿用 0 就会把「确实跑了 N 轮」报成「一轮都没跑」。
    let model_turns = model.model_turns;
    let tool_calls = tools.tool_calls;
    match outcome {
        Ok(result) => {
            let (status, summary, obstacle, screenshots) =
                crate::computer_use::parse_child_report(&result.final_text);
            crate::computer_use::ComputerUseOutcome {
                status,
                summary,
                obstacle,
                screenshots,
                model_turns,
                tool_calls,
                tool_outputs,
            }
        }
        // 超时不是失败：已完成的操作仍然有效，把已有进度交回去比丢掉更有用。
        Err(LoopError::BudgetExceeded(message)) => crate::computer_use::ComputerUseOutcome {
            status: "timeout".to_string(),
            summary: format!("时间预算用尽，任务未完成：{message}"),
            obstacle: String::new(),
            screenshots: Vec::new(),
            model_turns,
            tool_calls,
            tool_outputs,
        },
        Err(LoopError::Cancelled(_)) => crate::computer_use::ComputerUseOutcome {
            status: "cancelled".to_string(),
            summary: "桌面任务已取消。".to_string(),
            obstacle: String::new(),
            screenshots: Vec::new(),
            model_turns,
            tool_calls,
            tool_outputs,
        },
        Err(error) => crate::computer_use::ComputerUseOutcome {
            status: "failed".to_string(),
            summary: format!("桌面任务中止：{}", error.tag()),
            obstacle: String::new(),
            screenshots: Vec::new(),
            model_turns,
            tool_calls,
            tool_outputs,
        },
    }
}

/// 专职子代理的模型配置：换掉系统提示词与模型，工具只留桌面那几件。
pub(crate) fn child_config(
    parent: &KernelModelConfig,
    runtime: &crate::computer_use::ComputerUseRuntime,
    tool_names: &[String],
) -> KernelModelConfig {
    let mut config = parent.clone();
    config.tools = parent
        .tools
        .iter()
        .filter(|item| {
            item.pointer("/function/name")
                .and_then(Value::as_str)
                .map(|name| tool_names.iter().any(|candidate| candidate == name))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    config.system_prompt = runtime.definition.system_prompt.clone();
    // 模型优先级：定义文件 < `[computer_use] model`（显式配置优先于角色模板）。
    let model = runtime
        .config
        .effective_model()
        .map(str::to_string)
        .or_else(|| {
            let from_definition = runtime.definition.model.trim();
            if from_definition.is_empty() || from_definition.eq_ignore_ascii_case("inherit") {
                None
            } else {
                Some(from_definition.to_string())
            }
        });
    if let Some(model) = model {
        config.model = model;
    }
    // 思考强度只覆盖生成选项里的那一项，其余选项沿用主模型通道。
    if !config.options.is_object() {
        // `null` 是「没给 options」的正常形状；非对象则说明宿主给错了，按没给处理。
        config.options = serde_json::json!({});
    }
    if let Some(options) = config.options.as_object_mut() {
        options.insert(
            "reasoning_effort".to_string(),
            Value::String(runtime.config.thinking_effort.clone()),
        );
    }
    config
}

/// 从子回合的消息流里取回工具结果文本：截图路径只能从这里补。
pub(crate) fn tool_outputs(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// 内核自持的 `subagent`：校验 → 准备 → 逐个跑子回合（子模型请求由内核发，子工具批次交宿主）。
///
/// `call_id` 是发起本次批次的工具调用 id：随 `subagent.batch.created` 外发后，宿主才能把
/// 「查看会话」按钮挂到那张工具卡上（没有它的批次属于宿主自发入口，如 `/review`）。
pub(crate) fn run_subagent_batch(
    conn: &Rc<RefCell<Conn>>,
    arguments: &Map<String, Value>,
    call_id: &str,
) -> (bool, String) {
    let runtime = match SubAgentRuntime::load() {
        Ok(runtime) => runtime,
        Err(detail) => {
            return (
                false,
                json_result_text(&top_level_error("SUBAGENT_DISABLED", &detail)),
            )
        }
    };
    if !runtime.enabled() {
        return (false, runtime.disabled_text());
    }
    // 后台任务查询（list / get / cancel）由内核处理：任务表在内核里。
    if matches!(
        arguments.get("action").and_then(Value::as_str),
        Some("list") | Some("get") | Some("cancel")
    ) {
        return run_subagent_query(conn, arguments);
    }
    // worktree 控制动作（列 / 应用 / 丢弃）由内核处理：会话信息落在托管根的元数据里。
    if matches!(
        arguments.get("action").and_then(Value::as_str),
        Some("list_worktrees") | Some("apply_worktree") | Some("discard_worktree")
    ) {
        return run_worktree_control(arguments);
    }
    let (model_config, fork_messages) = {
        let borrowed = conn.borrow();
        let Some(model) = borrowed.model.clone() else {
            return (
                false,
                json_result_text(&top_level_error(
                    "SUBAGENT_MODEL_ERROR",
                    "内核未持有模型配置，无法执行子任务。",
                )),
            );
        };
        let history = borrowed
            .session
            .as_ref()
            .map(|session| session.history.clone())
            .unwrap_or_default();
        (model, history)
    };
    let parent_tools = SubAgentRuntime::parent_tool_names(&model_config);
    let batch_id = next_subagent_batch_id();
    let task = match runtime.prepare(arguments, &parent_tools, &batch_id) {
        Ok(task) => task,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };

    emit_subagent_event(
        conn,
        "subagent.batch.created",
        json!({
            "batch_id": batch_id,
            "status": "queued",
            "task_count": 1,
            "origin_call_id": call_id,
        }),
    );
    let result = run_task_in_isolation(
        &runtime,
        &model_config,
        &task,
        &fork_messages,
        conn,
        &CancelToken::new(),
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        false,
        true,
    );
    let (ok, summary) = batch_summary(&batch_id, vec![result]);
    emit_batch_finished(conn, &batch_id);
    (ok, json_result_text(&summary))
}

/// 内核侧的 SubAgent 所有者标识（协议一条连接只服务一个宿主）。
pub(crate) const SUBAGENT_OWNER_ID: &str = "kernel";

/// `action=list|get|cancel`：查询与取消后台任务。
pub(crate) fn run_subagent_query(conn: &Rc<RefCell<Conn>>, arguments: &Map<String, Value>) -> (bool, String) {
    let request = match query_request(&Value::Object(arguments.clone())) {
        Ok(request) => request,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };
    let borrowed = conn.borrow();
    let Some(manager) = borrowed.tasks.as_ref() else {
        return (
            false,
            json_result_text(&top_level_error(
                "SUBAGENT_NOT_FOUND",
                "当前还没有后台 SubAgent 任务。",
            )),
        );
    };
    let session_id = borrowed
        .session
        .as_ref()
        .map(|session| session.session_id.clone())
        .unwrap_or_default();
    match request {
        QueryRequest::List => {
            let tasks = manager.list(SUBAGENT_OWNER_ID, Some(&session_id));
            let notifications = manager.drain_notifications(SUBAGENT_OWNER_ID, Some(&session_id));
            (
                true,
                json_result_text(&json!({
                    "tasks": tasks,
                    "notifications": notifications,
                })),
            )
        }
        QueryRequest::Get { task_id } => {
            match manager.get(&task_id, SUBAGENT_OWNER_ID, Some(&session_id)) {
                Some(task) => (true, json_result_text(&json!({"task": task}))),
                None => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        "未找到当前 Agent 的任务。",
                    )),
                ),
            }
        }
        QueryRequest::Cancel { task_id, batch_id } => {
            let value = manager.cancel(
                SUBAGENT_OWNER_ID,
                Some(&session_id),
                (!task_id.is_empty()).then_some(task_id.as_str()),
                (!batch_id.is_empty()).then_some(batch_id.as_str()),
            );
            (true, json_result_text(&value))
        }
    }
}

/// 一个子任务从隔离区到公开结果的全过程：建工作树 → 跑子回合 → 收集产物 → 发事件。
///
/// `keep_full_text` 为真时把子 Agent 的收尾文本原样挂进结果的 `full_text`：
/// `/review` 这类宿主入口要自己解析结构化 JSON 报告，而公开的 `summary` 会按
/// `result_summary_chars` 截断，截断会直接把 JSON 打碎（与 Python `keep_full_text=True` 同义）。
///
/// `stream_conversation` 为真时子回合的过程文本与工具调用另发 `subagent.turn.text` /
/// `subagent.tool.*` 事件：宿主的子代理窗口靠这条流渲染，与「要不要完整结果」无关。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_task_in_isolation(
    runtime: &SubAgentRuntime,
    model_config: &KernelModelConfig,
    task: &PreparedTask,
    fork_messages: &[Value],
    conn: &Rc<RefCell<Conn>>,
    cancel: &CancelToken,
    workspace: &std::path::Path,
    keep_full_text: bool,
    stream_conversation: bool,
) -> Value {
    let view = task.view();
    emit_subagent_event(
        conn,
        "subagent.task.started",
        task_event_payload(&view, "started"),
    );
    let session = if task.definition.isolation == "worktree" {
        match crate::worktree::create_session(workspace, &task.task_id, "HEAD", None) {
            Ok(session) => Some(session),
            Err(error) => {
                let result =
                    failure_payload(&view, "SUBAGENT_WORKTREE_ERROR", error.message(), None);
                emit_subagent_event(
                    conn,
                    &terminal_event_name(&result),
                    terminal_event_payload(&view, &result),
                );
                return result;
            }
        }
    } else {
        None
    };
    let root = session
        .as_ref()
        .map(|session| session.worktree_path.to_string_lossy().to_string());
    let result = match run_background_task(
        runtime,
        model_config,
        task,
        fork_messages,
        conn,
        cancel,
        root.as_deref(),
        stream_conversation,
    ) {
        Ok(execution) => {
            let mut result = runtime.completed_result(task, &execution);
            if keep_full_text {
                if let Some(object) = result.as_object_mut() {
                    object.insert("full_text".to_string(), Value::String(execution.final_text));
                }
            }
            result
        }
        Err((code, message)) => failure_payload(&view, &code, &message, None),
    };
    let result = match session.as_ref().map(crate::worktree::collect_artifacts) {
        Some(Ok(artifacts)) => attach_worktree_artifacts(result, &artifacts),
        Some(Err(error)) => attach_worktree_note(
            result,
            &format!("worktree 产物收集失败：{}", error.message()),
        ),
        None => result,
    };
    emit_subagent_event(
        conn,
        &terminal_event_name(&result),
        terminal_event_payload(&view, &result),
    );
    result
}

pub(crate) fn terminal_event_name(result: &Value) -> String {
    let status = result
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed");
    format!("subagent.task.{status}")
}

/// 把 worktree 产物摘要挂到子任务结果的 `artifacts` 上（与 Python 的 `[worktree]` 标记同义）。
pub(crate) fn attach_worktree_artifacts(mut result: Value, artifacts: &WorktreeArtifacts) -> Value {
    let lines = artifact_lines(Some(artifacts));
    if lines.is_empty() {
        return result;
    }
    if let Some(object) = result.as_object_mut() {
        let entry = json!({"type": "worktree", "content": lines.join("\n")});
        match object.get_mut("artifacts").and_then(Value::as_array_mut) {
            Some(items) => items.push(entry),
            None => {
                object.insert("artifacts".to_string(), Value::Array(vec![entry]));
            }
        }
    }
    result
}

/// 产物收集失败时留下可诊断的说明，不改动任务状态。
pub(crate) fn attach_worktree_note(mut result: Value, note: &str) -> Value {
    if let Some(object) = result.as_object_mut() {
        object.insert("worktree_note".to_string(), Value::String(note.to_string()));
    }
    result
}

/// 处理 `list_worktrees` / `apply_worktree` / `discard_worktree`。
pub(crate) fn run_worktree_control(arguments: &Map<String, Value>) -> (bool, String) {
    let request = match worktree_control_request(&Value::Object(arguments.clone())) {
        Ok(request) => request,
        Err((code, message)) => {
            return (false, json_result_text(&top_level_error(&code, &message)))
        }
    };
    match request {
        WorktreeControlRequest::List => {
            let sessions: Vec<Value> = crate::worktree::list_sessions(None)
                .iter()
                .map(crate::worktree::session_value)
                .collect();
            (true, json_result_text(&json!({"worktrees": sessions})))
        }
        WorktreeControlRequest::Apply {
            key,
            strategy,
            cleanup,
        } => {
            let Some(session) = crate::worktree::find_session(&key, None) else {
                return (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        &format!("未找到 SubAgent worktree 会话：{key}"),
                    )),
                );
            };
            match crate::worktree::apply_to_main(&session, &strategy) {
                Ok(message) => {
                    if cleanup {
                        let _ = crate::worktree::cleanup_session(&session, true);
                    }
                    (
                        true,
                        json_result_text(&json!({
                            "action": "apply_worktree",
                            "key": key,
                            "strategy": strategy,
                            "cleanup": cleanup,
                            "message": message,
                        })),
                    )
                }
                Err(error) => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_MODEL_ERROR",
                        &format!("应用 worktree 失败：{}", error.message()),
                    )),
                ),
            }
        }
        WorktreeControlRequest::Discard {
            key,
            remove_branch,
            force,
        } => {
            let Some(session) = crate::worktree::find_session(&key, None) else {
                return (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_NOT_FOUND",
                        &format!("未找到 SubAgent worktree 会话：{key}"),
                    )),
                );
            };
            if !force {
                match crate::worktree::summarize_changes(&session) {
                    Ok(Some(changes)) => {
                        if let Some(reason) = discard_guard(changes) {
                            return (
                                false,
                                json_result_text(&top_level_error(
                                    "SUBAGENT_PERMISSION_DENIED",
                                    &reason,
                                )),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return (
                            false,
                            json_result_text(&top_level_error(
                                "SUBAGENT_MODEL_ERROR",
                                &format!(
                                    "worktree 变更检查失败（可传 force=true 强制丢弃）：{}",
                                    error.message()
                                ),
                            )),
                        )
                    }
                }
            }
            match crate::worktree::cleanup_session(&session, remove_branch) {
                Ok(()) => (
                    true,
                    json_result_text(&json!({
                        "action": "discard_worktree",
                        "key": key,
                        "remove_branch": remove_branch,
                        "force": force,
                        "message": format!("已清理 worktree 会话：{}", session.branch_name),
                    })),
                ),
                Err(error) => (
                    false,
                    json_result_text(&top_level_error(
                        "SUBAGENT_MODEL_ERROR",
                        &format!("清理 worktree 失败：{}", error.message()),
                    )),
                ),
            }
        }
    }
}

/// 同步路径的子代理事件：直接经连接外发。
pub(crate) fn emit_subagent_event(conn: &Rc<RefCell<Conn>>, name: &str, payload: Value) {
    conn.borrow_mut()
        .notify(HostEvent::SubagentEvent(SubagentEventPayload {
            name: name.to_string(),
            payload,
        }));
}

/// 批次收尾事件：所有子任务都已落定，宿主的子代理窗口收到后自动关闭。
///
/// 只在批次真的跑完之后发一次；`computer_use` 这类不发 `subagent.task.*` 的入口
/// 也靠它关窗，否则窗口会一直停在「已完成」的状态上等用户手动关。
pub(crate) fn emit_batch_finished(conn: &Rc<RefCell<Conn>>, batch_id: &str) {
    emit_subagent_event(
        conn,
        "subagent.batch.finished",
        json!({ "batch_id": batch_id, "status": "finished" }),
    );
}

/// 批次号：12 位十六进制，时间戳与自增计数混合，避免同进程内重复。
pub(crate) fn next_subagent_batch_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let tick = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mixed = nanos ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("batch-{:012x}", mixed & 0xffff_ffff_ffff)
}

/// `subagent.query`：后台任务的查询与取消，返回结构化 JSON（不再走工具结果信封）。
///
/// 没有任务管理器时回 `unavailable: true`，让宿主区分「功能未启用」（503）与
/// 「任务不存在」（404）；动作非法按协议错误回。
pub(crate) fn subagent_query_value(
    conn: &Rc<RefCell<Conn>>,
    params: &SubagentQueryParams,
) -> Result<Value, String> {
    let action = params.action.trim().to_lowercase();
    // worktree 清单放在任务表检查之前：托管根里的残留 worktree 与后台任务管理器是否启用无关，
    // 切换工作区前必须能看到它们（对映 Python `list_subagent_worktrees` 的拦阻项）。
    if action == "list_worktrees" {
        let sessions: Vec<Value> = crate::worktree::list_sessions(None)
            .iter()
            .map(crate::worktree::session_value)
            .collect();
        return Ok(json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": {},
            "worktrees": sessions,
        }));
    }
    if !matches!(action.as_str(), "list" | "get" | "cancel") {
        return Err(format!(
            "action 仅支持 list、get、cancel 或 list_worktrees，收到：{}",
            params.action
        ));
    }
    let borrowed = conn.borrow();
    let Some(manager) = borrowed.tasks.as_ref() else {
        return Ok(json!({
            "unavailable": true,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": {},
        }));
    };
    let session_id = borrowed
        .session
        .as_ref()
        .map(|session| session.session_id.clone())
        .unwrap_or_default();
    let task_id = params.task_id.trim();

    Ok(match action.as_str() {
        "list" => json!({
            "unavailable": false,
            "action": action,
            "tasks": manager.list(SUBAGENT_OWNER_ID, Some(&session_id)),
            "task": Value::Null,
            "result": {},
        }),
        "get" => json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": manager
                .get(task_id, SUBAGENT_OWNER_ID, Some(&session_id))
                .unwrap_or(Value::Null),
            "result": {},
        }),
        _ => json!({
            "unavailable": false,
            "action": action,
            "tasks": [],
            "task": Value::Null,
            "result": manager.cancel(
                SUBAGENT_OWNER_ID,
                Some(&session_id),
                (!task_id.is_empty()).then_some(task_id),
                None,
            ),
        }),
    })
}

/// 失败结果里的可判定代码（`data.kind` 用它分支）；缺失时给一个通用值。
pub(crate) fn subagent_failure_code(summary: &Value) -> String {
    summary
        .pointer("/results/0/error/code")
        .or_else(|| summary.pointer("/error/code"))
        .and_then(Value::as_str)
        .unwrap_or("SUBAGENT_TASK_FAILED")
        .to_string()
}

