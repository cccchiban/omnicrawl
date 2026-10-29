//! 确定性策略层：窗口聚合、阈值规则、迟滞、冷却与触发去重。
//!
//! 这一层永远跑在模型之前，且不依赖任何模型：只有它判断「值得干预」，才会去拼模板唤醒模型。
//! 判断「要不要叫醒模型」这件事不能交给模型自己——否则稳态数据也会把 token 烧光。

use std::collections::BTreeMap;

use serde_json::json;

use super::config::{CompareOp, RuleSpec};

/// 一条由 payload 解析出来的读数。
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub zone: String,
    pub metric: String,
    pub value: f64,
    pub ts_ms: i64,
}

/// 一个 (棚, 指标) 的滑动窗口统计：决策看的是这一段，而不是最后一条。
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub zone: String,
    pub metric: String,
    pub count: u64,
    pub last: f64,
    pub min: f64,
    pub max: f64,
    pub first_ms: i64,
    pub last_ms: i64,
}

impl Window {
    fn new(reading: &Reading) -> Self {
        Self {
            zone: reading.zone.clone(),
            metric: reading.metric.clone(),
            count: 1,
            last: reading.value,
            min: reading.value,
            max: reading.value,
            first_ms: reading.ts_ms,
            last_ms: reading.ts_ms,
        }
    }

    fn observe(&mut self, value: f64, ts_ms: i64) {
        self.count += 1;
        self.last = value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.last_ms = ts_ms;
    }

    /// 进模板的投影：模型只需要分布与边界，不需要逐条时序。
    pub fn to_value(&self) -> serde_json::Value {
        json!({
            "last": round2(self.last),
            "min": round2(self.min),
            "max": round2(self.max),
            "count": self.count,
            "first_ms": self.first_ms,
            "last_ms": self.last_ms,
        })
    }
}

/// 保留两位小数：模板里的浮点噪声没有信息量。
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 规则状态机：待定（条件已满足但没到持续时长）→ 已触发（等恢复）。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Pending { since_ms: i64 },
    Fired,
}

/// 一次触发：规则命中了，需要唤醒模型（或直接执行安全兜底）。
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub zone: String,
    pub rule_id: String,
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    pub op: CompareOp,
    /// 规则是否标记为安全兜底：这类触发不叫模型，直接执行 fallback。
    pub fail_safe: bool,
    pub actions: Vec<String>,
    pub fallback: Option<String>,
    /// 该棚当前所有指标的窗口快照（进模板用）。
    pub snapshot: serde_json::Value,
    pub triggered_at_ms: i64,
}

/// 策略引擎：规则表 + 每 (棚, 指标) 窗口 + 每 (棚, 规则) 状态。
pub struct Engine {
    rules: Vec<RuleSpec>,
    phases: Vec<Phase>,
    windows: BTreeMap<(String, String), Window>,
    /// 上次真正下发动作的时间：冷却按 (棚, 规则) 记住。
    last_action_ms: BTreeMap<(String, String), i64>,
    window_ms: i64,
    cooldown_ms: i64,
}

impl Engine {
    pub fn new(rules: Vec<RuleSpec>, window_seconds: u64, cooldown_seconds: u64) -> Self {
        let phases = vec![Phase::Idle; rules.len()];
        Self {
            rules,
            phases,
            windows: BTreeMap::new(),
            last_action_ms: BTreeMap::new(),
            window_ms: window_seconds as i64 * 1000,
            cooldown_ms: cooldown_seconds as i64 * 1000,
        }
    }

    pub fn rules(&self) -> &[RuleSpec] {
        &self.rules
    }

