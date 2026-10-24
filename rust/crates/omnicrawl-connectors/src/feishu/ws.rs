//! 飞书长连接：`pbbp2` 帧编解码、WebSocket 握手/帧收发与重连退避。
//!
//! 语义基准是 Python 侧 `lark-oapi` 的 `lark_oapi.ws.client`（端点、心跳、事件应答）
//! 与它依赖的 `pbbp2.proto`（`Header` / `Frame`）。协议栈自实现，不引入第三方
//! WebSocket 库：仓库既有做法是手写匹配器与 SHA-1（见 `omnicrawl-llm`），
//! 这里沿用同一取舍。

use std::io::{Read, Write};
use std::time::Duration;

use base64::Engine;

use super::api::{ClientConfig, FeishuApi, FeishuApiError};
use crate::json;

/// 心跳上限：即便对端给了更短的间隔也不低于 10 秒。
const MIN_PING_INTERVAL_SECONDS: u64 = 10;

/// 单次读取超时：够短以便心跳与取消都能及时插进来。
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 重连默认值（对端未给 ClientConfig 时）。
const DEFAULT_RECONNECT_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_RECONNECT_COUNT: u64 = 5;

/// `pbbp2.proto` 的等价实现（`Header` / `Frame` 与帧类型、消息类型常量）。
pub mod pbbp2 {
    /// 控制帧。
    pub const FRAME_TYPE_CONTROL: i32 = 0;
    /// 数据帧。
    pub const FRAME_TYPE_DATA: i32 = 1;

    /// 帧头键名（与 SDK 的 `const.py` 一致）。
    pub const HEADER_TYPE: &str = "type";
    pub const HEADER_MESSAGE_ID: &str = "message_id";
    pub const HEADER_SUM: &str = "sum";
    pub const HEADER_SEQ: &str = "seq";
    pub const HEADER_TRACE_ID: &str = "trace_id";
    pub const HEADER_BIZ_RT: &str = "biz_rt";
    pub const HEADER_HANDSHAKE_STATUS: &str = "handshake-status";
    pub const HEADER_HANDSHAKE_MSG: &str = "handshake-msg";
    pub const HEADER_HANDSHAKE_AUTH_ERRCODE: &str = "handshake-autherrcode";

    /// 消息类型（与 SDK 的 `MessageType` 一致）。
    pub const MESSAGE_TYPE_EVENT: &str = "event";
    pub const MESSAGE_TYPE_CARD: &str = "card";
    pub const MESSAGE_TYPE_PING: &str = "ping";
    pub const MESSAGE_TYPE_PONG: &str = "pong";

    /// 帧头。
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct Header {
        pub key: String,
        pub value: String,
    }

    /// `pbbp2.Frame`：序号、路由字段、帧头与负载。
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct Frame {
        pub seq_id: u64,
        pub log_id: u64,
        pub service: i32,
        pub method: i32,
        pub headers: Vec<Header>,
        pub payload_encoding: String,
        pub payload_type: String,
        pub payload: Vec<u8>,
        pub log_id_new: String,
    }

    impl Frame {
        /// 读取帧头值；缺键返回空串（与 SDK 的 `_get_by_key` 一致）。
        pub fn header(&self, key: &str) -> String {
            self.headers
                .iter()
                .find(|header| header.key == key)
                .map(|header| header.value.clone())
                .unwrap_or_default()
        }

        /// 覆盖或追加帧头。
        pub fn set_header(&mut self, key: &str, value: &str) {
            match self.headers.iter_mut().find(|header| header.key == key) {
                Some(header) => header.value = value.to_string(),
                None => self.headers.push(Header {
                    key: key.to_string(),
                    value: value.to_string(),
                }),
            }
        }

        /// 数据帧判定（其余按控制帧处理）。
        pub fn is_data(&self) -> bool {
            self.method == FRAME_TYPE_DATA
        }

        /// `sum` 帧头：分包总数，缺省按 1。
        pub fn sum(&self) -> i64 {
            self.header(HEADER_SUM).trim().parse::<i64>().unwrap_or(1)
        }

        /// `seq` 帧头：分包序号。
        pub fn seq(&self) -> i64 {
            self.header(HEADER_SEQ).trim().parse::<i64>().unwrap_or(0)
        }

        /// `message_id` 帧头：分包归并用的分组键。
        pub fn message_id(&self) -> String {
            self.header(HEADER_MESSAGE_ID)
        }
    }

