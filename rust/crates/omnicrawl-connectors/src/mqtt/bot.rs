//! 触发器主循环：订阅遥测 → 聚合 → 规则 → 唤醒模型 → 校验 → 下发 → 等回执 → 审计。
//!
//! 关键边界：
//! * **模型不常驻**。模型只在规则命中时被调一次，拿到 JSON 动作就结束；两次触发之间它不存在。
//! * **单线程、单一连接**。决策是低频事件（规则 + 持续时长 + 冷却），阻塞几十秒可以接受；
//!   换来的是没有并发读写、没有跨线程同步。代价是决策期间的新遥测不参与当轮判断——
//!   它们本来也会被窗口聚合掉。
//! * **失败一律回落确定性规则**。模型超时、输出不合法、动作越界，都走规则里声明的 fallback；
//!   fallback 也没有就只记审计，绝不「判断不了就不管」。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::audit::AuditLog;
use super::client::MqttClient;
use super::codec::Packet;
use super::config::{validate_config, ActionSpec, MqttConfig};
use super::planner::Planner;
use super::policy::{parse_readings, Engine, Trigger};
use super::prompt::{command_payload, validate_plan, PlanRequest, RawPlan, ValidatedPlan};

/// 断线重连的退避上限。
const MAX_BACKOFF_SECONDS: u64 = 60;
/// 等回执时的读切片：比主循环更细，回执不必等到下一个整秒。
const ACK_SLICE: Duration = Duration::from_millis(200);

/// 一次会话的结束方式：可重连的故障，或不可恢复的配置错误。
enum ServeError {
    Fatal(String),
    Reconnect(String),
}

/// 指令回执的等待结果，直接进审计。
#[derive(Debug, Clone, PartialEq)]
pub enum AckOutcome {
    /// 没有配置回执主题。
    NotConfigured,
    /// 收到同 request_id 的回执。
    Received(String),
    /// 等超时了：QoS1 只保证到达 broker，不保证执行。
    Timeout,
    /// 等回执期间连接断了。
    Failed(String),
}

impl AckOutcome {
    fn as_str(&self) -> &'static str {
        match self {
            AckOutcome::NotConfigured => "not_configured",
            AckOutcome::Received(_) => "received",
            AckOutcome::Timeout => "timeout",
            AckOutcome::Failed(_) => "failed",
        }
    }
}

/// MQTT 自动化回路。
pub struct MqttBot {
    config: MqttConfig,
    planner: Arc<dyn Planner>,
    audit: AuditLog,
    engine: Engine,
    stop: Arc<AtomicBool>,
    seq: u64,
}

impl MqttBot {
    /// 装配回路：校验配置、建策略引擎、从审计回放冷却状态。
    pub fn new(
        config: MqttConfig,
        planner: Arc<dyn Planner>,
        audit: AuditLog,
    ) -> Result<Self, String> {
        validate_config(&config)?;
        let engine = Engine::new(
            config.rules.clone(),
            config.window_seconds,
            config.cooldown_seconds,
        );
        let mut bot = Self {
            config,
            planner,
            audit,
            engine,
            stop: Arc::new(AtomicBool::new(false)),
            seq: 0,
        };
        bot.restore_cooldowns();
        Ok(bot)
    }

