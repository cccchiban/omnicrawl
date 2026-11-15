//! 最小 WebSocket 客户端：握手、帧编解码与 TLS 连接。
//!
//! DeepSeek 的流式合成要走 `wss://`，而本 crate 只需要「发几条文本帧、收一批二进制帧」。
//! 因此不复用 `omnicrawl-connectors` 的整套长连接（那会把连接器与配置层一起拖进 TTS），
//! 只保留这里需要的部分。取舍与仓库其余平台接入一致：协议栈自实现，不引第三方库。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 收到的一条 WebSocket 消息（控制帧不交给上层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Close,
}

/// 一次轮询的结果：`Idle` 表示这一轮读取超时，调用方按总时限决定是否继续等。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsPoll {
    Message(WsMessage),
    Idle,
}

/// 一条 WebSocket 连接：发文本帧、轮询收帧，`Ping` 就地回 `Pong`。
///
/// `S` 是握手之后的字节流：生产用 TLS 流，测试可直接喂内存缓冲。
pub struct WsClient<S: Read + Write> {
    stream: S,
    buffer: Vec<u8>,
    mask_seed: u64,
}

impl<S: Read + Write> WsClient<S> {
    pub fn new(stream: S) -> WsClient<S> {
        WsClient {
            stream,
            buffer: Vec::new(),
            mask_seed: 0x2545_F491_4F6C_DD1D,
        }
    }

    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_seed = self
            .mask_seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut state = self.mask_seed;
        let mut mask = [0_u8; 4];
        for slot in mask.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = (state & 0xff) as u8;
        }
        mask
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mask = self.next_mask();
        let bytes = encode_frame(opcode, payload, mask);
        self.stream
            .write_all(&bytes)
            .map_err(|error| format!("WebSocket 发送失败：{error}"))?;
        self.stream
            .flush()
            .map_err(|error| format!("WebSocket 发送失败：{error}"))
    }

    /// 发送一个文本帧（合成请求的文字与结束标记都走这里）。
    pub fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.write_frame(OPCODE_TEXT, text.as_bytes())
    }

    /// 轮询一条消息；`Ping` 就地回 `Pong` 后继续等，读取超时返回 [`WsPoll::Idle`]。
    pub fn poll(&mut self) -> Result<WsPoll, String> {
        loop {
            if let Some((frame, consumed)) = decode_frame(&self.buffer)? {
                self.buffer.drain(..consumed);
                match frame {
                    Frame::Message(message) => return Ok(WsPoll::Message(message)),
                    Frame::Ping(payload) => {
                        self.write_frame(OPCODE_PONG, &payload)?;
                        continue;
                    }
                    Frame::Pong => continue,
                }
            }
            let mut chunk = [0_u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(WsPoll::Message(WsMessage::Close)),
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(error) if is_timeout(&error) => return Ok(WsPoll::Idle),
                Err(error) => return Err(format!("WebSocket 读取失败：{error}")),
            }
        }
    }
}

/// 解码出的一帧：数据帧交给上层，控制帧在此就地处理。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Frame {
    Message(WsMessage),
    Ping(Vec<u8>),
    Pong,
}

/// 读取错误是否只是「这一轮没有数据」。
pub fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// 计算 `Sec-WebSocket-Accept`（RFC 6455：key + GUID 的 SHA-1 再 base64）。
pub fn accept_for(key: &str) -> String {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(ACCEPT_GUID);
    base64::engine::general_purpose::STANDARD.encode(sha1(&data))
}

/// 拆出 `wss://host:port/path?query`。
pub fn split_url(url: &str) -> Result<(String, u16, String, bool), String> {
    let secure = url.starts_with("wss://");
    let stripped = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .ok_or_else(|| format!("不支持的连接地址：{url}"))?;
    let (authority, path) = match stripped.find('/') {
        Some(index) => (&stripped[..index], &stripped[index..]),
        None => (stripped, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|error| format!("连接地址端口无效：{error}"))?,
        ),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err("连接地址缺少主机名".to_string());
    }
    Ok((host.to_string(), port, path.to_string(), secure))
}

/// 建立 `wss://` 连接并完成握手校验，返回可发帧的客户端。
///
/// `extra_headers` 是握手请求要额外带的头（此处用于 `Authorization`）。
/// `read_timeout` 是每次底层读取的上限，用作轮询节奏而不是总超时。
pub fn connect_wss(
    url: &str,
    extra_headers: &[(&str, &str)],
    read_timeout: Duration,
    user_agent: &str,
) -> Result<WsClient<TlsStream>, String> {
    let (host, port, path, secure) = split_url(url)?;
    if !secure {
        return Err(format!("流式合成只支持 wss:// 地址：{url}"));
    }
    let tcp = TcpStream::connect((host.as_str(), port))
        .map_err(|error| format!("连接 {host}:{port} 失败：{error}"))?;
    tcp.set_read_timeout(Some(read_timeout))
        .map_err(|error| error.to_string())?;

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|error| format!("主机名无法用于 TLS：{error}"))?;
    let connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|error| format!("TLS 初始化失败：{error}"))?;
    let mut stream = rustls::StreamOwned::new(connection, tcp);

    let key = base64::engine::general_purpose::STANDARD.encode(random_key_bytes());
    let mut request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         User-Agent: {user_agent}\r\n",
    );
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("发送握手请求失败：{error}"))?;

    let (headers, remainder) = read_http_response(&mut stream)?;
    verify_handshake(&headers, &key)?;
    Ok(WsClient::new(TlsStream {
        inner: stream,
        pending: remainder,
    }))
}

