//! MQTT 接入配置解析（`config.toml` 的 `[mqtt]` 段）。
//!
//! 与 Telegram / 飞书连接器同源：段对象由宿主配置层解析后传入，TOML 读取不属于本 crate。
//! 这里的配置只描述「接入 + 规则 + 动作表」，不含任何模型选择——决策通道由宿主装配。

use serde_json::Value;

/// 默认 keepalive：决策可能阻塞几十秒，留足 broker 的 1.5 倍容忍窗口。
pub const DEFAULT_KEEPALIVE_SECONDS: u64 = 120;
/// 默认聚合窗口：一次决策看 30 秒内的读数，不按单条报文抖动。
pub const DEFAULT_WINDOW_SECONDS: u64 = 30;
/// 默认冷却：同一棚同一规则两次动作的最小间隔。
pub const DEFAULT_COOLDOWN_SECONDS: u64 = 1800;
/// 默认轮询粒度：读报文超时后回到主循环检查窗口与冷却。
pub const DEFAULT_POLL_SECONDS: u64 = 1;
/// 默认回执等待：发出指令后等设备 ack 的上限。
pub const DEFAULT_ACK_TIMEOUT_SECONDS: u64 = 15;
/// 默认指令有效期：超过它的陈旧指令不得覆盖新状态。
pub const DEFAULT_TTL_SECONDS: u64 = 300;
/// 最近动作条数：进模板的审计投影条数。
pub const DEFAULT_RECENT_ACTIONS: usize = 5;
/// 回合循环的超时：现场决策不能无限等，超时就取消并回落规则兜底。
pub const DEFAULT_TURN_TIMEOUT_SECONDS: u64 = 120;

/// 无人值守默认白名单：只读 + 记忆/知识库 + 联网检索 + 待办维护。
///
/// 不含 shell、文件写入、桌面控制、子代理与配置改写——这些在无人值守下没有安全的失败模式。
/// 要放开就显式写 `[mqtt] tools`，那是用户的决定，不是默认值。
pub const DEFAULT_AGENT_TOOLS: [&str; 13] = [
    "read",
    "read_image",
    "list",
    "find",
    "grep",
    "memory_search",
    "memory_read",
    "memory_expand_related",
    "memory_write",
    "kb_search",
    "kb_read",
    "web_search",
    "fetcher",
];

/// 规划器：回合循环（完整工具面）或单次结构化决策（便宜、不碰工具）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerMode {
    /// 拉内核子进程跑一个回合：模型可以查资料、翻记忆、多步推理。
    Agent,
    /// 一次结构化决策：决策模型优先，未配置时回落主模型单轮补全。
    Decision,
}

impl PlannerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PlannerMode::Agent => "agent",
            PlannerMode::Decision => "decision",
        }
    }
}

/// 一条规则允许的比较方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl CompareOp {
    /// 比较运算；两个浮点数直接比，配置层已保证 `value` 是有限数。
    pub fn holds(self, left: f64, right: f64) -> bool {
        match self {
            CompareOp::Gt => left > right,
            CompareOp::Ge => left >= right,
            CompareOp::Lt => left < right,
            CompareOp::Le => left <= right,
            CompareOp::Eq => (left - right).abs() < f64::EPSILON,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CompareOp::Gt => "gt",
            CompareOp::Ge => "ge",
            CompareOp::Lt => "lt",
            CompareOp::Le => "le",
            CompareOp::Eq => "eq",
        }
    }
}

/// 动作可带的一个数值参数及其允许范围（宿主侧夹取，模型不能越界）。
#[derive(Debug, Clone, PartialEq)]
pub struct ParamSpec {
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
}

/// 模型可以选择的动作：`id` 是唯一键，label / description 进模板。
#[derive(Debug, Clone, PartialEq)]
pub struct ActionSpec {
    pub id: String,
    pub label: String,
    pub description: String,
    pub params: Vec<ParamSpec>,
}

