//! `omnicrawl/agent/controllers/plugins.py` 的判定层。
//!
//! 收进来的是 Hook 分发后的判定与文案：fail-closed 判定、拒绝事实提取、拒绝错误文案、
//! 以及「无 Manager / 分发不可用 / 基础设施异常 / 插件显式拒绝」四种结局的归一化。
//! 真正的进程级 Plugin Runtime、Worker 生命周期与配置读写仍是宿主的活。

use crate::error::AgentError;
use serde_json::{Map, Value};

/// 插件故障导致的 Hook 拒绝：文案必须与插件显式 deny 区分（code 来自扩展层）。
pub const PLUGIN_DENY_LABELS: [(&str, &str); 5] = [
    ("timeout", "超时"),
    ("protocol-error", "通信协议错误"),
    ("handler-error", "执行失败"),
    ("invalid-patch", "返回了非法改动"),
    ("dispatch-error", "分发异常"),
];

pub const DISPATCH_ERROR_CODE: &str = "dispatch-error";

/// 单个 Hook Handler 的执行结果里与拒绝文案有关的字段。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginHookResult {
    pub status: String,
    pub handler_key: String,
    pub elapsed_ms: Option<f64>,
}

/// 拒绝事实：状态码、原因、出错 Handler 与耗时。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DenialFacts {
    pub hook: String,
    pub code: String,
    pub reason: String,
    pub handler: String,
    pub elapsed_ms: Option<f64>,
}

/// 分发结局：原样放行（可能带插件改写后的 payload）或被拒绝。
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    Forward(Value),
    Denied(DenialFacts),
}

/// 插件分发的原始结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DispatchResult {
    pub denied: bool,
    pub deny_code: String,
    pub deny_reason: String,
    pub results: Vec<PluginHookResult>,
    pub payload: Option<Value>,
}

/// 与 `HOOK_POLICIES` 对齐：任一 `on_*` 策略为 reject-operation 时，Host 边界异常也必须
/// 拒绝操作，不能静默放行。
pub fn hook_requires_fail_closed(
    on_deny: &str,
    on_timeout: &str,
    on_protocol_error: &str,
    on_handler_error: &str,
) -> bool {
    [on_deny, on_timeout, on_protocol_error, on_handler_error].contains(&"reject-operation")
}

/// 提取拒绝事实：只认超时／协议错误／Handler 异常三类结果，取第一个。
pub fn denial_facts(
    hook_name: &str,
    deny_code: &str,
    deny_reason: &str,
    results: &[PluginHookResult],
) -> DenialFacts {
    let mut detail = DenialFacts {
        hook: hook_name.to_string(),
        code: deny_code.to_string(),
        reason: deny_reason.to_string(),
        ..DenialFacts::default()
    };
    for result in results {
        if !matches!(
            result.status.as_str(),
            "timeout" | "protocol-error" | "handler-error"
        ) {
            continue;
        }
        detail.handler = result.handler_key.clone();
        if let Some(elapsed) = result.elapsed_ms {
            detail.elapsed_ms = Some(elapsed);
        }
        break;
    }
    detail
}

/// Hook 被拒时的错误文案；区分插件故障与插件显式拒绝。
pub fn denial_error(hook_name: &str, detail: Option<&DenialFacts>) -> AgentError {
    let Some(detail) = detail.filter(|item| item.hook == hook_name) else {
        return AgentError::new(format!("{hook_name} 被插件拒绝。"));
    };
    let reason = detail.reason.clone();
    let label = PLUGIN_DENY_LABELS
        .iter()
        .find(|(code, _)| *code == detail.code)
        .map(|(_, label)| *label);
    let Some(label) = label else {
        let suffix = if reason.is_empty() {
            "。".to_string()
        } else {
            format!("：{reason}")
        };
        return AgentError::new(format!("{hook_name} 被插件拒绝{suffix}"));
    };
    let facts = [
        detail.handler.clone(),
        detail
            .elapsed_ms
            .map(|elapsed| format!("{elapsed:.0}ms"))
            .unwrap_or_default(),
    ]
    .into_iter()
    .filter(|item| !item.is_empty())
    .collect::<Vec<_>>()
    .join("，");
    let detail_text = [facts, reason]
        .into_iter()
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join("；");
    if detail_text.is_empty() {
        return AgentError::new(format!("{hook_name} 插件{label}。"));
    }
    AgentError::new(format!("{hook_name} 插件{label}（{detail_text}）"))
}

