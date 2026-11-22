use super::*;
use super::protocol::{Inbound, PortFailure};
use super::turn::Conn;
use std::collections::VecDeque;
use std::time::Duration;

pub(crate) struct ChildFrame {
    pub frame: Frame,
    pub replies: Sender<Inbound>,
    pub cancel: CancelToken,
}

#[derive(Default)]
pub(crate) struct ChildDispatch {
    queued: VecDeque<ChildFrame>,
    active: Option<(Id, ChildFrame)>,
}

struct ChildWriter {
    parent: Sender<Inbound>,
    replies: Sender<Inbound>,
    cancel: CancelToken,
    buffer: Vec<u8>,
}

impl Write for ChildWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let line = std::str::from_utf8(&self.buffer)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let frame = Frame::parse(line)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.parent.send(Inbound::Child(Box::new(ChildFrame {
            frame,
            replies: self.replies.clone(),
            cancel: self.cancel.clone(),
        }))).map_err(|_| std::io::Error::other("父内核连接已关闭。"))?;
        self.buffer.clear();
        Ok(())
    }
}

pub(crate) fn child_connection(
    parent: Sender<Inbound>,
    model: KernelModelConfig,
    cancel: CancelToken,
) -> Rc<RefCell<Conn>> {
    let (sender, receiver) = mpsc::channel();
    let out = ChildWriter {
        parent,
        replies: sender.clone(),
        cancel,
        buffer: Vec::new(),
    };
    let mut conn = Conn::new(sender, receiver, Box::new(out));
    conn.handshaken = true;
    conn.model = Some(model);
    Rc::new(RefCell::new(conn))
}

impl Conn {
    pub(crate) fn accept_child(&mut self, child: ChildFrame) {
        if child.frame.is_notification() {
            let _ = self.send(&child.frame);
        } else if child.frame.is_request() {
            self.children.queued.push_back(child);
        }
    }

    pub(crate) fn start_child_request(&mut self) {
        if self.request_active || self.children.active.is_some() {
            return;
        }
        while let Some(child) = self.children.queued.pop_front() {
            if child.cancel.is_set() || self.exit_requested {
                let _ = child.replies.send(Inbound::Frame(Box::new(Frame::error_response(
                    child.frame.id().cloned().expect("子任务请求包含 id"),
                    ErrorObject::new(error_code::TURN_CANCELLED, "子任务已取消。"),
                ))));
                continue;
            }
            self.next_id += 1;
            let id = Id::Number(self.next_id);
            let mut frame = child.frame.clone();
            frame.id = Some(id.clone());
            if self.send(&frame).is_err() {
                let _ = child.replies.send(Inbound::Closed);
                continue;
            }
            self.children.active = Some((id, child));
            break;
        }
    }

    pub(crate) fn route_child_response(&mut self, frame: &Frame) -> bool {
        if !frame.is_response() || !self.children.active.as_ref()
            .is_some_and(|(id, _)| frame.id() == Some(id)) {
            return false;
        }
        let (_, child) = self.children.active.take().expect("响应已匹配");
        let mut response = frame.clone();
        response.id = child.frame.id().cloned();
        let _ = child.replies.send(Inbound::Frame(Box::new(response)));
        true
    }

    pub(crate) fn wait_for_child_batch(&mut self) -> Result<(), PortFailure> {
        while self.children.active.is_some() {
            match self.inbound.recv() {
                Ok(Inbound::Child(child)) => self.accept_child(*child),
                Ok(Inbound::Frame(frame)) => {
                    if !self.route_child_response(&frame) {
                        if let Some(failure) = self.handle_inbound(&frame) {
                            return Err(failure);
                        }
                    }
                }
                Ok(Inbound::Closed) | Err(_) => return Err(PortFailure::Disconnected),
            }
        }
        Ok(())
    }

    pub(crate) fn cancel_children(&mut self) {
        if let Some(manager) = self.tasks.as_ref() {
            manager.cancel_all(super::tools::subagent_tools::SUBAGENT_OWNER_ID, None);
        }
        // 只置 token 不够：`active` 只有 `route_child_response` 会清，而被转发的请求
        // 在宿主取消回合时可能永远不会得到回答，那之后队列就卡死了。这里当场结算。
        if let Some((_, child)) = self.children.active.take() {
            child.cancel.set();
            close_child(child, "子任务已取消。");
        }
        while let Some(child) = self.children.queued.pop_front() {
            child.cancel.set();
            close_child(child, "子任务已取消。");
        }
    }
}

/// 把一条子任务通道收尾：能拿到请求 id 就回一条取消错误，否则直接断开。
///
/// 两种情况子回合都不会再收到响应，因此这里必须给个明确的终态，否则对面永远等。
fn close_child(child: ChildFrame, message: &str) {
    let sent = match child.frame.id().cloned() {
        Some(id) => child.replies.send(Inbound::Frame(Box::new(Frame::error_response(
            id,
            ErrorObject::new(error_code::TURN_CANCELLED, message),
        )))),
        None => child.replies.send(Inbound::Closed),
    };
    let _ = sent;
}

