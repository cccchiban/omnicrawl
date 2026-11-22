use super::*;
use super::subagent_tools::{emit_subagent_event, SUBAGENT_OWNER_ID};
use crate::session::protocol::Inbound;
use crate::session::subagent_dispatch::{child_connection, wait_for_tasks, ChildFrame};
use omnicrawl_controllers::subagents::coordinator::PreparedTaskView;
use omnicrawl_controllers::subagents::tasks::{is_terminal, SubAgentTaskSpec, TaskObserver, TaskRunner};

/// 一批任务的执行入口：登记进任务管理器，前台等全部落定，后台立即返回任务编号。
pub(crate) fn execute_tasks(
    runtime: SubAgentRuntime,
    model_config: KernelModelConfig,
    tasks: Vec<PreparedTask>,
    fork_messages: Vec<Value>,
    conn: &Rc<RefCell<Conn>>,
    keep_full_text: bool,
    origin_call_id: &str,
) -> Result<Value, (String, String)> {
    let batch_id = tasks
        .first()
        .map(|task| task.batch_id.clone())
        .unwrap_or_default();
    // 一条命令只走一种运行方式：所有任务都要求后台时才整批后台，混批按前台等待。
    let background = tasks.iter().all(|task| task.background);
    let workspace = conn.borrow().session.as_ref()
        .and_then(|session| session.workspace.as_deref()).map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let sender = conn.borrow().sender.clone();
    let session_id = conn.borrow().session.as_ref()
        .map(|session| session.session_id.clone()).unwrap_or_default();
    let (results_sender, results) = mpsc::channel();
    let retention = runtime.config.task_retention_minutes as f64 * 60.0;
    let workers = runtime.config.model_request_concurrency as usize;

    let prepared: Arc<Vec<PreparedTask>> = Arc::new(tasks);
    let planned = Arc::clone(&prepared);
    let worker_runtime = runtime.clone();
    let worker_model = model_config.clone();
    let worker_fork = fork_messages.clone();
    let worker_workspace = workspace.clone();
    let worker_sender = sender.clone();
    let runner: TaskRunner = Arc::new(move |spec, cancel| {
        let Some(task) = planned
            .iter()
            .find(|task| task.task_id == spec.task_id)
            .cloned()
        else {
            let payload = failure_payload(
                &PreparedTaskView {
                    batch_id: spec.batch_id.clone(),
                    task_id: spec.task_id.clone(),
                    description: spec.description.clone(),
                    agent_type: spec.agent_type.clone(),
                    definition_source: String::new(),
                },
                "SUBAGENT_NOT_FOUND",
                "未找到该子任务的任务定义。",
                None,
            );
            let _ = results_sender.send(payload.clone());
            return payload.as_object().cloned().unwrap_or_default();
        };
        let child = child_connection(worker_sender.clone(), worker_model.clone(), cancel.clone());
        child.borrow_mut().child_cancel = Some(cancel.clone());
        let result = run_task_in_isolation(
            &worker_runtime, &worker_model, &task, &worker_fork, &child,
            cancel, &worker_workspace, keep_full_text, true,
        );
        let _ = results_sender.send(result.clone());
        result.as_object().cloned().unwrap_or_default()
    });

    let remaining = Arc::new(std::sync::atomic::AtomicUsize::new(prepared.len()));
    let observer_view = Arc::clone(&prepared);
    let finished_batch = batch_id.clone();
    let observer: TaskObserver = Arc::new(move |name, payload| {
        let status = payload.get("status").and_then(Value::as_str).unwrap_or("failed").to_string();
        let task_id = payload.get("task_id").and_then(Value::as_str).unwrap_or_default().to_string();
        let view = observer_view
            .iter()
            .find(|task| task.task_id == task_id)
            .map(PreparedTask::view);
        let (event_name, event_payload) = match (status == "running", view) {
            (true, Some(view)) => ("subagent.task.started".to_string(), task_event_payload(&view, "started")),
            _ => (name.to_string(), payload),
        };
        send_event(&sender, &event_name, event_payload);
        if is_terminal(&status) {
            // 收尾事件只在最后一个任务落定时发一次：早发会让宿主的子代理窗口在剩下的
            // 任务还在跑时关掉。
            let left = remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            if left <= 1 {
                send_event(&sender, "subagent.batch.finished", json!({
                    "batch_id": finished_batch, "status": "finished",
                }));
            }
        }
    });

    let specs: Vec<SubAgentTaskSpec> = prepared
        .iter()
        .map(|task| {
            let mut spec = SubAgentTaskSpec::new(
                &task.task_id, &task.description, &task.agent_type, &task.batch_id,
            );
            spec.metadata.insert("background".to_string(), Value::Bool(background));
            spec
        })
        .collect();
    emit_subagent_event(conn, "subagent.batch.created", json!({
        "batch_id": batch_id, "status": "queued", "task_count": specs.len(),
        "origin_call_id": origin_call_id,
    }));
    let queued = {
        let mut connection = conn.borrow_mut();
        let manager = connection.tasks.get_or_insert_with(|| SubAgentTaskManager::new(
            retention, workers,
        ));
        manager.spawn(SUBAGENT_OWNER_ID, &session_id, &specs, runner, Some(observer))
    }.map_err(|error| ("SUBAGENT_MODEL_ERROR".to_string(), error.message().to_string()))?;
    if background {
        return Ok(queued);
    }
    let results = wait_for_tasks(conn, &prepared[..], results);
    Ok(batch_summary(&batch_id, results).1)
}

fn send_event(sender: &Sender<Inbound>, name: &str, payload: Value) {
    let (replies, _) = mpsc::channel();
    let frame = HostEvent::SubagentEvent(SubagentEventPayload {
        name: name.to_string(), payload,
    }).to_frame();
    let _ = sender.send(Inbound::Child(Box::new(ChildFrame {
        frame, replies, cancel: CancelToken::new(),
    })));
}