/// 一条确定性规则：满足条件并持续 `sustain_seconds` 后唤醒一次模型。
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSpec {
    pub id: String,
    /// 作用范围：`*` 表示任意棚，其余按棚号精确匹配。
    pub zone: String,
    pub metric: String,
    pub op: CompareOp,
    pub value: f64,
    /// 需要连续满足的秒数（0 表示立即触发）。
    pub sustain_seconds: u64,
    /// 迟滞的恢复阈值：越过它才算「恢复」，避免在阈值附近反复触发。
    pub clear_value: Option<f64>,
    /// 命中后交给模型挑选的动作候选（必须是动作表里的 id）。
    pub actions: Vec<String>,
    /// 模型不可用时的安全动作：宁可执行确定性兜底，也不放任不管。
    pub fallback: Option<String>,
    /// 安全兜底规则：命中后不叫模型，直接执行 fallback（物理上限不能等模型判断）。
    pub fail_safe: bool,
}

/// 解析后的 MQTT 接入配置。
#[derive(Debug, Clone, PartialEq)]
pub struct MqttConfig {
    pub enabled: bool,
    pub broker: String,
    pub client_id: String,
    pub username: String,
    pub password: String,
    /// 密码优先取这个环境变量（未设置时用 `password` 字段）。
    pub password_env: String,
    pub keepalive_seconds: u64,
    pub qos: u8,
    pub topics: Vec<String>,
    /// 棚号取值：先读 payload 的该字段，缺失时回落到 topic 层级。
    pub zone_field: String,
    pub zone_topic_level: usize,
    pub window_seconds: u64,
    pub cooldown_seconds: u64,
    pub poll_seconds: u64,
    /// 指令下发主题模板，`{zone}` 会被替换成棚号。
    pub command_topic: String,
    /// 回执主题模板（空串表示不等 ack）。
    pub ack_topic: String,
    pub ack_timeout_seconds: u64,
    pub ttl_seconds: u64,
    pub recent_actions: usize,
    /// 审计文件路径（空串表示用用户配置目录下的默认位置）。
    pub audit_path: String,
    /// 规划器：回合循环或单次决策。
    pub planner: PlannerMode,
    /// 无人值守工具白名单（仅回合循环用）：不在表里的工具既不可见也不可执行。
    pub tools: Vec<String>,
    /// 回合超时（仅回合循环用）。
    pub turn_timeout_seconds: u64,
    pub actions: Vec<ActionSpec>,
    pub rules: Vec<RuleSpec>,
}

impl MqttConfig {
    /// 按 id 取动作。
    pub fn action(&self, id: &str) -> Option<&ActionSpec> {
        self.actions.iter().find(|action| action.id == id)
    }

    /// 生效的密码：环境变量优先。
    pub fn resolve_password(&self) -> String {
        if self.password_env.trim().is_empty() {
            return self.password.clone();
        }
        std::env::var(&self.password_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.password.clone())
    }

    /// 指令主题：把 `{zone}` 换成棚号。
    pub fn command_topic_for(&self, zone: &str) -> String {
        self.command_topic.replace("{zone}", zone)
    }

    /// 回执主题：未配置时返回 None（不等 ack）。
    pub fn ack_topic_for(&self, zone: &str) -> Option<String> {
        let template = self.ack_topic.trim();
        if template.is_empty() {
            None
        } else {
            Some(template.replace("{zone}", zone))
        }
    }
}