    pub fn rule(&self, id: &str) -> Option<&RuleSpec> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// 吃进一条读数，返回本轮需要唤醒模型的触发（通常为空）。
    pub fn ingest(&mut self, reading: &Reading, now_ms: i64) -> Vec<Trigger> {
        let key = (reading.zone.clone(), reading.metric.clone());
        match self.windows.get_mut(&key) {
            Some(window) if now_ms - window.last_ms <= self.window_ms => {
                window.observe(reading.value, reading.ts_ms);
            }
            _ => {
                self.windows.insert(key.clone(), Window::new(reading));
            }
        }

        let mut triggers = Vec::new();
        for index in 0..self.rules.len() {
            // 先取快照再推进状态机：advance 要可变借用，不能同时持有规则引用。
            let (matches, rule_id) = {
                let rule = &self.rules[index];
                let matches = rule.metric == reading.metric
                    && (rule.zone == "*" || rule.zone == reading.zone);
                (matches, rule.id.clone())
            };
            if !matches {
                continue;
            }
            if !self.advance(index, reading, now_ms) {
                continue;
            }
            let cooldown_key = (reading.zone.clone(), rule_id);
            if let Some(last) = self.last_action_ms.get(&cooldown_key) {
                if now_ms - *last < self.cooldown_ms {
                    continue;
                }
            }
            triggers.push(self.trigger(index, reading, now_ms));
        }
        triggers
    }

    /// 推进一条规则的状态机；返回 true 表示「此刻可以动作」。
    fn advance(&mut self, index: usize, reading: &Reading, now_ms: i64) -> bool {
        let rule = &self.rules[index];
        let holds = rule.op.holds(reading.value, rule.value);
        let recovered = match self.phases[index] {
            Phase::Fired => self.recovered(rule, reading.value),
            _ => false,
        };
        if recovered {
            self.phases[index] = Phase::Idle;
        }
        match self.phases[index] {
            Phase::Fired => false,
            _ => {
                if !holds {
                    self.phases[index] = Phase::Idle;
                    return false;
                }
                match self.phases[index] {
                    Phase::Pending { since_ms } => {
                        if now_ms - since_ms >= rule.sustain_seconds as i64 * 1000 {
                            self.phases[index] = Phase::Fired;
                            true
                        } else {
                            false
                        }
                    }
                    _ => {
                        if rule.sustain_seconds == 0 {
                            self.phases[index] = Phase::Fired;
                            true
                        } else {
                            self.phases[index] = Phase::Pending { since_ms: now_ms };
                            false
                        }
                    }
                }
            }
        }
    }

    /// 已触发后是否恢复：配了迟滞阈值就按它，否则按原阈值反向判定。
    fn recovered(&self, rule: &RuleSpec, value: f64) -> bool {
        match rule.clear_value {
            Some(clear) => match rule.op {
                CompareOp::Gt | CompareOp::Ge => value < clear,
                CompareOp::Lt | CompareOp::Le => value > clear,
                CompareOp::Eq => (value - clear).abs() >= f64::EPSILON,
            },
            None => !rule.op.holds(value, rule.value),
        }
    }

    fn trigger(&self, index: usize, reading: &Reading, now_ms: i64) -> Trigger {
        let rule = &self.rules[index];
        Trigger {
            zone: reading.zone.clone(),
            rule_id: rule.id.clone(),
            metric: rule.metric.clone(),
            value: reading.value,
            threshold: rule.value,
            op: rule.op,
            fail_safe: rule.fail_safe,
            actions: rule.actions.clone(),
            fallback: rule.fallback.clone(),
            snapshot: self.snapshot(&reading.zone),
            triggered_at_ms: now_ms,
        }
    }

    /// 记下「这个棚这条规则已经动作过」，冷却从这里开始算。
    pub fn note_action(&mut self, zone: &str, rule_id: &str, now_ms: i64) {
        self.last_action_ms
            .insert((zone.to_string(), rule_id.to_string()), now_ms);
    }

    /// 某棚所有指标的窗口快照；窗口过期的指标不进快照。
    pub fn snapshot(&self, zone: &str) -> serde_json::Value {
        let mut metrics = serde_json::Map::new();
        for ((window_zone, metric), window) in &self.windows {
            if window_zone != zone {
                continue;
            }
            metrics.insert(metric.clone(), window.to_value());
        }
        json!(metrics)
    }

    /// 清掉过期窗口：没有新读数的指标不该永远占着模板位置。
    pub fn prune(&mut self, now_ms: i64) {
        let window_ms = self.window_ms;
        self.windows
            .retain(|_, window| now_ms - window.last_ms <= window_ms);
    }
}

