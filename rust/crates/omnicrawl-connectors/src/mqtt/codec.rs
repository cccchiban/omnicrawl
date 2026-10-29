//! MQTT 3.1.1 报文编解码（最小面：连接、订阅、发布、确认与心跳）。
//!
//! 只实现本 crate 需要的那几种报文，不引第三方 MQTT 客户端：仓库其余平台接入（WebSocket
//! 握手、pbbp2 帧）都是同样手写帧的路线，这里保持一致，也不给构建加原生依赖。
//!
//! 解码走「定长头 + 变长剩余长度 + 整包读入」：剩余长度按字节读到最高位清零为止，
//! 之后一次性 `read_exact`。因此超时只可能发生在包边界（第一个字节），不会读到半包
//! 就丢同步——半包超时一律按致命错误处理，由调用方重连。

use std::io::{self, Read};

/// 控制报文类型（固定头高 4 位）。
pub const CONNECT: u8 = 1;
pub const CONNACK: u8 = 2;
pub const PUBLISH: u8 = 3;
pub const PUBACK: u8 = 4;
pub const SUBSCRIBE: u8 = 8;
pub const SUBACK: u8 = 9;
pub const PINGREQ: u8 = 12;
pub const PINGRESP: u8 = 13;
pub const DISCONNECT: u8 = 14;

/// 单个报文体的上限：遥测都是小 JSON，超过它一定是异常输入。
pub const MAX_PACKET_BYTES: usize = 1024 * 1024;

/// 解析后的入站报文（只覆盖本 crate 关心的类型）。
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    ConnAck {
        session_present: bool,
        return_code: u8,
    },
    Publish {
        topic: String,
        payload: Vec<u8>,
        qos: u8,
        packet_id: Option<u16>,
        retained: bool,
        duplicate: bool,
    },
    PubAck {
        packet_id: u16,
    },
    Subscribe {
        packet_id: u16,
        topics: Vec<(String, u8)>,
    },
    SubAck {
        packet_id: u16,
        codes: Vec<u8>,
    },
    PingResp,
    /// 其余类型（PINGREQ、DISCONNECT 等）：读掉即可，不参与业务。
    Other {
        packet_type: u8,
    },
}

/// CONNACK 返回码 → 中文说明（0 之外都是拒绝）。
pub fn connack_reason(code: u8) -> &'static str {
    match code {
        0 => "连接已接受",
        1 => "协议版本不被 broker 支持",
        2 => "client_id 被拒绝",
        3 => "服务不可用",
        4 => "用户名或密码错误",
        5 => "未授权",
        _ => "未知的 CONNACK 返回码",
    }
}

/// 变长剩余长度编码（每字节 7 位，最高位表示后续还有字节）。
pub fn encode_remaining_length(mut length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if length == 0 {
            break;
        }
    }
    out
}

/// 拼一个完整报文：固定头 + 剩余长度 + 报文体。
fn frame(packet_type: u8, flags: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push((packet_type << 4) | (flags & 0x0f));
    out.extend_from_slice(&encode_remaining_length(body.len()));
    out.extend_from_slice(&body);
    out
}

