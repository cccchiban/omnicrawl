//! 由 `turn.rs` 按职责拆出，语义未变。

use super::*;

/// 读取 `[run_guard].continuation`：与其它内核旁路配置同一套解析
/// （显式路径 > 用户目录）。
///
/// 总开关或续跑开关关闭、配置读不出来都返回 `None`：调用点因此不必区分「没配置」与「关掉了」。
pub(crate) fn load_continue_config() -> Option<ContinueConfig> {
    let environment = omnicrawl_config::core::runtime::ConfigEnvironment::from_process();
    let config =
        omnicrawl_config::features::run_guard::load_run_guard_config(&environment, None).ok()?;
    ContinueConfig::resolve(
        config.enabled,
        config.continuation.enabled,
        config.continuation.max_auto_followups,
    )
}

/// 会话投影里的清单条目 → 判定用的清单项（形状不符的条目直接跳过）。
pub(crate) fn todo_item_from_value(value: &Value) -> Option<TodoItem> {
    let object = value.as_object()?;
    let step = object
        .get("step")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if step.is_empty() {
        return None;
    }
    Some(TodoItem {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        step,
        completed: object
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// 本轮用户消息的模型侧形状：没有图片时是纯文本，有图片时是 OpenAI 多模态部件数组。
///
/// 把 `TurnImageAttachment` 转成事件载荷那份 JSON 形状后交给
/// [`omnicrawl_session::user_message_with_images`]——实时请求与重启后的历史重建共用同一句，
/// 两边的消息形状因此始终一致（否则前缀缓存会在恢复后失效）。
pub(crate) fn user_message_value(text: &str, images: &[omnicrawl_ipc::bridge::TurnImageAttachment]) -> Value {
    if images.is_empty() {
        return user_message_with_images(text, None);
    }
    let payload = Value::Array(
        images
            .iter()
            .map(|image| {
                json!({
                    "media_type": image.media_type,
                    "data_base64": image.data_base64,
                    "detail": image.detail,
                })
            })
            .collect(),
    );
    user_message_with_images(text, Some(&payload))
}

/// 跑一个回合：用户输入进上下文 → 模型与工具交替 → `turn.finished`。
pub(crate) fn run_turn(conn: &Rc<RefCell<Conn>>, request_id: Id, params: TurnSubmitParams) {
    let turn_id = params.turn_id.clone();
    conn.borrow().cancel.store(false, Ordering::SeqCst);

    let user_text = params.user_text.clone();
    // 随本轮提问粘贴的图片（`Ctrl+V`）：既是用户消息的一部分（落盘转录），
    // 也要进这一轮的请求。原生视觉没开时宿主根本不会送来（那边已拦下并提示）。
    let user_images = params.images.clone();
    let usage = Rc::clone(&conn.borrow().usage);
    usage.borrow_mut().reset();
    // 上一轮可能被取消/出错而没走到收尾：标注账本每回合清零，绝不跨回合累积。
    conn.borrow().evict_marks.borrow_mut().clear();
    // 短「继续/重试」要还原成上一轮未完成的真实任务：待续文本与清单从会话推测（只有真是
    // 「继续」才读一次转录），之后的持久化与模型上下文都用还原后的文本——与 Python 的
    // `_resolve_continue_request` 同一时机（早于 user_message 落盘与请求组装）。
    let continue_requested = is_continue_last_task_request(&user_text);
    let (recovered_pending, recovered_todos) = if continue_requested {
        conn.borrow()
            .session
            .as_ref()
            .map(|session| session.run_guard_state())
            .unwrap_or_default()
    } else {
        (String::new(), Vec::new())
    };
    let model_text = resolve_continue_request(&user_text, Some(&recovered_pending));
    let model_config = conn.borrow().model.clone();
    // system 之外的上下文消息（项目规范、Skill、工具能力说明、运行环境）由宿主整段给来，
    // 每轮插在历史之前：这些是「本轮读到的真实环境」，不能随会话落盘（Python 侧同语义）。
    let context_messages = model_config
        .as_ref()
        .map(|config| config.context_messages.clone())
        .unwrap_or_default();
    // 有会话就从转录恢复的运行期历史接着走；没有会话维持「一回合一条 user 消息」的旧行为。
    let mut messages = conn
        .borrow()
        .session
        .as_ref()
        .map(|session| session.history.clone())
        .unwrap_or_default();
    messages.push(user_message_value(&model_text, &user_images));
    // 宿主给了模型配置就由内核自己发请求；没给则维持 model.reply 代答的兼容路径。
    if !context_messages.is_empty() {
        let mut combined: Vec<Value> = Vec::with_capacity(context_messages.len() + messages.len());
        combined.extend(context_messages.iter().cloned());
        combined.extend(messages);
        messages = combined;
    }
    // 本批 assistant 原文：模型端口写、工具端口读，两个端口共用同一个格子。
    let active_assistant: ActiveAssistantSlot = Rc::new(RefCell::new(None));
    let mut model: Box<dyn ReplySource> = match model_config.clone() {
        Some(config) => Box::new(KernelModelPort {
            conn: Rc::clone(conn),
            config,
            usage: Rc::clone(&usage),
            active_assistant: Rc::clone(&active_assistant),
            runtime: RefCell::new(None),
            request: RefCell::new(None),
        }),
        None => Box::new(RemoteModelPort {
            conn: Rc::clone(conn),
            turn_id: turn_id.clone(),
            active_assistant: Rc::clone(&active_assistant),
        }),
    };
    // 本轮 undo 账本：工作区来自 initialize 的 session 配置，非 Git 仓库时自动不记账。
    let undo_workspace = {
        let connection = conn.borrow();
        connection
            .session
            .as_ref()
            .and_then(|session| session.workspace.clone())
    };
    let undo = Rc::new(RefCell::new(crate::undo::TurnUndo::begin(
        undo_workspace.as_deref(),
    )));
    // 本轮执行清单：`update_todos` 的投影由工具批次线程写进来，续跑判定与 `run_guard_*`
    // 事件载荷读同一份——对应 Python 的 `_active_todo_items`。用户发「继续」时，先把
    // 会话投影里的清单当起点（Python 的 `source_todos = session_todos or previous_todo_items`）。
    let active_todos: Rc<RefCell<Vec<TodoItem>>> = Rc::new(RefCell::new(Vec::new()));
    if continue_requested {
        active_todos
            .borrow_mut()
            .extend(recovered_todos.iter().filter_map(todo_item_from_value));
    }
    let mut tools = RemoteTools {
        conn: Rc::clone(conn),
        turn_id: turn_id.clone(),
        workspace_root: None,
        vision_model: model_config.clone(),
        undo: Some(Rc::clone(&undo)),
        todos: Some(Rc::clone(&active_todos)),
        active_assistant: Some(active_assistant),
        task_hint: model_text.clone(),
        previous_batch: Vec::new(),
        previous_batch_event_ids: Vec::new(),
    };
    let build_cancel_check = || {
        let cancel_source = Rc::clone(conn);
        move || {
            if cancel_source.borrow().cancel.load(Ordering::SeqCst) {
                Err(LoopError::Cancelled("回合已取消。".to_string()))
            } else {
                Ok(())
            }
        }
    };
    let runner = AgentLoopRunner::new(Box::new(SystemClock::new()));

    // run_guard 续跑：一段循环跑完后，模型只给 reasoning（没有文本与工具调用）或执行清单
    // 仍有未完成项时自动补发一轮「继续」，上限是 `[run_guard].continuation.max_auto_followups`；
    // 判定面在 `omnicrawl_controllers::turn::continuation`，这里只做模型调用与事件累积。
    let continuation = load_continue_config();
    let mut run_guard = TurnRunGuard::begin(&model_text, continue_requested, &recovered_pending);
    // 用户消息**先落盘**再发请求（与 Python `loop.py` 同序）：回合中途失败或被取消时，
    // 下一轮与恢复投影仍然看得到这次提问，而不是把整轮丢在转录之外。
    run_guard.todos = active_todos.borrow().clone();
    if let Some(session) = conn.borrow_mut().session.as_mut() {
        let payload = run_guard.user_message_payload(&user_images);
        if let Err(detail) = session.append("user_message", payload) {
            eprintln!("[kernel] 会话写入用户消息失败：{detail}");
        }
    }

    // 主 Agent 的预算是无限的：与 Python 侧一致，靠取消与停止检查来收敛。
    // 上下文超限时压缩当前未完成回合，用返回的投影重试一次（与 Python 的恢复路径同口径）。
    let outcome = 'episodes: loop {
        let mut recovered = false;
        let attempt = loop {
            let mut cancel_check = build_cancel_check();
            let guards = LoopGuards {
                cancel_check: Some(&mut cancel_check),
                stop_check: None,
            };
            let attempt = runner.run(
                &mut messages,
                &mut *model,
                &mut tools,
                AgentLoopLimits::default(),
                guards,
            );
            match attempt {
                Err(LoopError::ReplySource(message))
                    if !recovered
                        && (usage.borrow().context_overflow || is_context_overflow(&message)) =>
                {
                    recovered = true;
                    let recovered_history = model_config
                        .clone()
                        .and_then(|config| recover_context_overflow(conn, &messages, &config));
                    match recovered_history {
                        Some(projection) => {
                            // 压缩投影只覆盖历史；上下文消息每轮照旧插在前面。
                            let mut combined: Vec<Value> =
                                Vec::with_capacity(context_messages.len() + projection.len());
                            combined.extend(context_messages.iter().cloned());
                            combined.extend(projection);
                            messages = combined;
                            continue;
                        }
                        None => break Err(LoopError::ReplySource(message)),
                    }
                }
                other => break other,
            }
        };
        let result = match attempt {
            Ok(result) => result,
            Err(error) => break 'episodes Err(error),
        };
        let last_reply = result.last_reply.as_ref().map(|reply| {
            LastReplyFacts::new(&reply.reasoning, &reply.content, reply.tool_calls.len())
        });
        run_guard.record_episode(&result);
        run_guard.paused = result.paused;
        let todos = active_todos.borrow().clone();
        match continuation::next_step(
            continuation.as_ref(),
            run_guard.followups,
            result.paused,
            last_reply.as_ref(),
            &todos,
        ) {
            ContinueStep::Stop => {
                // 上限用尽仍未完成：落可恢复终态，事件保留待续文本（用户可显式发「继续」）。
                run_guard.exhausted = continuation::continuation_exhausted(
                    continuation.as_ref(),
                    run_guard.followups,
                    result.paused,
                    last_reply.as_ref(),
                    &todos,
                );
                run_guard.todos = todos;
                break 'episodes Ok(result);
            }
            ContinueStep::Continue { followup, reason } => {
                run_guard.record_continue(followup, reason);
                conn.borrow_mut().notify(HostEvent::Status(MessagePayload {
                    message: continuation::continue_status_text(followup),
                }));
                // 上一段循环的最终回复要进上下文（它不在 `messages` 里），随后才是补发指令。
                if let Some(reply) = result.last_reply.as_ref() {
                    messages.push(reply.message.clone());
                }
                messages.push(json!({
                    "role": "user",
                    "content": continuation::CONTINUE_PROMPT,
                }));
            }
        }
    };

    let mut connection = conn.borrow_mut();
    match outcome {
        Ok(result) => {
            // 续跑各段的回复按 `"\n\n"` 合并后去掉首尾空白（与 Python 的 `final_reply` 同一算法）。
            let final_text = run_guard.final_reply();
            let final_reasoning = run_guard.final_reasoning();
            // 先落盘再通知：宿主收到 turn.finished 时转录必须已经持久化，
            // 否则宿主此刻退出（或被强杀）就会丢掉这一轮的问答。
            let session = connection.session.take();
            drop(connection);
            // 压缩后大小由 `run_session_tail` 带回：压缩发生在回合收尾，之后不会再发模型请求，
            // 遥测若不在这里刷新就会一直停在压缩前那次的用量上。
            let mut post_compaction_context_tokens = None;
            if let Some(mut session) = session {
                post_compaction_context_tokens = run_session_tail(
                    conn,
                    &mut session,
                    &run_guard,
                    &mut messages,
                    &final_text,
                    &user_text,
                    &turn_id,
                );
                conn.borrow_mut().session = Some(session);
                // 快照事件跟在会话事件之后：`/undo` 的副作用回滚依赖它还原工作区。
                if let Some(session) = conn.borrow().session.as_ref() {
                    let store = Arc::clone(&session.store);
                    let session_id = session.session_id.clone();
                    if let Err(detail) = undo.borrow_mut().complete(&store, &session_id) {
                        eprintln!("[kernel] 本轮快照落盘失败：{detail}");
                    }
                }
            }
            conn.borrow_mut()
                .notify(HostEvent::TurnFinished(TurnFinishedPayload {
                    turn_id: turn_id.clone(),
                    final_text,
                    reasoning: final_reasoning,
                    model_turns: result.model_turns,
                    tool_calls: result.tool_calls,
                    paused: result.paused,
                    post_compaction_context_tokens,
                }));
            conn.borrow_mut().respond(request_id, json!({}));
        }
        Err(error) => {
            eprintln!("[kernel] 回合失败：{}", error.message());
            // 失败/取消同样要留下可恢复的终态（Python `loop.py` 的两个 except 分支）：
            // 用户消息此前已落盘，这里补一条终态事件并把投影写回运行期历史，否则紧接着的
            // 下一次提问看不到被中断的任务与已执行的工具。
            let cancelled = matches!(error, LoopError::Cancelled(_));
            let executed = undo.borrow().executed_tools().to_vec();
            let reason = error.message();
            let response = turn_error(&error);
            let session = connection.session.take();
            drop(connection);
            if let Some(mut session) = session {
                let (event_type, payload) = if cancelled {
                    (
                        "turn_cancelled",
                        json!({
                            "user_text": user_text.clone(),
                            "pending_user_text": run_guard.pending_user_text.clone(),
                            "reason": reason,
                            "summary": cancelled_turn_summary(&executed),
                        }),
                    )
                } else {
                    (
                        "session_interrupted",
                        json!({
                            "user_text": user_text.clone(),
                            "pending_user_text": run_guard.pending_user_text.clone(),
                            "reason": reason,
                        }),
                    )
                };
                if let Err(detail) = session.append(event_type, payload) {
                    eprintln!("[kernel] 会话写入回合终态失败：{detail}");
                }
                // 与 Python `_commit_turn_history()` 同义：把本轮已落盘的事件投影回运行期
                // 历史，下一轮接着走时上下文完整。
                if let Err(detail) = session.reload_history() {
                    eprintln!("[kernel] 重建运行期历史失败：{detail}");
                }
                conn.borrow_mut().session = Some(session);
            }
            conn.borrow_mut().respond_error(request_id, response);
        }
    }
}

/// 循环错误 → 协议错误对象；`data.kind` 用 `LoopError::tag()`，宿主可据此分支。
pub(crate) fn turn_error(error: &LoopError) -> ErrorObject {
    let code = match error {
        LoopError::Cancelled(_) => error_code::TURN_CANCELLED,
        LoopError::InvalidBudget(_) => error_code::INVALID_PARAMS,
        _ => error_code::TURN_FAILED,
    };
    ErrorObject::with_data(code, error.message(), json!({"kind": error.tag()}))
}

/// 回合进行中（内核自己发模型请求）抽干入站帧：让 `turn.cancel` / `shutdown` 立刻生效。
///
/// 返回 `true` 表示应当中止当前请求（已取消、已请求退出或宿主已断开）。其余帧交给
/// [`Conn::handle_inbound`] 正常应答（重复 `turn.submit` 仍会收到 `-32002`）。
pub(crate) fn drain_inbound_during_turn(conn: &Rc<RefCell<Conn>>) -> bool {
    loop {
        let frame = match conn.borrow().inbound.try_recv() {
            Ok(Inbound::Frame(frame)) => frame,
            Ok(Inbound::Closed) => return true,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => return true,
        };
        let failure = conn.borrow_mut().handle_inbound(&frame);
        if matches!(
            failure,
            Some(PortFailure::Cancelled(_)) | Some(PortFailure::Shutdown)
        ) {
            return true;
        }
        let connection = conn.borrow();
        if connection.cancel.load(Ordering::SeqCst) || connection.exit_requested {
            return true;
        }
    }
    let connection = conn.borrow();
    connection.cancel.load(Ordering::SeqCst) || connection.exit_requested
}


