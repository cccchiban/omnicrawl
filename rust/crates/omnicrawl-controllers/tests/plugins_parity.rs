//! `agent/controllers/plugins.py` 判定层的跨语言对照。
//!
//! 数据集是冻结的对照契约：`python rust/tools/gen_controllers_fixture.py` 重新生成
//! `tests/controllers` 数据集里的 `plugins` 段后，本套件重放 fail-closed 判定、拒绝事实、
//! 拒绝文案与分发结局逐条比对。进程级 Plugin Runtime 与 Worker 生命周期属于宿主。

use omnicrawl_controllers::plugins;
use omnicrawl_controllers::plugins as plugin_impl;

type PluginHookResult = plugin_impl::PluginHookResult;
type DispatchResult = plugin_impl::DispatchResult;
type DispatchOutcome = plugin_impl::HookOutcome;
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/controllers_parity.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("fixture 不是合法 JSON")
}

fn plugins_section() -> Value {
    fixture()["plugins"].clone()
}

fn results(value: &Value) -> Vec<PluginHookResult> {
    value
        .as_array()
        .expect("results")
        .iter()
        .map(|item| PluginHookResult {
            status: item["status"].as_str().expect("status").to_string(),
            handler_key: item["handler_key"]
                .as_str()
                .expect("handler_key")
                .to_string(),
            elapsed_ms: item["elapsed_ms"].as_f64(),
        })
        .collect()
}

#[test]
fn plugins_constants_match_python() {
    let data = plugins_section();
    let expected = data["constants"]["deny_labels"]
        .as_object()
        .expect("deny_labels");
    assert_eq!(plugins::PLUGIN_DENY_LABELS.len(), expected.len());
    for (code, label) in plugins::PLUGIN_DENY_LABELS {
        assert_eq!(
            Some(&Value::from(label)),
            expected.get(code),
            "拒绝标签（{code}）"
        );
    }
}

#[test]
fn plugins_fail_closed_matches_python() {
    let data = plugins_section();
    for case in data["fail_closed"].as_array().expect("cases") {
        let hook = case["hook"].as_str().expect("hook");
        assert_eq!(
            plugins::hook_requires_fail_closed(
                case["on_deny"].as_str().expect("on_deny"),
                case["on_timeout"].as_str().expect("on_timeout"),
                case["on_protocol_error"]
                    .as_str()
                    .expect("on_protocol_error"),
                case["on_handler_error"].as_str().expect("on_handler_error"),
            ),
            case["expected"].as_bool().expect("expected"),
            "fail-closed 判定（{hook}）"
        );
    }
}

#[test]
fn plugins_denial_facts_matches_python() {
    let data = plugins_section();
    for case in data["denial_facts"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let facts = plugins::denial_facts(
            case["hook"].as_str().expect("hook"),
            case["code"].as_str().expect("code"),
            case["reason"].as_str().expect("reason"),
            &results(&case["results"]),
        );
        let expected = &case["expected"];
        assert_eq!(
            facts.hook,
            expected
                .get("hook")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "{label}"
        );
        assert_eq!(
            facts.code,
            expected
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "{label}"
        );
        assert_eq!(
            facts.reason,
            expected
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "{label}"
        );
        assert_eq!(
            facts.handler,
            expected
                .get("handler")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "出错 Handler（{label}）"
        );
        assert_eq!(
            facts.elapsed_ms,
            expected.get("elapsed_ms").and_then(Value::as_f64),
            "耗时（{label}）"
        );
    }
}

#[test]
fn plugins_denial_error_matches_python() {
    let data = plugins_section();
    for case in data["denial_error"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let detail = case["detail"].as_object().map(|map| plugins::DenialFacts {
            hook: map
                .get("hook")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            code: map
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            reason: map
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            handler: map
                .get("handler")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            elapsed_ms: map.get("elapsed_ms").and_then(Value::as_f64),
        });
        let error = plugins::denial_error("tool.call.before", detail.as_ref());
        assert_eq!(
            error.message(),
            case["error"].as_str().expect("error"),
            "拒绝文案（{label}）"
        );
    }
}

#[test]
fn plugins_dispatch_matches_python() {
    let data = plugins_section();
    for case in data["dispatch"].as_array().expect("cases") {
        let label = case["label"].as_str().expect("label");
        let payload = case["payload"].clone();
        let outcome = &case["outcome"];
        let dispatched = match outcome["error"].as_str() {
            Some(error) => Err(error.to_string()),
            None => Ok(DispatchResult {
                denied: outcome["denied"].as_bool().expect("denied"),
                deny_code: outcome["deny_code"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                deny_reason: outcome["deny_reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                results: results(&outcome["results"]),
                payload: outcome["payload"]
                    .as_object()
                    .map(|_| outcome["payload"].clone()),
            }),
        };
        let resolved = plugins::resolve_dispatch(
            case["hook"].as_str().expect("hook"),
            &payload,
            case["manager_present"].as_bool().expect("manager_present"),
            case["dispatch_callable"]
                .as_bool()
                .expect("dispatch_callable"),
            case["fail_closed"].as_bool().expect("fail_closed"),
            dispatched,
        );
        match (&resolved, case["denial_detail"].as_object()) {
            (DispatchOutcome::Forward(value), None) => {
                assert_eq!(value, &case["result"], "放行载荷（{label}）");
            }
            (DispatchOutcome::Denied(facts), Some(expected)) => {
                assert!(case["result"].is_null(), "被拒绝时应无载荷（{label}）");
                assert_eq!(
                    facts.hook,
                    expected
                        .get("hook")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "拒绝 Hook（{label}）"
                );
                assert_eq!(
                    facts.code,
                    expected
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "拒绝码（{label}）"
                );
                assert_eq!(
                    facts.reason,
                    expected
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "拒绝原因（{label}）"
                );
                assert_eq!(
                    facts.handler,
                    expected
                        .get("handler")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "出错 Handler（{label}）"
                );
                assert_eq!(
                    facts.elapsed_ms,
                    expected.get("elapsed_ms").and_then(Value::as_f64),
                    "耗时（{label}）"
                );
            }
            (DispatchOutcome::Denied(facts), None) => {
                panic!("本该放行却拒绝了：{}（{label}）", facts.code)
            }
            (DispatchOutcome::Forward(value), Some(_)) => {
                panic!("本该拒绝却放行了：{value}（{label}）")
            }
        }
    }
}
