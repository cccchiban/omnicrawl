//! 客户端握手状态机：把记录层与密钥调度串成一条可以真实推进的流程。
//!
//! 状态迁移严格按 RFC 8446 §4 的顺序，任何乱序都要报错而不是「尽量继续」——
//! 自研栈在这里放水，后续所有校验都会建立在错误的前提上。
//!
//! ```text
//! Start ──send ClientHello──> WaitServerHello
//!   ──recv ServerHello──>      WaitEncryptedExtensions   （同时派生握手密钥）
//!   ──recv EncryptedExtensions──> WaitCertificate
//!   ──recv Certificate──>      WaitCertificateVerify
//!   ──recv CertificateVerify──> WaitFinished            （此处验签）
//!   ──recv Finished──>         SendClientFinished       （派生应用密钥，只换读密钥）
//!   ──send Finished──>         Connected                （换写密钥）
//! ```
//!
//! 刻意不做的事：不处理 HelloRetryRequest（对端拒绝我们的 key_share 时直接失败，
//! 由调用方决定是否重试）、不处理 0-RTT、不处理客户端认证。

use ring::rand::SecureRandom;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

use crate::handshake::{
    hs, parse_certificate_chain, parse_certificate_verify, parse_encrypted_extensions,
    parse_server_hello, take_handshake_messages, HandshakeMessage, X25519Exchange,
};
use crate::keyschedule::{ApplicationSecrets, HandshakeSecrets, KeySchedule, Transcript};
use crate::profile::{spec_for, Family};
use crate::record::{ContentType, Record, RecordKey};
use crate::verify::{
    finished_verify_data, server_verify_context, verify_certificate_verify, verify_server_chain,
    TrustAnchors, VerifyError,
};

/// 握手状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 尚未发出 ClientHello。
    Start,
    /// 已发 ClientHello，等 ServerHello。
    WaitServerHello,
    /// 等 EncryptedExtensions（此时握手密钥已就绪，后续记录都是加密的）。
    WaitEncryptedExtensions,
    /// 等 Certificate。
    WaitCertificate,
    /// 等 CertificateVerify。
    WaitCertificateVerify,
    /// 等服务端 Finished。
    WaitFinished,
    /// 服务端 Finished 已通过校验，等本端发出 Finished。
    ///
    /// 这个状态单独存在是有意义的：此时应用读密钥已经就绪，但**写**密钥仍是握手密钥。
    /// 若在这里就允许发应用数据，服务端会因为「没收到客户端 Finished」直接断开。
    SendClientFinished,
    /// 握手完成，可以发应用数据。
    Connected,
}

/// 对端身份校验策略。
///
/// `CertificateVerify` 的**签名**校验永远开启，不受这个策略影响：那一步只证明对端持有
/// 叶证书私钥，代价极低，且失败必然意味着对方拿不出私钥。这个策略只决定要不要把
/// 叶证书往受信根上追。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerPolicy {
    /// 不验链、不验主机名。用于指纹采集、本地测试与自签证书服务器。
    ///
    /// 这个模式下的连接**不能**用于传输机密数据：任何人都能用自签证书冒充任何站点。
    Inspect,
    /// 验链、有效期与主机名（受信根由调用方给出）。
    VerifyChain,
}

/// 握手失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// 当前状态下收到了不该出现的消息。
    UnexpectedMessage { state: State, received: u8 },
    /// 对端发来 Alert。
    Alert { level: u8, description: u8 },
    /// 结构解析失败。
    Malformed(String),
    /// 校验失败。
    Verification(VerifyError),
    /// 密钥派生失败。
    KeySchedule(String),
    /// 记录层加解密失败。
    Record(String),
    /// 缺少必要信息（例如尚未解析出的密码套件）。
    Missing(&'static str),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedMessage { state, received } => {
                write!(formatter, "状态 {state:?} 下收到了握手消息类型 {received}")
            }
            Self::Alert { level, description } => {
                write!(
                    formatter,
                    "对端发来 Alert：level={level} description={description}"
                )
            }
            Self::Malformed(detail) => write!(formatter, "握手消息结构错误：{detail}"),
            Self::Verification(detail) => write!(formatter, "握手校验失败：{detail}"),
            Self::KeySchedule(detail) => write!(formatter, "密钥派生失败：{detail}"),
            Self::Record(detail) => write!(formatter, "记录层错误：{detail}"),
            Self::Missing(what) => write!(formatter, "握手状态缺少：{what}"),
        }
    }
}