/// 读取配置：只读配置段，缺项回落默认值；解析失败返回中文错误。
pub fn load_mqtt_config(section: Option<&Value>) -> Result<MqttConfig, String> {
    let section = section.cloned().unwrap_or(Value::Null);
    let enabled = section
        .get("enabled")
        .map(value_as_bool)
        .transpose()?
        .unwrap_or(true);
    let broker = text_or(&section, "broker", "mqtt://127.0.0.1:1883");
    let client_id = text_or(&section, "client_id", "omnicrawl-farm");
    let username = text_or(&section, "username", "");
    let password = text_or(&section, "password", "");
    let password_env = text_or(&section, "password_env", "");
    let keepalive_seconds = seconds_or(&section, "keepalive_seconds", DEFAULT_KEEPALIVE_SECONDS)?;
    let qos = parse_qos(section.get("qos"))?;
    let topics = parse_topics(section.get("topics"))?;
    let zone_field = text_or(&section, "zone_field", "zone");
    let zone_topic_level = count_or(&section, "zone_topic_level", 1)? as usize;
    let window_seconds = seconds_or(&section, "window_seconds", DEFAULT_WINDOW_SECONDS)?;
    let cooldown_seconds = seconds_or(&section, "cooldown_seconds", DEFAULT_COOLDOWN_SECONDS)?;
    let poll_seconds = seconds_or(&section, "poll_seconds", DEFAULT_POLL_SECONDS)?;
    let command_topic = text_or(&section, "command_topic", "farm/{zone}/command");
    let ack_topic = text_or(&section, "ack_topic", "farm/{zone}/ack");
    let ack_timeout_seconds =
        seconds_or(&section, "ack_timeout_seconds", DEFAULT_ACK_TIMEOUT_SECONDS)?;
    let ttl_seconds = seconds_or(&section, "ttl_seconds", DEFAULT_TTL_SECONDS)?;
    let recent_actions = count_or(&section, "recent_actions", DEFAULT_RECENT_ACTIONS as u64)?
        .min(20) as usize;
    let audit_path = text_or(&section, "audit_path", "");
    let planner = parse_planner(section.get("planner"))?;
    let tools = match section.get("tools") {
        Some(raw) => parse_topics(Some(raw))?,
        None => DEFAULT_AGENT_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    };
    let turn_timeout_seconds =
        seconds_or(&section, "turn_timeout_seconds", DEFAULT_TURN_TIMEOUT_SECONDS)?;
    let actions = parse_actions(section.get("actions"))?;
    let rules = parse_rules(section.get("rules"))?;

    Ok(MqttConfig {
        enabled,
        broker: broker.trim().to_string(),
        client_id: client_id.trim().to_string(),
        username: username.trim().to_string(),
        password,
        password_env: password_env.trim().to_string(),
        keepalive_seconds: keepalive_seconds.max(5),
        qos,
        topics,
        zone_field: zone_field.trim().to_string(),
        zone_topic_level,
        window_seconds: window_seconds.max(1),
        cooldown_seconds,
        poll_seconds: poll_seconds.clamp(1, 60),
        command_topic: command_topic.trim().to_string(),
        ack_topic: ack_topic.trim().to_string(),
        ack_timeout_seconds: ack_timeout_seconds.max(1),
        ttl_seconds: ttl_seconds.max(1),
        recent_actions,
        audit_path: audit_path.trim().to_string(),
        planner,
        tools,
        turn_timeout_seconds: turn_timeout_seconds.max(10),
        actions,
        rules,
    })
}

/// 构造 Bot 前的校验：地址、订阅、动作与规则的引用关系都必须自洽。
pub fn validate_config(config: &MqttConfig) -> Result<(), String> {
    if config.broker.trim().is_empty() {
        return Err("缺少 mqtt.broker（形如 mqtt://host:1883 或 mqtts://host:8883）。".to_string());
    }
    if config.client_id.trim().is_empty() {
        return Err("mqtt.client_id 不能为空：它是 broker 侧的会话标识。".to_string());
    }
    if config.topics.is_empty() {
        return Err("mqtt.topics 不能为空：至少要订阅一个遥测主题。".to_string());
    }
    if config.command_topic.trim().is_empty() {
        return Err("mqtt.command_topic 不能为空：模型的动作要有一条下发通道。".to_string());
    }
    if config.actions.is_empty() {
        return Err("mqtt.actions 不能为空：至少要有 do_nothing。".to_string());
    }
    if config.action("do_nothing").is_none() {
        return Err("mqtt.actions 必须包含 do_nothing（模型「不干预」的唯一出口）。".to_string());
    }
    for rule in &config.rules {
        if rule.actions.is_empty() {
            return Err(format!("mqtt.rules.{} 的 actions 不能为空。", rule.id));
        }
        for action in &rule.actions {
            if config.action(action).is_none() {
                return Err(format!(
                    "mqtt.rules.{} 引用了未定义的动作：{action}。",
                    rule.id
                ));
            }
        }
        if let Some(fallback) = rule.fallback.as_deref() {
            if config.action(fallback).is_none() {
                return Err(format!(
                    "mqtt.rules.{} 的 fallback 引用了未定义的动作：{fallback}。",
                    rule.id
                ));
            }
            if !rule.actions.iter().any(|action| action == fallback) {
                return Err(format!(
                    "mqtt.rules.{} 的 fallback 必须是该规则的动作候选之一：{fallback}。",
                    rule.id
                ));
            }
        }
    }
    Ok(())
}