/// MQTT UTF-8 字符串：两字节长度前缀 + 字节。
fn push_string(out: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// CONNECT：协议名/版本 + 连接标志 + keepalive + 载荷（client_id、用户名、密码）。
///
/// 连接标志固定 `clean_session`，不带遗嘱：本进程没有「半死」需要对外声明。
pub fn connect_packet(
    client_id: &str,
    username: &str,
    password: &str,
    keepalive_seconds: u16,
    clean_session: bool,
) -> Vec<u8> {
    let mut body = Vec::new();
    push_string(&mut body, "MQTT");
    body.push(4); // 协议级别：3.1.1
    let mut flags = 0u8;
    if clean_session {
        flags |= 0x02;
    }
    if !username.is_empty() {
        flags |= 0x80;
    }
    if !password.is_empty() {
        // 3.1.1 规定有密码就必须有用户名；调用方在配置校验里保证这一点。
        flags |= 0x40;
    }
    body.push(flags);
    body.extend_from_slice(&keepalive_seconds.to_be_bytes());
    push_string(&mut body, client_id);
    if !username.is_empty() {
        push_string(&mut body, username);
    }
    if !password.is_empty() {
        push_string(&mut body, password);
    }
    frame(CONNECT, 0, body)
}

/// SUBSCRIBE：固定头低 4 位必须是 0b0010。
pub fn subscribe_packet(packet_id: u16, topics: &[String], qos: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    for topic in topics {
        push_string(&mut body, topic);
        body.push(qos.min(2));
    }
    frame(SUBSCRIBE, 0x02, body)
}

/// PUBLISH：QoS1 带 packet_id，QoS0 不带。
pub fn publish_packet(topic: &str, payload: &[u8], qos: u8, packet_id: Option<u16>, retain: bool) -> Vec<u8> {
    let qos = qos.min(2);
    let mut body = Vec::new();
    push_string(&mut body, topic);
    if qos > 0 {
        body.extend_from_slice(&packet_id.unwrap_or(1).to_be_bytes());
    }
    body.extend_from_slice(payload);
    let mut flags = qos << 1;
    if retain {
        flags |= 0x01;
    }
    frame(PUBLISH, flags, body)
}

/// PUBACK：QoS1 发布的确认。
pub fn puback_packet(packet_id: u16) -> Vec<u8> {
    frame(PUBACK, 0, packet_id.to_be_bytes().to_vec())
}

/// CONNACK：broker 侧握手回应（测试与本地假 broker 用）。
pub fn connack_packet(session_present: bool, return_code: u8) -> Vec<u8> {
    frame(
        CONNACK,
        0,
        vec![if session_present { 0x01 } else { 0x00 }, return_code],
    )
}

/// SUBACK：按订阅顺序回每个主题的授予 QoS，0x80 表示拒绝。
pub fn suback_packet(packet_id: u16, codes: &[u8]) -> Vec<u8> {
    let mut body = packet_id.to_be_bytes().to_vec();
    body.extend_from_slice(codes);
    frame(SUBACK, 0, body)
}

pub fn pingreq_packet() -> Vec<u8> {
    frame(PINGREQ, 0, Vec::new())
}

pub fn disconnect_packet() -> Vec<u8> {
    frame(DISCONNECT, 0, Vec::new())
}

/// 读一个完整报文。
///
/// 只有第一个字节允许读到 `WouldBlock` / `TimedOut`（调用方据此当作「本轮没有事件」）；
/// 一旦进入包体，任何读取失败都按致命错误返回，避免留下半包。
pub fn read_packet<R: Read>(reader: &mut R) -> io::Result<Packet> {
    let first = read_byte(reader)?;
    let remaining = read_remaining_length(reader)?;
    if remaining > MAX_PACKET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("MQTT 报文过大：{remaining} 字节"),
        ));
    }
    let mut body = vec![0u8; remaining];
    if remaining > 0 {
        reader.read_exact(&mut body)?;
    }
    decode(first, &body)
}

fn read_byte<R: Read>(reader: &mut R) -> io::Result<u8> {
    let mut one = [0u8; 1];
    reader.read_exact(&mut one)?;
    Ok(one[0])
}

fn read_remaining_length<R: Read>(reader: &mut R) -> io::Result<usize> {
    let mut multiplier = 1usize;
    let mut value = 0usize;
    for _ in 0..4 {
        let byte = read_byte(reader)?;
        value += (byte & 0x7f) as usize * multiplier;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        multiplier *= 128;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "MQTT 剩余长度超过 4 字节",
    ))
}

/// 从报文体里按顺序取字段。
struct Cursor<'a> {
    body: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, position: 0 }
    }

    fn u8(&mut self) -> io::Result<u8> {
        let byte = *self.body.get(self.position).ok_or_else(short_packet)?;
        self.position += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> io::Result<u16> {
        let high = self.u8()?;
        let low = self.u8()?;
        Ok(u16::from_be_bytes([high, low]))
    }

    fn string(&mut self) -> io::Result<String> {
        let length = self.u16()? as usize;
        let end = self.position + length;
        let slice = self.body.get(self.position..end).ok_or_else(short_packet)?;
        self.position = end;
        Ok(String::from_utf8_lossy(slice).to_string())
    }

    fn remaining(&self) -> usize {
        self.body.len().saturating_sub(self.position)
    }

    fn rest(&mut self) -> &'a [u8] {
        let slice = &self.body[self.position.min(self.body.len())..];
        self.position = self.body.len();
        slice
    }
}

fn short_packet() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "MQTT 报文体长度不足")
}

