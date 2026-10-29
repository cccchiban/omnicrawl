//! 回路端到端：本地假 broker 跑一遍「遥测 → 规则 → 唤醒 → 下发 → 回执 → 审计」。
//!
//! 假 broker 只做握手、订阅应答与两轮发布，不验证 MQTT 之外的东西；它证明的是主循环的
//! 顺序与边界：规则命中才发布、指令带 request_id 与 TTL、回执认同一 request_id、
//! 模型失败时走规则兜底、每次决策都落一行审计。

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use omnicrawl_connectors::mqtt::codec::{self, Packet};
use omnicrawl_connectors::mqtt::{
    load_mqtt_config, AuditLog, MqttBot, PlanRequest, Planner, RawPlan,
};
use serde_json::{json, Value};

/// 固定返回一个动作的规划器。
struct StubPlanner {
    action: &'static str,
    fail: bool,
}

impl Planner for StubPlanner {
    fn plan(&self, request: &PlanRequest) -> Result<RawPlan, String> {
        if self.fail {
            return Err("模拟决策服务不可用".to_string());
        }
        assert_eq!(request.zone, "3");
        assert!(request.actions.iter().any(|action| action.id == self.action));
        Ok(RawPlan {
            action: self.action.to_string(),
            params: json!({}),
            reason: "测试".to_string(),
            confidence: Some(0.9),
            source: "decision",
        })
    }

    fn describe(&self) -> &'static str {
        "stub"
    }
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("oc-mqtt-{tag}-{}-{nanos}", std::process::id()))
}

/// 假 broker：握手 → 一家订阅 → 发一条越界遥测 → 收指令 → 回执 → 关连接。
fn spawn_broker(listener: TcpListener, received: Arc<Mutex<Vec<Value>>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        let mut reader = socket.try_clone().expect("clone");

        match codec::read_packet(&mut reader).expect("connect") {
            Packet::Other { packet_type } => assert_eq!(packet_type, codec::CONNECT),
            other => panic!("期望 CONNECT，收到 {other:?}"),
        }
        write(&mut socket, &codec::connack_packet(false, 0));

        let packet_id = match codec::read_packet(&mut reader).expect("subscribe") {
            Packet::Subscribe { packet_id, topics } => {
                assert_eq!(topics[0].0, "farm/+/env");
                packet_id
            }
            other => panic!("期望 SUBSCRIBE，收到 {other:?}"),
        };
        write(&mut socket, &codec::suback_packet(packet_id, &[1]));

        write(
            &mut socket,
            &codec::publish_packet(
                "farm/3/env",
                br#"{"zone":"3","temp":31.2,"humidity":60}"#,
                0,
                None,
                false,
            ),
        );

        let body = loop {
            // 读超时即视为「本轮没有指令」：do_nothing 场景下 broker 就该这样收场。
            let Some(packet) = codec::read_packet(&mut reader).ok() else {
                return;
            };
            match packet {
                Packet::Publish {
                    topic,
                    payload,
                    qos,
                    packet_id,
                    ..
                } => {
                    if qos > 0 {
                        write(&mut socket, &codec::puback_packet(packet_id.expect("packet id")));
                    }
                    if topic == "farm/3/command" {
                        break payload;
                    }
                }
                Packet::Other { .. } => {}
                other => panic!("期望 PUBLISH，收到 {other:?}"),
            }
        };

        let value: Value = serde_json::from_slice(&body).expect("command json");
        received.lock().expect("lock").push(value.clone());
        let request_id = value["request_id"].as_str().unwrap_or_default();
        let ack = format!("{{\"request_id\":\"{request_id}\",\"ok\":true}}");
        write(
            &mut socket,
            &codec::publish_packet("farm/3/ack", ack.as_bytes(), 0, None, false),
        );
        thread::sleep(Duration::from_millis(300));
    })
}

fn write(socket: &mut std::net::TcpStream, bytes: &[u8]) {
    use std::io::Write;
    socket.write_all(bytes).expect("write");
    socket.flush().expect("flush");
}

fn config_for(port: u16, audit: &std::path::Path, fallback: &str) -> omnicrawl_connectors::mqtt::MqttConfig {
    load_mqtt_config(Some(&json!({
        "broker": format!("mqtt://127.0.0.1:{port}"),
        "client_id": "oc-mqtt-test",
        "topics": ["farm/+/env"],
        "window_seconds": 5,
        "cooldown_seconds": 60,
        "ack_timeout_seconds": 5,
        "poll_seconds": 1,
        "audit_path": audit.to_string_lossy(),
        "actions": [
            {"id": "open_vent", "label": "打开通风口", "description": "温度偏高时通风降温"},
            {"id": "do_nothing", "label": "维持现状", "description": "无需干预"}
        ],
        "rules": [{
            "id": "hot",
            "metric": "temp",
            "op": "gt",
            "value": 28.0,
            "sustain_seconds": 0,
            "actions": ["open_vent", "do_nothing"],
            "fallback": fallback
        }]
    })))
    .expect("config")
}

