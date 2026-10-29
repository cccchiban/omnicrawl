//! 触发时的固定模板：把「现场情况」拼成模型输入，并把模型输出收敛成一条可下发的动作。
//!
//! 模型是无状态的，每次唤醒都得重新告诉它现场：设备能做什么（动作表）、现在什么情况
//! （窗口快照）、上次做了什么（审计投影）、为什么叫它（命中的规则）。三段固定，不做自由发挥。
//!
//! 校验只在这里做一份：文本通道（主模型输出 JSON）与选项通道（决策模型选 o0/o1）都汇到
//! validate_plan，宿主据此夹取参数、拦下越界动作。

use serde_json::{json, Map, Value};

use super::config::{ActionSpec, CompareOp};

/// 固定系统提示：身份 + 输出格式 + 三条硬约束。
pub const SYSTEM_PROMPT: &str = "\
你是温室大棚的现场控制决策器。你会收到一份传感器事件与可选动作表，必须从中选且只选一个动作。

输出要求：只输出一个 JSON 对象，不要输出任何其它文字，不要代码块围栏：
{\"action\": \"<动作 id>\", \"params\": {\"<参数>\": <数字>}, \"reason\": \"<不超过 60 字的中文理由>\", \"confidence\": <0 到 1>}

约束：
1. action 必须取自 actions 里的 id，不得自造；
2. 指标已在合适范围、或你无法确定时，选 do_nothing；
3. 参数的取值范围见 actions[].params 的 min/max，越界会被宿主夹到边界值。";

/// 一次唤醒要交给规划器的输入。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRequest {
    pub zone: String,
    pub rule_id: String,
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    pub op: CompareOp,
    /// 该棚所有指标的窗口快照。
    pub snapshot: Value,
    /// 候选动作（已按规则裁剪）。
    pub actions: Vec<ActionSpec>,
    /// 最近几次动作的审计投影。
    pub recent: Vec<Value>,
    /// 事件时间（ISO 8601，本地时区）。
    pub now: String,
}

impl PlanRequest {
    /// 进模板的 state：字段顺序固定，便于对照与测试。
    pub fn state(&self) -> Value {
        let actions: Vec<Value> = self
            .actions
            .iter()
            .map(|action| {
                let params: Map<String, Value> = action
                    .params
                    .iter()
                    .map(|param| {
                        (
                            param.name.clone(),
                            json!({
                                "min": param.min,
                                "max": param.max,
                                "default": param.default,
                            }),
                        )
                    })
                    .collect();
                json!({
                    "id": action.id,
                    "label": action.label,
                    "description": action.description,
                    "params": Value::Object(params),
                })
            })
            .collect();
        json!({
            "zone": self.zone,
            "now": self.now,
            "trigger": {
                "rule": self.rule_id,
                "metric": self.metric,
                "op": self.op.as_str(),
                "threshold": round2(self.threshold),
                "value": round2(self.value),
            },
            "metrics": self.snapshot,
            "actions": actions,
            "recent_actions": self.recent,
        })
    }

    /// 文本通道的 user 消息：紧凑 JSON，省 token 也省得模型看到多余排版。
    pub fn user_content(&self) -> String {
        serde_json::to_string(&self.state()).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 决策通道的 questions：一个 choice 问题，候选项就是动作表。
///
/// 键按 o0、o1… 编号，与宿主 decision_wire::option_key 同形，调用方直接复用那套解析。
pub fn decision_questions(actions: &[ActionSpec]) -> Value {
    let criteria: Map<String, Value> = actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let text = if action.description.trim().is_empty() {
                action.label.clone()
            } else {
                format!("{}：{}", action.label, action.description)
            };
            (format!("o{index}"), Value::String(text))
        })
        .collect();
    json!({
        "action": {
            "type": "choice",
            "instructions": "根据传感器窗口与动作表选出当前最合适的一个动作。指标已合适或不确定时选 do_nothing。",
            "criteria": Value::Object(criteria),
        }
    })
}

/// 决策通道返回的选择下标 → 动作 id。
pub fn action_at(actions: &[ActionSpec], index: usize) -> Option<&ActionSpec> {
    actions.get(index)
}