fn decode(first: u8, body: &[u8]) -> io::Result<Packet> {
    let packet_type = first >> 4;
    let mut cursor = Cursor::new(body);
    match packet_type {
        CONNACK => {
            let flags = cursor.u8()?;
            let return_code = cursor.u8()?;
            Ok(Packet::ConnAck {
                session_present: flags & 0x01 != 0,
                return_code,
            })
        }
        PUBLISH => {
            let qos = (first & 0x06) >> 1;
            let retained = first & 0x01 != 0;
            let duplicate = first & 0x08 != 0;
            let topic = cursor.string()?;
            let packet_id = if qos > 0 { Some(cursor.u16()?) } else { None };
            Ok(Packet::Publish {
                topic,
                payload: cursor.rest().to_vec(),
                qos,
                packet_id,
                retained,
                duplicate,
            })
        }
        PUBACK => Ok(Packet::PubAck {
            packet_id: cursor.u16()?,
        }),
        SUBSCRIBE => {
            let packet_id = cursor.u16()?;
            let mut topics = Vec::new();
            while cursor.remaining() > 0 {
                let topic = cursor.string()?;
                topics.push((topic, cursor.u8()? & 0x03));
            }
            Ok(Packet::Subscribe { packet_id, topics })
        }
        SUBACK => {
            let packet_id = cursor.u16()?;
            Ok(Packet::SubAck {
                packet_id,
                codes: cursor.rest().to_vec(),
            })
        }
        PINGRESP => Ok(Packet::PingResp),
        other => Ok(Packet::Other { packet_type: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as ByteCursor;

    #[test]
    fn remaining_length_matches_the_spec_table() {
        assert_eq!(encode_remaining_length(0), vec![0]);
        assert_eq!(encode_remaining_length(127), vec![127]);
        assert_eq!(encode_remaining_length(128), vec![0x80, 0x01]);
        assert_eq!(encode_remaining_length(16383), vec![0xff, 0x7f]);
        assert_eq!(encode_remaining_length(16384), vec![0x80, 0x80, 0x01]);
        assert_eq!(
            encode_remaining_length(2_097_151),
            vec![0xff, 0xff, 0x7f]
        );
    }

    #[test]
    fn connect_carries_protocol_level_and_credentials() {
        let bytes = connect_packet("oc-farm", "user", "pass", 120, true);
        assert_eq!(bytes[0], CONNECT << 4);
        // 协议名 "MQTT" 紧跟在剩余长度之后。
        assert_eq!(bytes[2], 0x00);
        assert_eq!(bytes[3], 0x04);
        assert_eq!(&bytes[4..8], b"MQTT");
        assert_eq!(bytes[8], 4);
        assert_eq!(bytes[9], 0b1100_0010);
        assert_eq!(&bytes[10..12], &120u16.to_be_bytes());
        assert!(bytes
            .windows("oc-farm".len())
            .any(|window| window == b"oc-farm"));
    }

    #[test]
    fn connect_without_credentials_clears_both_flags() {
        let bytes = connect_packet("oc-farm", "", "", 60, true);
        assert_eq!(bytes[9] & 0b1100_0000, 0);
        assert_eq!(bytes[9] & 0b0000_0010, 0b10);
    }

    #[test]
    fn publish_round_trips_with_qos_one() {
        let payload = br#"{"temp":31.2}"#;
        let bytes = publish_packet("farm/3/env", payload, 1, Some(7), false);
        let mut reader = ByteCursor::new(bytes);
        match read_packet(&mut reader).expect("decode publish") {
            Packet::Publish {
                topic,
                payload: got,
                qos,
                packet_id,
                retained,
                duplicate,
            } => {
                assert_eq!(topic, "farm/3/env");
                assert_eq!(got, payload.to_vec());
                assert_eq!(qos, 1);
                assert_eq!(packet_id, Some(7));
                assert!(!retained);
                assert!(!duplicate);
            }
            other => panic!("expected publish, got {other:?}"),
        }
    }

    #[test]
    fn publish_qos_zero_has_no_packet_id() {
        let bytes = publish_packet("farm/1/env", b"{}", 0, None, true);
        let mut reader = ByteCursor::new(bytes);
        match read_packet(&mut reader).expect("decode publish") {
            Packet::Publish {
                packet_id, retained, ..
            } => {
                assert_eq!(packet_id, None);
                assert!(retained);
            }
            other => panic!("expected publish, got {other:?}"),
        }
    }

    #[test]
    fn subscribe_has_reserved_flags_and_qos_byte() {
        let bytes = subscribe_packet(3, &["farm/+/env".to_string()], 1);
        assert_eq!(bytes[0] & 0x0f, 0x02);
        assert_eq!(bytes[1] as usize, 2 + 2 + "farm/+/env".len() + 1);
    }

    #[test]
    fn suback_and_connack_decode() {
        let mut reader = ByteCursor::new(vec![CONNACK << 4, 2, 0x01, 0x05]);
        assert_eq!(
            read_packet(&mut reader).unwrap(),
            Packet::ConnAck {
                session_present: true,
                return_code: 5
            }
        );
        let mut reader = ByteCursor::new(vec![SUBACK << 4, 3, 0x00, 0x09, 0x01]);
        assert_eq!(
            read_packet(&mut reader).unwrap(),
            Packet::SubAck {
                packet_id: 9,
                codes: vec![1]
            }
        );
    }

    #[test]
    fn oversize_packet_is_rejected() {
        // 剩余长度 0x80 0x80 0x80 0x01 = 2097152 > 上限。
        let bytes = vec![PUBLISH << 4, 0x80, 0x80, 0x80, 0x01];
        let mut reader = ByteCursor::new(bytes);
        let error = read_packet(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_body_is_invalid_not_panic() {
        let bytes = vec![PUBLISH << 4, 4, 0x00, 0x01];
        let mut reader = ByteCursor::new(bytes);
        let error = read_packet(&mut reader).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
