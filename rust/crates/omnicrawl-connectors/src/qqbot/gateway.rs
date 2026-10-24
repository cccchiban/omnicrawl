//! QQ 网关长连接：`op` 帧语义、握手（Hello → Identify/Resume）、心跳与事件分发。
//!
//! 协议见官方「事件订阅与通知」：连接建立后先收 `op=10 Hello` 拿到心跳周期，再发
//! `op=2 Identify`（`token` 形如 `QQBot <AccessToken>`、`intents` 位掩码、`shard`）；
//! 鉴权成功下发 `READY`。断线后短时间内重连可以发 `op=6 Resume` 补发遗漏事件，
//! `op=7` 是服务端主动要求重连，`op=9` 表示会话失效需要重新 Identify。
//!
//! 帧编解码走 [`crate::ws`]（与飞书共用一份实现），这里只做 `op` 语义。

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::ws::{WebSocket, WsMessage, WsPoll};

/// 服务端推送。
pub const OP_DISPATCH: i64 = 0;
/// 心跳。
pub const OP_HEARTBEAT: i64 = 1;
/// 客户端鉴权。
pub const OP_IDENTIFY: i64 = 2;
/// 恢复连接。
pub const OP_RESUME: i64 = 6;
/// 服务端要求重连。
pub const OP_RECONNECT: i64 = 7;
/// 会话失效。
pub const OP_INVALID_SESSION: i64 = 9;
/// 连接建立后的第一条消息。
pub const OP_HELLO: i64 = 10;
/// 心跳回执。
pub const OP_HEARTBEAT_ACK: i64 = 11;

/// 默认心跳周期：没收到 `Hello` 之前按官方示例的 45 秒兜底。
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(45);

/// 握手等待上限：连上却没等到 `Hello` 就按失败处理，避免挂着不动。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// 单次读取的上限：够短，让心跳与停止标志都能及时插进来。
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// 一帧网关消息的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum GatewaySignal {
    /// `op=0`：事件推送，`event` 是 `t` 字段。
    Dispatch {
        event: String,
        data: Value,
        seq: Option<i64>,
        id: String,
    },
    /// `op=11`：心跳已回执。
    HeartbeatAck,
    /// `op=7`：服务端要求重连（应带 Resume 重连）。
    Reconnect,
    /// `op=9`：会话失效；`resumable` 为 true 时可以 Resume，否则必须重新 Identify。
    InvalidSession { resumable: bool },
    /// 与本层无关的帧（未知 op、Hello 之后重复下发等）。
    Ignored,
}

/// Identify 负载。
pub fn identify_payload(access_token: &str, intents: i64) -> Value {
    json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": format!("QQBot {access_token}"),
            "intents": intents,
            "shard": [0, 1],
            "properties": {
                "$os": std::env::consts::OS,
                "$browser": "omnicrawl-connectors",
                "$device": "omnicrawl-connectors",
            },
        },
    })
}

/// Resume 负载。
pub fn resume_payload(access_token: &str, session_id: &str, seq: i64) -> Value {
    json!({
        "op": OP_RESUME,
        "d": {
            "token": format!("QQBot {access_token}"),
            "session_id": session_id,
            "seq": seq,
        },
    })
}

/// 心跳负载：`d` 是收到的最新序列号，还没收到过就是 null。
pub fn heartbeat_payload(seq: Option<i64>) -> Value {
    json!({"op": OP_HEARTBEAT, "d": seq})
}