    /// 解析一帧；字段缺失按默认值处理（proto2 的 required 在这里放宽）。
    pub fn decode(bytes: &[u8]) -> Result<Frame, String> {
        let mut frame = Frame::default();
        let mut cursor = 0_usize;
        while cursor < bytes.len() {
            let (key, next) = read_varint(bytes, cursor)?;
            cursor = next;
            let field = (key >> 3) as u32;
            let wire_type = (key & 0x07) as u8;
            match (field, wire_type) {
                (1, 0) => {
                    let (value, next) = read_varint(bytes, cursor)?;
                    frame.seq_id = value;
                    cursor = next;
                }
                (2, 0) => {
                    let (value, next) = read_varint(bytes, cursor)?;
                    frame.log_id = value;
                    cursor = next;
                }
                (3, 0) => {
                    let (value, next) = read_varint(bytes, cursor)?;
                    frame.service = value as i32;
                    cursor = next;
                }
                (4, 0) => {
                    let (value, next) = read_varint(bytes, cursor)?;
                    frame.method = value as i32;
                    cursor = next;
                }
                (5, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    frame.headers.push(decode_header(block)?);
                    cursor = next;
                }
                (6, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    frame.payload_encoding = String::from_utf8_lossy(block).to_string();
                    cursor = next;
                }
                (7, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    frame.payload_type = String::from_utf8_lossy(block).to_string();
                    cursor = next;
                }
                (8, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    frame.payload = block.to_vec();
                    cursor = next;
                }
                (9, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    frame.log_id_new = String::from_utf8_lossy(block).to_string();
                    cursor = next;
                }
                (_, wire) => {
                    cursor = skip_field(bytes, cursor, wire)?;
                }
            }
        }
        Ok(frame)
    }

    /// 编码一帧。
    ///
    /// proto2 的 `required` 字段（`SeqID` / `service` / `method`）一律写出，可选项只在
    /// 非默认值时写出——Python 的生成器保留字段存在性（显式赋空串也会写出来），
    /// 解码后无法区分「未设置」与「设成默认值」，因此重编码以本规则为准。
    pub fn encode(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint_field(&mut out, 1, frame.seq_id);
        write_varint_field(&mut out, 2, frame.log_id);
        write_varint_field(&mut out, 3, frame.service as u64);
        write_varint_field(&mut out, 4, frame.method as u64);
        for header in &frame.headers {
            let mut block = Vec::new();
            write_string_field(&mut block, 1, &header.key);
            write_string_field(&mut block, 2, &header.value);
            write_bytes_field(&mut out, 5, &block);
        }
        if !frame.payload_encoding.is_empty() {
            write_string_field(&mut out, 6, &frame.payload_encoding);
        }
        if !frame.payload_type.is_empty() {
            write_string_field(&mut out, 7, &frame.payload_type);
        }
        if !frame.payload.is_empty() {
            write_bytes_field(&mut out, 8, &frame.payload);
        }
        if !frame.log_id_new.is_empty() {
            write_string_field(&mut out, 9, &frame.log_id_new);
        }
        out
    }

    fn decode_header(bytes: &[u8]) -> Result<Header, String> {
        let mut header = Header::default();
        let mut cursor = 0_usize;
        while cursor < bytes.len() {
            let (key, next) = read_varint(bytes, cursor)?;
            cursor = next;
            let field = (key >> 3) as u32;
            let wire_type = (key & 0x07) as u8;
            match (field, wire_type) {
                (1, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    header.key = String::from_utf8_lossy(block).to_string();
                    cursor = next;
                }
                (2, 2) => {
                    let (block, next) = read_bytes(bytes, cursor)?;
                    header.value = String::from_utf8_lossy(block).to_string();
                    cursor = next;
                }
                (_, wire) => {
                    cursor = skip_field(bytes, cursor, wire)?;
                }
            }
        }
        Ok(header)
    }

    fn read_varint(bytes: &[u8], start: usize) -> Result<(u64, usize), String> {
        let mut result = 0_u64;
        let mut shift = 0_u32;
        let mut cursor = start;
        loop {
            let byte = *bytes
                .get(cursor)
                .ok_or_else(|| "帧数据在 varint 中间截断".to_string())?;
            cursor += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok((result, cursor));
            }
            shift += 7;
            if shift >= 64 {
                return Err("varint 过长".to_string());
            }
        }
    }

    fn read_bytes(bytes: &[u8], start: usize) -> Result<(&[u8], usize), String> {
        let (length, cursor) = read_varint(bytes, start)?;
        let end = cursor + length as usize;
        if end > bytes.len() {
            return Err("长度前缀超出帧范围".to_string());
        }
        Ok((&bytes[cursor..end], end))
    }

    fn skip_field(bytes: &[u8], cursor: usize, wire_type: u8) -> Result<usize, String> {
        match wire_type {
            0 => read_varint(bytes, cursor).map(|(_value, next)| next),
            1 => Ok(cursor + 8),
            2 => read_bytes(bytes, cursor).map(|(_block, next)| next),
            5 => Ok(cursor + 4),
            other => Err(format!("不支持的 protobuf 线类型 {other}")),
        }
    }

    fn write_varint(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn write_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
        write_varint(out, u64::from(field) << 3);
        write_varint(out, value);
    }

    fn write_bytes_field(out: &mut Vec<u8>, field: u32, block: &[u8]) {
        write_varint(out, (u64::from(field) << 3) | 2);
        write_varint(out, block.len() as u64);
        out.extend_from_slice(block);
    }

    fn write_string_field(out: &mut Vec<u8>, field: u32, value: &str) {
        write_bytes_field(out, field, value.as_bytes());
    }
}

