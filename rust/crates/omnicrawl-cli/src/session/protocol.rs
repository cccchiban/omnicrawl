//! 由 `session.rs` 按职责拆出，语义未变。

use super::*;
use super::model_port::{TurnUsage};
use super::session_query::{SessionQuery, no_session_error, respond_compact, respond_session_append, respond_session_archive, respond_session_delete, respond_session_new, respond_session_query, respond_session_rename, respond_session_resume, respond_undo};
use super::subagent_run::{respond_subagent_run};
use super::tools::{subagent_query_value};
use super::turn::{Conn, run_turn};

/// 入站消息：解析好的帧，或读取端已关闭。
///
/// 帧走箱装指针：`Frame` 装着 JSON 载荷，直接内联会让每次通道投递都搬几百字节，
/// 也会让枚举尺寸被最大的变体拖大。
pub(super) enum Inbound {
    Frame(Box<Frame>),
    Closed,
}

/// 端口调用失败；区分宿主主动取消与远端错误，才能映射成不同的 `LoopError`。
pub(super) enum PortFailure {
    Cancelled(String),
    Shutdown,
    Disconnected,
    Remote(ErrorObject),
    Io(String),
}

impl PortFailure {
    pub(super) fn into_reply_error(self) -> LoopError {
        match self {
            Self::Cancelled(message) => LoopError::Cancelled(message),
            Self::Shutdown => LoopError::Cancelled("收到 shutdown，回合中止。".to_string()),
            Self::Disconnected => LoopError::ReplySource("宿主连接已关闭。".to_string()),
            Self::Remote(error) => {
                LoopError::ReplySource(format!("宿主代答模型回复失败：{}", error.message))
            }
            Self::Io(detail) => LoopError::ReplySource(format!("写请求失败：{detail}")),
        }
    }

    pub(super) fn into_tool_error(self) -> LoopError {
        match self {
            Self::Cancelled(message) => LoopError::Cancelled(message),
            Self::Shutdown => LoopError::Cancelled("收到 shutdown，回合中止。".to_string()),
            Self::Disconnected => LoopError::ToolBatch("宿主连接已关闭。".to_string()),
            Self::Remote(error) => {
                LoopError::ToolBatch(format!("宿主执行工具批次失败：{}", error.message))
            }
            Self::Io(detail) => LoopError::ToolBatch(format!("写请求失败：{detail}")),
        }
    }
}

/// 处理一条空闲状态下的入站帧；返回 `true` 表示会话应当结束。
pub(super) fn dispatch(conn: &Rc<RefCell<Conn>>, frame: Frame) -> bool {
    let Some(id) = frame.id().cloned() else {
        eprintln!(
            "[kernel] 忽略宿主的通知：{}",
            frame.method().unwrap_or("<无方法>")
        );
        return false;
    };
    if frame.method().is_none() {
        eprintln!("[kernel] 忽略没有 method 的响应帧。");
        return false;
    }
    if !conn.borrow().handshaken && frame.method() != Some(method::INITIALIZE) {
        conn.borrow_mut().respond_error(
            id,
            ErrorObject::new(error_code::INVALID_REQUEST, "未完成 initialize 握手。"),
        );
        return false;
    }

    let pending = match Command::from_frame(&frame) {
        Ok(pending) => pending,
        Err(failure) => {
            let error = match failure {
                BridgeError::UnknownMethod(name) => {
                    ErrorObject::new(error_code::METHOD_NOT_FOUND, format!("未知方法 {name}。"))
                }
                other => ErrorObject::new(error_code::INVALID_PARAMS, other.to_string()),
            };
            conn.borrow_mut().respond_error(id, error);
            return false;
        }
    };

    let (id, command) = (pending.id, pending.command);
    match command {
        Command::Initialize(InitializeParams {
            protocol_version,
            model,
            session,
            tool_call_prune,
            ..
        }) => {
            if let Some(config) = model {
                conn.borrow_mut().model = Some(*config);
            }
            conn.borrow_mut().tool_call_prune = tool_call_prune;
            if let Some(settings) = session {
                match KernelSession::open(*settings) {
                    Ok(opened) => {
                        eprintln!("[kernel] 会话已就绪：{}", opened.session_id);
                        conn.borrow_mut().session = Some(opened);
                    }
                    Err(detail) => {
                        conn.borrow_mut().respond_error(
                            id,
                            ErrorObject::new(
                                error_code::INVALID_PARAMS,
                                format!("会话初始化失败：{detail}"),
                            ),
                        );
                        return false;
                    }
                }
            }
            conn.borrow_mut().handshake(id, &protocol_version);
            false
        }
        Command::TurnSubmit(params) => {
            run_turn(conn, id, params);
            conn.borrow().exit_requested
        }
        Command::TurnCancel(TurnCancelParams { .. }) => {
            conn.borrow_mut().respond(id, json!({}));
            false
        }
        Command::TurnUndo => {
            respond_undo(conn, id);
            false
        }
        Command::SessionSettings(params) => {
            // 借用必须在同一条语句里结束：respond 还要可变借用同一条连接。
            let outcome = conn.borrow_mut().apply_settings(&params);
            match outcome {
                Ok(applied) => {
                    let result = crate::settings::applied_result(&applied);
                    conn.borrow_mut().respond(id, result);
                }
                Err(rejection) => conn.borrow_mut().respond_error(id, rejection.to_error()),
            }
            false
        }
        Command::SessionCompact => {
            respond_compact(conn, id);
            false
        }
        Command::SubagentQuery(params) => {
            match subagent_query_value(conn, &params) {
                Ok(value) => conn.borrow_mut().respond(id, value),
                Err(message) => conn
                    .borrow_mut()
                    .respond_error(id, ErrorObject::new(error_code::INVALID_PARAMS, message)),
            }
            false
        }
        Command::SessionList(params) => {
            respond_session_query(conn, id, &SessionQuery::List(params));
            false
        }
        Command::SessionRename(params) => {
            respond_session_rename(conn, id, &params);
            false
        }
        Command::SessionArchive => {
            respond_session_archive(conn, id);
            false
        }
        Command::SessionDelete(params) => {
            respond_session_delete(conn, id, &params);
            false
        }
        Command::SessionHistory(params) => {
            respond_session_query(conn, id, &SessionQuery::History(params));
            false
        }
        Command::SessionEvents(params) => {
            respond_session_query(conn, id, &SessionQuery::Events(params));
            false
        }
        Command::SessionNew => {
            respond_session_new(conn, id);
            false
        }
        Command::SessionResume(params) => {
            respond_session_resume(conn, id, &params);
            false
        }
        Command::SubagentRun(params) => {
            respond_subagent_run(conn, id, &params);
            false
        }
        Command::SessionAppend(params) => {
            respond_session_append(conn, id, &params);
            false
        }
        Command::WorkspaceSwitch(params) => {
            handle_workspace_switch(&mut conn.borrow_mut(), id, &params);
            false
        }
        Command::Shutdown => {
            // 正常退出：先收尾会话（写 `session_closed` 并丢弃空占位），再回包退出。
            conn.borrow().close_session();
            conn.borrow_mut().respond(id, json!({}));
            true
        }
    }
}