/// 从一条遥测报文里解析读数：数值字段各算一个指标，嵌套对象展开一层（用点号连接）。
///
/// 棚号优先取 payload 的 zone 字段，缺失时回落到主题层级——同一批设备两种写法都常见。
/// 非 JSON、非对象、取不到棚号的报文一律丢弃（返回空表），坏包不进窗口。
pub fn parse_readings(
    topic: &str,
    payload: &[u8],
    zone_field: &str,
    zone_level: usize,
    now_ms: i64,
) -> Vec<Reading> {
    let Ok(text) = std::str::from_utf8(payload) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let Some(zone) = zone_of(topic, object, zone_field, zone_level) else {
        return Vec::new();
    };
    let ts = timestamp_of(object).unwrap_or(now_ms);
    let mut readings = Vec::new();
    for (key, item) in object {
        if key == zone_field || key == "ts" || key == "timestamp" {
            continue;
        }
        collect_metric(key, item, &zone, ts, &mut readings);
    }
    readings
}

fn collect_metric(
    name: &str,
    item: &serde_json::Value,
    zone: &str,
    ts_ms: i64,
    out: &mut Vec<Reading>,
) {
    match item {
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_f64() {
                out.push(Reading {
                    zone: zone.to_string(),
                    metric: name.to_string(),
                    value,
                    ts_ms,
                });
            }
        }
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                collect_metric(&format!("{name}.{key}"), nested, zone, ts_ms, out);
            }
        }
        _ => {}
    }
}

fn zone_of(
    topic: &str,
    object: &serde_json::Map<String, serde_json::Value>,
    zone_field: &str,
    zone_level: usize,
) -> Option<String> {
    if let Some(value) = object.get(zone_field) {
        let text = match value {
            serde_json::Value::String(text) => text.trim().to_string(),
            serde_json::Value::Number(number) => number.to_string(),
            _ => String::new(),
        };
        if !text.is_empty() {
            return Some(text);
        }
    }
    let segment = topic.split('/').nth(zone_level)?.trim();
    if segment.is_empty() || segment.contains('+') || segment.contains('#') {
        return None;
    }
    Some(segment.to_string())
}