impl std::error::Error for HandshakeError {}

impl From<VerifyError> for HandshakeError {
    fn from(error: VerifyError) -> Self {
        Self::Verification(error)
    }
}

/// 客户端握手。
///
/// 用法是「产出要发的字节 → 喂入收到的记录」：
///
/// ```ignore
/// let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
/// let outgoing = handshake.start(&rng)?;      // 发出去
/// handshake.feed_record(&record)?;            // 喂入收到的一条记录
/// let finished = handshake.finish(&rng)?;     // 发出去（握手在此时完成）
/// ```
pub struct ClientHandshake {
    family: Family,
    host: String,
    state: State,
    exchange: Option<X25519Exchange>,
    transcript: Transcript,
    /// 已协商套件（收到 ServerHello 后才有）。
    suite: Option<crate::keyschedule::CipherSuite>,
    handshake_secrets: Option<HandshakeSecrets>,
    application_secrets: Option<ApplicationSecrets>,
    /// 服务端方向与客户端方向的记录密钥（1.3 两个方向分离）。
    read_key: Option<RecordKey>,
    write_key: Option<RecordKey>,
    /// 对端证书链（第一张是叶证书）。
    server_chain: Vec<CertificateDer<'static>>,
    /// 已解析的 CertificateVerify（转发给调用方做审计）。
    certificate_verify: Option<crate::handshake::CertificateVerify>,
    alpn: Option<String>,
    /// ClientHello 的线上字节，供重试或诊断。
    client_hello_bytes: Option<Vec<u8>>,
    /// HRR 之后要发出的第二个 ClientHello 体（由驱动层取走）。
    retry_pending: Option<Vec<u8>>,
    /// 是否已经因 HRR 重试过一次。
    retried: bool,
    policy: PeerPolicy,
    anchors: Option<TrustAnchors>,
    /// 校验时间点；`None` 表示用系统当前时间。
    verify_at: Option<UnixTime>,
}

impl ClientHandshake {
    /// 新建握手。`host` 同时用于 SNI 与后续的证书主机名校验。
    ///
    /// 默认 [`PeerPolicy::Inspect`]：不验链。要真正信任对端，用 [`Self::with_trust`]。
    pub fn new(family: Family, host: impl Into<String>) -> Self {
        Self {
            family,
            host: host.into(),
            state: State::Start,
            exchange: None,
            transcript: Transcript::new(crate::keyschedule::CipherSuite::Aes128GcmSha256),
            suite: None,
            handshake_secrets: None,
            application_secrets: None,
            read_key: None,
            write_key: None,
            server_chain: Vec::new(),
            certificate_verify: None,
            alpn: None,
            client_hello_bytes: None,
            retry_pending: None,
            retried: false,
            policy: PeerPolicy::Inspect,
            anchors: None,
            verify_at: None,
        }
    }

    /// 要求证书链能追到给定的受信根，并校验主机名。
    pub fn with_trust(mut self, anchors: TrustAnchors) -> Self {
        self.policy = PeerPolicy::VerifyChain;
        self.anchors = Some(anchors);
        self
    }