    /// 外部停止信号（测试与和平退出用）。
    pub fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.stop = stop;
        self
    }

    /// 重启后不能立刻重复动作：用审计里的最后动作重建冷却。
    fn restore_cooldowns(&mut self) {
        for entry in self.audit.recent(None, 200) {
            let zone = entry.get("zone").and_then(Value::as_str).unwrap_or_default();
            let rule = entry.get("rule").and_then(Value::as_str).unwrap_or_default();
            let ts = entry.get("ts_ms").and_then(Value::as_i64).unwrap_or_default();
            if zone.is_empty() || rule.is_empty() || ts <= 0 {
                continue;
            }
            self.engine.note_action(zone, rule, ts);
        }
    }

    /// 主循环：连不上就退避重连，连上就进读循环。
    pub fn run(&mut self) -> Result<(), String> {
        log(format!(
            "启动：broker={} 订阅={:?} 规则={} 动作={} 规划器={}",
            self.config.broker,
            self.config.topics,
            self.config.rules.len(),
            self.config.actions.len(),
            self.planner.describe()
        ));
        log(format!("审计文件：{}", self.audit.path().display()));
        let mut backoff = Duration::from_secs(1);
        while !self.stop.load(Ordering::Relaxed) {
            let mut client = match MqttClient::connect(&self.config) {
                Ok(client) => client,
                Err(error) => {
                    if is_fatal(&error) {
                        return Err(error);
                    }
                    log(format!(
                        "连接失败：{error}；{} 秒后重试。",
                        backoff.as_secs()
                    ));
                    sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(MAX_BACKOFF_SECONDS));
                    continue;
                }
            };
            backoff = Duration::from_secs(1);
            match self.serve(&mut client) {
                Ok(()) => {
                    client.disconnect();
                    return Ok(());
                }
                Err(ServeError::Fatal(message)) => {
                    client.disconnect();
                    return Err(message);
                }
                Err(ServeError::Reconnect(message)) => {
                    client.disconnect();
                    log(format!("会话中断：{message}；正在重连。"));
                }
            }
        }
        Ok(())
    }

    /// 单次会话：订阅后进入读循环。
    fn serve(&mut self, client: &mut MqttClient) -> Result<(), ServeError> {
        // 订阅被 broker 拒绝（主题过滤器非法）重连也不会好：按配置错误上报。
        client
            .subscribe(&self.config.topics, self.config.qos)
            .map_err(ServeError::Fatal)?;
        log("已连接并完成订阅。");
        while !self.stop.load(Ordering::Relaxed) {
            if client.keepalive_due() {
                client.ping().map_err(ServeError::Reconnect)?;
            }
            match client.poll(Duration::from_secs(self.config.poll_seconds)) {
                Ok(Some(packet)) => self.handle_packet(client, packet)?,
                Ok(None) => self.engine.prune(now_ms()),
                Err(error) => return Err(ServeError::Reconnect(error)),
            }
        }
        Ok(())
    }

    fn handle_packet(
        &mut self,
        client: &mut MqttClient,
        packet: Packet,
    ) -> Result<(), ServeError> {
        let Packet::Publish {
            topic,
            payload,
            qos,
            packet_id,
            ..
        } = packet
        else {
            return Ok(());
        };
        // 订阅是 QoS1 时，入站发布必须先确认，否则 broker 会一直重投同一条遥测。
        if qos > 0 {
            if let Some(packet_id) = packet_id {
                client.ack_publish(packet_id).map_err(ServeError::Reconnect)?;
            }
        }
        let readings = parse_readings(
            &topic,
            &payload,
            &self.config.zone_field,
            self.config.zone_topic_level,
            now_ms(),
        );
        if readings.is_empty() {
            log(format!("收到无法解析的报文（{topic}），已忽略。"));
            return Ok(());
        }
        for reading in readings {
            let now = now_ms();
            let triggers = self.engine.ingest(&reading, now);
            for trigger in triggers {
                self.handle_trigger(client, trigger)?;
            }
        }
        Ok(())
    }

    /// 一条触发：模型给动作，宿主编译并下发。
    fn handle_trigger(
        &mut self,
        client: &mut MqttClient,
        trigger: Trigger,
    ) -> Result<(), ServeError> {
        let now = now_ms();
        let actions = self.candidate_actions(&trigger);
        let mut failure: Option<String> = None;

        let plan = if trigger.fail_safe {
            log(format!(
                "安全规则命中：{} / {}={}（不等模型）。",
                trigger.rule_id, trigger.metric, trigger.value
            ));
            self.fallback_plan(&trigger)
        } else {
            match self.ask_model(&trigger, &actions) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    log(format!("模型未能给出可用动作：{error}"));
                    failure = Some(error);
                    self.fallback_plan(&trigger)
                }
            }
        };

        let Some(plan) = plan else {
            self.write_audit(&trigger, None, "", AckOutcome::NotConfigured, failure);
            self.engine.note_action(&trigger.zone, &trigger.rule_id, now);
            return Ok(());
        };

        if plan.action == "do_nothing" {
            log(format!(
                "{}：维持现状（{}）。",
                trigger.zone,
                if plan.reason.is_empty() {
                    "模型未给理由"
                } else {
                    plan.reason.as_str()
                }
            ));
            self.write_audit(&trigger, Some(&plan), "", AckOutcome::NotConfigured, failure);
            self.engine.note_action(&trigger.zone, &trigger.rule_id, now);
            return Ok(());
        }

        self.seq += 1;
        let request_id = format!("{now}-{}", self.seq);
        let issued_at = now_iso();
        let payload = command_payload(
            &trigger.zone,
            &plan,
            &request_id,
            self.config.ttl_seconds,
            &issued_at,
        );
        let topic = self.config.command_topic_for(&trigger.zone);
        let bytes = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
        client
            .publish(&topic, &bytes, self.config.qos)
            .map_err(ServeError::Reconnect)?;
        log(format!(
            "{}：下发 {} → {}（{}）",
            trigger.zone,
            plan.action,
            topic,
            if plan.reason.is_empty() {
                "无理由"
            } else {
                plan.reason.as_str()
            }
        ));

        let ack = self.wait_ack(client, &trigger.zone, &request_id);
        if let AckOutcome::Timeout = ack {
            log(format!(
                "{}：指令 {} 未在 {} 秒内收到回执。",
                trigger.zone, request_id, self.config.ack_timeout_seconds
            ));
        }
        self.write_audit(&trigger, Some(&plan), &request_id, ack, failure);
        self.engine.note_action(&trigger.zone, &trigger.rule_id, now);
        Ok(())
    }

    /// 规则声明的候选动作（按配置顺序，do_nothing 也在里面）。
    fn candidate_actions(&self, trigger: &Trigger) -> Vec<ActionSpec> {
        trigger
            .actions
            .iter()
            .filter_map(|id| self.config.action(id).cloned())
            .collect()
    }

    /// 唤醒模型一次：模板由 prompt 层拼，校验也在那里做。
    fn ask_model(
        &self,
        trigger: &Trigger,
        actions: &[ActionSpec],
    ) -> Result<ValidatedPlan, String> {
        if actions.is_empty() {
            return Err("规则没有可用的候选动作。".to_string());
        }
        let request = PlanRequest {
            zone: trigger.zone.clone(),
            rule_id: trigger.rule_id.clone(),
            metric: trigger.metric.clone(),
            value: trigger.value,
            threshold: trigger.threshold,
            op: trigger.op,
            snapshot: trigger.snapshot.clone(),
            actions: actions.to_vec(),
            recent: self
                .audit
                .recent(Some(&trigger.zone), self.config.recent_actions),
            now: now_iso(),
        };
        let raw = self.planner.plan(&request)?;
        validate_plan(&raw, actions)
    }

    /// 规则兜底：不经过模型，直接取规则声明的 fallback。
    fn fallback_plan(&self, trigger: &Trigger) -> Option<ValidatedPlan> {
        let fallback = trigger.fallback.as_deref()?;
        let spec = self.config.action(fallback)?;
        let raw = RawPlan {
            action: fallback.to_string(),
            params: json!({}),
            reason: "规则兜底：模型不可用或输出不合法，按预设动作执行".to_string(),
            confidence: None,
            source: "fail_safe",
        };
        validate_plan(&raw, std::slice::from_ref(spec)).ok()
    }

    /// 等设备回执：只认同主题、且 request_id 一致（或没带 request_id）的报文。
    fn wait_ack(&mut self, client: &mut MqttClient, zone: &str, request_id: &str) -> AckOutcome {
        let Some(topic) = self.config.ack_topic_for(zone) else {
            return AckOutcome::NotConfigured;
        };
        let deadline = Instant::now() + Duration::from_secs(self.config.ack_timeout_seconds);
        while Instant::now() < deadline {
            if client.keepalive_due() {
                if let Err(error) = client.ping() {
                    return AckOutcome::Failed(error);
                }
            }
            match client.poll(ACK_SLICE) {
                Ok(Some(Packet::Publish {
                    topic: incoming,
                    payload,
                    qos,
                    packet_id,
                    ..
                })) => {
                    if qos > 0 {
                        if let Some(packet_id) = packet_id {
                            if let Err(error) = client.ack_publish(packet_id) {
                                return AckOutcome::Failed(error);
                            }
                        }
                    }
                    if incoming != topic {
                        // 等回执期间的遥测不参与当轮判断：动作已完成，下一帧遥测会重建窗口。
                        continue;
                    }
                    let text = String::from_utf8_lossy(&payload).trim().to_string();
                    if ack_matches(&text, request_id) {
                        return AckOutcome::Received(truncate(&text, 200));
                    }
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(error) => return AckOutcome::Failed(error),
            }
        }
        AckOutcome::Timeout
    }

    fn write_audit(
        &self,
        trigger: &Trigger,
        plan: Option<&ValidatedPlan>,
        request_id: &str,
        ack: AckOutcome,
        failure: Option<String>,
    ) {
        let entry = json!({
            "ts": now_iso(),
            "ts_ms": trigger.triggered_at_ms,
            "zone": trigger.zone,
            "rule": trigger.rule_id,
            "metric": trigger.metric,
            "value": trigger.value,
            "threshold": trigger.threshold,
            "op": trigger.op.as_str(),
            "snapshot": trigger.snapshot,
            "action": plan.map(|plan| plan.action.clone()),
            "params": plan.map(|plan| Value::Object(plan.params.clone())),
            "reason": plan.map(|plan| plan.reason.clone()),
            "confidence": plan.and_then(|plan| plan.confidence),
            "source": plan.map(|plan| plan.source),
            "request_id": request_id,
            "ack": ack.as_str(),
            "ack_detail": match &ack {
                AckOutcome::Received(text) => Some(text.clone()),
                AckOutcome::Failed(error) => Some(error.clone()),
                _ => None,
            },
            "failure": failure,
        });
        if let Err(error) = self.audit.append(&entry) {
            log(format!("写审计失败：{error}"));
        }
    }
}

/// 回执是否对应本次指令：没带 request_id 的设备按「同主题即认」处理。
fn ack_matches(text: &str, request_id: &str) -> bool {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => match value.get("request_id").and_then(Value::as_str) {
            Some(found) => found == request_id,
            None => true,
        },
        Err(_) => true,
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

/// 认证类失败重试也没用：直接退回让用户改配置。
fn is_fatal(error: &str) -> bool {
    error.contains("用户名或密码错误") || error.contains("未授权") || error.contains("client_id 被拒绝")
}

fn log(message: impl AsRef<str>) {
    eprintln!("[mqtt] {}", message.as_ref());
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_requires_matching_request_id() {
        assert!(ack_matches("{\"request_id\":\"7-1\",\"ok\":true}", "7-1"));
        assert!(!ack_matches("{\"request_id\":\"7-2\",\"ok\":true}", "7-1"));
        assert!(ack_matches("{\"ok\":true}", "7-1"));
        assert!(ack_matches("ok", "7-1"));
    }

    #[test]
    fn auth_failures_are_fatal() {
        assert!(is_fatal("broker 拒绝连接（用户名或密码错误）：4"));
        assert!(!is_fatal("连接 broker 失败（10.0.0.1:1883）：Connection refused"));
    }
}