/// 分发 Hook 的结局：通知/观察类失败 fail-open，守卫类基础设施异常 fail-closed。
pub fn resolve_dispatch(
    hook_name: &str,
    payload: &Value,
    manager_present: bool,
    dispatch_callable: bool,
    fail_closed: bool,
    dispatched: Result<DispatchResult, String>,
) -> HookOutcome {
    if !manager_present || !dispatch_callable {
        return HookOutcome::Forward(payload.clone());
    }
    match dispatched {
        Err(error) => {
            if fail_closed {
                return HookOutcome::Denied(DenialFacts {
                    hook: hook_name.to_string(),
                    code: DISPATCH_ERROR_CODE.to_string(),
                    reason: error,
                    ..DenialFacts::default()
                });
            }
            HookOutcome::Forward(payload.clone())
        }
        Ok(outcome) if outcome.denied => HookOutcome::Denied(denial_facts(
            hook_name,
            &outcome.deny_code,
            &outcome.deny_reason,
            &outcome.results,
        )),
        Ok(outcome) => match outcome.payload {
            Some(Value::Object(_)) => HookOutcome::Forward(outcome.payload.unwrap_or_default()),
            _ => HookOutcome::Forward(payload.clone()),
        },
    }
}

/// 会话创建/恢复完成后要发的 before Hook 名与随后的 after Hook 名。
pub fn session_lifecycle_hooks(resume_session_id: &str) -> (&'static str, &'static str) {
    if resume_session_id.is_empty() {
        ("", "session.start.after")
    } else {
        ("session.resume.before", "session.resume.after")
    }
}

/// Hook 载荷里的会话标识（键序固定）。
pub fn session_hook_payload(session_id: &str) -> Value {
    let mut map = Map::new();
    map.insert("sessionId".to_string(), Value::from(session_id));
    Value::Object(map)
}

/// 子任务在父线程冻结出来的只读 Hook 分发上下文。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrozenDispatchContext {
    pub handlers: Vec<String>,
    /// 上下文来源；缺失或为空一律记为 `none`。
    pub source: String,
}

/// 空上下文：PluginManager 不在、不提供 `freeze`，或冻结结果形状不可用时使用。
///
/// 它仍要在 worker 内激活，否则子任务会回退到父 Agent 的实时 plan。
pub fn empty_dispatch_context() -> FrozenDispatchContext {
    FrozenDispatchContext {
        handlers: Vec::new(),
        source: "none".to_string(),
    }
}

/// 普通对象形态的归一化：`handlers` 缺失即空，`source` 空即 `none`。
pub fn normalize_dispatch_context(
    handlers: Option<&[String]>,
    source: Option<&str>,
) -> FrozenDispatchContext {
    FrozenDispatchContext {
        handlers: handlers.map(<[String]>::to_vec).unwrap_or_default(),
        source: source
            .filter(|value| !value.is_empty())
            .unwrap_or("none")
            .to_string(),
    }
}

/// 冻结结果的可能形态。
#[derive(Debug, Clone, Copy)]
pub enum FrozenShape<'a> {
    /// 管理器给出的就是标准上下文：原样采用，连空串也保持原样。
    Standard {
        handlers: &'a [String],
        source: &'a str,
    },
    /// 普通对象：按字段归一化。
    Object {
        handlers: Option<&'a [String]>,
        source: Option<&'a str>,
    },
    /// 没有管理器，或它不提供 `freeze`。
    Missing,
}

/// 按冻结结果的形态决定子任务可见的分发上下文。
pub fn frozen_dispatch_context(shape: FrozenShape<'_>) -> FrozenDispatchContext {
    match shape {
        FrozenShape::Standard { handlers, source } => FrozenDispatchContext {
            handlers: handlers.to_vec(),
            source: source.to_string(),
        },
        FrozenShape::Object { handlers, source } => normalize_dispatch_context(handlers, source),
        FrozenShape::Missing => empty_dispatch_context(),
    }
}

/// 回合级 Hook（`begin_turn` / `end_turn`）的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnHookAction {
    /// 没有 PluginManager，或它不提供这个方法：跳过。
    Skip,
    /// 调用；异常由宿主吞掉，不阻断主流程。
    Call,
}

pub fn turn_hook_action(manager_present: bool, hook_available: bool) -> TurnHookAction {
    if manager_present && hook_available {
        TurnHookAction::Call
    } else {
        TurnHookAction::Skip
    }
}

/// 会话生命周期 Hook 用的会话标识：状态里的标识为空时回落当前会话标识。
pub fn session_hook_id(state_session_id: Option<&str>, current_session_id: &str) -> String {
    match state_session_id {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => current_session_id.to_string(),
    }
}