/// TLS 上的阻塞流，保留握手时多读到的字节。
///
/// 服务端可能把 `101` 响应与第一条数据帧放在同一批字节里，那部分必须交还给帧解析，
/// 否则首批音频会丢。
pub struct TlsStream {
    inner: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    pending: Vec<u8>,
}

impl Read for TlsStream {
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

impl Write for TlsStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// 校验 `101` 与 `Sec-WebSocket-Accept`。
fn verify_handshake(headers: &str, key: &str) -> Result<(), String> {
    let status = headers.lines().next().unwrap_or("");
    if !status.starts_with("HTTP/1.1 101") {
        return Err(format!(
            "WebSocket 握手失败：{}",
            headers.lines().take(8).collect::<Vec<_>>().join(" | ")
        ));
    }
    let expected = accept_for(key);
    if !headers.to_lowercase().contains(&expected.to_lowercase()) {
        return Err("WebSocket 握手未返回预期的 Sec-WebSocket-Accept".to_string());
    }
    Ok(())
}

/// 读取握手响应头，连同紧随其后的多读字节一起返回。
fn read_http_response<R: Read>(stream: &mut R) -> Result<(String, Vec<u8>), String> {
    let mut raw = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if is_timeout(&error) => {
                return Err("等待 WebSocket 握手响应超时".to_string())
            }
            Err(error) => return Err(format!("读取握手响应失败：{error}")),
        };
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
    let mut state = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut key = [0_u8; 16];
    for slot in key.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = (state & 0xff) as u8;
    }
    key
}

/// 编码一个客户端帧（客户端发出的帧必须掩码）。
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

/// 从缓冲区取出一帧；数据不齐时返回 `Ok(None)`。
fn decode_frame(buffer: &[u8]) -> Result<Option<(Frame, usize)>, String> {
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
    let frame = match opcode {
        OPCODE_TEXT => Frame::Message(WsMessage::Text(
            String::from_utf8_lossy(&payload).to_string(),
        )),
        // 服务端会把大数据拆片：首帧后继的 `continuation` 同样是音频，按二进制收。
        OPCODE_BINARY | OPCODE_CONTINUATION => Frame::Message(WsMessage::Binary(payload)),
        OPCODE_CLOSE => Frame::Message(WsMessage::Close),
        OPCODE_PING => Frame::Ping(payload),
        OPCODE_PONG => Frame::Pong,
        other => return Err(format!("未知的 WebSocket opcode {other}")),
    };
    Ok(Some((frame, end)))
}

/// SHA-1（RFC 3174）：握手需要；仓库其余位置也是手写实现，不为此引依赖。
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
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
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
        let hex: String = sha1(b"abc")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
        let empty: String = sha1(b"").iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(empty, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            accept_for("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn text_frames_round_trip() {
        let encoded = encode_frame(OPCODE_TEXT, "你好".as_bytes(), [1, 2, 3, 4]);
        let (frame, consumed) = decode_frame(&encoded).expect("可解码").expect("完整帧");
        assert_eq!(consumed, encoded.len());
        assert_eq!(
            frame,
            Frame::Message(WsMessage::Text("你好".to_string()))
        );
    }

    #[test]
    fn binary_frames_round_trip_across_length_encodings() {
        for size in [0usize, 1, 125, 126, 300, 70_000] {
            let payload: Vec<u8> = (0..size).map(|value| value as u8).collect();
            let encoded = encode_frame(OPCODE_BINARY, &payload, [9, 8, 7, 6]);
            let (frame, consumed) = decode_frame(&encoded).expect("可解码").expect("完整帧");
            assert_eq!(consumed, encoded.len(), "size={size}");
            assert_eq!(frame, Frame::Message(WsMessage::Binary(payload)), "size={size}");
        }
    }

    #[test]
    fn incomplete_frames_wait_for_more_bytes() {
        let encoded = encode_frame(OPCODE_BINARY, &[0u8; 300], [0; 4]);
        assert!(decode_frame(&encoded[..10]).expect("半截不算错").is_none());
    }

    #[test]
    fn url_is_split_into_host_port_path() {
        let (host, port, path, secure) =
            split_url("wss://api.deepseek.com/v1/tools/tts?voice_id=stella").expect("可切分");
        assert_eq!(host, "api.deepseek.com");
        assert_eq!(port, 443);
        assert_eq!(path, "/v1/tools/tts?voice_id=stella");
        assert!(secure);
        assert!(split_url("https://example.com").is_err());
    }

    /// `Ping` 不交给上层：`poll` 就地回 `Pong`，随后返回真正的数据帧。
    #[test]
    fn ping_is_answered_and_skipped() {
        let mut raw = encode_frame(OPCODE_PING, b"hb", [0; 4]);
        raw.extend_from_slice(&encode_frame(OPCODE_BINARY, b"audio", [0; 4]));
        let mut client = WsClient::new(std::io::Cursor::new(raw));
        assert_eq!(
            client.poll().expect("能读到消息"),
            WsPoll::Message(WsMessage::Binary(b"audio".to_vec()))
        );
    }
}
