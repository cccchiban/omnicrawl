//! `controllers/session/{control,settings}.py` 生命周期编排的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/fixtures/controllers_parity.json` 的 `lifecycle` 段——用探针对象真跑一遍
//! `close()` / `_finalize_attached_isolation()` / `_cancel_subagents_for_session_transition()`，
//! 记录副作用轨迹与失败文案。本套件用同一批输入重放 Rust 的决策，逐字段比对。

use omnicrawl_controllers::control::{
    close_callback_action, close_guard, finalize_isolation_summary, session_transition_drain,
    CloseCallbackAction, TransitionDrain, CLOSE_PHASES, SESSION_TRANSITION_DRAIN_FAILED,
};
use omnicrawl_controllers::settings::model_selection;
use omnicrawl_controllers::undo::{
    restore_conflict_failed, restore_load_failed, turn_snapshot_session_required,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn section() -> Value {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON");
    fixture["lifecycle"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("字符串数组")
        .iter()
        .map(|item| item.as_str().expect("字符串").to_string())
        .collect()
}

fn phase_labels() -> Vec<String> {
    CLOSE_PHASES
        .iter()
        .map(|phase| phase.label().to_string())
        .collect()
}

#[test]
fn close_phases_match_python() {
    let data = section();
    for case in data["close"].as_array().expect("close") {
        let label = case["label"].as_str().expect("label");
        let closed = case["closed_in"].as_bool().expect("closed_in");
        let closing = case["closing_in"].as_bool().expect("closing_in");
        let trace = strings(&case["trace"]);

        assert_eq!(
            close_guard(closed, closing),
            trace.is_empty(),
            "守卫结论与 Python 是否进入关闭流程一致（{label}）"
        );
        if trace.is_empty() {
            assert_eq!(
                case["closed_after"].as_bool().expect("closed_after"),
                closed,
                "守卫生效时「已关闭」状态不应被改变（{label}）"
            );
            continue;
        }
        assert_eq!(phase_labels(), trace, "关闭阶段顺序（{label}）");
        assert!(
            case["client_none"].as_bool().expect("client_none"),
            "关闭后应清空 LLM 客户端（{label}）"
        );
        assert!(
            case["runtime_none"].as_bool().expect("runtime_none"),
            "关闭后应清空 Runtime（{label}）"
        );
        assert!(
            case["callbacks_cleared"]
                .as_bool()
                .expect("callbacks_cleared"),
            "关闭后回调列表应清空（{label}）"
        );
    }
}

#[test]
fn close_callback_action_matches_python() {
    let data = section();
    for case in data["callback"].as_array().expect("callback") {
        let label = case["label"].as_str().expect("label");
        let closed = case["closed"].as_bool().expect("closed");
        let trace = strings(&case["trace"]);
        let queued = case["queued"].as_u64().expect("queued");

        match close_callback_action(closed) {
            CloseCallbackAction::RunNow => {
                assert_eq!(trace, ["callback"], "已关闭时立即执行（{label}）");
                assert_eq!(queued, 0, "已关闭时不再排队（{label}）");
            }
            CloseCallbackAction::Enqueue => {
                assert!(trace.is_empty(), "未关闭时不立即执行（{label}）");
                assert_eq!(queued, 1, "未关闭时排队一条（{label}）");
            }
        }
    }
}

#[test]
fn isolation_finalize_matches_python() {
    let data = section();
    for case in data["isolation"].as_array().expect("isolation") {
        let label = case["label"].as_str().expect("label");
        let session_present = case["session_present"].as_bool().expect("session_present");
        let isolation = step(&case["isolation"]);
        let subagents = step(&case["subagents"]).expect("子任务收尾结果");

        let result = finalize_isolation_summary(session_present, isolation, subagents);
        assert_eq!(
            result.notify,
            case["notified"].as_bool().expect("notified"),
            "回调条件（{label}）"
        );
        if let Some(expected) = case["summary"].as_str() {
            assert_eq!(result.summary, expected, "收尾摘要（{label}）");
        }
    }
}

/// 把「成功值 / 失败文案」的对照载荷还原成 Rust 侧的结果类型。
fn step(value: &Value) -> Option<Result<String, String>> {
    if value.is_null() {
        return None;
    }
    if value["ok"].as_bool().expect("ok") {
        Some(Ok(value["value"].as_str().expect("value").to_string()))
    } else {
        Some(Err(value["error"].as_str().expect("error").to_string()))
    }
}

#[test]
fn session_transition_drain_matches_python() {
    let data = section();
    for case in data["transition"].as_array().expect("transition") {
        let label = case["label"].as_str().expect("label");
        let has_coordinator = case["has_coordinator"].as_bool().expect("has_coordinator");
        let cancel_failed = case["cancel_failed"].as_bool().expect("cancel_failed");
        let drained = case["drained"].as_bool().expect("drained");
        let trace = strings(&case["trace"]);
        let error = case["error"].as_str();

        let cancelled = trace.iter().any(|item| item == "cancel");
        let resumed = trace.iter().any(|item| item == "resume");
        match session_transition_drain(has_coordinator, cancel_failed, drained) {
            TransitionDrain::Skip => {
                assert!(!cancelled && !resumed, "没有编排器时不动作（{label}）");
                assert!(error.is_none(), "没有编排器时不报错（{label}）");
            }
            TransitionDrain::Resume => {
                assert!(cancelled && resumed, "排空成功后恢复接单（{label}）");
                assert!(error.is_none(), "排空成功不报错（{label}）");
            }
            TransitionDrain::ResumeAndReraise => {
                assert!(cancelled && resumed, "取消抛异常时先恢复接单（{label}）");
                assert!(
                    error.is_some() && error != Some(SESSION_TRANSITION_DRAIN_FAILED),
                    "取消抛异常时原样抛出该异常（{label}）"
                );
            }
            TransitionDrain::ResumeAndReject => {
                assert!(cancelled && resumed, "未排空时先恢复接单（{label}）");
                assert_eq!(
                    error,
                    Some(SESSION_TRANSITION_DRAIN_FAILED),
                    "拒绝文案（{label}）"
                );
            }
        }
    }
}

#[test]
fn model_selection_matches_python() {
    let data = section();
    for case in data["settings"].as_array().expect("settings") {
        let label = case["label"].as_str().expect("label");
        let input = case["input"].as_str().expect("input");
        let result = model_selection(input);
        assert_eq!(
            result.is_ok(),
            case["ok"].as_bool().expect("ok"),
            "模型 ID 判定（{label}）"
        );
        if let Err(error) = result {
            assert_eq!(
                error.message(),
                case["error"].as_str().expect("error"),
                "模型 ID 文案（{label}）"
            );
        }
    }
}

#[test]
fn undo_restore_messages_match_python() {
    let data = section();
    let undo = &data["undo_restore"];

    for case in undo["restore"].as_array().expect("restore") {
        let label = case["label"].as_str().expect("label");
        let load_failed = case["load_failed"].as_bool().expect("load_failed");
        let behavior = case["behavior"].as_str().expect("behavior");
        let trace = strings(&case["trace"]);

        let produced = if load_failed {
            Some(restore_load_failed("reading boom").message().to_string())
        } else if behavior == "raise" {
            Some(
                restore_conflict_failed("conflict boom")
                    .message()
                    .to_string(),
            )
        } else {
            None
        };
        assert_eq!(
            produced.as_deref(),
            case["error"].as_str(),
            "回退失败文案（{label}）"
        );
        if load_failed {
            assert!(trace.is_empty(), "读取失败不应进入补丁过渡（{label}）");
        }
        if case["ok"].as_bool().expect("ok") {
            assert!(
                case["returned_callable"]
                    .as_bool()
                    .expect("returned_callable"),
                "恢复成功应交出反向回滚（{label}）"
            );
        }
    }

    for case in undo["complete"].as_array().expect("complete") {
        let label = case["label"].as_str().expect("label");
        assert_eq!(
            case["error"].as_str(),
            Some(turn_snapshot_session_required().message()),
            "无会话时的快照持久化文案（{label}）"
        );
    }
}
