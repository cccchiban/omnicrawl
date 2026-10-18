//! 驱动层：把 TCP、TLS 握手、HTTP/2 串成一条能真正发起请求的通路。
//!
//! 前三层各自都能独立测试，但它们的组合会引入一类新问题：**边界不对齐**。
//! TLS 记录最大 2^14，HTTP/2 帧最大 2^14（默认），两者毫无关系——一个 HTTP/2 帧
//! 可能横跨两三条 TLS 记录，一条 TLS 记录也可能含多个帧的碎片。所以这一层必须同时
//! 维护两个缓冲：收方向的「未成记录的字节」与交给 `h2conn` 的「未成帧的字节」。
//!
//! 传输通过 [`Transport`] 抽象，因此真实 socket 与内存回环走同一条代码路径——
//! 测试不需要网络，也不需要用别的 TLS 实现当对端。

use std::io::{Read, Write};

use crate::h2conn::{ClientConnection, ConnectionError, Event, StreamState};
use crate::hpack::Header;
use crate::profile::Family;
use crate::record::{parse_record, ContentType, Record, RecordError, MAX_PLAINTEXT};
use crate::session::{ClientHandshake, HandshakeError, State};
use crate::verify::TrustAnchors;
use crate::wire::EncodeError;

/// 传输抽象。只有读与写两件事——TLS 1.3 不需要在这一层做任何重传或分帧。
pub trait Transport {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize>;
    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()>;
}

impl Transport for std::net::TcpStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        Read::read(self, buffer)
    }

    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        Write::write_all(self, buffer)
    }
}

#[derive(Debug)]
pub enum DriverError {
    Io(std::io::Error),
    Handshake(HandshakeError),
    Record(RecordError),
    Connection(ConnectionError),
    Encode(EncodeError),
    /// 对端在握手完成前就关了连接。
    EofBeforeHandshake,
    /// 连接被对端正常关闭。
    Eof,
    /// ALPN 没有协商出 `h2`。
    UnsupportedAlpn(Option<String>),
    /// 响应不完整或不合规。
    BadResponse(String),
    /// 在握手完成前调用了需要加密通道的操作。
    NotEstablished,
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "IO 错误：{error}"),
            Self::Handshake(error) => write!(formatter, "握手失败：{error}"),
            Self::Record(error) => write!(formatter, "记录层错误：{error}"),
            Self::Connection(error) => write!(formatter, "HTTP/2 错误：{error}"),
            Self::Encode(error) => write!(formatter, "编码错误：{error}"),
            Self::EofBeforeHandshake => formatter.write_str("握手完成前连接被关闭"),
            Self::Eof => formatter.write_str("连接被对端关闭"),
            Self::UnsupportedAlpn(protocol) => match protocol {
                Some(value) => write!(formatter, "对端选了 ALPN {value}，本实现只支持 h2"),
                None => formatter.write_str("对端没有协商 ALPN，无法确定协议"),
            },
            Self::BadResponse(detail) => write!(formatter, "响应不合规：{detail}"),
            Self::NotEstablished => formatter.write_str("TLS 握手尚未完成"),
        }
    }
}

impl std::error::Error for DriverError {}

impl From<std::io::Error> for DriverError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<HandshakeError> for DriverError {
    fn from(error: HandshakeError) -> Self {
        Self::Handshake(error)
    }
}

impl From<RecordError> for DriverError {
    fn from(error: RecordError) -> Self {
        Self::Record(error)
    }
}

impl From<ConnectionError> for DriverError {
    fn from(error: ConnectionError) -> Self {
        Self::Connection(error)
    }
}

impl From<EncodeError> for DriverError {
    fn from(error: EncodeError) -> Self {
        Self::Encode(error)
    }
}

/// 一个响应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

impl Response {
    /// 取某个头部的值（同名多值时取第一个）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }
}

/// 应用层协议：由 ALPN 协商结果决定，一套 TLS 记录层同时服务两者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Http2,
    Http1,
}

impl Application {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Http2 => "h2",
            Self::Http1 => "http/1.1",
        }
    }
}

