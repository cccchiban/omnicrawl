//! stream_registry 的跨语言 parity：数据集是冻结的对照契约。
//!
//! 数据集是「脚本化操作 → 结果序列」：每例把注册 / 注销 / 计数 / 关闭 / 进出作用域做成
//! 轨迹，Rust 侧按同一脚本重放，比对每步返回值、关闭顺序与收尾计数。
//!
//! 另有三个手写用例覆盖 Python 单测里出现过的形态：无 close 方法的对象不被计数、
//! 注册守卫在丢弃时注销、显式归属可跨线程注册（对应 Python 的
//! `test_worker_registration_inherits_stream_scope`）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use omnicrawl_llm::{
    active_resource_count, active_stream_count, close_active_streams, current_stream_scope,
    register_stream, registered_resource, registered_stream_events, stream_scope,
    unregister_stream, CancelHandle, CloseAction, ScopeOwner, StreamRegistry, StreamScope,
};
use serde_json::{json, Value};

const FIXTURE: &str = include_str!("fixtures/stream_registry_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn run_case(case: &Value) -> Value {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let registry = StreamRegistry::new();
    let mut owners: HashMap<String, ScopeOwner> = HashMap::new();
    let mut handles: HashMap<String, Arc<CancelHandle>> = HashMap::new();
    let mut scopes: Vec<StreamScope> = Vec::new();
    let mut results: Vec<Value> = Vec::new();

    let kinds = &case["handles"];
    let mut owner_for =
        |name: &str| -> ScopeOwner { owners.entry(name.to_string()).or_default().clone() };

    for op in case["ops"].as_array().expect("ops") {
        let kind = op["op"].as_str().expect("op");
        let target = op["target"].as_str().unwrap_or("");
        let explicit_owner = op.get("owner").and_then(Value::as_str).map(&mut owner_for);

        let handle_of = |handles: &mut HashMap<String, Arc<CancelHandle>>,
                         log: &Arc<Mutex<Vec<String>>>,
                         name: &str| {
            handles
                .entry(name.to_string())
                .or_insert_with(|| {
                    let declared = kinds[name].as_str().unwrap_or("bare");
                    match declared {
                        "close" => {
                            let name = name.to_string();
                            let log = Arc::clone(log);
                            CancelHandle::with_close(move || {
                                log.lock().expect("日志锁").push(name.clone());
                            })
                        }
                        _ => CancelHandle::without_close(),
                    }
                })
                .clone()
        };

        match kind {
            "register" => {
                let name = op["handle"].as_str();
                let handle = name.map(|name| handle_of(&mut handles, &log, name));
                let callback: Option<CloseAction> =
                    match op.get("callback").and_then(Value::as_bool) {
                        Some(true) => {
                            let label = format!("cb:{}", name.unwrap_or_default());
                            let log = Arc::clone(&log);
                            Some(Arc::new(move || {
                                log.lock().expect("日志锁").push(label.clone());
                            }))
                        }
                        _ => None,
                    };
                if target == "resources" {
                    registry.register_resource(handle.as_ref(), explicit_owner, callback);
                } else {
                    registry.register_stream(handle.as_ref(), explicit_owner, callback);
                }
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "unregister" => {
                let handle = handle_of(
                    &mut handles,
                    &log,
                    op["handle"].as_str().expect("unregister 需要 handle"),
                );
                if target == "resources" {
                    registry.unregister_resource(&handle);
                } else {
                    registry.unregister_stream(&handle);
                }
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "count" => {
                let value = if target == "resources" {
                    registry.active_resource_count(explicit_owner.as_ref())
                } else {
                    registry.active_stream_count(explicit_owner.as_ref())
                };
                results.push(json!({"op": kind, "value": value}));
            }
            "close" => {
                let value = if target == "resources" {
                    registry.close_active_resources(explicit_owner.as_ref())
                } else {
                    registry.close_active_streams(explicit_owner.as_ref())
                };
                results.push(json!({"op": kind, "value": value}));
            }
            "enter_scope" => {
                scopes.push(stream_scope(
                    explicit_owner.expect("enter_scope 需要 owner"),
                ));
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            "exit_scope" => {
                drop(scopes.pop().expect("没有可退出的作用域"));
                results.push(json!({"op": kind, "value": Value::Null}));
            }
            other => panic!("未知的 op：{other}"),
        }
    }

    drop(scopes);
    let reflected_log = log.lock().expect("日志锁").clone();
    let final_counts = json!({
        "streams": registry.active_stream_count(None),
        "resources": registry.active_resource_count(None),
    });

    json!({
        "label": case["label"],
        "handles": case["handles"],
        "ops": case["ops"],
        "results": results,
        "log": reflected_log,
        "final": final_counts,
    })
}

#[test]
fn traces_match_python() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "数据集为空");

    for case in cases {
        let label = case["label"].as_str().unwrap_or("");
        let produced = run_case(case);
        assert_eq!(produced["results"], case["results"], "结果序列（{label}）");
        assert_eq!(produced["log"], case["log"], "关闭顺序（{label}）");
        assert_eq!(produced["final"], case["final"], "收尾计数（{label}）");
    }
}

#[test]
fn bare_handles_are_counted_but_not_closed() {
    let owner = ScopeOwner::new();
    let handle = CancelHandle::without_close();
    let registry = StreamRegistry::new();
    registry.register_stream(Some(&handle), Some(owner.clone()), None);
    assert_eq!(registry.active_stream_count(Some(&owner)), 1);
    assert_eq!(registry.close_active_streams(Some(&owner)), 0);
    assert_eq!(registry.active_stream_count(Some(&owner)), 0);
}

#[test]
fn registered_events_unregister_on_drop() {
    let owner = ScopeOwner::new();
    let handle = CancelHandle::without_close();
    let events = vec![1, 2, 3];
    let wrapped = registered_stream_events(
        Arc::clone(&handle),
        events.into_iter(),
        Some(owner.clone()),
        None,
    );
    assert_eq!(active_stream_count(Some(&owner)), 1);
    assert_eq!(wrapped.count(), 3);
    assert_eq!(active_stream_count(Some(&owner)), 0);

    // 提前丢弃（不迭代完）同样要注销。
    let handle = CancelHandle::without_close();
    let wrapped = registered_stream_events(
        Arc::clone(&handle),
        vec![1, 2, 3].into_iter(),
        Some(owner.clone()),
        None,
    );
    assert_eq!(active_stream_count(Some(&owner)), 1);
    drop(wrapped);
    assert_eq!(active_stream_count(Some(&owner)), 0);
}

#[test]
fn registered_resource_guard_unregisters_on_drop() {
    let owner = ScopeOwner::new();
    let handle = CancelHandle::without_close();
    {
        let guard = registered_resource(Arc::clone(&handle), Some(owner.clone()), None);
        assert_eq!(active_resource_count(Some(&owner)), 1);
        assert!(Arc::ptr_eq(guard.handle(), &handle));
    }
    assert_eq!(active_resource_count(Some(&owner)), 0);
}

#[test]
fn scope_stack_restores_previous_owner() {
    let outer = ScopeOwner::new();
    let inner = ScopeOwner::new();
    assert!(current_stream_scope().is_none());
    {
        let _outer = stream_scope(outer.clone());
        assert!(current_stream_scope().expect("外层归属").same(&outer));
        {
            let _inner = stream_scope(inner.clone());
            assert!(current_stream_scope().expect("内层归属").same(&inner));
        }
        assert!(current_stream_scope().expect("回到外层").same(&outer));
    }
    assert!(current_stream_scope().is_none());
}

/// Python 侧对应用例：并行工具线程注册的资源必须继承父回合归属。
/// Rust 侧没有 `copy_context`，等价做法是把调用线程解析出的归属显式带进 worker。
#[test]
fn explicit_owner_can_be_registered_from_worker_thread() {
    let owner = ScopeOwner::new();
    let handle = CancelHandle::without_close();
    let worker_handle = Arc::clone(&handle);
    let worker_owner = owner.clone();

    std::thread::spawn(move || {
        register_stream(Some(&worker_handle), Some(worker_owner), None);
    })
    .join()
    .expect("worker 线程");

    assert_eq!(active_stream_count(Some(&owner)), 1);
    // 收尾用显式归属过滤，避免影响同进程内其他用例的全局注册项。
    assert_eq!(close_active_streams(Some(&owner)), 0);
    assert_eq!(active_stream_count(Some(&owner)), 0);
    unregister_stream(&handle);
}
