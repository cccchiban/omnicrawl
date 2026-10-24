//! 平台无关的 WebSocket 客户端：握手、帧编解码、TLS 连接与 SHA-1。
//!
//! 两个长连接平台（飞书的 `pbbp2` 二进制帧、QQ 网关的 JSON 文本帧）共用这套原语。
//! 协议栈自实现而不引入第三方 WebSocket 库：仓库既有做法是手写匹配器与 SHA-1
//! （见 `omnicrawl-llm`），这里沿用同一取舍。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;

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

/// 一次轮询的结果：`Idle` 表示读取超时（没有新帧），调用方可以借机发心跳或检查停止标志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsPoll {
    Message(WsMessage),
    Idle,
}

/// 计算 `Sec-WebSocket-Accept`（RFC 6455：key + GUID 的 SHA-1 再 base64）。
pub fn websocket_accept(key: &str) -> String {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = sha1(&data);
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// 生成握手请求文本（GET 升级请求）。
pub fn handshake_request(host: &str, path: &str, key: &str, user_agent: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         User-Agent: {user_agent}\r\n\
         \r\n",
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

/// TLS 上的阻塞流。
///
/// 保留握手阶段多读到的字节：服务端可能把 `101` 响应与第一条数据帧放在同一个 TLS 记录里，
/// 那部分必须由后续帧解析先消费，否则首帧丢失。
pub struct TlsStream<S: Read + Write = rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    inner: S,
    pending: Vec<u8>,
}

impl<S: Read + Write> TlsStream<S> {
    fn new(inner: S, pending: Vec<u8>) -> TlsStream<S> {
        TlsStream { inner, pending }
    }
}

impl<S: Read + Write> Read for TlsStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.pending.is_empty() {
            let take = self.pending.len().min(buffer.len());
            buffer[..take].copy_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
            return Ok(take);
        }
        self.inner.read(buffer)
    }
}

impl<S: Read + Write> Write for TlsStream<S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 建立 `wss://` 连接并完成 WebSocket 握手校验。
///
/// `read_timeout` 是每次底层读取的上限：长连接要靠它把控制权交回主循环（发心跳、看停止标志），
/// 因此它是「多久轮询一次」而不是「多久算超时」。
pub fn connect_wss(
    url: &str,
    read_timeout: Duration,
    user_agent: &str,
) -> Result<TlsStream, String> {
    let (host, port, path) = split_wss_url(url)?;
    let tcp = TcpStream::connect((host.as_str(), port)).map_err(|error| error.to_string())?;
    tcp.set_read_timeout(Some(read_timeout))
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
    let request = handshake_request(&host, &path, &key, user_agent);
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;
    let (response, remainder) = read_http_response(&mut stream)?;
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
    Ok(TlsStream::new(stream, remainder))
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

/// 一条 WebSocket 连接：发送文本帧、轮询收到帧，`Ping` 自动回 `Pong`。
pub struct WebSocket<S: Read + Write> {
    stream: S,
    read_buffer: Vec<u8>,
    mask_seed: u64,
}

impl<S: Read + Write> WebSocket<S> {
    pub fn new(stream: S, seed: u64) -> WebSocket<S> {
        WebSocket {
            stream,
            read_buffer: Vec::new(),
            mask_seed: seed,
        }
    }

    /// 下一条掩码。
    pub fn next_mask(&mut self) -> [u8; 4] {
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        masking_key(self.mask_seed)
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .map_err(|error| error.to_string())?;
        self.stream.flush().map_err(|error| error.to_string())
    }

    /// 发送一个文本帧。
    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        let mask = self.next_mask();
        let bytes = encode_frame(OPCODE_TEXT, text.as_bytes(), mask);
        self.write_all(&bytes)
    }

    /// 轮询一条消息：`Ping` 就地回 `Pong` 并继续等，读取超时返回 [`WsPoll::Idle`]。
    pub fn poll(&mut self) -> Result<WsPoll, String> {
        loop {
            if let Some((message, consumed)) = decode_frame(&self.read_buffer)? {
                self.read_buffer.drain(..consumed);
                if let WsMessage::Ping(payload) = &message {
                    let mask = self.next_mask();
                    let bytes = encode_frame(OPCODE_PONG, payload, mask);
                    self.write_all(&bytes)?;
                    continue;
                }
                return Ok(WsPoll::Message(message));
            }
            let mut chunk = [0_u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("长连接被对端关闭".to_string()),
                Ok(read) => self.read_buffer.extend_from_slice(&chunk[..read]),
                Err(error) if is_timeout(&error) => return Ok(WsPoll::Idle),
                Err(error) => return Err(error.to_string()),
            }
        }
    }
}