/// 就绪判定（自动启动与诊断用）：配置齐全且校验通过。
pub fn check_config(config: &MqttConfig) -> Value {
    let mut issues: Vec<String> = Vec::new();
    if config.broker.trim().is_empty() {
        issues.push("缺少 broker".to_string());
    }
    if config.topics.is_empty() {
        issues.push("缺少 topics".to_string());
    }
    if config.actions.is_empty() {
        issues.push("缺少 actions".to_string());
    }
    if let Err(error) = validate_config(config) {
        issues.push(error);
    }
    serde_json::json!({
        "ready": issues.is_empty(),
        "issues": issues,
    })
}

fn parse_planner(raw: Option<&Value>) -> Result<PlannerMode, String> {
    let Some(raw) = raw else {
        return Ok(PlannerMode::Agent);
    };
    let text = value_as_text(raw.clone()).trim().to_lowercase();
    match text.as_str() {
        "agent" | "turn" | "kernel" | "loop" => Ok(PlannerMode::Agent),
        "decision" | "single" | "model" | "one_shot" => Ok(PlannerMode::Decision),
        other => Err(format!(
            "mqtt.planner 只能是 agent（回合循环）或 decision（单次决策）：{other}"
        )),
    }
}

fn parse_qos(raw: Option<&Value>) -> Result<u8, String> {
    let Some(raw) = raw else { return Ok(1) };
    let number = match raw {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    };
    match number {
        Some(value) if value <= 2 => Ok(value as u8),
        _ => Err(format!("mqtt.qos 只能是 0、1 或 2：{}", display(raw))),
    }
}

fn parse_topics(raw: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut topics = Vec::new();
    match raw {
        Value::Array(items) => {
            for item in items {
                let text = value_as_text(item.clone()).replace('，', ",");
                for part in text.split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        topics.push(part.to_string());
                    }
                }
            }
        }
        other => {
            let text = value_as_text(other.clone()).replace('，', ",");
            for part in text.split(',') {
                let part = part.trim();
                if !part.is_empty() {
                    topics.push(part.to_string());
                }
            }
        }
    }
    Ok(topics)
}

fn parse_actions(raw: Option<&Value>) -> Result<Vec<ActionSpec>, String> {
    let Some(Value::Array(items)) = raw else {
        return Ok(Vec::new());
    };
    let mut actions = Vec::new();
    for item in items {
        let id = text_of(item, "id");
        if id.trim().is_empty() {
            return Err("mqtt.actions 的每一项都必须有 id。".to_string());
        }
        let mut params = Vec::new();
        if let Some(Value::Object(table)) = item.get("params") {
            for (name, spec) in table {
                let min = number_of(spec, "min")
                    .ok_or_else(|| format!("mqtt.actions.{id}.params.{name} 缺少 min。"))?;
                let max = number_of(spec, "max")
                    .ok_or_else(|| format!("mqtt.actions.{id}.params.{name} 缺少 max。"))?;
                if max < min {
                    return Err(format!(
                        "mqtt.actions.{id}.params.{name} 的 max 不能小于 min。"
                    ));
                }
                let default = number_of(spec, "default").unwrap_or(min).clamp(min, max);
                params.push(ParamSpec {
                    name: name.clone(),
                    min,
                    max,
                    default,
                });
            }
        }
        actions.push(ActionSpec {
            id: id.trim().to_string(),
            label: text_of(item, "label"),
            description: text_of(item, "description"),
            params,
        });
    }
    Ok(actions)
}

fn parse_rules(raw: Option<&Value>) -> Result<Vec<RuleSpec>, String> {
    let Some(Value::Array(items)) = raw else {
        return Ok(Vec::new());
    };
    let mut rules = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let id = text_of(item, "id");
        if id.trim().is_empty() {
            return Err(format!("mqtt.rules 第 {} 项缺少 id。", index + 1));
        }
        let metric = text_of(item, "metric");
        if metric.trim().is_empty() {
            return Err(format!("mqtt.rules.{id} 缺少 metric。"));
        }
        let op_text = text_of(item, "op");
        let op = parse_op(op_text.trim())
            .ok_or_else(|| format!("mqtt.rules.{id} 的 op 不支持：{op_text}"))?;
        let value =
            number_of(item, "value").ok_or_else(|| format!("mqtt.rules.{id} 缺少数值 value。"))?;
        let actions = parse_topics(item.get("actions"))?;
        rules.push(RuleSpec {
            id: id.trim().to_string(),
            zone: {
                let zone = text_of(item, "zone");
                if zone.trim().is_empty() {
                    "*".to_string()
                } else {
                    zone.trim().to_string()
                }
            },
            metric: metric.trim().to_string(),
            op,
            value,
            sustain_seconds: number_of(item, "sustain_seconds")
                .filter(|value| *value >= 0.0)
                .unwrap_or(0.0) as u64,
            clear_value: number_of(item, "clear_value"),
            actions,
            fallback: {
                let fallback = text_of(item, "fallback");
                if fallback.trim().is_empty() {
                    None
                } else {
                    Some(fallback.trim().to_string())
                }
            },
            fail_safe: item
                .get("fail_safe")
                .map(value_as_bool)
                .transpose()?
                .unwrap_or(false),
        });
    }
    Ok(rules)
}

