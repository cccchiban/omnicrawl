//! 回合注册表与内核运行时的接线：在飞的模型流登记进注册表后，**另一个线程**
//! 可以按回合归属关掉它，消费方在下一次轮询即返回取消。
//!
//! 对照 Python `omnicrawl/llm/stream_registry.py` 的用法：回合开始时 `stream_scope(owner)`，
//! 流在建立时 `register_stream`，取消时 `close_active_streams(owner)`。此前 Rust 侧只有
//! sink 自己回答取消（`cancel_check`），跨线程（审批取消 / 界面中断）没有着力点。
//!
//! 全部离线：本机回环服务端把响应头与首段数据写出后**保持连接**，读侧因此停在
//! 「连接还在但没数据」的状态——正是取消要能打断的那种静默期。

mod common;

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use common::{CaseInput, Reply, StubServer};
use omnicrawl_llm::{
    active_stream_count, close_active_streams, stream_scope, ChatEndpoint, DiscardSink,
    OpenAiChatRuntime, RuntimeErrorKind, ScopeOwner,
};

const HEAD: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n";

/// 「连接还在但不再有数据」的响应：`Content-Length` 声明的尾部**从不写出**，
/// 因此读侧会一直等——只有取消能结束这种静默期（对应 Python 里被 `close_active_streams`
/// 关掉的响应对象）。
fn stalled(head: &str) -> Reply {
    Reply {
        status: 200,
        head: head.to_string(),
        tail: "data: [DONE]\n\n".to_string(),
        hold: Duration::from_secs(5),
    }
}

/// 注册表是**进程级**全局表，而这些用例跑在同一个测试二进制的并行线程里：
/// 「关掉全部归属」的用例会跨用例误伤，所以整文件串行。
fn serialize() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK.get_or_init(|| Mutex::new(()));
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime_for(server: &StubServer) -> OpenAiChatRuntime {
    OpenAiChatRuntime::new(ChatEndpoint {
        base_url: server.base_url(),
        api_key: "test-key".to_string(),
        user_agent: "omnicrawl-test".to_string(),
    })
}

/// 等到注册表里出现本回合的流；超时即断言失败。
fn wait_for_registration(owner: &ScopeOwner) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while active_stream_count(Some(owner)) == 0 {
        assert!(
            Instant::now() < deadline,
            "模型流未在超时前登记进回合注册表"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn closing_a_stream_from_another_thread_cancels_the_turn() {
    let _serial = serialize();
    // 首段数据之后连接不再给数据：只有取消能结束这个回合。
    let server = StubServer::spawn(vec![stalled(HEAD)]);
    let runtime = Arc::new(runtime_for(&server));
    let owner = ScopeOwner::new();
    let (ready, wait) = mpsc::channel::<()>();

    let worker = {
        let owner = owner.clone();
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            // 归属按线程解析（与 Python 的 `stream_owner_for(cancel_check)` 同义）。
            let _scope = stream_scope(owner);
            let case = CaseInput::simple("qwen-test", "你好");
            let mut sink = DiscardSink;
            let result = runtime.run_turn(&case.chat_input(), &mut sink);
            let _ = ready.send(());
            result
        })
    };

    wait_for_registration(&owner);
    let started = Instant::now();
    assert_eq!(
        close_active_streams(Some(&owner)),
        1,
        "本回合的活跃流应当恰好一条"
    );

    wait.recv_timeout(Duration::from_secs(3))
        .expect("取消后回合应当及时返回");
    let error = worker
        .join()
        .expect("工作线程不应 panic")
        .expect_err("被取消的回合应当报错");
    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "取消应在轮询窗口内生效，而不是等响应体超时：{:?}",
        started.elapsed()
    );

    // 条目随流一起注销：正常结束、被取消、被放弃都不会留下悬挂条目。
    let deadline = Instant::now() + Duration::from_secs(2);
    while active_stream_count(Some(&owner)) != 0 {
        assert!(Instant::now() < deadline, "回合结束后仍有未注销的流");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 归属过滤是硬边界：关别人的回合不得影响本回合在飞的流。
#[test]
fn closing_another_scope_leaves_the_stream_running() {
    let _serial = serialize();
    let server = StubServer::spawn(vec![Reply::sse(
        200,
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ),
    )]);
    let runtime = Arc::new(runtime_for(&server));
    let owner = ScopeOwner::new();
    let other = ScopeOwner::new();

    let worker = {
        let owner = owner.clone();
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            let _scope = stream_scope(owner);
            let case = CaseInput::simple("qwen-test", "你好");
            let mut sink = DiscardSink;
            runtime.run_turn(&case.chat_input(), &mut sink)
        })
    };

    // 别的归属关不掉本回合的流（此时它可能还在飞，也可能已经结束）。
    assert_eq!(close_active_streams(Some(&other)), 0);
    let reply = worker
        .join()
        .expect("工作线程不应 panic")
        .expect("别的归属被关闭不应影响本回合");
    assert_eq!(reply.content, "你好");
}

/// 没有回合归属时也能登记（归属为 `None`）：关闭全部即覆盖这种流。
#[test]
fn streams_without_a_scope_are_closed_by_the_global_owner() {
    let _serial = serialize();
    let server = StubServer::spawn(vec![stalled(HEAD)]);
    let runtime = Arc::new(runtime_for(&server));
    let (ready, wait) = mpsc::channel::<()>();

    let worker = {
        let runtime = Arc::clone(&runtime);
        std::thread::spawn(move || {
            let case = CaseInput::simple("qwen-test", "你好");
            let mut sink = DiscardSink;
            let result = runtime.run_turn(&case.chat_input(), &mut sink);
            let _ = ready.send(());
            result
        })
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while active_stream_count(None) == 0 {
        assert!(Instant::now() < deadline, "无归属的流未在超时前登记");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(close_active_streams(None) >= 1);
    wait.recv_timeout(Duration::from_secs(3))
        .expect("取消后回合应当及时返回");
    let error = worker
        .join()
        .expect("工作线程不应 panic")
        .expect_err("被取消的回合应当报错");
    assert_eq!(error.kind, RuntimeErrorKind::Cancelled);
}