/// 读取错误是否只是「这一轮没有数据」。
pub fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// 读取握手响应头，连同紧随其后的多读字节一起返回。
///
/// 服务端常把 `101` 响应与第一条数据帧写在同一批字节里，剩余部分必须交还给帧解析。
fn read_http_response<R: Read>(stream: &mut R) -> Result<(String, Vec<u8>), String> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let body = raw.split_off(index + 4);
            return Ok((String::from_utf8_lossy(&raw).to_string(), body));
        }
    }
    Ok((String::from_utf8_lossy(&raw).to_string(), Vec::new()))
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

    #[test]
    fn handshake_request_carries_the_user_agent() {
        let request = handshake_request("h", "/p", "key", "agent/1");
        assert!(request.contains("User-Agent: agent/1"));
        assert!(request.starts_with("GET /p HTTP/1.1\r\n"));
    }

    /// QQ 网关会把 `101` 响应与 `op=10 Hello` 帧放在同一批字节里，剩余部分不能丢。
    #[test]
    fn bytes_after_the_handshake_headers_are_kept_for_frame_parsing() {
        let mut raw = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n".to_vec();
        raw.extend_from_slice(&encode_frame(
            OPCODE_TEXT,
            br#"{"d":{"heartbeat_interval":41250},"op":10}"#,
            [0_u8; 4],
        ));
        let mut reader = std::io::Cursor::new(raw);
        let (headers, remainder) = read_http_response(&mut reader).expect("读到响应头");
        assert!(headers.starts_with("HTTP/1.1 101"), "{headers}");
        let (message, _) = decode_frame(&remainder).expect("可解码").expect("完整帧");
        assert_eq!(
            message,
            WsMessage::Text(r#"{"d":{"heartbeat_interval":41250},"op":10}"#.to_string())
        );
    }

    /// 响应头单独到达时没有剩余字节，后续读取直接走底层流。
    #[test]
    fn headers_without_extra_bytes_leave_no_remainder() {
        let mut reader = std::io::Cursor::new(b"HTTP/1.1 101 OK\r\n\r\n".to_vec());
        let (headers, remainder) = read_http_response(&mut reader).expect("读到响应头");
        assert!(headers.starts_with("HTTP/1.1 101"));
        assert!(remainder.is_empty());
    }

    #[test]
    fn pending_bytes_are_served_before_the_underlying_stream() {
        let mut stream = TlsStream::new(std::io::Cursor::new(b"rest".to_vec()), vec![1_u8, 2, 3]);
        let mut buffer = [0_u8; 16];
        assert_eq!(stream.read(&mut buffer).expect("读 pending"), 3);
        assert_eq!(&buffer[..3], &[1, 2, 3]);
        assert_eq!(stream.read(&mut buffer).expect("读底层"), 4);
        assert_eq!(&buffer[..4], b"rest");
    }

    /// 读取超时不算错误：长连接要靠它把控制权交回主循环。
    #[test]
    fn timeout_errors_are_idle_not_failure() {
        assert!(is_timeout(&std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "x"
        )));
        assert!(is_timeout(&std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "x"
        )));
        assert!(!is_timeout(&std::io::Error::other("x")));
    }
}