    /// 固定校验时间点（测试用；默认取系统时间）。
    pub fn with_verify_time(mut self, now: UnixTime) -> Self {
        self.verify_at = Some(now);
        self
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn policy(&self) -> PeerPolicy {
        self.policy
    }

    pub fn alpn(&self) -> Option<&str> {
        self.alpn.as_deref()
    }

    /// 对端证书链（第一张为叶证书）。
    pub fn server_chain(&self) -> &[CertificateDer<'static>] {
        &self.server_chain
    }

    /// 对端叶证书。
    pub fn server_certificate(&self) -> Option<&[u8]> {
        self.server_chain.first().map(|der| der.as_ref())
    }

    /// 已通过签名校验的 CertificateVerify。
    pub fn certificate_verify(&self) -> Option<&crate::handshake::CertificateVerify> {
        self.certificate_verify.as_ref()
    }

    pub fn application_secrets(&self) -> Option<&ApplicationSecrets> {
        self.application_secrets.as_ref()
    }

    /// 生成 ClientHello 并切到「等 ServerHello」。
    ///
    /// 返回的是完整的明文记录字节（待放进 TCP 流）。
    pub fn start(&mut self, rng: &dyn SecureRandom) -> Result<Vec<u8>, HandshakeError> {
        if self.state != State::Start {
            return Err(HandshakeError::Malformed(format!(
                "握手已在进行中（{:?}），不能重复 start",
                self.state
            )));
        }
        let exchange = X25519Exchange::generate(rng)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        let key_share = [(
            crate::client_hello::group::X25519,
            exchange.public_key.clone(),
        )];
        let spec = spec_for(self.family, &self.host, &key_share);

        let seed = random_byte(rng)?;
        let grease = crate::client_hello::pick_grease(seed);
        let mut random = [0u8; 32];
        rng.fill(&mut random)
            .map_err(|_| HandshakeError::Malformed("随机源不可用".to_string()))?;
        let mut session_id = vec![0u8; spec.session_id_len];
        if !session_id.is_empty() {
            rng.fill(&mut session_id)
                .map_err(|_| HandshakeError::Malformed("随机源不可用".to_string()))?;
        }

        let body = spec
            .encode(grease, &random, &session_id)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        self.transcript.push(&body);
        let record = Record {
            kind: ContentType::Handshake,
            payload: body,
        };
        let bytes = record
            .encode(0x0303)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        self.exchange = Some(exchange);
        self.client_hello_bytes = Some(bytes.clone());
        self.state = State::WaitServerHello;
        Ok(bytes)
    }

    /// 喂入一条已解析的记录。
    pub fn feed_record(&mut self, record: &Record) -> Result<(), HandshakeError> {
        match record.kind {
            ContentType::Alert => self.handle_alert(&record.payload),
            // 1.3 中间设备的兼容性 CCS，忽略。
            ContentType::ChangeCipherSpec => Ok(()),
            ContentType::Handshake => self.handle_plain_handshake(&record.payload),
            ContentType::ApplicationData => self.handle_encrypted(&record.payload),
        }
    }

    fn handle_alert(&mut self, payload: &[u8]) -> Result<(), HandshakeError> {
        let level = payload.first().copied().unwrap_or(0);
        let description = payload.get(1).copied().unwrap_or(0);
        Err(HandshakeError::Alert { level, description })
    }

    /// 明文握手消息：只可能是 ServerHello（兼容性 CCS 已在上层拦掉）。
    fn handle_plain_handshake(&mut self, payload: &[u8]) -> Result<(), HandshakeError> {
        let (messages, consumed) = take_handshake_messages(payload);
        if consumed != payload.len() {
            return Err(HandshakeError::Malformed(
                "握手负载尾部有残缺消息".to_string(),
            ));
        }
        for message in messages {
            if message.kind != hs::SERVER_HELLO {
                return Err(HandshakeError::UnexpectedMessage {
                    state: self.state,
                    received: message.kind,
                });
            }
            self.handle_server_hello(&message)?;
        }
        Ok(())
    }

    fn handle_server_hello(&mut self, message: &HandshakeMessage) -> Result<(), HandshakeError> {
        if self.state != State::WaitServerHello {
            return Err(HandshakeError::UnexpectedMessage {
                state: self.state,
                received: message.kind,
            });
        }
        let parsed = parse_server_hello(&message.body).map_err(HandshakeError::Malformed)?;

        // HelloRetryRequest：结构与 ServerHello 相同，只有 random 是固定值。
        // 它要求我们换一个组重新发 ClientHello，此时还没有任何密钥可派生。
        if crate::handshake::is_hello_retry_request(&parsed) {
            return self.handle_hello_retry_request(message, &parsed);
        }

        let suite = crate::keyschedule::CipherSuite::from_code(parsed.cipher_suite).ok_or(
            HandshakeError::Malformed(format!("对端选了非 1.3 套件 {:#06x}", parsed.cipher_suite)),
        )?;
        if parsed.selected_version != 0x0304 {
            return Err(HandshakeError::Malformed(format!(
                "对端协商出的版本是 {:#06x}，不是 TLS 1.3",
                parsed.selected_version
            )));
        }
        if parsed.key_share_group != crate::client_hello::group::X25519 {
            return Err(HandshakeError::Malformed(format!(
                "对端选择的组 {:#06x} 不是 X25519（{:#06x}）",
                parsed.key_share_group,
                crate::client_hello::group::X25519
            )));
        }
        if parsed.key_share.is_empty() {
            return Err(HandshakeError::Malformed(
                "ServerHello 的 key_share 没有公钥".to_string(),
            ));
        }

        self.transcript.push(&message.raw);
        let exchange = self
            .exchange
            .take()
            .ok_or(HandshakeError::Missing("X25519 密钥对"))?;
        let shared = exchange
            .agree(&parsed.key_share)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;

        let schedule = KeySchedule::new(suite);
        let transcript_hash = self.transcript.hash();
        let secrets = schedule
            .handshake_secrets(&shared, &transcript_hash)
            .map_err(HandshakeError::KeySchedule)?;

        self.read_key = Some(
            RecordKey::new(suite.aead(), &secrets.server.key, secrets.server.iv)
                .map_err(HandshakeError::Record)?,
        );
        self.write_key = Some(
            RecordKey::new(suite.aead(), &secrets.client.key, secrets.client.iv)
                .map_err(HandshakeError::Record)?,
        );
        self.suite = Some(suite);
        self.handshake_secrets = Some(secrets);
        self.state = State::WaitEncryptedExtensions;
        Ok(())
    }

    /// 处理 HelloRetryRequest：换组后重发 ClientHello。
    ///
    /// 转录的处理方式是 RFC 8446 §4.4.1 规定的特殊形式：把 ClientHello1 换成
    /// `message_hash`（一字节哈希类型 + 哈希长度 + 哈希值），再接着放 HRR 与 ClientHello2。
    /// 直接用原始 ClientHello1 会算出不同的转录哈希，服务端的 Finished 就永远对不上。
    ///
    /// 只允许重试一次：服务端连续两次要求换组是协议违规，继续重试会变成死循环。
    fn handle_hello_retry_request(
        &mut self,
        message: &HandshakeMessage,
        parsed: &crate::handshake::ServerHello,
    ) -> Result<(), HandshakeError> {
        if self.retried {
            return Err(HandshakeError::Malformed(
                "服务端再次要求 HelloRetryRequest；只允许一次".to_string(),
            ));
        }
        if parsed.key_share_group != crate::client_hello::group::X25519 {
            return Err(HandshakeError::Malformed(format!(
                "服务端要求改用组 {:#06x}，但本实现只支持 X25519（{:#06x}）",
                parsed.key_share_group,
                crate::client_hello::group::X25519
            )));
        }

        let suite = crate::keyschedule::CipherSuite::from_code(parsed.cipher_suite).ok_or(
            HandshakeError::Malformed(format!(
                "HRR 里选了非 1.3 套件 {:#06x}",
                parsed.cipher_suite
            )),
        )?;
        self.suite = Some(suite);

        // 转录改写成 message_hash，丢弃原来的 ClientHello1 字节。
        let first_hash = self.transcript.hash();
        self.transcript = Transcript::new(suite);
        self.transcript.push_message_hash(&first_hash);
        self.transcript.push(&message.raw);

        // 重新生成 X25519 密钥对并重发 ClientHello。
        let rng = ring::rand::SystemRandom::new();
        let exchange = X25519Exchange::generate(&rng)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        let key_share = [(
            crate::client_hello::group::X25519,
            exchange.public_key.clone(),
        )];
        let spec = spec_for(self.family, &self.host, &key_share);
        let seed = random_byte(&rng)?;
        let grease = crate::client_hello::pick_grease(seed);
        let mut random = [0u8; 32];
        rng.fill(&mut random)
            .map_err(|_| HandshakeError::Malformed("随机源不可用".to_string()))?;
        let mut session_id = vec![0u8; spec.session_id_len];
        if !session_id.is_empty() {
            rng.fill(&mut session_id)
                .map_err(|_| HandshakeError::Malformed("随机源不可用".to_string()))?;
        }
        let body = spec
            .encode(grease, &random, &session_id)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        self.transcript.push(&body);

        self.exchange = Some(exchange);
        self.retry_pending = Some(body);
        self.retried = true;
        Ok(())
    }

    /// 取出 HRR 后需要发送的第二个 ClientHello（若有）。
    ///
    /// 由驱动层调用：收到 HRR 时 `feed_record` 不报错但也不推进状态，
    /// 驱动层必须把这里拿到的字节发出去。
    pub fn take_retry_client_hello(&mut self) -> Option<Vec<u8>> {
        let body = self.retry_pending.take()?;
        let record = Record {
            kind: ContentType::Handshake,
            payload: body,
        };
        record.encode(0x0303).ok()
    }

    /// 加密握手消息：先解密，再按内层类型分派。
    fn handle_encrypted(&mut self, payload: &[u8]) -> Result<(), HandshakeError> {
        let read_key = self
            .read_key
            .as_mut()
            .ok_or(HandshakeError::Missing("握手读密钥"))?;
        let record = Record {
            kind: ContentType::ApplicationData,
            payload: payload.to_vec(),
        };
        let (kind, plaintext) = read_key.open(&record).map_err(HandshakeError::Record)?;
        match kind {
            ContentType::Handshake => {
                let (messages, consumed) = take_handshake_messages(&plaintext);
                if consumed != plaintext.len() {
                    return Err(HandshakeError::Malformed(
                        "加密握手负载尾部有残缺消息".to_string(),
                    ));
                }
                for message in messages {
                    self.handle_inner_handshake(&message)?;
                }
                Ok(())
            }
            ContentType::Alert => {
                let level = plaintext.first().copied().unwrap_or(0);
                let description = plaintext.get(1).copied().unwrap_or(0);
                Err(HandshakeError::Alert { level, description })
            }
            other => Err(HandshakeError::Malformed(format!(
                "握手阶段收到意外的内层类型 {}",
                other as u8
            ))),
        }
    }

    fn handle_inner_handshake(&mut self, message: &HandshakeMessage) -> Result<(), HandshakeError> {
        match (self.state, message.kind) {
            (State::WaitEncryptedExtensions, hs::ENCRYPTED_EXTENSIONS) => {
                let extensions =
                    parse_encrypted_extensions(&message.body).map_err(HandshakeError::Malformed)?;
                self.alpn = crate::handshake::negotiated_alpn(&extensions);
                self.transcript.push(&message.raw);
                self.state = State::WaitCertificate;
                Ok(())
            }
            // 服务端可能跳过 CertificateRequest（我们不支持客户端认证），
            // 因此这里直接接受 Certificate。
            (State::WaitCertificate, hs::CERTIFICATE) => {
                let chain =
                    parse_certificate_chain(&message.body).map_err(HandshakeError::Malformed)?;
                self.server_chain = chain
                    .into_iter()
                    .map(|der| CertificateDer::from(der).into_owned())
                    .collect();
                self.transcript.push(&message.raw);
                self.state = State::WaitCertificateVerify;
                Ok(())
            }
            (State::WaitCertificateVerify, hs::CERTIFICATE_VERIFY) => {
                self.handle_certificate_verify(message)
            }
            (State::WaitFinished, hs::FINISHED) => self.handle_server_finished(message),
            (state, received) => Err(HandshakeError::UnexpectedMessage { state, received }),
        }
    }

    fn handle_certificate_verify(
        &mut self,
        message: &HandshakeMessage,
    ) -> Result<(), HandshakeError> {
        let parsed = parse_certificate_verify(&message.body).map_err(HandshakeError::Malformed)?;
        let leaf = self
            .server_chain
            .first()
            .ok_or(HandshakeError::Missing("叶证书"))?
            .clone();

        // 签名校验用的转录哈希不含 CertificateVerify 本身（它就是被签名的对象）。
        verify_certificate_verify(
            &leaf,
            parsed.scheme,
            &self.transcript.hash(),
            &parsed.signature,
        )?;

        if self.policy == PeerPolicy::VerifyChain {
            let anchors = self
                .anchors
                .as_ref()
                .ok_or(HandshakeError::Missing("受信根"))?;
            let name = ServerName::try_from(self.host.as_str())
                .map_err(|error| VerifyError::WrongHostName(format!("{error}")))?;
            verify_server_chain(
                anchors,
                &self.server_chain,
                &name,
                self.verify_at.unwrap_or_else(UnixTime::now),
            )?;
        }

        self.certificate_verify = Some(parsed);
        self.transcript.push(&message.raw);
        self.state = State::WaitFinished;
        Ok(())
    }

    fn handle_server_finished(&mut self, message: &HandshakeMessage) -> Result<(), HandshakeError> {
        let suite = self.suite.ok_or(HandshakeError::Missing("协商套件"))?;
        let secrets = self
            .handshake_secrets
            .as_ref()
            .ok_or(HandshakeError::Missing("握手密钥"))?;
        // 转录此时只含 server Finished 之前的消息。
        let expected = finished_verify_data(suite, &secrets.server_secret, &self.transcript)
            .map_err(HandshakeError::KeySchedule)?;
        if expected.len() != message.body.len() {
            return Err(HandshakeError::Verification(
                VerifyError::BadFinishedLength {
                    expected: expected.len(),
                    actual: message.body.len(),
                },
            ));
        }
        if expected != message.body {
            return Err(HandshakeError::Verification(VerifyError::FinishedMismatch));
        }
        self.transcript.push(&message.raw);

        let application = secrets
            .application_secrets(&self.transcript.hash())
            .map_err(HandshakeError::KeySchedule)?;
        // 只换读密钥：写方向还要用握手密钥把本端 Finished 发出去。
        self.read_key = Some(
            RecordKey::new(suite.aead(), &application.server.key, application.server.iv)
                .map_err(HandshakeError::Record)?,
        );
        self.application_secrets = Some(application);
        self.state = State::SendClientFinished;
        Ok(())
    }

    /// 发出本端 Finished，并切换到应用写密钥。返回待发送的记录载荷。
    ///
    /// 调用方要把它包进 `application_data` 记录（类型 23）发出去——1.3 的加密握手消息
    /// 外层一律走这个类型。
    pub fn finish(&mut self) -> Result<Vec<u8>, HandshakeError> {
        if self.state != State::SendClientFinished {
            return Err(HandshakeError::Malformed(format!(
                "当前状态 {:?} 不能发送 Finished",
                self.state
            )));
        }
        let suite = self.suite.ok_or(HandshakeError::Missing("协商套件"))?;
        let secrets = self
            .handshake_secrets
            .as_ref()
            .ok_or(HandshakeError::Missing("握手密钥"))?;
        let verify_data = finished_verify_data(suite, &secrets.client_secret, &self.transcript)
            .map_err(HandshakeError::KeySchedule)?;

        let mut raw = Vec::with_capacity(4 + verify_data.len());
        raw.push(hs::FINISHED);
        raw.extend_from_slice(&(verify_data.len() as u32).to_be_bytes()[1..]);
        raw.extend_from_slice(&verify_data);

        let sealed = self
            .write_key
            .as_mut()
            .ok_or(HandshakeError::Missing("握手写密钥"))?
            .seal(ContentType::Handshake, &raw)
            .map_err(HandshakeError::Record)?;

        self.transcript.push(&raw);
        let application = self
            .application_secrets
            .as_ref()
            .ok_or(HandshakeError::Missing("应用密钥"))?;
        self.write_key = Some(
            RecordKey::new(suite.aead(), &application.client.key, application.client.iv)
                .map_err(HandshakeError::Record)?,
        );
        self.state = State::Connected;
        Ok(sealed.payload)
    }

    /// 加密一条应用数据记录（握手完成后可用）。
    pub fn seal_application(&mut self, data: &[u8]) -> Result<Record, HandshakeError> {
        if self.state != State::Connected {
            return Err(HandshakeError::Malformed(format!(
                "握手未完成（{:?}），不能发应用数据",
                self.state
            )));
        }
        self.write_key
            .as_mut()
            .ok_or(HandshakeError::Missing("应用写密钥"))?
            .seal(ContentType::ApplicationData, data)
            .map_err(HandshakeError::Record)
    }

    /// 解密一条对端的应用数据记录。
    pub fn open_application(&mut self, payload: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        if self.state != State::Connected {
            return Err(HandshakeError::Malformed(format!(
                "握手未完成（{:?}），不能读应用数据",
                self.state
            )));
        }
        let record = Record {
            kind: ContentType::ApplicationData,
            payload: payload.to_vec(),
        };
        let (kind, body) = self
            .read_key
            .as_mut()
            .ok_or(HandshakeError::Missing("应用读密钥"))?
            .open(&record)
            .map_err(HandshakeError::Record)?;
        match kind {
            ContentType::ApplicationData => Ok(body),
            // 握手完成后服务端仍会发 NewSessionTicket（每条一个握手记录）。
            // 我们不做会话恢复，但**必须容忍**它——报错会让正常连接直接断掉。
            ContentType::Handshake => Ok(Vec::new()),
            ContentType::Alert => {
                let level = body.first().copied().unwrap_or(0);
                let description = body.get(1).copied().unwrap_or(0);
                Err(HandshakeError::Alert { level, description })
            }
            other => Err(HandshakeError::Malformed(format!(
                "期望应用数据，收到内层类型 {}",
                other as u8
            ))),
        }
    }
}

fn random_byte(rng: &dyn SecureRandom) -> Result<u8, HandshakeError> {
    let mut byte = [0u8; 1];
    rng.fill(&mut byte)
        .map_err(|_| HandshakeError::Malformed("随机源不可用".to_string()))?;
    Ok(byte[0])
}

/// 便于调用方拿到 CertificateVerify 需要校验的原始消息。
pub fn signed_message(transcript_hash: &[u8]) -> Vec<u8> {
    crate::verify::certificate_verify_message(server_verify_context(), transcript_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng() -> ring::rand::SystemRandom {
        ring::rand::SystemRandom::new()
    }

    #[test]
    fn start_produces_a_parsable_client_hello_record() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        let bytes = handshake.start(&rng()).expect("应当生成");
        let (record, consumed) = crate::record::parse_record(&bytes).expect("应当能解析");
        assert_eq!(consumed, bytes.len());
        assert_eq!(record.kind, ContentType::Handshake);
        // 首字节是握手类型 0x01。
        assert_eq!(record.payload[0], hs::CLIENT_HELLO);
        assert_eq!(handshake.state(), State::WaitServerHello);
    }

    #[test]
    fn start_cannot_be_called_twice() {
        let mut handshake = ClientHandshake::new(Family::Firefox, "example.com");
        handshake.start(&rng()).expect("第一次应当成功");
        assert!(handshake.start(&rng()).is_err(), "重复 start 必须报错");
    }

    #[test]
    fn finish_before_server_finished_is_rejected() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        assert!(
            matches!(handshake.finish(), Err(HandshakeError::Malformed(_))),
            "还没开始就不该能发 Finished"
        );
        handshake.start(&rng()).expect("生成");
        assert!(
            matches!(handshake.finish(), Err(HandshakeError::Malformed(_))),
            "只发了 ClientHello 就发 Finished 必须被拒绝"
        );
        assert_eq!(handshake.state(), State::WaitServerHello);
    }

    #[test]
    fn a_server_hello_in_the_wrong_state_is_rejected() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        // 还没 start 就喂消息。
        let record = Record {
            kind: ContentType::Handshake,
            payload: vec![hs::SERVER_HELLO, 0, 0, 0],
        };
        assert!(matches!(
            handshake.feed_record(&record),
            Err(HandshakeError::UnexpectedMessage { .. })
        ));
    }

    #[test]
    fn the_default_policy_is_inspect() {
        let handshake = ClientHandshake::new(Family::Chrome, "example.com");
        assert_eq!(handshake.policy(), PeerPolicy::Inspect);
        assert!(handshake.server_chain().is_empty());
        assert!(handshake.certificate_verify().is_none());
    }
}