/// 模型给出的原始动作（校验前）。
#[derive(Debug, Clone, PartialEq)]
pub struct RawPlan {
    pub action: String,
    pub params: Value,
    pub reason: String,
    pub confidence: Option<f64>,
    /// decision / main_model / fail_safe：进了审计就能分辨是谁的决定。
    pub source: &'static str,
}

/// 校验并收敛后的动作：参数已夹取，理由已裁剪。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedPlan {
    pub action: String,
    pub params: Map<String, Value>,
    pub reason: String,
    pub confidence: Option<f64>,
    pub source: &'static str,
}

/// 从模型文本里取出动作 JSON。
///
/// 容忍代码块围栏与前后缀说明，但**不猜**：取不到合法 JSON 就报错，由调用方走安全兜底。
pub fn parse_plan_text(text: &str, source: &'static str) -> Result<RawPlan, String> {
    let body = strip_code_fence(text.trim());
    let start = body
        .find('{')
        .ok_or_else(|| "模型输出里没有 JSON 对象。".to_string())?;
    let end = body
        .rfind('}')
        .ok_or_else(|| "模型输出里没有 JSON 对象的结尾。".to_string())?;
    if end < start {
        return Err("模型输出的 JSON 对象不完整。".to_string());
    }
    let parsed: Value = serde_json::from_str(&body[start..=end])
        .map_err(|error| format!("模型输出不是合法 JSON：{error}"))?;
    let action = parsed
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "模型输出缺少 action 字段。".to_string())?;
    Ok(RawPlan {
        action: action.to_string(),
        params: parsed.get("params").cloned().unwrap_or(json!({})),
        reason: parsed
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        confidence: parsed.get("confidence").and_then(Value::as_f64),
        source,
    })
}

/// 校验动作是否在候选内、参数是否越界。
///
/// 越界的数字被夹到 min/max（模型说 90 分钟灌溉、配置上限 30 分钟，就按 30 执行），
/// 动作 id 不认识则直接拒绝——不认识的 id 没有对应的执行语义，猜不得。
pub fn validate_plan(plan: &RawPlan, actions: &[ActionSpec]) -> Result<ValidatedPlan, String> {
    let spec = actions
        .iter()
        .find(|action| action.id == plan.action)
        .ok_or_else(|| format!("模型选择了不在候选内的动作：{}。", plan.action))?;

    let mut params = Map::new();
    for param in &spec.params {
        let raw = plan.params.get(&param.name).and_then(Value::as_f64);
        let value = raw.unwrap_or(param.default).clamp(param.min, param.max);
        params.insert(param.name.clone(), json!(value));
    }

    let reason = collapse(&plan.reason, 120);
    Ok(ValidatedPlan {
        action: spec.id.clone(),
        params,
        reason,
        confidence: plan.confidence.map(|value| value.clamp(0.0, 1.0)),
        source: plan.source,
    })
}

/// 下发指令的负载：带 request_id 与 TTL，供设备侧丢弃陈旧指令与去重。
pub fn command_payload(
    zone: &str,
    plan: &ValidatedPlan,
    request_id: &str,
    ttl_seconds: u64,
    issued_at: &str,
) -> Value {
    json!({
        "request_id": request_id,
        "zone": zone,
        "action": plan.action,
        "params": Value::Object(plan.params.clone()),
        "reason": plan.reason,
        "confidence": plan.confidence,
        "source": plan.source,
        "ttl_seconds": ttl_seconds,
        "issued_at": issued_at,
    })
}

fn strip_code_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let rest = match rest.split_once('\n') {
        Some((_language, body)) => body,
        None => rest,
    };
    rest.trim_end().trim_end_matches("```").trim()
}