/// HTTP/2 禁止的连接专有头（RFC 9113 §8.2.2）：`Connection` 及其点名的头、
/// `Keep-Alive`、`Proxy-Connection`、`Transfer-Encoding`、`Upgrade`。
fn is_connection_specific(name: &str) -> bool {
    [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "upgrade",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// 已建立的连接。
///
/// 不实现 `Debug`：持有传输与密钥状态，打印出来既无用又容易泄露。
pub struct Connection<S: Transport> {
    transport: S,
    handshake: ClientHandshake,
    h2: ClientConnection,
    authority: String,
    /// ALPN 协商出的应用层协议。握手完成后才确定。
    protocol: Application,
    /// 收方向的原始字节：可能停在一条记录中间。
    inbound: Vec<u8>,
    /// HTTP/1.1 已经解密但还没被上层消费的字节（一条记录可能带来多个响应的片段）。
    app_inbound: Vec<u8>,
}

impl Connection<std::net::TcpStream> {
    /// 解析域名并建立连接（带超时）。
    ///
    /// 只负责「域名 → 一条已就绪的 TCP 连接 → 握手」。不做连接池、不做重试、
    /// 不跟随重定向——那些属于调用方的策略，放进来会让这一层的职责说不清。
    ///
    /// `timeout` 同时作用于 DNS 解析之后的 TCP 连接与后续每次读写：没有超时的
    /// 阻塞 socket 在真实网络里会永久挂住，这类问题在测试里永远复现不出来。
    pub fn connect_to(
        host: &str,
        port: u16,
        family: Family,
        trust: Option<TrustAnchors>,
        timeout: std::time::Duration,
    ) -> Result<Self, DriverError> {
        let stream = connect_stream(host, port, timeout)?;
        Self::connect(stream, family, host, trust)
    }

    /// 经 HTTP 代理的 CONNECT 隧道建立连接。
    ///
    /// `proxy` 形如 `http://127.0.0.1:7890`（也接受不带协议的 `host:port`）。只实现
    /// `CONNECT` 这一种：目标一律是 TLS，明文代理转发会让 TLS 指纹失去意义，也会让
    /// 代理看到完整请求。
    ///
    /// 隧道建立后代理可能在同一个 TCP 段里带了后续字节，因此 CONNECT 应答必须逐字节读，
    /// 读到空行为止——一次 `read` 多吞一个字节就会把服务端第一条 TLS 记录的开头吃掉。
    pub fn connect_via_proxy(
        proxy: &str,
        host: &str,
        port: u16,
        family: Family,
        trust: Option<TrustAnchors>,
        timeout: std::time::Duration,
    ) -> Result<Self, DriverError> {
        let (proxy_host, proxy_port) = parse_proxy(proxy)?;
        let mut stream = connect_stream(&proxy_host, proxy_port, timeout)?;
        let mut request = format!(
            "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\n\r\n"
        );
        if let Some((user, password)) = proxy_credentials(proxy) {
            request = format!(
                "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic {}\r\n\r\n",
                base64_encode(format!("{user}:{password}").as_bytes())
            );
        }
        Write::write_all(&mut stream, request.as_bytes())?;

        // 逐个字节读到空行为止：多读一个字节就会把 TLS 记录的开头吞进这里。
        let mut head = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let read = Read::read(&mut stream, &mut byte)?;
            if read == 0 {
                return Err(DriverError::BadResponse(
                    "代理在 CONNECT 应答中途关闭了连接".to_string(),
                ));
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
            if head.len() > 16 * 1024 {
                return Err(DriverError::BadResponse(
                    "代理的 CONNECT 应答头过长".to_string(),
                ));
            }
        }
        let head_text = String::from_utf8_lossy(&head).to_string();
        let status_line = head_text.lines().next().unwrap_or_default();
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| DriverError::BadResponse(format!("代理应答无法解析：{status_line}")))?;
        if status != 200 {
            return Err(DriverError::BadResponse(format!(
                "代理拒绝 CONNECT {host}:{port}（HTTP {status}）"
            )));
        }
        let connection = Self::connect(stream, family, host, trust)?;
        Ok(connection)
    }
}

/// 解析代理地址：允许 `http://`、`https://` 前缀（后者按明文代理处理并在此处拒绝）与裸
/// `host:port`。代理认证写在 URL 的 userinfo 里（`http://user:pass@host:port`）。
fn parse_proxy(proxy: &str) -> Result<(String, u16), DriverError> {
    let trimmed = proxy.trim();
    let without_scheme = match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            if scheme.eq_ignore_ascii_case("https") {
                return Err(DriverError::BadResponse(
                    "不支持 TLS 代理（https://）：本实现只做明文 HTTP CONNECT".to_string(),
                ));
            }
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("socks5") {
                return Err(DriverError::BadResponse(format!(
                    "不支持的代理协议：{scheme}"
                )));
            }
            if scheme.eq_ignore_ascii_case("socks5") {
                return Err(DriverError::BadResponse(
                    "不支持 SOCKS 代理：本实现只做 HTTP CONNECT".to_string(),
                ));
            }
            rest
        }
        None => trimmed,
    };
    // 去掉可能的 userinfo 与路径。
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let authority = match authority.rsplit_once('@') {
        Some((_, rest)) => rest,
        None => authority,
    };
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| DriverError::BadResponse(format!("代理端口无效：{port}")))?;
            Ok((host.to_string(), port))
        }
        None => Ok((authority.to_string(), 80)),
    }
}