pub(crate) fn pump_child_tasks(conn: &Rc<RefCell<Conn>>) {
    loop {
        let inbound = conn.borrow().inbound.try_recv();
        match inbound {
            Ok(Inbound::Child(child)) => conn.borrow_mut().accept_child(*child),
            Ok(Inbound::Frame(frame)) => {
                let mut connection = conn.borrow_mut();
                if !connection.route_child_response(&frame) {
                    let _ = connection.handle_inbound(&frame);
                }
            }
            Ok(Inbound::Closed) | Err(mpsc::TryRecvError::Disconnected) => {
                let mut connection = conn.borrow_mut();
                connection.exit_requested = true;
                connection.cancel_children();
                break;
            }
            Err(mpsc::TryRecvError::Empty) => break,
        }
    }
    conn.borrow_mut().start_child_request();
}

/// 前台批次：等到每个任务都落定，按派发顺序交出各自的公开结果。
///
/// 只看任务表的终态与 worker 交回的结果，不等额外信号：某个任务在准入阶段就没能起
/// 子回合时，等信号会让整批永远卡住。
pub(crate) fn wait_for_tasks(
    conn: &Rc<RefCell<Conn>>,
    tasks: &[PreparedTask],
    results: Receiver<Value>,
) -> Vec<Value> {
    let mut collected: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    let mut grace: Option<std::time::Instant> = None;
    loop {
        pump_child_tasks(conn);
        drain_results(&results, &mut collected);
        let snapshots: Vec<Option<Value>> = {
            let borrowed = conn.borrow();
            tasks
                .iter()
                .map(|task| {
                    borrowed.tasks.as_ref().and_then(|manager| {
                        manager.get(
                            &task.task_id,
                            super::tools::subagent_tools::SUBAGENT_OWNER_ID,
                            None,
                        )
                    })
                })
                .collect()
        };
        let all_terminal = snapshots.iter().all(|snapshot| {
            snapshot.as_ref().is_some_and(|snapshot| {
                snapshot
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(omnicrawl_controllers::subagents::tasks::is_terminal)
            })
        });
        if all_terminal {
            // worker 先发结果、再把任务标成终态，所以这里通常已经收齐。
            pump_child_tasks(conn);
            drain_results(&results, &mut collected);
            // 取消的任务可能根本没起过子回合（排队期被取消），结果永远不会来：
            // 这类缺口直接按取消快照结算，不必陪等。
            let missing_started = tasks
                .iter()
                .zip(&snapshots)
                .all(|(task, snapshot)| {
                    collected.contains_key(&task.task_id)
                        || snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot["status"].as_str())
                            == Some("cancelled")
                });
            if missing_started {
                return settle(tasks, snapshots, collected);
            }
            let started = *grace.get_or_insert_with(std::time::Instant::now);
            if started.elapsed() > Duration::from_secs(2) {
                return settle(tasks, snapshots, collected);
            }
        } else {
            grace = None;
        }
        if conn.borrow().exit_requested {
            return tasks
                .iter()
                .map(|task| {
                    failure_payload(&task.view(), "SUBAGENT_CANCELLED", "宿主连接已关闭。", None)
                })
                .collect();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn drain_results(results: &Receiver<Value>, collected: &mut std::collections::BTreeMap<String, Value>) {
    while let Ok(result) = results.try_recv() {
        let task_id = result["task_id"].as_str().unwrap_or_default().to_string();
        collected.insert(task_id, result);
    }
}

fn settle(
    tasks: &[PreparedTask],
    snapshots: Vec<Option<Value>>,
    mut collected: std::collections::BTreeMap<String, Value>,
) -> Vec<Value> {
    tasks
        .iter()
        .zip(snapshots)
        .map(|(task, snapshot)| settle_task_result(task, snapshot, collected.remove(&task.task_id)))
        .collect()
}

/// 单个任务的最终公开结果：优先用 worker 交回的那份，缺失时按任务快照补。
fn settle_task_result(
    task: &PreparedTask,
    snapshot: Option<Value>,
    result: Option<Value>,
) -> Value {
    let Some(snapshot) = snapshot else {
        return result.unwrap_or_else(|| {
            failure_payload(&task.view(), "SUBAGENT_CANCELLED", "子任务已取消。", None)
        });
    };
    let cancelled = snapshot["status"].as_str() == Some("cancelled");
    let result = if cancelled { None } else { result };
    result
        .or_else(|| snapshot.get("result").filter(|value| !value.is_null()).cloned())
        .unwrap_or_else(|| {
            let mut value =
                failure_payload(&task.view(), "SUBAGENT_CANCELLED", "子任务已取消。", None);
            value["status"] = snapshot["status"].clone();
            value["error"] = snapshot["error"].clone();
            value
        })
}
