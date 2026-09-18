//! 飞书长连接：`pbbp2` 帧编解码、WebSocket 握手/帧收发与重连退避。
//!
//! 语义基准是 Python 侧 `lark-oapi` 的 `lark_oapi.ws.client`（端点、心跳、事件应答）
//! 与它依赖的 `pbbp2.proto`（`Header` / `Frame`）。协议栈自实现，不引入第三方
//! WebSocket 库：仓库既有做法是手写匹配器与 SHA-1（见 `omnicrawl-llm`），
//! 这里沿用同一取舍。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// WebSocket 帧类型。
pub const OPCODE_CONTINUATION: u8 = 0x0;
pub const OPCODE_TEXT: u8 = 0x1;
pub const OPCODE_BINARY: u8 = 0x2;
pub const OPCODE_CLOSE: u8 = 0x8;
pub const OPCODE_PING: u8 = 0x9;
pub const OPCODE_PONG: u8 = 0xA;

/// 收到的一条 WebSocket 消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close,
}

/// 计算 `Sec-WebSocket-Accept`（RFC 6455：key + GUID 的 SHA-1 再 base64）。
pub fn websocket_accept(key: &str) -> String {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = sha1(&data);
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// 生成握手请求文本（GET 升级请求）。
pub fn handshake_request(host: &str, path: &str, key: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         User-Agent: {}\r\n\
         \r\n",
        super::api::USER_AGENT
    )
}

/// 生成一轮 16 字节随机掩码（客户端发出的帧必须掩码）。
pub fn masking_key(seed: u64) -> [u8; 4] {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut key = [0_u8; 4];
    for slot in key.iter_mut() {
        // xorshift：只用于掩码，不需要密码学强度。
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state & 0xff) as u8;
    }
    key
}

/// 编码一个客户端帧。
pub fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut out = vec![0x80 | (opcode & 0x0f)];
    let length = payload.len();
    if length < 126 {
        out.push(0x80 | length as u8);
    } else if length <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(length as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        out.push(byte ^ mask[index % 4]);
    }
    out
}

/// 从缓冲区里取出一条完整消息（数据可能跨多次读取到达；不处理分片重组的中间态）。
pub fn decode_frame(buffer: &[u8]) -> Result<Option<(WsMessage, usize)>, String> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let opcode = buffer[0] & 0x0f;
    let masked = buffer[1] & 0x80 != 0;
    let mut length = u64::from(buffer[1] & 0x7f);
    let mut cursor = 2;
    if length == 126 {
        if buffer.len() < cursor + 2 {
            return Ok(None);
        }
        length = u64::from(u16::from_be_bytes([buffer[cursor], buffer[cursor + 1]]));
        cursor += 2;
    } else if length == 127 {
        if buffer.len() < cursor + 8 {
            return Ok(None);
        }
        let mut raw = [0_u8; 8];
        raw.copy_from_slice(&buffer[cursor..cursor + 8]);
        length = u64::from_be_bytes(raw);
        cursor += 8;
    }
    let mut mask = [0_u8; 4];
    if masked {
        if buffer.len() < cursor + 4 {
            return Ok(None);
        }
        mask.copy_from_slice(&buffer[cursor..cursor + 4]);
        cursor += 4;
    }
    let end = cursor + length as usize;
    if buffer.len() < end {
        return Ok(None);
    }
    let mut payload = buffer[cursor..end].to_vec();
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    let message = match opcode {
        OPCODE_TEXT => WsMessage::Text(String::from_utf8_lossy(&payload).to_string()),
        OPCODE_BINARY | OPCODE_CONTINUATION => WsMessage::Binary(payload),
        OPCODE_CLOSE => WsMessage::Close,
        OPCODE_PING => WsMessage::Ping(payload),
        OPCODE_PONG => WsMessage::Pong(payload),
        other => return Err(format!("未知的 WebSocket opcode {other}")),
    };
    Ok(Some((message, end)))
}

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