/// 跑一次完整回路，返回（指令负载，审计行）。
fn run_once(
    planner: Arc<dyn Planner>,
    fallback: &str,
    tag: &str,
) -> (Value, Vec<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let received = Arc::new(Mutex::new(Vec::new()));
    let broker = spawn_broker(listener, Arc::clone(&received));

    let dir = unique_dir(tag);
    let audit_path = dir.join("audit.jsonl");
    let config = config_for(port, &audit_path, fallback);
    let audit = AuditLog::open(&audit_path).expect("audit");
    let stop = Arc::new(AtomicBool::new(false));
    let bot = MqttBot::new(config, planner, audit)
        .expect("bot")
        .with_stop(Arc::clone(&stop));
    let handle = thread::spawn(move || {
        let mut bot = bot;
        bot.run()
    });

    let deadline = Instant::now() + Duration::from_secs(15);
    while received.lock().expect("lock").is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let done = std::fs::read_to_string(&audit_path)
            .map(|text| text.lines().any(|line| !line.trim().is_empty()))
            .unwrap_or(false);
        if done {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);
    let result = handle.join().expect("join");
    broker.join().expect("broker");
    assert!(result.is_ok(), "回路异常退出：{result:?}");

    let command = received
        .lock()
        .expect("lock")
        .first()
        .cloned()
        .expect("broker 没有收到指令");
    let entries = std::fs::read_to_string(&audit_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    let _ = std::fs::remove_dir_all(&dir);
    (command, entries)
}

#[test]
fn telemetry_rule_triggers_a_command_with_ack() {
    let planner = Arc::new(StubPlanner {
        action: "open_vent",
        fail: false,
    });
    let (command, entries) = run_once(planner, "do_nothing", "happy");

    assert_eq!(command["zone"], "3");
    assert_eq!(command["action"], "open_vent");
    assert_eq!(command["source"], "decision");
    assert!(command["request_id"].as_str().unwrap_or_default().contains('-'));
    assert!(command["ttl_seconds"].as_u64().unwrap_or(0) > 0);

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["rule"], "hot");
    assert_eq!(entries[0]["ack"], "received");
    assert_eq!(entries[0]["action"], "open_vent");
    assert_eq!(entries[0]["metric"], "temp");
    assert!(entries[0]["snapshot"]["temp"]["max"].as_f64().unwrap_or(0.0) >= 31.2);
}

#[test]
fn planner_failure_falls_back_to_the_rule_action() {
    let planner = Arc::new(StubPlanner {
        action: "open_vent",
        fail: true,
    });
    let (command, entries) = run_once(planner, "open_vent", "fallback");

    assert_eq!(command["action"], "open_vent");
    assert_eq!(command["source"], "fail_safe");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["source"], "fail_safe");
    assert!(entries[0]["failure"]
        .as_str()
        .unwrap_or_default()
        .contains("模拟决策服务不可用"));
}

#[test]
fn do_nothing_is_audited_without_publishing() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let received = Arc::new(Mutex::new(Vec::new()));
    let broker = spawn_broker(listener, Arc::clone(&received));

    let dir = unique_dir("noop");
    let audit_path = dir.join("audit.jsonl");
    let config = config_for(port, &audit_path, "do_nothing");
    let audit = AuditLog::open(&audit_path).expect("audit");
    let stop = Arc::new(AtomicBool::new(false));
    let planner = Arc::new(StubPlanner {
        action: "do_nothing",
        fail: false,
    });
    let bot = MqttBot::new(config, planner, audit)
        .expect("bot")
        .with_stop(Arc::clone(&stop));
    let handle = thread::spawn(move || {
        let mut bot = bot;
        bot.run()
    });

    // 没有指令可等：等审计落一行就够。
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::fs::read_to_string(&audit_path)
            .map(|text| text.lines().any(|line| !line.trim().is_empty()))
            .unwrap_or(false)
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join().expect("join");
    let _ = broker.join();

    assert!(received.lock().expect("lock").is_empty(), "do_nothing 不应下发");
    let entries: Vec<Value> = std::fs::read_to_string(&audit_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["action"], "do_nothing");
    let _ = std::fs::remove_dir_all(&dir);
}