// 帧编解码、握手与 TLS 建连与 QQ 网关共用一份实现（`crate::ws`）。这里只做再导出，
// 保持 `feishu::ws::…` 的既有引用路径不变。
pub use crate::ws::{
    decode_frame, encode_frame, handshake_request, masking_key, sha1, split_wss_url,
    websocket_accept, TlsStream, WsMessage, OPCODE_BINARY, OPCODE_CLOSE, OPCODE_CONTINUATION,
    OPCODE_PING, OPCODE_PONG, OPCODE_TEXT,
};

/// 与飞书的 WebSocket 长连接。
pub struct LongConnection<S: Read + Write> {
    stream: S,
    read_buffer: Vec<u8>,
    mask_seed: u64,
    pub config: ClientConfig,
}

impl<S: Read + Write> LongConnection<S> {
    pub fn new(stream: S, config: ClientConfig) -> LongConnection<S> {
        LongConnection {
            stream,
            read_buffer: Vec::new(),
            mask_seed: 0x1234_5678,
            config,
        }
    }

    /// 发送原始字节。
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .map_err(|error| error.to_string())?;
        self.stream.flush().map_err(|error| error.to_string())
    }

    /// 发送一个 `pbbp2` 帧（二进制消息）。
    pub fn send_frame(&mut self, frame: &pbbp2::Frame) -> Result<(), String> {
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mask = masking_key(self.mask_seed);
        let payload = pbbp2::encode(frame);
        let bytes = encode_frame(OPCODE_BINARY, &payload, mask);
        self.write_all(&bytes)
    }

    /// 发送心跳帧（`CONTROL` + `ping`）。
    pub fn send_ping(&mut self, service_id: i64) -> Result<(), String> {
        let mut frame = pbbp2::Frame {
            seq_id: 0,
            service: service_id as i32,
            method: pbbp2::FRAME_TYPE_CONTROL,
            ..Default::default()
        };
        frame.set_header(pbbp2::HEADER_TYPE, pbbp2::MESSAGE_TYPE_PING);
        self.send_frame(&frame)
    }

    /// 下一条 `pbbp2` 帧；控制帧（ping/pong）就地处理，只返回数据帧。
    ///
    /// 返回 `Ok(None)` 表示本次读取只是控制帧或空读，调用方继续循环即可。
    pub fn next_data_frame(&mut self) -> Result<Option<pbbp2::Frame>, String> {
        loop {
            let frame = match self.next_ws_message()? {
                WsMessage::Close => return Err("对端关闭了长连接".to_string()),
                WsMessage::Ping(payload) => {
                    let mask = masking_key(self.mask_seed);
                    let bytes = encode_frame(OPCODE_PONG, &payload, mask);
                    self.write_all(&bytes)?;
                    continue;
                }
                WsMessage::Pong(payload) => {
                    self.apply_config_payload(&payload);
                    continue;
                }
                WsMessage::Text(text) => pbbp2::decode(text.as_bytes())?,
                WsMessage::Binary(bytes) => pbbp2::decode(&bytes)?,
            };
            match frame.header(pbbp2::HEADER_TYPE).as_str() {
                pbbp2::MESSAGE_TYPE_PONG => {
                    self.apply_config_payload(&frame.payload);
                    return Ok(None);
                }
                pbbp2::MESSAGE_TYPE_PING => return Ok(None),
                _ if frame.is_data() => return Ok(Some(frame)),
                _ => return Ok(None),
            }
        }
    }

    /// 用同一帧回执（`{"code":200}`），可选带业务耗时与 ACK 数据。
    pub fn respond(
        &mut self,
        frame: &pbbp2::Frame,
        ok: bool,
        elapsed_ms: Option<i64>,
        data: Option<String>,
    ) -> Result<(), String> {
        let mut response = serde_json::Map::new();
        response.insert(
            "code".to_string(),
            serde_json::json!(if ok { 200 } else { 500 }),
        );
        if let Some(elapsed) = elapsed_ms {
            response.insert(
                "headers".to_string(),
                serde_json::json!([{"key": pbbp2::HEADER_BIZ_RT, "value": elapsed.to_string()}]),
            );
        }
        if let Some(data) = data {
            response.insert(
                "data".to_string(),
                serde_json::json!(base64::engine::general_purpose::STANDARD.encode(data)),
            );
        }
        let mut reply = frame.clone();
        reply.payload = json::dumps(&serde_json::Value::Object(response)).into_bytes();
        self.send_frame(&reply)
    }

    fn apply_config_payload(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(payload) else {
            return;
        };
        if let Some(interval) = parsed.get("PingInterval").and_then(|value| value.as_i64()) {
            self.config.ping_interval = Some(interval);
        }
        if let Some(count) = parsed
            .get("ReconnectCount")
            .and_then(|value| value.as_i64())
        {
            self.config.reconnect_count = Some(count);
        }
        if let Some(interval) = parsed
            .get("ReconnectInterval")
            .and_then(|value| value.as_i64())
        {
            self.config.reconnect_interval = Some(interval);
        }
    }

    fn next_ws_message(&mut self) -> Result<WsMessage, String> {
        loop {
            if let Some((message, consumed)) = decode_frame(&self.read_buffer)? {
                self.read_buffer.drain(..consumed);
                return Ok(message);
            }
            let mut chunk = [0_u8; 8192];
            let read = self
                .stream
                .read(&mut chunk)
                .map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("长连接被对端关闭".to_string());
            }
            self.read_buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// 重连策略：间隔与次数来自对端配置，指数退避到上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    pub interval_seconds: u64,
    pub max_attempts: u64,
}