fn parse_op(text: &str) -> Option<CompareOp> {
    match text {
        "gt" | ">" => Some(CompareOp::Gt),
        "ge" | ">=" => Some(CompareOp::Ge),
        "lt" | "<" => Some(CompareOp::Lt),
        "le" | "<=" => Some(CompareOp::Le),
        "eq" | "==" => Some(CompareOp::Eq),
        _ => None,
    }
}

fn text_or(section: &Value, key: &str, default: &str) -> String {
    match section.get(key) {
        Some(value) => value_as_text(value.clone()),
        None => default.to_string(),
    }
}

fn seconds_or(section: &Value, key: &str, default: u64) -> Result<u64, String> {
    match section.get(key) {
        Some(value) => {
            let parsed = match value {
                Value::Number(number) => number.as_u64(),
                Value::String(text) => text.trim().parse::<u64>().ok(),
                _ => None,
            };
            parsed.ok_or_else(|| format!("mqtt.{key} 必须是整数秒。"))
        }
        None => Ok(default),
    }
}

fn count_or(section: &Value, key: &str, default: u64) -> Result<u64, String> {
    seconds_or(section, key, default)
}

fn value_as_bool(value: &Value) -> Result<bool, String> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::String(text) => match text.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => Err(format!("mqtt.enabled 无法解析为布尔值：{other}")),
        },
        Value::Number(number) => Ok(number.as_f64().unwrap_or(0.0) != 0.0),
        _ => Err("mqtt.enabled 无法解析为布尔值。".to_string()),
    }
}

fn value_as_text(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Null => String::new(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    }
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .map(|found| value_as_text(found.clone()))
        .unwrap_or_default()
}

fn number_of(value: &Value, key: &str) -> Option<f64> {
    match value.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn display(value: &Value) -> String {
    value_as_text(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn section() -> Value {
        json!({
            "broker": "mqtt://127.0.0.1:1883",
            "topics": ["farm/+/env"],
            "actions": [
                {"id": "open_vent", "label": "打开通风口"},
                {"id": "do_nothing", "label": "维持现状"}
            ],
            "rules": [{
                "id": "hot",
                "metric": "temp",
                "op": "gt",
                "value": 28.0,
                "actions": ["open_vent", "do_nothing"],
                "fallback": "open_vent"
            }]
        })
    }

    #[test]
    fn planner_defaults_to_the_agent_loop() {
        let config = load_mqtt_config(Some(&section())).expect("配置应当可用");
        assert_eq!(config.planner, PlannerMode::Agent);
        assert!(config.tools.iter().any(|tool| tool == "read"));
        assert!(
            !config.tools.iter().any(|tool| tool == "bash"),
            "默认白名单不该包含 shell：{config:?}"
        );
        assert_eq!(config.turn_timeout_seconds, DEFAULT_TURN_TIMEOUT_SECONDS);
    }

    #[test]
    fn planner_and_tools_can_be_overridden() {
        let mut data = section();
        data["planner"] = json!("decision");
        data["tools"] = json!(["read", "memory_search"]);
        data["turn_timeout_seconds"] = json!(30);
        let config = load_mqtt_config(Some(&data)).expect("配置应当可用");
        assert_eq!(config.planner, PlannerMode::Decision);
        assert_eq!(config.tools, vec!["read".to_string(), "memory_search".to_string()]);
        assert_eq!(config.turn_timeout_seconds, 30);
    }

    #[test]
    fn unknown_planner_is_rejected() {
        let mut data = section();
        data["planner"] = json!("turbo");
        assert!(load_mqtt_config(Some(&data)).is_err());
    }
}