/// 解析一帧文本：只认 `op`，其余字段按需取。
pub fn parse_signal(text: &str) -> Result<GatewaySignal, String> {
    let payload: Value =
        serde_json::from_str(text).map_err(|error| format!("网关帧不是 JSON：{error}"))?;
    let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);
    let seq = payload.get("s").and_then(Value::as_i64);
    match op {
        OP_DISPATCH => Ok(GatewaySignal::Dispatch {
            event: payload
                .get("t")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            data: payload.get("d").cloned().unwrap_or(Value::Null),
            seq,
            id: payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        OP_HEARTBEAT_ACK => Ok(GatewaySignal::HeartbeatAck),
        OP_RECONNECT => Ok(GatewaySignal::Reconnect),
        OP_INVALID_SESSION => Ok(GatewaySignal::InvalidSession {
            resumable: payload.get("d").and_then(Value::as_bool).unwrap_or(false),
        }),
        _ => Ok(GatewaySignal::Ignored),
    }
}

/// 从 `Hello` 帧里取心跳周期。
pub fn parse_hello_interval(payload: &Value) -> Option<Duration> {
    payload
        .get("d")
        .and_then(|data| data.get("heartbeat_interval"))
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
}

/// 一条网关连接：握手、心跳与事件读取。
pub struct GatewayConnection<S: Read + Write> {
    socket: WebSocket<S>,
    pub heartbeat_interval: Duration,
    /// 收到的最新序列号（Resume 与心跳都要它）。
    pub last_seq: Option<i64>,
    next_heartbeat: Instant,
}

impl<S: Read + Write> GatewayConnection<S> {
    /// 握手：等 `Hello` 拿心跳周期，再按 `resume` 决定发 Identify 还是 Resume。
    ///
    /// 返回时鉴权请求已发出，但 `READY` / `RESUMED` 由调用方在事件循环里收到。
    pub fn connect(
        stream: S,
        access_token: &str,
        intents: i64,
        resume: Option<(&str, Option<i64>)>,
    ) -> Result<GatewayConnection<S>, String> {
        let mut socket = WebSocket::new(stream, 0x5DEE_CE66);
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let mut heartbeat_interval = DEFAULT_HEARTBEAT;
        while Instant::now() < deadline {
            match socket.poll()? {
                WsPoll::Idle => continue,
                WsPoll::Message(WsMessage::Text(text)) => {
                    let payload: Value = serde_json::from_str(&text)
                        .map_err(|error| format!("网关帧不是 JSON：{error}"))?;
                    let op = payload.get("op").and_then(Value::as_i64).unwrap_or(-1);
                    if op == OP_HELLO {
                        if let Some(interval) = parse_hello_interval(&payload) {
                            heartbeat_interval = interval;
                        }
                        break;
                    }
                    // Hello 之前的其它帧（例如平台的欢迎语）一律忽略。
                }
                WsPoll::Message(WsMessage::Close) => {
                    return Err("网关在握手阶段关闭了连接".to_string())
                }
                WsPoll::Message(_) => continue,
            }
        }
        if Instant::now() >= deadline {
            return Err("等待网关 Hello 超时".to_string());
        }
        let auth = match resume {
            Some((session_id, seq)) if !session_id.is_empty() => {
                resume_payload(access_token, session_id, seq.unwrap_or(0))
            }
            _ => identify_payload(access_token, intents),
        };
        socket.send_text(&crate::json::dumps(&auth))?;
        Ok(GatewayConnection {
            socket,
            heartbeat_interval,
            last_seq: resume.and_then(|(_id, seq)| seq),
            next_heartbeat: Instant::now() + heartbeat_interval,
        })
    }

    /// 到了心跳点就发一次。
    pub fn heartbeat_if_due(&mut self) -> Result<bool, String> {
        if Instant::now() < self.next_heartbeat {
            return Ok(false);
        }
        let payload = heartbeat_payload(self.last_seq);
        self.socket.send_text(&crate::json::dumps(&payload))?;
        self.next_heartbeat = Instant::now() + self.heartbeat_interval;
        Ok(true)
    }

    /// 轮询一帧：先把到期的心跳发出去，再读。`Idle` 表示这一轮没有数据。
    pub fn poll(&mut self) -> Result<Option<GatewaySignal>, String> {
        self.heartbeat_if_due()?;
        match self.socket.poll()? {
            WsPoll::Idle => Ok(None),
            WsPoll::Message(WsMessage::Close) => Err("网关关闭了长连接".to_string()),
            WsPoll::Message(WsMessage::Text(text)) => {
                let signal = parse_signal(&text)?;
                if let GatewaySignal::Dispatch { seq: Some(seq), .. } = &signal {
                    self.last_seq = Some(*seq);
                }
                Ok(Some(signal))
            }
            // 网关只发文本帧；二进制与 ping/pong 由底层处理。
            WsPoll::Message(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_supplies_the_heartbeat_interval() {
        let payload: Value = serde_json::from_str(r#"{"op":10,"d":{"heartbeat_interval":45000}}"#)
            .expect("合法 JSON");
        assert_eq!(
            parse_hello_interval(&payload),
            Some(Duration::from_millis(45000))
        );
        assert_eq!(parse_hello_interval(&json!({"op": 10, "d": {}})), None);
    }

    #[test]
    fn identify_uses_the_qqbot_token_prefix() {
        let payload = identify_payload("ACCESS", 33554432 | 1073741824);
        assert_eq!(payload["op"], OP_IDENTIFY);
        assert_eq!(payload["d"]["token"], "QQBot ACCESS");
        assert_eq!(payload["d"]["intents"], 1107296256_i64);
        assert_eq!(payload["d"]["shard"], json!([0, 1]));
    }

    #[test]
    fn resume_carries_session_and_seq() {
        let payload = resume_payload("ACCESS", "s-1", 42);
        assert_eq!(payload["op"], OP_RESUME);
        assert_eq!(payload["d"]["session_id"], "s-1");
        assert_eq!(payload["d"]["seq"], 42);
    }

    #[test]
    fn heartbeat_uses_null_before_the_first_dispatch() {
        assert_eq!(heartbeat_payload(None), json!({"op": 1, "d": null}));
        assert_eq!(heartbeat_payload(Some(7)), json!({"op": 1, "d": 7}));
    }

    #[test]
    fn signals_are_classified_by_op() {
        let dispatch = parse_signal(
            r#"{"op":0,"s":42,"t":"C2C_MESSAGE_CREATE","id":"e1","d":{"content":"hi"}}"#,
        )
        .expect("可解析");
        match dispatch {
            GatewaySignal::Dispatch {
                event,
                data,
                seq,
                id,
            } => {
                assert_eq!(event, "C2C_MESSAGE_CREATE");
                assert_eq!(data["content"], "hi");
                assert_eq!(seq, Some(42));
                assert_eq!(id, "e1");
            }
            other => panic!("应为事件推送：{other:?}"),
        }
        assert_eq!(
            parse_signal(r#"{"op":11}"#).unwrap(),
            GatewaySignal::HeartbeatAck
        );
        assert_eq!(
            parse_signal(r#"{"op":7}"#).unwrap(),
            GatewaySignal::Reconnect
        );
        assert_eq!(
            parse_signal(r#"{"op":9,"d":true}"#).unwrap(),
            GatewaySignal::InvalidSession { resumable: true }
        );
        assert_eq!(
            parse_signal(r#"{"op":10}"#).unwrap(),
            GatewaySignal::Ignored
        );
        assert!(parse_signal("not json").is_err());
    }
}