fn collapse(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= limit {
        return joined;
    }
    joined.chars().take(limit).collect::<String>() + "…"
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::config::ParamSpec;

    fn actions() -> Vec<ActionSpec> {
        vec![
            ActionSpec {
                id: "open_vent".to_string(),
                label: "打开通风口".to_string(),
                description: "温度偏高时通风降温".to_string(),
                params: Vec::new(),
            },
            ActionSpec {
                id: "irrigate".to_string(),
                label: "启动灌溉".to_string(),
                description: "土壤湿度偏低时补水".to_string(),
                params: vec![ParamSpec {
                    name: "minutes".to_string(),
                    min: 1.0,
                    max: 30.0,
                    default: 10.0,
                }],
            },
            ActionSpec {
                id: "do_nothing".to_string(),
                label: "维持现状".to_string(),
                description: "无需干预".to_string(),
                params: Vec::new(),
            },
        ]
    }

    #[test]
    fn state_carries_trigger_metrics_and_actions() {
        let request = PlanRequest {
            zone: "3".to_string(),
            rule_id: "high_temp".to_string(),
            metric: "temp".to_string(),
            value: 31.234,
            threshold: 28.0,
            op: CompareOp::Gt,
            snapshot: json!({"temp": {"last": 31.234}}),
            actions: actions(),
            recent: vec![json!({"action": "do_nothing"})],
            now: "2026-10-29T10:00:00+08:00".to_string(),
        };
        let state = request.state();
        assert_eq!(state["zone"], "3");
        assert_eq!(state["trigger"]["op"], "gt");
        assert_eq!(state["trigger"]["value"], 31.23);
        assert_eq!(state["actions"][1]["id"], "irrigate");
        assert_eq!(state["actions"][1]["params"]["minutes"]["max"], 30.0);
        assert_eq!(state["recent_actions"][0]["action"], "do_nothing");
        assert!(request.user_content().starts_with("{\"zone\":\"3\""));
    }

    #[test]
    fn decision_questions_number_the_options() {
        let questions = decision_questions(&actions());
        assert_eq!(questions["action"]["type"], "choice");
        assert_eq!(
            questions["action"]["criteria"]["o0"],
            "打开通风口：温度偏高时通风降温"
        );
        assert_eq!(questions["action"]["criteria"]["o2"], "维持现状：无需干预");
    }

    #[test]
    fn parse_accepts_fenced_json_with_chatter() {
        let text = "我先看一下。\n```json\n{\"action\":\"open_vent\",\"reason\":\"  温度 31 度  \",\"confidence\":1.4}\n```";
        let plan = parse_plan_text(text, "main_model").expect("parse");
        assert_eq!(plan.action, "open_vent");
        assert_eq!(plan.reason, "温度 31 度");
        assert_eq!(plan.confidence, Some(1.4));
    }

    #[test]
    fn parse_rejects_text_without_action() {
        assert!(parse_plan_text("我觉得可以开窗", "main_model").is_err());
        assert!(parse_plan_text("{}", "main_model").is_err());
    }

    #[test]
    fn validate_clamps_params_and_rejects_unknown_actions() {
        let plan = RawPlan {
            action: "irrigate".to_string(),
            params: json!({"minutes": 90, "extra": 1}),
            reason: "土壤偏干".to_string(),
            confidence: Some(2.0),
            source: "decision",
        };
        let validated = validate_plan(&plan, &actions()).expect("validate");
        assert_eq!(validated.params["minutes"], 30.0);
        assert!(validated.params.get("extra").is_none());
        assert_eq!(validated.confidence, Some(1.0));

        let unknown = RawPlan {
            action: "flood".to_string(),
            params: json!({}),
            reason: String::new(),
            confidence: None,
            source: "main_model",
        };
        assert!(validate_plan(&unknown, &actions()).is_err());
    }

    #[test]
    fn validate_falls_back_to_declared_default() {
        let plan = RawPlan {
            action: "irrigate".to_string(),
            params: json!({}),
            reason: String::new(),
            confidence: None,
            source: "decision",
        };
        let validated = validate_plan(&plan, &actions()).expect("validate");
        assert_eq!(validated.params["minutes"], 10.0);
    }

    #[test]
    fn command_payload_is_self_describing() {
        let plan = ValidatedPlan {
            action: "open_vent".to_string(),
            params: Map::new(),
            reason: "温度过高".to_string(),
            confidence: Some(0.8),
            source: "decision",
        };
        let payload = command_payload("3", &plan, "req-1", 300, "2026-10-29T10:00:00+08:00");
        assert_eq!(payload["request_id"], "req-1");
        assert_eq!(payload["zone"], "3");
        assert_eq!(payload["action"], "open_vent");
        assert_eq!(payload["ttl_seconds"], 300);
        assert_eq!(payload["source"], "decision");
    }
}