impl ReconnectPolicy {
    pub fn from_config(config: &ClientConfig) -> ReconnectPolicy {
        ReconnectPolicy {
            interval_seconds: config
                .reconnect_interval
                .filter(|value| *value > 0)
                .map(|value| value as u64)
                .unwrap_or(DEFAULT_RECONNECT_INTERVAL_SECONDS),
            max_attempts: config
                .reconnect_count
                .filter(|value| *value > 0)
                .map(|value| value as u64)
                .unwrap_or(DEFAULT_RECONNECT_COUNT),
        }
    }

    /// 第 `attempt` 次重连（从 1 开始）的等待秒数。
    pub fn backoff_seconds(&self, attempt: u64) -> u64 {
        let shifted = self
            .interval_seconds
            .saturating_mul(1_u64 << attempt.min(6));
        shifted.min(120)
    }
}

/// 心跳间隔秒数：不低于 [`MIN_PING_INTERVAL_SECONDS`]。
pub fn ping_interval_seconds(config: &ClientConfig) -> u64 {
    config
        .ping_interval_seconds()
        .max(MIN_PING_INTERVAL_SECONDS)
}

/// 连接飞书长连接端点并完成握手。
pub fn connect(api: &FeishuApi) -> Result<LongConnection<TlsStream>, FeishuApiError> {
    let endpoint = api.ws_endpoint()?;
    let stream = connect_wss(&endpoint.url).map_err(FeishuApiError::Transport)?;
    Ok(LongConnection::new(stream, endpoint.config))
}

/// 建立 `wss://` 连接并完成 WebSocket 握手校验（读取超时取本模块的 [`READ_TIMEOUT`]）。
pub fn connect_wss(url: &str) -> Result<TlsStream, String> {
    crate::ws::connect_wss(url, READ_TIMEOUT, super::api::USER_AGENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbbp2_round_trip_keeps_fields() {
        let mut frame = pbbp2::Frame {
            seq_id: 7,
            log_id: 0,
            service: 3,
            method: pbbp2::FRAME_TYPE_DATA,
            payload: b"{\"schema\":\"2.0\"}".to_vec(),
            ..Default::default()
        };
        frame.set_header(pbbp2::HEADER_TYPE, pbbp2::MESSAGE_TYPE_EVENT);
        frame.set_header(pbbp2::HEADER_MESSAGE_ID, "m-1");
        frame.set_header(pbbp2::HEADER_SUM, "1");
        let bytes = pbbp2::encode(&frame);
        let decoded = pbbp2::decode(&bytes).expect("可解码");
        assert_eq!(decoded.seq_id, 7);
        assert_eq!(decoded.service, 3);
        assert!(decoded.is_data());
        assert_eq!(
            decoded.header(pbbp2::HEADER_TYPE),
            pbbp2::MESSAGE_TYPE_EVENT
        );
        assert_eq!(decoded.message_id(), "m-1");
        assert_eq!(decoded.sum(), 1);
        assert_eq!(decoded.payload, frame.payload);
    }

    #[test]
    fn reconnect_policy_backs_off_and_falls_back() {
        let mut config = ClientConfig::default();
        let policy = ReconnectPolicy::from_config(&config);
        assert_eq!(policy.interval_seconds, 5);
        assert_eq!(policy.max_attempts, 5);
        assert_eq!(policy.backoff_seconds(1), 10);
        assert_eq!(policy.backoff_seconds(9), 120);
        config.ping_interval = Some(3);
        assert_eq!(ping_interval_seconds(&config), MIN_PING_INTERVAL_SECONDS);
    }
}