/// 报文里的时间戳：毫秒（>= 1e12）、秒（>= 1e9）都接受，其余按到达时间。
fn timestamp_of(object: &serde_json::Map<String, serde_json::Value>) -> Option<i64> {
    let raw = object.get("ts").or_else(|| object.get("timestamp"))?;
    let value = match raw {
        serde_json::Value::Number(number) => number.as_f64()?,
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if value >= 1_000_000_000_000.0 {
        Some(value as i64)
    } else if value >= 1_000_000_000.0 {
        Some((value * 1000.0) as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::RuleSpec;
    use super::*;

    fn rule(id: &str, metric: &str, op: CompareOp, value: f64, sustain: u64) -> RuleSpec {
        RuleSpec {
            id: id.to_string(),
            zone: "*".to_string(),
            metric: metric.to_string(),
            op,
            value,
            sustain_seconds: sustain,
            clear_value: None,
            actions: vec!["open_vent".to_string(), "do_nothing".to_string()],
            fallback: Some("open_vent".to_string()),
            fail_safe: false,
        }
    }

    fn reading(metric: &str, value: f64, ts_ms: i64) -> Reading {
        Reading {
            zone: "3".to_string(),
            metric: metric.to_string(),
            value,
            ts_ms,
        }
    }

    #[test]
    fn sustain_must_elapse_before_firing() {
        let mut engine = Engine::new(vec![rule("hot", "temp", CompareOp::Gt, 28.0, 30)], 30, 1800);
        assert!(engine.ingest(&reading("temp", 31.0, 0), 0).is_empty());
        assert!(engine.ingest(&reading("temp", 31.5, 10_000), 10_000).is_empty());
        assert!(engine.ingest(&reading("temp", 32.0, 29_000), 29_000).is_empty());
        let fired = engine.ingest(&reading("temp", 33.0, 30_000), 30_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].rule_id, "hot");
        assert_eq!(fired[0].value, 33.0);
    }

    #[test]
    fn condition_clearing_resets_the_pending_window() {
        let mut engine = Engine::new(vec![rule("hot", "temp", CompareOp::Gt, 28.0, 30)], 30, 0);
        assert!(engine.ingest(&reading("temp", 31.0, 0), 0).is_empty());
        assert!(engine.ingest(&reading("temp", 20.0, 20_000), 20_000).is_empty());
        assert!(engine.ingest(&reading("temp", 31.0, 30_000), 30_000).is_empty());
        // 重新计时后仍要满 30 秒。
        assert!(engine.ingest(&reading("temp", 31.0, 59_000), 59_000).is_empty());
        assert_eq!(engine.ingest(&reading("temp", 31.0, 60_000), 60_000).len(), 1);
    }

    #[test]
    fn hysteresis_needs_the_clear_threshold_before_rearming() {
        let mut spec = rule("hot", "temp", CompareOp::Gt, 28.0, 0);
        spec.clear_value = Some(26.0);
        let mut engine = Engine::new(vec![spec], 30, 0);
        assert_eq!(engine.ingest(&reading("temp", 30.0, 0), 0).len(), 1);
        // 掉到 27℃：低于阈值但没到恢复线，仍算未恢复。
        assert!(engine.ingest(&reading("temp", 27.0, 10_000), 10_000).is_empty());
        assert!(engine.ingest(&reading("temp", 30.0, 20_000), 20_000).is_empty());
        // 掉到 25℃ 才算恢复。
        assert!(engine.ingest(&reading("temp", 25.0, 30_000), 30_000).is_empty());
        assert_eq!(engine.ingest(&reading("temp", 30.0, 40_000), 40_000).len(), 1);
    }

    #[test]
    fn cooldown_blocks_a_second_action_on_the_same_rule() {
        let mut engine = Engine::new(vec![rule("hot", "temp", CompareOp::Gt, 28.0, 0)], 30, 600);
        assert_eq!(engine.ingest(&reading("temp", 30.0, 0), 0).len(), 1);
        engine.note_action("3", "hot", 0);
        engine.ingest(&reading("temp", 20.0, 1000), 1000);
        assert!(engine.ingest(&reading("temp", 30.0, 2000), 2000).is_empty());
        engine.ingest(&reading("temp", 20.0, 300_000), 300_000);
        assert!(engine.ingest(&reading("temp", 30.0, 400_000), 400_000).is_empty());
        engine.ingest(&reading("temp", 20.0, 601_000), 601_000);
        assert_eq!(engine.ingest(&reading("temp", 30.0, 602_000), 602_000).len(), 1);
    }

    #[test]
    fn zone_scope_keeps_other_greenhouses_quiet() {
        let mut spec = rule("hot", "temp", CompareOp::Gt, 28.0, 0);
        spec.zone = "1".to_string();
        let mut engine = Engine::new(vec![spec], 30, 0);
        assert!(engine.ingest(&reading("temp", 40.0, 0), 0).is_empty());
        let mut other = reading("temp", 40.0, 0);
        other.zone = "1".to_string();
        assert_eq!(engine.ingest(&other, 0).len(), 1);
    }

    #[test]
    fn window_aggregates_min_max_and_count() {
        let mut engine = Engine::new(Vec::new(), 30, 0);
        engine.ingest(&reading("humidity", 60.0, 0), 0);
        engine.ingest(&reading("humidity", 71.0, 5_000), 5_000);
        engine.ingest(&reading("humidity", 55.0, 10_000), 10_000);
        let snapshot = engine.snapshot("3");
        let humidity = &snapshot["humidity"];
        assert_eq!(humidity["last"], 55.0);
        assert_eq!(humidity["min"], 55.0);
        assert_eq!(humidity["max"], 71.0);
        assert_eq!(humidity["count"], 3);
    }

    #[test]
    fn stale_windows_are_dropped_by_prune() {
        let mut engine = Engine::new(Vec::new(), 30, 0);
        engine.ingest(&reading("temp", 20.0, 0), 0);
        engine.prune(60_000);
        assert!(engine.snapshot("3").as_object().unwrap().is_empty());
    }

    #[test]
    fn fail_safe_rules_fire_immediately() {
        let mut spec = rule("critical_heat", "temp", CompareOp::Gt, 38.0, 0);
        spec.fail_safe = true;
        let mut engine = Engine::new(vec![spec], 30, 0);
        let fired = engine.ingest(&reading("temp", 39.0, 0), 0);
        assert_eq!(fired.len(), 1);
        assert!(fired[0].fail_safe);
    }
}