/// `session.append`：把宿主产生的文本作为 assistant 消息追加到内核会话历史。
///
/// 对映 Python 的 `remember_review_report`：`/review` 在模型循环之外跑，报告默认只作界面
/// 消息；注入后下一轮请求才会带上它。只允许 assistant 角色——不借这个入口伪造用户输入
/// 或工具结果。空内容不注入（与 Python 的 `remember_review_report` 同口径）。
/// `workspace.switch` 的统一应答：校验 path → 会话侧切换 → 回包。
///
/// 内核只做会话一致性（把自持会话的工作区指向新根 + 转录 `workspace_switched`）；
/// 工具表、MCP、临时目录等宿主侧资源由宿主自己重建，协议不回传。空闲路径与
/// 回合内路径（`handle_inbound`）共用这一份，避免两条路径的文案与校验分叉。
pub(super) fn handle_workspace_switch(conn: &mut Conn, id: Id, params: &WorkspaceSwitchParams) {
    let path = params.path.trim().to_string();
    if path.is_empty() {
        conn.respond_error(
            id,
            ErrorObject::new(
                error_code::INVALID_PARAMS,
                "workspace.switch 需要非空 path。",
            ),
        );
        return;
    }
    match conn.switch_workspace(&path) {
        // 没有自持会话时与 `session.append` 同一口径。
        Ok(None) => conn.respond_error(id, no_session_error()),
        Ok(Some((from, to))) => conn.respond(
            id,
            json!({ "switched": from != to, "from": from, "to": to }),
        ),
        Err(detail) => {
            eprintln!("[kernel] 转录 workspace_switched 失败：{detail}");
            conn.respond_error(
                id,
                ErrorObject::new(
                    error_code::INVALID_REQUEST,
                    format!("工作区切换记录失败：{detail}"),
                ),
            );
        }
    }
}

pub(super) fn protocol_version_of(params: Option<&Value>) -> String {
    params
        .and_then(|params| params.get("protocol_version"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub(super) fn spawn_reader(sender: Sender<Inbound>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match Frame::parse(&line) {
                        Ok(frame) => {
                            if sender.send(Inbound::Frame(Box::new(frame))).is_err() {
                                break;
                            }
                        }
                        // 无法解析的行不回 -32700：id 不可信时响应也无处可去，丢弃并记日志。
                        Err(error) => eprintln!("[kernel] 丢弃无法解析的行：{error}"),
                    }
                }
                Err(error) => {
                    eprintln!("[kernel] 读取 stdin 失败：{error}");
                    break;
                }
            }
        }
        let _ = sender.send(Inbound::Closed);
    });
}

/// 在 stdio 上跑完一个会话，直到 EOF、`shutdown` 或读取失败。
pub fn run_stdio() -> Result<(), String> {
    let (sender, receiver) = mpsc::channel::<Inbound>();
    spawn_reader(sender);

    let conn = Rc::new(RefCell::new(Conn {
        inbound: receiver,
        out: Box::new(std::io::stdout()),
        cancel: Arc::new(AtomicBool::new(false)),
        handshaken: false,
        exit_requested: false,
        model: None,
        session: None,
        tool_call_prune: false,
        usage: Rc::new(RefCell::new(TurnUsage::default())),
        evict_marks: Rc::new(RefCell::new(Vec::new())),
        tool_compaction_saved: 0,
        tool_compaction_batches: 0,
        tasks: None,
        runtime_cache: None,
        next_id: 0,
    }));

    eprintln!(
        "[kernel] omnicrawl {} 就绪（协议 v1，等待 initialize）。",
        env!("CARGO_PKG_VERSION")
    );

    loop {
        // 借用必须在本条语句内结束：dispatch 里还要可变借用同一条连接。
        let inbound = conn.borrow().inbound.recv();
        match inbound {
            Ok(Inbound::Frame(frame)) => {
                if dispatch(&conn, *frame) {
                    break;
                }
            }
            Ok(Inbound::Closed) => break,
            Err(_) => break,
        }
    }
    Ok(())
}