/// 建立 `wss://` 连接并完成 WebSocket 握手校验。
pub fn connect_wss(url: &str) -> Result<TlsStream, String> {
    let (host, port, path) = split_wss_url(url)?;
    let tcp = TcpStream::connect((host.as_str(), port)).map_err(|error| error.to_string())?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| error.to_string())?;
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name =
        rustls::pki_types::ServerName::try_from(host.clone()).map_err(|error| error.to_string())?;
    let connection = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .map_err(|error| error.to_string())?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);
    let key = base64::engine::general_purpose::STANDARD.encode(random_key_bytes());
    let request = handshake_request(&host, &path, &key);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let response = read_http_response(&mut stream)?;
    if !response.starts_with("HTTP/1.1 101") {
        return Err(format!(
            "长连接握手失败：{}",
            response.lines().next().unwrap_or("")
        ));
    }
    let expected = websocket_accept(&key);
    if !response.to_lowercase().contains(&expected.to_lowercase()) {
        return Err("长连接握手未返回预期的 Sec-WebSocket-Accept".to_string());
    }
    Ok(stream)
}

/// 拆出 `wss://host:port/path?query`。
pub fn split_wss_url(url: &str) -> Result<(String, u16, String), String> {
    let stripped = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or_else(|| format!("不支持的连接地址：{url}"))?;
    let secure = url.starts_with("wss://");
    let (authority, path) = match stripped.find('/') {
        Some(index) => (&stripped[..index], &stripped[index..]),
        None => (stripped, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>().map_err(|error| error.to_string())?,
        ),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("连接地址缺少主机名".to_string());
    }
    Ok((host.to_string(), port, path.to_string()))
}

/// 长连接类型别名：TLS 上的阻塞流。
pub type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn read_http_response(stream: &mut TlsStream) -> Result<String, String> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&raw).to_string())
}

fn random_key_bytes() -> [u8; 16] {
    let mut state = Instant::now().elapsed().as_nanos() as u64 ^ std::process::id() as u64;
    let mut key = [0_u8; 16];
    for slot in key.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state & 0xff) as u8;
    }
    key
}

/// SHA-1（RFC 3174）：WebSocket 握手需要，仓库既有的 `omnicrawl-llm` 里也是手写实现。
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut words = [0_u32; 80];
        for (index, slot) in words.iter_mut().take(16).enumerate() {
            *slot = u32::from_be_bytes([
                chunk[index * 4],
                chunk[index * 4 + 1],
                chunk[index * 4 + 2],
                chunk[index * 4 + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut digest = [0_u8; 20];
    for (index, value) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        let digest = sha1(b"abc");
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
        let empty = sha1(b"");
        let hex: String = empty.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn client_frames_are_masked_and_decodable() {
        let mask = [0x01, 0x02, 0x03, 0x04];
        let encoded = encode_frame(OPCODE_BINARY, b"hello", mask);
        let (message, consumed) = decode_frame(&encoded).expect("可解码").expect("完整帧");
        assert_eq!(consumed, encoded.len());
        assert_eq!(message, WsMessage::Binary(b"hello".to_vec()));
    }

    #[test]
    fn decode_waits_for_more_bytes() {
        let mask = [0x0a, 0x0b, 0x0c, 0x0d];
        let encoded = encode_frame(OPCODE_TEXT, b"partial", mask);
        assert!(decode_frame(&encoded[..4]).expect("半截不算错").is_none());
    }

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

    #[test]
    fn wss_url_is_split_into_host_port_path() {
        assert_eq!(
            split_wss_url("wss://example.feishu.cn/connect?device_id=d1").expect("可切分"),
            (
                "example.feishu.cn".to_string(),
                443,
                "/connect?device_id=d1".to_string()
            )
        );
        assert_eq!(
            split_wss_url("wss://host:8443/x").expect("可切分"),
            ("host".to_string(), 8443, "/x".to_string())
        );
        assert!(split_wss_url("https://example.com").is_err());
    }
}