fn proxy_credentials(proxy: &str) -> Option<(String, String)> {
    let without_scheme = match proxy.split_once("://") {
        Some((_, rest)) => rest,
        None => proxy,
    };
    let authority = without_scheme.split('/').next()?;
    let (userinfo, _) = authority.rsplit_once('@')?;
    let (user, password) = userinfo.split_once(':')?;
    Some((user.to_string(), password.to_string()))
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 0x3f] as char);
        out.push(TABLE[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// 建一条带超时的 TCP 连接：多个候选地址依次尝试，全失败时回报最后一个错误。
fn connect_stream(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
) -> Result<std::net::TcpStream, DriverError> {
    let address = format!("{host}:{port}");
    let mut last_error = None;
    let mut stream = None;
    for candidate in std::net::ToSocketAddrs::to_socket_addrs(&address)? {
        match std::net::TcpStream::connect_timeout(&candidate, timeout) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = match stream {
        Some(stream) => stream,
        None => {
            return Err(last_error.map_or_else(
                || DriverError::BadResponse(format!("{address} 没有解析出任何地址")),
                DriverError::Io,
            ))
        }
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // Nagle 会把小记录攒在一起，导致握手多等一个 RTT。
    stream.set_nodelay(true)?;
    Ok(stream)
}

impl<S: Transport> Connection<S> {
    /// 完成 TLS 握手与 HTTP/2 前言。
    ///
    /// `trust` 为 `None` 走 [`crate::session::PeerPolicy::Inspect`]：只验
    /// CertificateVerify 签名，不验证书链。指纹采集与自签服务器用这个；
    /// 要信任对端必须给出受信根。
    pub fn connect(
        transport: S,
        family: Family,
        host: &str,
        trust: Option<TrustAnchors>,
    ) -> Result<Self, DriverError> {
        let handshake = match trust {
            Some(anchors) => ClientHandshake::new(family, host).with_trust(anchors),
            None => ClientHandshake::new(family, host),
        };
        let mut h2 = ClientConnection::new();
        // HTTP/2 连接指纹（SETTINGS 内容与顺序）必须与 TLS 档案同族，否则两层各说各话。
        h2.apply_profile(&crate::profile::http2_profile_for(family));
        let mut connection = Self {
            transport,
            handshake,
            h2,
            authority: host.to_string(),
            protocol: Application::Http2,
            inbound: Vec::new(),
            app_inbound: Vec::new(),
        };
        connection.perform_handshake()?;
        Ok(connection)
    }

    fn perform_handshake(&mut self) -> Result<(), DriverError> {
        let rng = ring::rand::SystemRandom::new();
        let client_hello = self.handshake.start(&rng)?;
        self.transport.write_all(&client_hello)?;

        loop {
            if self.handshake.state() == State::SendClientFinished {
                break;
            }
            if self.handshake.state() == State::Connected {
                break;
            }
            let record = self.read_record().map_err(|error| match error {
                DriverError::Eof => DriverError::EofBeforeHandshake,
                other => other,
            })?;
            self.handshake.feed_record(&record)?;
            // HRR 不推进状态，但要求我们换组重发 ClientHello。
            if let Some(retry) = self.handshake.take_retry_client_hello() {
                self.transport.write_all(&retry)?;
            }
        }

        if self.handshake.state() == State::SendClientFinished {
            let sealed = self.handshake.finish()?;
            // `finish` 返回的是已加密的载荷，外层记录要自己封。
            let record = Record {
                kind: ContentType::ApplicationData,
                payload: sealed,
            };
            self.transport.write_all(&record.encode(0x0303)?)?;
        }

        match self.handshake.alpn() {
            Some("h2") => {
                self.protocol = Application::Http2;
                // 连接前言：先发 24 字节固定串，再发 SETTINGS。漏掉前言服务端会直接 GOAWAY。
                self.send_h2(crate::http2::CONNECTION_PREFACE)?;
                let preamble = self.h2.preamble();
                self.send_h2(&preamble)?;

                // 等服务端的 SETTINGS 并回 ACK。真实浏览器会在这个点完成设置交换再发请求，
                // 不等待就发会在部分服务端（例如 example.com 的 nginx）被判为协议违规并 GOAWAY。
                self.exchange_settings()?;
                Ok(())
            }
            // 服务端只支持 1.1 时不必重连：TLS 层已经完全一致，换掉的只是应用层编解码。
            // 这让指纹在「只回 http/1.1 的老站点」上同样生效——那类站点恰恰更爱按指纹拦。
            Some("http/1.1") => {
                self.protocol = Application::Http1;
                Ok(())
            }
            other => Err(DriverError::UnsupportedAlpn(other.map(str::to_string))),
        }
    }

    /// 读到服务端 SETTINGS 并回 ACK，之后才允许发请求。
    ///
    /// 读到服务端 SETTINGS 并回 ACK，之后才允许发请求。
    ///
    /// 只处理设置交换期间会出现的帧。若同一次读取里还夹带了别的帧（例如服务端
    /// 抢先发来的响应），`pump` 已经把它们交给 HTTP/2 层处理，事件会被丢弃——
    /// 因此这里必须在**发请求之前**调用，不能复用后面的读取。
    fn exchange_settings(&mut self) -> Result<(), DriverError> {
        loop {
            let events = self.pump()?;
            if events
                .iter()
                .any(|event| matches!(event, Event::PeerSettings(_)))
            {
                return Ok(());
            }
            if events
                .iter()
                .any(|event| matches!(event, Event::GoAway { .. }))
            {
                return Ok(());
            }
        }
    }

    /// 读一条完整记录，不足则继续向传输层要字节。
    fn read_record(&mut self) -> Result<Record, DriverError> {
        loop {
            match parse_record(&self.inbound) {
                Ok((record, used)) => {
                    self.inbound.drain(..used);
                    return Ok(record);
                }
                Err(RecordError::Incomplete { .. }) => self.fill()?,
                Err(error) => return Err(DriverError::Record(error)),
            }
        }
    }

    fn fill(&mut self) -> Result<(), DriverError> {
        let mut buffer = [0u8; 8192];
        let read = self.transport.read(&mut buffer)?;
        if read == 0 {
            return Err(DriverError::Eof);
        }
        self.inbound.extend_from_slice(&buffer[..read]);
        Ok(())
    }

    /// 把明文 HTTP/2 字节按 2^14 切成记录后发出。
    fn send_h2(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        if bytes.is_empty() {
            return Ok(());
        }
        for chunk in bytes.chunks(MAX_PLAINTEXT) {
            let record = self.handshake.seal_application(chunk)?;
            self.transport.write_all(&record.encode(0x0303)?)?;
        }
        Ok(())
    }

    /// 读一条记录、解密、喂给 HTTP/2 层，并把该层产出的控制帧发回去。
    fn pump(&mut self) -> Result<Vec<Event>, DriverError> {
        loop {
            let record = self.read_record()?;
            // 服务端可能在握手后仍发一条兼容性 CCS，忽略。
            if record.kind == ContentType::ChangeCipherSpec {
                continue;
            }
            let plaintext = self.handshake.open_application(&record.payload)?;
            // NewSessionTicket 等握手后消息会解出空载荷，跳过即可。
            if plaintext.is_empty() {
                continue;
            }
            let feed = self.h2.feed(&plaintext)?;
            self.send_h2(&feed.outbound)?;
            return Ok(feed.events);
        }
    }

    pub fn alpn(&self) -> Option<&str> {
        self.handshake.alpn()
    }

    /// 协商出的应用层协议。
    pub fn protocol(&self) -> Application {
        self.protocol
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// 对端叶证书（DER）。证书可信性取决于连接时的 `trust` 参数。
    pub fn peer_certificate(&self) -> Option<&[u8]> {
        self.handshake.server_certificate()
    }

    /// 发一个请求并等到响应结束。
    ///
    /// `body` 为 `None` 表示没有请求体（HEADERS 直接带 END_STREAM）。
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<Response, DriverError> {
        let headers = [
            Header::new("user-agent", "omnicrawl/1.0"),
            Header::new("accept", "*/*"),
            // 不声明 `accept-encoding`：本 crate 不做内容解压，声明了会让服务端返回
            // 压缩体，调用方拿到的是二进制而不是页面。需要压缩时由上层自己加。
        ];
        self.request_with_headers(method, path, &headers, body)
    }

    /// 带自定义请求头的请求。
    ///
    /// `headers` 只给普通头：伪头（`:method` `:scheme` `:authority` `:path`）由这一层
    /// 自己生成并固定在最前面——RFC 9113 §8.3.1 要求伪头先于普通头，且调用方无从知道
    /// 权威来源（可能是直连域名，也可能是代理隧道的目标）。普通头里出现以 `:` 开头的
    /// 名字是编码错误，直接拒绝而不是静默丢进 HPACK。
    pub fn request_with_headers(
        &mut self,
        method: &str,
        path: &str,
        headers: &[Header],
        body: Option<&[u8]>,
    ) -> Result<Response, DriverError> {
        for header in headers {
            if header.name.starts_with(':') {
                return Err(DriverError::BadResponse(format!(
                    "普通请求头不能以冒号开头：{}",
                    header.name
                )));
            }
        }
        match self.protocol {
            Application::Http2 => self.request_h2(method, path, headers, body),
            Application::Http1 => self.request_h1(method, path, headers, body),
        }
    }

    fn request_h2(
        &mut self,
        method: &str,
        path: &str,
        headers: &[Header],
        body: Option<&[u8]>,
    ) -> Result<Response, DriverError> {
        let mut all = Vec::with_capacity(headers.len() + 4);
        all.push(Header::new(":method", method));
        all.push(Header::new(":scheme", "https"));
        all.push(Header::new(":authority", self.authority.clone()));
        all.push(Header::new(":path", path));
        for header in headers {
            // 连接专有头在 HTTP/2 里非法（RFC 9113 §8.2.2），上游若漏了过滤就在这里挡住。
            if is_connection_specific(&header.name) {
                return Err(DriverError::BadResponse(format!(
                    "HTTP/2 不允许连接专有头：{}",
                    header.name
                )));
            }
            all.push(header.clone());
        }
        self.exchange(all, body)
    }

    /// HTTP/1.1 请求。
    ///
    /// 每个请求都带 `Connection: close`：本连接不复用（复用需要处理 `Content-Length`
    /// 之外的定界细节与流水线，收益远小于风险）。对 `UntilEof` 定界的响应，这一点是
    /// **必须**的——不发就永远等不到 EOF。
    fn request_h1(
        &mut self,
        method: &str,
        path: &str,
        headers: &[Header],
        body: Option<&[u8]>,
    ) -> Result<Response, DriverError> {
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(headers.len() + 2);
        let mut has_host = false;
        let mut has_connection = false;
        for header in headers {
            if header.name.eq_ignore_ascii_case("host") {
                has_host = true;
            }
            if header.name.eq_ignore_ascii_case("connection") {
                has_connection = true;
            }
            pairs.push((header.name.clone(), header.value.clone()));
        }
        if !has_host {
            pairs.push(("Host".to_string(), self.authority.clone()));
        }
        if !has_connection {
            pairs.push(("Connection".to_string(), "close".to_string()));
        }
        let body_len = body.map(|value| value.len());
        let head = crate::http1::encode_request(method, path, &self.authority, &pairs, body_len)
            .map_err(|error| DriverError::BadResponse(error.to_string()))?;
        let mut out = head;
        if let Some(body) = body {
            out.extend_from_slice(body);
        }
        self.send_app(&out)?;
        self.read_h1_response(method)
    }

    fn read_h1_response(&mut self, method: &str) -> Result<Response, DriverError> {
        let head = loop {
            match crate::http1::parse_head(&self.app_inbound)
                .map_err(|error| DriverError::BadResponse(error.to_string()))?
            {
                Some((head, used)) => {
                    self.app_inbound.drain(..used);
                    break head;
                }
                None => self.read_app()?,
            }
        };
        // 1xx 是中间响应（`100 Continue`、`103 Early Hints`）：跳过继续等真正的头。
        if (100..200).contains(&head.status) && head.status != 101 {
            return self.read_h1_response(method);
        }
        let kind = crate::http1::body_kind(&head, method)
            .map_err(|error| DriverError::BadResponse(error.to_string()))?;
        let body = match kind {
            crate::http1::BodyKind::Empty => Vec::new(),
            crate::http1::BodyKind::Length(length) => {
                while self.app_inbound.len() < length {
                    self.read_app()?;
                }
                self.app_inbound.drain(..length).collect()
            }
            crate::http1::BodyKind::Chunked => loop {
                match crate::http1::decode_chunked(&self.app_inbound)
                    .map_err(|error| DriverError::BadResponse(error.to_string()))?
                {
                    Some((body, used)) => {
                        self.app_inbound.drain(..used);
                        break body;
                    }
                    None => self.read_app()?,
                }
            },
            crate::http1::BodyKind::UntilEof => {
                // 服务端发完就关连接；已发 `Connection: close`，这里等 EOF 即可。
                loop {
                    match self.read_app() {
                        Ok(()) => {}
                        Err(DriverError::Eof) => break,
                        Err(error) => return Err(error),
                    }
                }
                std::mem::take(&mut self.app_inbound)
            }
        };
        let headers = head
            .headers
            .iter()
            .map(|(name, value)| Header::new(name.as_str(), value.as_str()))
            .collect();
        Ok(Response {
            status: head.status,
            headers,
            body,
        })
    }

    /// 发一段明文应用数据（按记录上限切片并加密）。
    ///
    /// 两个应用层协议在这里没有区别：HTTP/2 的帧与 HTTP/1.1 的报文字节都只是「按 2^14
    /// 切成 TLS 记录」的载荷，分片由记录层负责。
    fn send_app(&mut self, bytes: &[u8]) -> Result<(), DriverError> {
        self.send_h2(bytes)
    }

    /// 从传输读一批字节、解密、按当前协议交给对应层。
    fn read_app(&mut self) -> Result<(), DriverError> {
        loop {
            let record = match self.read_record() {
                Ok(record) => record,
                Err(DriverError::Eof) => return Err(DriverError::Eof),
                Err(error) => return Err(error),
            };
            // 服务端可能在握手后仍发一条兼容性 CCS，忽略。
            if record.kind == ContentType::ChangeCipherSpec {
                continue;
            }
            let plaintext = self.handshake.open_application(&record.payload)?;
            // NewSessionTicket 等握手后消息会解出空载荷，跳过即可。
            if plaintext.is_empty() {
                continue;
            }
            match self.protocol {
                Application::Http2 => {
                    let feed = self.h2.feed(&plaintext)?;
                    self.send_h2(&feed.outbound)?;
                }
                Application::Http1 => self.app_inbound.extend_from_slice(&plaintext),
            }
            return Ok(());
        }
    }

    fn exchange(
        &mut self,
        headers: Vec<Header>,
        body: Option<&[u8]>,
    ) -> Result<Response, DriverError> {
        let (stream_id, bytes) = self.h2.send_request(&headers, body.is_none())?;
        self.send_h2(&bytes)?;

        if let Some(body) = body {
            self.send_body_all(stream_id, body)?;
        }
        self.await_response(stream_id)
    }

    pub fn get(&mut self, path: &str) -> Result<Response, DriverError> {
        self.request("GET", path, None)
    }

    /// 发送请求体。窗口不够时先读一轮，等在途的 WINDOW_UPDATE。
    fn send_body_all(&mut self, stream_id: u32, body: &[u8]) -> Result<(), DriverError> {
        let mut sent_total = 0;
        while sent_total < body.len() {
            let (bytes, sent) = self.h2.send_body(stream_id, &body[sent_total..], true)?;
            if sent > 0 {
                self.send_h2(&bytes)?;
                sent_total += sent;
                continue;
            }
            // 窗口已满：必须读到 WINDOW_UPDATE 才能继续，否则会原地打转。
            let events = self.pump()?;
            let progressed = events.iter().any(|event| {
                matches!(
                    event,
                    Event::WindowUpdate { .. } | Event::GoAway { .. } | Event::StreamReset { .. }
                )
            });
            if !progressed {
                return Err(DriverError::BadResponse(
                    "发送窗口耗尽且对端没有补窗口".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn await_response(&mut self, stream_id: u32) -> Result<Response, DriverError> {
        loop {
            match self.h2.stream_state(stream_id) {
                Some(StreamState::Closed) => break,
                Some(StreamState::Reset) => {
                    return Err(DriverError::BadResponse(format!(
                        "流 {stream_id} 被对端重置"
                    )));
                }
                _ => {}
            }
            if let Some((last_stream_id, code)) = self.h2.goaway_received() {
                // RFC 9113 §6.8：GOAWAY 只表示「不再接受新流」。`last_stream_id` 及更小的
                // 流仍可能正常完成，**不能**一看到 GOAWAY 就判失败。
                //
                // 这里踩过一次：某站点把响应帧与 GOAWAY 一起发来（NO_ERROR 收尾），我们
                // 先处理 GOAWAY 就报「流未完成」，表现为明明拿到了 200 却报错。
                if stream_id > last_stream_id {
                    // `debug` 常常是唯一能说明「为什么关」的东西。
                    let debug = self
                        .h2
                        .goaway_debug()
                        .map(|bytes| format!("：{}", String::from_utf8_lossy(bytes)))
                        .unwrap_or_default();
                    return Err(DriverError::BadResponse(format!(
                        "对端发来 GOAWAY（last_stream_id={last_stream_id} code={code}），流 {stream_id} 未被处理{debug}"
                    )));
                }
            }
            self.pump()?;
        }

        let status = self.h2.status(stream_id).ok_or_else(|| {
            DriverError::BadResponse(format!("流 {stream_id} 的响应缺少 :status"))
        })?;
        let headers = self
            .h2
            .response_headers(stream_id)
            .map(|headers| headers.to_vec())
            .unwrap_or_default();
        let body = self
            .h2
            .response_body(stream_id)
            .map(|body| body.to_vec())
            .unwrap_or_default();
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    /// 取出内层连接，供需要复用传输的调用方使用。
    pub fn into_inner(self) -> S {
        self.transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只记录写入内容的假传输，用于验证「不会在没有握手时发数据」。
    struct Recorder {
        written: Vec<u8>,
        read_returns_eof: bool,
    }

    impl Transport for Recorder {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.read_returns_eof {
                Ok(0)
            } else {
                Err(std::io::Error::other("不应当读"))
            }
        }

        fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
            self.written.extend_from_slice(buffer);
            Ok(())
        }
    }

    #[test]
    fn connecting_to_a_peer_that_closes_immediately_reports_eof() {
        let transport = Recorder {
            written: Vec::new(),
            read_returns_eof: true,
        };
        match Connection::connect(transport, Family::Chrome, "example.com", None) {
            Err(error) => assert!(
                matches!(error, DriverError::EofBeforeHandshake),
                "{error:?}"
            ),
            Ok(_) => panic!("对端立刻关闭应当失败"),
        }
    }

    #[test]
    fn the_client_hello_is_written_before_any_read() {
        // 假传输的读会报 IO 错误；能走到 IO 错误说明 ClientHello 已经写过了。
        let transport = Recorder {
            written: Vec::new(),
            read_returns_eof: false,
        };
        match Connection::connect(transport, Family::Chrome, "example.com", None) {
            Err(error) => assert!(matches!(error, DriverError::Io(_)), "{error:?}"),
            Ok(_) => panic!("假传输的读会报错"),
        }
    }

    #[test]
    fn a_response_can_be_read_from_its_headers() {
        let response = Response {
            status: 200,
            headers: vec![
                Header::new("content-type", "text/html"),
                Header::new("x-trace", "abc"),
            ],
            body: b"hi".to_vec(),
        };
        assert_eq!(response.header("content-type"), Some("text/html"));
        assert_eq!(
            response.header("Content-Type"),
            Some("text/html"),
            "名字比较不分大小写"
        );
        assert_eq!(response.header("missing"), None);
    }

    #[test]
    fn proxy_addresses_parse_with_and_without_scheme() {
        assert_eq!(
            parse_proxy("http://127.0.0.1:7890").expect("应当解析"),
            ("127.0.0.1".to_string(), 7890)
        );
        assert_eq!(
            parse_proxy("127.0.0.1:7890").expect("裸地址也要解析"),
            ("127.0.0.1".to_string(), 7890)
        );
        // 缺端口时按明文代理的惯例取 80，而不是直接失败。
        assert_eq!(
            parse_proxy("proxy.internal").expect("缺端口"),
            ("proxy.internal".to_string(), 80)
        );
        // userinfo 不能进主机名，否则会去连一个叫 `user` 的主机。
        assert_eq!(
            parse_proxy("http://user:secret@127.0.0.1:3128").expect("带认证"),
            ("127.0.0.1".to_string(), 3128)
        );
        assert_eq!(
            proxy_credentials("http://user:secret@127.0.0.1:3128"),
            Some(("user".to_string(), "secret".to_string()))
        );
        assert_eq!(proxy_credentials("http://127.0.0.1:3128"), None);
    }

    /// 不支持的代理形态必须明确报错，而不是连到一个错误的地方后表现为「网络不可达」。
    #[test]
    fn unsupported_proxy_schemes_are_rejected_with_a_reason() {
        for (input, needle) in [
            ("https://proxy:443", "TLS 代理"),
            ("socks5://proxy:1080", "SOCKS"),
            ("ftp://proxy:21", "不支持的代理协议"),
        ] {
            let error = parse_proxy(input).expect_err(input);
            assert!(
                matches!(&error, DriverError::BadResponse(detail) if detail.contains(needle)),
                "{input} → {error}"
            );
        }
    }

    #[test]
    fn proxy_port_must_be_a_number() {
        let error = parse_proxy("http://127.0.0.1:不是端口").expect_err("端口非法");
        assert!(
            matches!(&error, DriverError::BadResponse(detail) if detail.contains("代理端口无效")),
            "{error}"
        );
    }

    /// 伪头与连接专有头必须在编码前就被挡住：进了 HPACK 就是一个服务端立刻 GOAWAY 的
    /// 协议错误，而错误信息完全指不回调用点。
    #[test]
    fn illegal_headers_are_refused_before_encoding() {
        let transport = Recorder {
            written: Vec::new(),
            read_returns_eof: true,
        };
        // `connect` 会先失败（对端立刻 EOF），所以这里直接构造一个未握手连接来测入参校验。
        let mut connection = Connection {
            transport,
            handshake: ClientHandshake::new(Family::Chrome, "example.com"),
            h2: ClientConnection::new(),
            authority: "example.com".to_string(),
            protocol: Application::Http2,
            inbound: Vec::new(),
            app_inbound: Vec::new(),
        };
        for name in [":method", "Connection", "Transfer-Encoding"] {
            let error = connection
                .request_with_headers("GET", "/", &[Header::new(name, "x")], None)
                .expect_err(name);
            assert!(
                matches!(error, DriverError::BadResponse(_)),
                "{name} → {error:?}"
            );
        }
    }
}
