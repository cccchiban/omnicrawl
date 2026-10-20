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
    parse_server_hello, take_handshake_messages, HandshakeMessage, KeyExchange,
};
use crate::keyschedule::{ApplicationSecrets, HandshakeSecrets, KeySchedule, Transcript};
use crate::profile::{spec_for, Family};
use crate::record::{ContentType, Record, RecordKey};
use crate::tls12::{RecordKey12, Suite12, Transcript12, MASTER_LEN};
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
    /// TLS 1.2 分支：等 Certificate。
    WaitCertificate12,
    /// TLS 1.2 分支：等 ServerKeyExchange。
    WaitServerKeyExchange,
    /// TLS 1.2 分支：等 ServerHelloDone。
    WaitServerHelloDone,
    /// TLS 1.2 分支：本端 flight 已发出，等服务端 CCS 与 Finished。
    WaitServerFinished12,
    /// 握手完成，可以发应用数据。
    Connected,
}

/// 协商出的 TLS 版本。
///
/// 两个版本在这条实现里是**两套独立的记录层与密钥调度**（1.3 的密钥藏在
/// `application_data` 里、1.2 保留真实记录类型），因此协商结果必须显式暴露给上层，
/// 不能靠「有没有 application_secrets」猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Tls12,
    Tls13,
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
    /// 对端回落到了 TLS 1.2 或更低版本，且本实现无法与该版本协商。
    ///
    /// TLS 1.2 已经支持，因此这个错误只在对方选到 **1.1 及更低** 时出现。
    UnsupportedVersion(u16),
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
            Self::UnsupportedVersion(version) => write!(
                formatter,
                "对端协商到了 TLS {}（本实现支持 TLS 1.2 与 1.3）",
                format_version(*version)
            ),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// 当前 Unix 秒。票据年龄与过期判断都以秒为单位，与 [`crate::session_cache::Session`] 一致。
fn system_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 TLS 版本号写成惯用的「1.2」形式。
///
/// 编码是 `major << 8 | minor`，但直接移位会得到「3.3」这种没人用的写法（0x0303 是
/// TLS 1.2 的 legacy_version）。业界写法是 `(major - 2).(minor - 1)`：0x0301 是 1.0、
/// 0x0303 是 1.2、0x0304 是 1.3。只减 major 会把它渲染成 1.3，报错文案于是自相矛盾。
fn format_version(version: u16) -> String {
    format!(
        "{}.{}",
        (version >> 8).saturating_sub(2),
        (version & 0xff).saturating_sub(1)
    )
}

impl From<VerifyError> for HandshakeError {
    fn from(error: VerifyError) -> Self {
        Self::Verification(error)
    }
}

/// ClientHello 里必须跨重发复用的三个值：random、session_id 与 GREASE 值。
type HelloSeed = ([u8; 32], Vec<u8>, u16);

/// HRR 重发时沿用的 `(random, session_id, GREASE, cookie)`。
type ReusedHello<'a> = (&'a [u8; 32], &'a [u8], u16, &'a [u8]);

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
    exchange: Option<KeyExchange>,
    transcript: Transcript,
    /// 已经进入转录的握手消息原文（按发送/接收顺序）。
    ///
    /// 客户端的转录在发 ClientHello 时只能先用一个占位套件开张，真正的套件要等
    /// ServerHello。若协商出的套件用别的哈希（SHA384、SHA512），累积到那一刻的
    /// 摘要必须按新套件重算，因此得留着原文——摘要本身是无法「换算法」的。
    transcript_messages: Vec<u8>,
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
    /// 首个 ClientHello 的 random、session_id 与 GREASE 值：HRR 之后必须原样复用。
    first_hello: Option<HelloSeed>,
    policy: PeerPolicy,
    anchors: Option<TrustAnchors>,
    /// 校验时间点；`None` 表示用系统当前时间。
    verify_at: Option<UnixTime>,
    /// TLS 1.2 分支的密钥与转录状态。`None` 表示还没协商到 1.2。
    tls12: Option<Handshake12>,
    /// 已经在 `start` 里抽好的客户端 random（1.2 的 PRF 与 SKE 验签都要用它）。
    client_random: Option<[u8; 32]>,
    /// 1.2 分支里待发出的本端 flight（ClientKeyExchange + CCS + Finished）。
    tls12_flight: Option<Vec<u8>>,
    /// 1.2 的 ClientKeyExchange 需要的客户端 ECDHE 公钥。
    tls12_client_key: Option<Vec<u8>>,
    /// 首个 ClientHello 里为 P-256 准备的密钥对：1.2 没有 HRR，服务端可能挑它。
    p256_exchange: Option<KeyExchange>,
    /// 待恢复的会话：非空时 ClientHello 会带上 `pre_shared_key`。
    offered_session: Option<crate::session_cache::Session>,
    /// 本次连接开始时从缓存取出的那份会话（用于在成功后判断是否需要更新缓存）。
    resumed: bool,
    /// 本次连接结束时拿到的可恢复会话（服务端下发了 NewSessionTicket 才有）。
    new_session: Option<crate::session_cache::Session>,
}

/// TLS 1.2 分支的中间状态。
///
/// 1.3 的密钥是「握手/应用两阶段、两方向四个 secret」，1.2 只有「master secret 派生出
/// 四个 key/iv」，因此单独一个结构体比往 1.3 的字段里塞 `Option` 更不容易用错。
struct Handshake12 {
    suite: Suite12,
    client_random: [u8; 32],
    server_random: [u8; 32],
    /// `pre_master_secret`（ECDHE 共享密钥）。
    pre_master: Vec<u8>,
    master: [u8; MASTER_LEN],
    /// 累积到 ServerHelloDone 为止的握手原文：EMS 的 session_hash 与两条 Finished
    /// 都要用它，且 client Finished 之后还要继续累积（server Finished 覆盖它）。
    messages: Vec<u8>,
    /// extended master secret 是否协商成功（决定 client Finished 之后是否要换转录）。
    extended_master: bool,
    read_key: Option<RecordKey12>,
    write_key: Option<RecordKey12>,
    server_chain: Vec<CertificateDer<'static>>,
    alpn: Option<String>,
    /// 服务端 flight（到 ServerHelloDone）的转录哈希，EMS 的 `session_hash` 用它。
    session_hash: Vec<u8>,
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
            transcript_messages: Vec::new(),
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
            first_hello: None,
            policy: PeerPolicy::Inspect,
            anchors: None,
            verify_at: None,
            tls12: None,
            client_random: None,
            tls12_flight: None,
            tls12_client_key: None,
            p256_exchange: None,
            offered_session: None,
            resumed: false,
            new_session: None,
        }
    }

    /// 用一条缓存会话尝试恢复。
    ///
    /// 服务端可以选择不接受（票据过期、密钥轮换、限流都是常态），因此这只是「提出请求」；
    /// 真正的结果要看 ServerHello 里有没有 `pre_shared_key` 扩展。
    pub fn with_session(mut self, session: crate::session_cache::Session) -> Self {
        self.offered_session = Some(session);
        self
    }

    /// 本次握手是否真的走了恢复（服务端接受了 PSK）。
    pub fn resumed(&self) -> bool {
        self.resumed
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
        match &self.tls12 {
            Some(state) => state.alpn.as_deref(),
            None => self.alpn.as_deref(),
        }
    }

    /// 协商出的 TLS 版本；握手还没定版本时返回 `None`。
    pub fn version(&self) -> Option<Version> {
        if self.tls12.is_some() {
            Some(Version::Tls12)
        } else if self.suite.is_some() {
            Some(Version::Tls13)
        } else {
            None
        }
    }

    /// 1.2 的 master secret（1.3 没有这个概念，返回 `None`）。
    pub fn master_secret(&self) -> Option<&[u8]> {
        self.tls12.as_ref().map(|state| state.master.as_slice())
    }

    /// 对端证书链（第一张为叶证书）。
    pub fn server_chain(&self) -> &[CertificateDer<'static>] {
        match &self.tls12 {
            Some(state) => &state.server_chain,
            None => &self.server_chain,
        }
    }

    /// 对端叶证书。
    pub fn server_certificate(&self) -> Option<&[u8]> {
        self.server_chain().first().map(|der| der.as_ref())
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
        // 首次 key_share 同时给出 X25519 与 P-256 的公钥。
        //
        // 1.3 允许服务端用 HelloRetryRequest 指定组，因此只发 X25519 也够用；但 1.2
        // **没有** HRR，服务端只能在 ClientHello 已有的公钥里挑——只发 X25519 的话，
        // 认死 P-256 的站点（例如百度）会在 ServerKeyExchange 里返回我们没给过的组，
        // 握手当场失败。真实浏览器档案本来也同时带这两组，因此这不改变指纹形状。
        let exchange = KeyExchange::generate(crate::client_hello::group::X25519, rng)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        let p256 = KeyExchange::generate(crate::client_hello::group::SECP256R1, rng)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        let (body, used) = self.encode_client_hello(&exchange, Some(&p256), rng, None)?;
        // 重发时必须逐字节复用的三个值，趁现在留住。
        self.client_random = Some(used.0);
        self.p256_exchange = Some(p256);
        self.first_hello = Some(used);
        self.push_transcript(&body);
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

    /// 按当前档案把一次密钥交换的公钥编成 ClientHello 的消息体。
    ///
    /// `reuse` 只给 HelloRetryRequest 之后的重发用：RFC 8446 §4.1.2 要求第二个
    /// ClientHello 除 `key_share`（以及回带的 cookie）外必须与第一个逐字节一致，
    /// 尤其是 `random`、`session_id` 与 GREASE 值。重新抽这些值会被服务端判为
    /// illegal_parameter 并直接 Alert。
    fn encode_client_hello(
        &self,
        exchange: &KeyExchange,
        extra: Option<&KeyExchange>,
        rng: &dyn SecureRandom,
        reuse: Option<ReusedHello>,
    ) -> Result<(Vec<u8>, HelloSeed), HandshakeError> {
        let mut key_share = vec![(exchange.group, exchange.public_key.clone())];
        // 第二组的公钥只对 1.2 有意义（1.3 有 HRR）。按组的常见优先级排在前一组之后。
        if let Some(extra) = extra {
            key_share.push((extra.group, extra.public_key.clone()));
        }
        let mut spec = spec_for(self.family, &self.host, &key_share);

        let (random, session_id, grease, cookie) = match reuse {
            Some((random, session_id, grease, cookie)) => {
                (*random, session_id.to_vec(), grease, cookie)
            }
            None => {
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
                (random, session_id, grease, &[][..])
            }
        };

        if !cookie.is_empty() {
            // cookie 放在 key_share 之前：顺序不影响语义，但与真实浏览器一致。
            let mut data = Vec::with_capacity(cookie.len() + 2);
            data.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
            data.extend_from_slice(cookie);
            let position = spec
                .extensions
                .iter()
                .position(|slot| slot.kind == crate::client_hello::ext::KEY_SHARE)
                .unwrap_or(spec.extensions.len());
            spec.extensions.insert(
                position,
                crate::client_hello::ExtensionSlot::new(crate::client_hello::ext::COOKIE, data),
            );
        }

        if reuse.is_some() {
            // RFC 8446 §4.1.4：HRR 之后这个 ClientHello 的 key_share 必须只含
            // **一个** KeyShareEntry。Chromium 档案会在首位塞一个 GREASE 占位项，
            // 重发时带着它就会被判 illegal_parameter（Alert 47）。
            let single = crate::client_hello::key_share(&[(
                exchange.group,
                exchange.public_key.clone(),
            )]);
            if let Some(slot) = spec
                .extensions
                .iter_mut()
                .find(|slot| slot.kind == crate::client_hello::ext::KEY_SHARE)
            {
                slot.data = single;
            }
        }

        let body = if let Some(session) = self.offered_session.as_ref() {
            // 会话恢复：把 `pre_shared_key` 追加为最后一个扩展，再对「截断到 binder 之前」
            // 的 ClientHello 算 binder 并回填。RFC 8446 §4.2.11.2 要求长度字段按 binder
            // 已存在来写，因此必须用 `encode_with_psk` 返回的完整字节再截断。
            let suite = crate::keyschedule::CipherSuite::from_code(session.suite)
                .unwrap_or(crate::keyschedule::CipherSuite::Aes128GcmSha256);
            let binder_len = suite.hash_len();
            let age = session.obfuscated_age(system_unix_seconds());
            let (mut hello, binder_offset, binder_len) = spec
                .encode_with_psk(
                    grease,
                    &random,
                    &session_id,
                    &[(session.ticket.clone(), age)],
                    binder_len,
                )
                .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
            // 截断点 = binder 数据起点往回 3 字节：跳过 binders 列表长度（2）与
            // entry 长度（1）。RFC 8448 §4 的 prefix 正是到 `identities` 之后、
            // 这两个长度字段之前... 实测以 RFC 向量为准：prefix 包含它们，故只退 3 到
            // 「binder 数据之前」。见 `psk_binder_matches_the_rfc8448_vector`。
            // RFC 8448 §4 的 `ClientHello prefix` 包含 binders 列表长度前缀与 entry
            // 长度字节（它们按完整 binder 已存在来写），只截掉 binder 数据本身。
            // `binder_offset` 是 binder 数据起点，因此截断点要往回 3 字节
            // （2 字节 binders 长度 + 1 字节 entry 长度）。
            let truncate_at = binder_offset.saturating_sub(3);
            let binder = crate::keyschedule::KeySchedule::psk_binder(
                suite,
                &session.psk,
                &hello[..truncate_at],
            )
            .map_err(HandshakeError::Malformed)?;
            // `binder_offset` 指向 binder 数据起点（entry 的 1 字节长度在它之前 1 字节）。
            hello[binder_offset..binder_offset + binder_len].copy_from_slice(&binder[..binder_len]);
            hello
        } else {
            spec.encode(grease, &random, &session_id)
                .map_err(|error| HandshakeError::Malformed(format!("{error}")))?
        };
        Ok((body, (random, session_id, grease)))
    }

    /// 收到 NewSessionTicket 后派生 PSK 并留下，供下一次连接复用。
    ///
    /// 派生需要 `resumption_master_secret`，而它的上下文是 **client Finished 之后**的
    /// 转录哈希。因此这里必须等 `self.application_secrets` 已经就位；否则说明时序不对
    /// （票据在成立前就到了），直接放弃这一条。
    fn capture_session_ticket(&mut self, payload: &[u8]) {
        let Some(ticket) = payload
            .first()
            .filter(|kind| **kind == hs::NEW_SESSION_TICKET)
            .and_then(|_| {
                let len = ((payload[1] as usize) << 16)
                    | ((payload[2] as usize) << 8)
                    | payload[3] as usize;
                payload.get(4..4 + len)
            })
            .and_then(|body| crate::handshake::parse_new_session_ticket(body).ok())
        else {
            return;
        };
        let (Some(suite), Some(application)) = (self.suite, self.application_secrets.as_ref()) else {
            return;
        };
        // `res master` 的上下文是 client Finished 之后的转录哈希。
        let resumption_master = match crate::keyschedule::KeySchedule::resumption_master_secret(
            suite,
            &application.master_secret,
            &self.transcript.hash(),
        ) {
            Ok(value) => value,
            Err(_) => return,
        };
        let psk = match crate::keyschedule::KeySchedule::resumption_psk(
            suite,
            &resumption_master,
            &ticket.nonce,
        ) {
            Ok(value) => value,
            Err(_) => return,
        };
        self.new_session = Some(crate::session_cache::Session {
            ticket: ticket.ticket,
            psk,
            ticket_age_add: ticket.age_add,
            issued_at: system_unix_seconds(),
            lifetime: ticket.lifetime,
            suite: suite.code(),
            alpn: self.alpn.clone(),
        });
    }

    /// 取走本次连接拿到的可恢复会话（启用恢复时由驱动层写入缓存）。
    pub fn take_new_session(&mut self) -> Option<crate::session_cache::Session> {
        self.new_session.take()
    }

    /// 把一条完整握手消息推进转录，同时留下原文以备换哈希重算。
    fn push_transcript(&mut self, raw: &[u8]) {
        self.transcript.push(raw);
        self.transcript_messages.extend_from_slice(raw);
    }

    /// 喂入一条已解析的记录。
    pub fn feed_record(&mut self, record: &Record) -> Result<(), HandshakeError> {
        // 1.2 一旦协商成功，记录层就完全换了一套：类型是真实的、CCS 有语义、
        // 加密记录的类型是 handshake/alert 而不是 application_data。
        if self.tls12.is_some() {
            return self.feed_record_12(record);
        }
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

        // 版本决定后面走哪套密钥调度：1.2 与 1.3 在记录层、PRF、转录上都不共用。
        //
        // 1.2 服务端不会回 `supported_versions`（那是 1.3 的扩展），因此
        // `selected_version` 停在 `legacy_version`=0x0303 就是回落。
        if parsed.selected_version == crate::tls12::TLS12_VERSION {
            return self.handle_server_hello_12(message, &parsed);
        }
        if parsed.selected_version != 0x0304 {
            return Err(HandshakeError::UnsupportedVersion(parsed.selected_version));
        }
        let suite = crate::keyschedule::CipherSuite::from_code(parsed.cipher_suite).ok_or(
            HandshakeError::Malformed(format!(
                "对端选了本实现不支持的 1.3 套件 {:#06x}",
                parsed.cipher_suite
            )),
        )?;
        if !matches!(
            parsed.key_share_group,
            crate::client_hello::group::X25519 | crate::client_hello::group::SECP256R1
        ) {
            return Err(HandshakeError::Malformed(format!(
                "对端选择的组 {:#06x} 不受支持（仅支持 X25519 与 secp256r1）",
                parsed.key_share_group
            )));
        }
        if parsed.key_share.is_empty() {
            return Err(HandshakeError::Malformed(
                "ServerHello 的 key_share 没有公钥".to_string(),
            ));
        }

        // 转录在发 ClientHello 时只能按占位套件开张，真正的套件到这里才知道。若协商出的
        // 套件用别的哈希（例如 0x1302 用 SHA384），必须把已累积的消息按新哈希重算一遍：
        // 漏掉这一步，从这里开始的每个密钥都会错，症状是第一条加密记录就解不开，
        // 而握手协商本身看起来完全正常。
        self.push_transcript(&message.raw);
        if self.transcript.hash_len() != suite.hash_len() {
            self.transcript = Transcript::restarted(suite, &self.transcript_messages);
        }
        // 服务端只能选我们发过的 key_share。首份 ClientHello 同时发了 X25519 与 P-256，
        // 因此这两个组都可能被直接选中（不必经过 HRR）；HRR 之后只剩一个候选。
        let exchange = if self.exchange.as_ref().map(|inner| inner.group)
            == Some(parsed.key_share_group)
        {
            self.exchange.take()
        } else if self.p256_exchange.as_ref().map(|inner| inner.group)
            == Some(parsed.key_share_group)
        {
            self.p256_exchange.take()
        } else {
            let offered: Vec<String> = [self.exchange.as_ref(), self.p256_exchange.as_ref()]
                .into_iter()
                .flatten()
                .map(|inner| format!("{:#06x}", inner.group))
                .collect();
            return Err(HandshakeError::Malformed(format!(
                "对端选了组 {:#06x}，但我们只为组 {} 发过公钥",
                parsed.key_share_group,
                offered.join("、")
            )));
        };
        let exchange = exchange.ok_or(HandshakeError::Missing("ECDHE 密钥对"))?;
        let shared = exchange
            .agree(&parsed.key_share)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;

        // 服务端是否接受了我们提议的 PSK：体现在 ServerHello 里有没有 `pre_shared_key`。
        // 接受时不发 Certificate / CertificateVerify（RFC 8446 §4.1.3），且密钥调度
        // 必须用 PSK 作为 `early_secret` 的 ikm，否则两侧算出的密钥不同。
        let accepted_psk = parsed
            .extensions
            .iter()
            .any(|(kind, _)| *kind == crate::client_hello::ext::PRE_SHARED_KEY)
            && self.offered_session.is_some();
        self.resumed = accepted_psk;

        let schedule = if accepted_psk {
            let session = self
                .offered_session
                .as_ref()
                .ok_or(HandshakeError::Missing("提议的会话"))?;
            // 服务端选中的套件必须与票据绑定的套件一致，否则 PSK 根本不匹配。
            if session.suite != parsed.cipher_suite {
                return Err(HandshakeError::Malformed(format!(
                    "会话恢复的套件不一致：票据是 {:#06x}，对端选了 {:#06x}",
                    session.suite, parsed.cipher_suite
                )));
            }
            KeySchedule::with_psk(suite, Some(&session.psk))
        } else {
            KeySchedule::new(suite)
        };
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
        if !matches!(
            parsed.key_share_group,
            crate::client_hello::group::X25519 | crate::client_hello::group::SECP256R1
        ) {
            return Err(HandshakeError::Malformed(format!(
                "服务端要求改用组 {:#06x}，但本实现只支持 X25519（{:#06x}）与 secp256r1（{:#06x}）",
                parsed.key_share_group,
                crate::client_hello::group::X25519,
                crate::client_hello::group::SECP256R1
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
        //
        // 这里的哈希必须用协商套件所对应的哈希：ClientHello1 从头到尾没有变过，
        // 但它被累加时转录还停在占位套件上，因此要按套件重算而不是取当前摘要。
        let first_hash = Transcript::restarted(suite, &self.transcript_messages).hash();
        self.transcript = Transcript::new(suite);
        self.transcript.push_message_hash(&first_hash);
        self.transcript_messages = vec![0xfe, 0, 0, first_hash.len() as u8];
        self.transcript_messages.extend_from_slice(&first_hash);
        self.push_transcript(&message.raw);

        // 按服务端要求的组重新生成密钥对，并只带这一组的 key_share 重发 ClientHello。
        // random、session_id 与 GREASE 必须沿用第一个 ClientHello：改掉其中任何一个，
        // 服务端都会以 illegal_parameter 拒掉这次重试。
        let rng = ring::rand::SystemRandom::new();
        let exchange = KeyExchange::generate(parsed.key_share_group, &rng)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;
        let first = self.first_hello.clone();
        let (random, session_id, grease) = first
            .as_ref()
            .map(|(random, session_id, grease)| (random, session_id.as_slice(), *grease))
            .ok_or(HandshakeError::Missing("首个 ClientHello 的随机参数"))?;
        let (body, _) = self.encode_client_hello(
            &exchange,
            None,
            &rng,
            Some((random, session_id, grease, &parsed.cookie)),
        )?;
        self.push_transcript(&body);

        // 重发之后只剩一个候选：另一个组必须清掉，否则「按组取密钥」会取到从未
        // 在第二个 ClientHello 里出现过的公钥。
        self.p256_exchange = None;
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

    // ---- TLS 1.2 分支 ----

    /// 1.2 的 ServerHello：只确定套件与两个 random，密钥要等 ServerKeyExchange。
    ///
    /// 1.2 的 PRF 用哪个哈希由套件决定（`0xc02c`/`0xc030` 是 SHA-384），因此转录不能
    /// 沿用 1.3 占位套件开的那一份，要按套件重建。
    fn handle_server_hello_12(
        &mut self,
        message: &HandshakeMessage,
        parsed: &crate::handshake::ServerHello,
    ) -> Result<(), HandshakeError> {
        let suite = Suite12::from_code(parsed.cipher_suite).ok_or_else(|| {
            // 明确区分「不支持这个版本」与「不支持这个套件」：后者常见于 CBC 老站点，
            // 报成版本问题会让人误以为我们连 1.2 都没实现。
            HandshakeError::Malformed(format!(
                "对端选了 1.2 套件 {:#06x}，本实现只支持 ECDHE + AEAD（GCM / ChaCha20-Poly1305）",
                parsed.cipher_suite
            ))
        })?;
        let client_random = self
            .client_random
            .ok_or(HandshakeError::Missing("客户端 random"))?;

        // 这里累积的原文是 1.2 的转录源头：EMS 的 session_hash、两条 Finished 都用它。
        // 已累积的只有 ClientHello 与 ServerHello。
        let mut messages = self.transcript_messages.clone();
        messages.extend_from_slice(&message.raw);

        let extended_master = crate::tls12::has_extended_master_secret(&parsed.extensions);
        let alpn = crate::handshake::negotiated_alpn(&parsed.extensions);
        self.tls12 = Some(Handshake12 {
            suite,
            client_random,
            server_random: parsed.random,
            pre_master: Vec::new(),
            master: [0u8; MASTER_LEN],
            messages,
            extended_master,
            read_key: None,
            write_key: None,
            server_chain: Vec::new(),
            alpn,
            session_hash: Vec::new(),
        });
        self.state = State::WaitCertificate12;
        Ok(())
    }

    /// 1.2 的记录分派。加密之后记录类型仍是真实的，因此按「读密钥是否已装」分两段。
    fn feed_record_12(&mut self, record: &Record) -> Result<(), HandshakeError> {
        let encrypted = self
            .tls12
            .as_ref()
            .map(|state| state.read_key.is_some())
            .unwrap_or(false);

        // CCS 必须在「有没有读密钥」之前判：服务端的 CCS 正是**安装**读密钥的时机，
        // 放在后面会被 "未加密" 分支当成意外类型。
        if record.kind == ContentType::ChangeCipherSpec {
            return self.handle_change_cipher_spec_12();
        }

        if !encrypted {
            // 服务端 flight 结束后、CCS 之前，OpenSSL 会先发一条 NewSessionTicket
            // （RFC 5077 允许它在 CCS 前或后）。这条记录**已经是加密的**：服务端在
            // 发完 ServerHelloDone 时就切了写密钥。此时我们还没装读密钥，无法解密，
            // 也不必解密——它只是会话票据。因此在这一阶段直接跳过握手记录，
            // 当普通明文处理会让状态机看到一个类型 4 并报错。
            if record.kind == ContentType::Handshake {
                let (messages, _) = take_handshake_messages(&record.payload);
                if !messages.is_empty()
                    && messages
                        .iter()
                        .all(|message| message.kind == hs::NEW_SESSION_TICKET)
                {
                    // 它是明文，且位于 server Finished **之前**，因此必须进转录：
                    // 1.2 的 Finished 覆盖「此前全部握手消息」，漏掉它只会表现为
                    // 「转录不一致」，不会指向 ticket。
                    let state = self.tls12.as_mut().expect("1.2 状态");
                    for message in &messages {
                        state.messages.extend_from_slice(&message.raw);
                    }
                    return Ok(());
                }
            }
            return match record.kind {
                ContentType::Alert => self.handle_alert(&record.payload),
                ContentType::Handshake => self.handle_plain_12(&record.payload),
                // 还没协商完就收到 CCS 或应用数据：顺序违规，必须拒绝而不是跳过。
                other => Err(HandshakeError::Malformed(format!(
                    "1.2 握手阶段收到意外的记录类型 {}",
                    other as u8
                ))),
            };
        }

        match record.kind {
            ContentType::Alert => self.handle_alert(&record.payload),
            ContentType::ChangeCipherSpec => self.handle_change_cipher_spec_12(),
            ContentType::Handshake => {
                let plaintext = self
                    .tls12
                    .as_mut()
                    .and_then(|state| state.read_key.as_mut())
                    .ok_or(HandshakeError::Missing("1.2 握手读密钥"))?
                    .open(record)
                    .map_err(HandshakeError::Record)?;
                self.handle_encrypted_12(&plaintext)
            }
            ContentType::ApplicationData => Err(HandshakeError::Malformed(
                "1.2 握手尚未完成就收到应用数据".to_string(),
            )),
        }
    }

    /// 1.2 的明文握手消息：Certificate → ServerKeyExchange → ServerHelloDone。
    fn handle_plain_12(&mut self, payload: &[u8]) -> Result<(), HandshakeError> {
        let (messages, consumed) = take_handshake_messages(payload);
        if consumed != payload.len() {
            return Err(HandshakeError::Malformed(
                "1.2 握手负载尾部有残缺消息".to_string(),
            ));
        }
        for message in messages {
            let current = self.state;
            match (current, message.kind) {
                (State::WaitCertificate12, hs::CERTIFICATE) => {
                    // 1.2 的 Certificate 没有 per-entry 扩展块，用 1.3 的解析器会读歪。
                    let chain = crate::tls12::parse_certificate_chain(&message.body)
                        .map_err(HandshakeError::Malformed)?;
                    let chain: Vec<CertificateDer<'static>> = chain
                        .into_iter()
                        .map(|der| CertificateDer::from(der).into_owned())
                        .collect();
                    self.verify_chain_12(&chain)?;
                    let state = self.tls12.as_mut().expect("1.2 状态");
                    state.server_chain = chain;
                    state.messages.extend_from_slice(&message.raw);
                    self.state = State::WaitServerKeyExchange;
                }
                (State::WaitServerKeyExchange, hs::SERVER_KEY_EXCHANGE) => {
                    self.handle_server_key_exchange_12(&message)?;
                }
                (State::WaitServerHelloDone, hs::SERVER_HELLO_DONE) => {
                    self.tls12
                        .as_mut()
                        .expect("1.2 状态")
                        .messages
                        .extend_from_slice(&message.raw);
                    self.finish_server_flight_12()?;
                }
                // 支持证书认证就必须回客户端证书；我们不发，假装没看见会让服务端一直等。
                (_, hs::CERTIFICATE_REQUEST) => {
                    return Err(HandshakeError::Malformed(
                        "服务端要求客户端证书（CertificateRequest），本实现不支持客户端认证"
                            .to_string(),
                    ));
                }
                (state, received) => {
                    return Err(HandshakeError::UnexpectedMessage { state, received });
                }
            }
        }
        Ok(())
    }

    /// 1.2 的 `ServerKeyExchange`：验签、算 pre_master，并留住 ClientKeyExchange 的公钥。
    fn handle_server_key_exchange_12(
        &mut self,
        message: &HandshakeMessage,
    ) -> Result<(), HandshakeError> {
        let parsed = crate::tls12::parse_server_key_exchange(&message.body)
            .map_err(HandshakeError::Malformed)?;

        let (leaf, client_random, server_random) = {
            let state = self.tls12.as_ref().expect("1.2 状态");
            let leaf = state
                .server_chain
                .first()
                .ok_or(HandshakeError::Missing("1.2 叶证书"))?
                .clone();
            (leaf, state.client_random, state.server_random)
        };

        // 1.2 的 SKE 签名覆盖两个 random 与曲线参数；漏掉 random 会让验签永远失败，
        // 而错误信息只会说「签名不匹配」。
        let signed =
            crate::tls12::signed_key_exchange_message(&client_random, &server_random, &parsed.params);
        crate::verify::verify_raw_signature(&leaf, parsed.scheme, &signed, &parsed.signature)?;

        // 1.2 没有 HRR，服务端只能在我们已给出公钥的组里挑。首份 ClientHello 同时发了
        // X25519 与 P-256，因此两个都可能被选中（百度就选 P-256）。
        let exchange = if self.exchange.as_ref().map(|inner| inner.group) == Some(parsed.group) {
            self.exchange.take()
        } else if self.p256_exchange.as_ref().map(|inner| inner.group) == Some(parsed.group) {
            self.p256_exchange.take()
        } else {
            let offered: Vec<String> = [self.exchange.as_ref(), self.p256_exchange.as_ref()]
                .into_iter()
                .flatten()
                .map(|inner| format!("{:#06x}", inner.group))
                .collect();
            return Err(HandshakeError::Malformed(format!(
                "ServerKeyExchange 用了组 {:#06x}，但我们只为组 {} 发过公钥",
                parsed.group,
                offered.join("、")
            )));
        };
        let exchange = exchange.ok_or(HandshakeError::Missing("ECDHE 密钥对"))?;
        let public_key = exchange.public_key.clone();
        let shared = exchange
            .agree(&parsed.point)
            .map_err(|error| HandshakeError::Malformed(format!("{error}")))?;

        let state = self.tls12.as_mut().expect("1.2 状态");
        state.pre_master = shared;
        state.messages.extend_from_slice(&message.raw);
        self.tls12_client_key = Some(public_key);
        self.state = State::WaitServerHelloDone;
        Ok(())
    }

    /// 服务端 flight 结束：派生 master secret 与密钥块，并备好本端 flight
    /// （ClientKeyExchange + CCS + Finished）交给驱动层发出。
    fn finish_server_flight_12(&mut self) -> Result<(), HandshakeError> {
        let public_key = self
            .tls12_client_key
            .take()
            .ok_or(HandshakeError::Missing("1.2 的 ECDHE 公钥"))?;
        let cke =
            crate::tls12::client_key_exchange_body(&public_key).map_err(HandshakeError::Malformed)?;

        let sealed = {
            let state = self.tls12.as_mut().expect("1.2 状态");
            let hash = state.suite.prf_hash();
            // RFC 7627 §4 与 RFC 5246 §7.4.9 对「会话转录」的边界是同一条：
            // ClientHello 一直到**并包含** ClientKeyExchange。少了 CKE 会让
            // master secret 全错，症状是服务端回 bad_record_mac（记录层解不开），
            // 而不是任何与 Finished 有关的错误——这条边界踩过一次。
            state
                .messages
                .extend_from_slice(&hs_message(hs::CLIENT_KEY_EXCHANGE, &cke));
            let session_hash = Transcript12::restarted(hash, &state.messages).hash();
            state.session_hash = session_hash.clone();
            let master = if state.extended_master {
                crate::tls12::extended_master_secret(hash, &state.pre_master, &session_hash)
            } else {
                // 没协商 EMS 时退回 RFC 5246 的派生（两个 random）。
                crate::tls12::master_secret(
                    hash,
                    &state.pre_master,
                    &state.client_random,
                    &state.server_random,
                )
            };
            state.master = master;
            let block = crate::tls12::key_block(
                hash,
                state.suite,
                &state.master,
                &state.client_random,
                &state.server_random,
            );
            state.write_key = Some(
                RecordKey12::new(state.suite, &block.client).map_err(HandshakeError::Record)?,
            );

            // client Finished 覆盖「到 ServerHelloDone 为止」的转录；算出之后才把它
            // 自己推进转录，供随后的 server Finished 校验使用。
            let verify = crate::tls12::finished_verify_data(
                hash,
                &state.master,
                "client finished",
                &session_hash,
            );
            let finished_raw = hs_message(hs::FINISHED, &verify);
            let sealed = state
                .write_key
                .as_mut()
                .expect("1.2 写密钥")
                .seal(ContentType::Handshake, &finished_raw)
                .map_err(HandshakeError::Record)?;

            state.messages.extend_from_slice(&finished_raw);
            sealed
        };

        let mut out = Vec::new();
        for record in [
            Record {
                kind: ContentType::Handshake,
                payload: hs_message(hs::CLIENT_KEY_EXCHANGE, &cke),
            },
            Record {
                kind: ContentType::ChangeCipherSpec,
                payload: vec![0x01],
            },
            sealed,
        ] {
            out.extend_from_slice(
                &record
                    .encode(crate::tls12::TLS12_VERSION)
                    .map_err(|error| HandshakeError::Malformed(format!("{error}")))?,
            );
        }
        self.tls12_flight = Some(out);
        self.state = State::WaitServerFinished12;
        Ok(())
    }

    /// 收到服务端 CCS：此时才装读密钥（服务端的加密记录紧随其后）。
    fn handle_change_cipher_spec_12(&mut self) -> Result<(), HandshakeError> {
        if self.state != State::WaitServerFinished12 {
            return Err(HandshakeError::UnexpectedMessage {
                state: self.state,
                received: ContentType::ChangeCipherSpec as u8,
            });
        }
        // 服务端的加密记录从 CCS 之后开始，因此读密钥在这里装。
        let state = self.tls12.as_mut().expect("1.2 状态");
        if state.read_key.is_none() {
            let block = crate::tls12::key_block(
                state.suite.prf_hash(),
                state.suite,
                &state.master,
                &state.client_random,
                &state.server_random,
            );
            state.read_key = Some(
                RecordKey12::new(state.suite, &block.server).map_err(HandshakeError::Record)?,
            );
        }
        Ok(())
    }

    /// 1.2 的加密握手消息：只可能是服务端 Finished。
    fn handle_encrypted_12(&mut self, plaintext: &[u8]) -> Result<(), HandshakeError> {
        let (messages, consumed) = take_handshake_messages(plaintext);
        if consumed != plaintext.len() {
            return Err(HandshakeError::Malformed(
                "1.2 加密握手负载尾部有残缺消息".to_string(),
            ));
        }
        for message in messages {
            match (self.state, message.kind) {
                (State::WaitServerFinished12, hs::FINISHED) => {
                    let state = self.tls12.as_mut().expect("1.2 状态");
                    let hash = state.suite.prf_hash();
                    let transcript = Transcript12::restarted(hash, &state.messages);
                    let expected = crate::tls12::finished_verify_data(
                        hash,
                        &state.master,
                        "server finished",
                        &transcript.hash(),
                    );
                    // 常量时间比较：逐字节早退会把「前几字节正确」泄漏给对端。
                    if !constant_time_eq(&expected, &message.body) {
                        return Err(HandshakeError::Verification(VerifyError::FinishedMismatch));
                    }
                    state.messages.extend_from_slice(&message.raw);
                    self.state = State::Connected;
                    return Ok(());
                }
                // 1.2 的服务端可能在 Finished 之前先发 NewSessionTicket（带
                // session_ticket 扩展时几乎必然如此）。不做会话恢复，但要容忍它：
                // 直接报「收到了握手消息类型 4」会让本来正常的连接断在最后一步。
                (State::WaitServerFinished12, hs::NEW_SESSION_TICKET) => {
                    self.tls12
                        .as_mut()
                        .expect("1.2 状态")
                        .messages
                        .extend_from_slice(&message.raw);
                }
                (current, received) => {
                    return Err(HandshakeError::UnexpectedMessage {
                        state: current,
                        received,
                    });
                }
            }
        }
        Ok(())
    }

    /// 1.2 分支里的证书链信任判定（策略与 1.3 一致）。
    fn verify_chain_12(&self, chain: &[CertificateDer<'static>]) -> Result<(), HandshakeError> {
        if self.policy != PeerPolicy::VerifyChain {
            return Ok(());
        }
        let anchors = self
            .anchors
            .as_ref()
            .ok_or(HandshakeError::Missing("受信根"))?;
        let name = ServerName::try_from(self.host.as_str())
            .map_err(|error| VerifyError::WrongHostName(format!("{error}")))?;
        crate::verify::verify_server_chain(
            anchors,
            chain,
            &name,
            self.verify_at.unwrap_or_else(UnixTime::now),
        )?;
        Ok(())
    }

    /// 取出 1.2 分支待发送的整段 flight（ClientKeyExchange + CCS + Finished）。
    pub fn take_tls12_flight(&mut self) -> Option<Vec<u8>> {
        self.tls12_flight.take()
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
                self.push_transcript(&message.raw);
                // 恢复路径下服务端不发 Certificate / CertificateVerify（RFC 8446 §4.1.3），
                // EncryptedExtensions 之后直接就是 Finished。仍按证书路径等下去会让握手卡死。
                self.state = if self.resumed {
                    State::WaitFinished
                } else {
                    State::WaitCertificate
                };
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
                self.push_transcript(&message.raw);
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
        self.push_transcript(&message.raw);
        self.state = State::WaitFinished;
        Ok(())
    }

    fn handle_server_finished(&mut self, message: &HandshakeMessage) -> Result<(), HandshakeError> {
        let suite = self.suite.ok_or(HandshakeError::Missing("协商套件"))?;
        // 转录此时只含 server Finished 之前的消息。
        let expected = {
            let secrets = self
                .handshake_secrets
                .as_ref()
                .ok_or(HandshakeError::Missing("握手密钥"))?;
            finished_verify_data(suite, &secrets.server_secret, &self.transcript)
                .map_err(HandshakeError::KeySchedule)?
        };
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
        self.push_transcript(&message.raw);

        // 转录刚被推进，`handle_server_finished` 里不能再挂着对 `handshake_secrets`
        // 的不可变借用，因此先把派生应用密钥所需的东西拷出来。
        let application = {
            let secrets = self
                .handshake_secrets
                .as_ref()
                .ok_or(HandshakeError::Missing("握手密钥"))?;
            secrets
                .application_secrets(&self.transcript.hash())
                .map_err(HandshakeError::KeySchedule)?
        };
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

        self.push_transcript(&raw);
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
    ///
    /// 1.2 与 1.3 的记录形状不同（1.2 保留真实类型、1.3 一律 `application_data`），
    /// 因此这里按协商版本分派，调用方不需要知道差别。
    pub fn seal_application(&mut self, data: &[u8]) -> Result<Record, HandshakeError> {
        if self.state != State::Connected {
            return Err(HandshakeError::Malformed(format!(
                "握手未完成（{:?}），不能发应用数据",
                self.state
            )));
        }
        if let Some(state) = self.tls12.as_mut() {
            return state
                .write_key
                .as_mut()
                .ok_or(HandshakeError::Missing("1.2 应用写密钥"))?
                .seal(ContentType::ApplicationData, data)
                .map_err(HandshakeError::Record);
        }
        self.write_key
            .as_mut()
            .ok_or(HandshakeError::Missing("应用写密钥"))?
            .seal(ContentType::ApplicationData, data)
            .map_err(HandshakeError::Record)
    }

    /// 解密一条对端的应用数据记录。
    pub fn open_application(&mut self, payload: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        self.open_application_typed(ContentType::ApplicationData, payload)
    }

    /// 解密一条对端记录，并返回它的内层类型。
    ///
    /// 1.2 的加密记录**保留真实类型**（可能是 handshake，例如会话票据），因此调用方
    /// 只传载荷是不够的，必须把记录类型一起交进来。
    pub fn open_application_typed(
        &mut self,
        kind: ContentType,
        payload: &[u8],
    ) -> Result<Vec<u8>, HandshakeError> {
        if self.state != State::Connected {
            return Err(HandshakeError::Malformed(format!(
                "握手未完成（{:?}），不能读应用数据",
                self.state
            )));
        }
        let record = Record {
            kind,
            payload: payload.to_vec(),
        };
        if let Some(state) = self.tls12.as_mut() {
            let body = state
                .read_key
                .as_mut()
                .ok_or(HandshakeError::Missing("1.2 应用读密钥"))?
                .open(&record)
                .map_err(HandshakeError::Record)?;
            return match kind {
                ContentType::ApplicationData => Ok(body),
                // 1.2 里握手记录是 NewSessionTicket 或 Finished 之后的补充消息，忽略。
                ContentType::Handshake => Ok(Vec::new()),
                ContentType::Alert => Err(HandshakeError::Alert {
                    level: body.first().copied().unwrap_or(0),
                    description: body.get(1).copied().unwrap_or(0),
                }),
                other => Err(HandshakeError::Malformed(format!(
                    "1.2 期望应用数据，收到记录类型 {}",
                    other as u8
                ))),
            };
        }
        let (inner, body) = self
            .read_key
            .as_mut()
            .ok_or(HandshakeError::Missing("应用读密钥"))?
            .open(&record)
            .map_err(HandshakeError::Record)?;
        match inner {
            ContentType::ApplicationData => Ok(body),
            // 握手完成后服务端仍会发 NewSessionTicket（每条一个握手记录）。
            // 启用恢复时把它解析出来备用；解析失败只是丢掉一条票据，**不能**报错——
            // 那会让本来正常的连接直接断掉。
            ContentType::Handshake => {
                self.capture_session_ticket(&body);
                Ok(Vec::new())
            }
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

/// 拼一条握手消息（类型 + 三字节长度 + 体）。
fn hs_message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(kind);
    out.push((body.len() >> 16) as u8);
    out.push((body.len() >> 8) as u8);
    out.push(body.len() as u8);
    out.extend_from_slice(body);
    out
}

/// 常量时间比较。逐字节早退会把「前几字节正确」这一信息泄漏给对端。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
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

    #[test]
    fn version_codes_are_rendered_as_the_public_version_names() {
        // 0x0303 是 TLS 1.2 的 legacy_version：渲染成 1.3 会让「对端回落」的报错自相矛盾。
        assert_eq!(format_version(0x0301), "1.0");
        assert_eq!(format_version(0x0302), "1.1");
        assert_eq!(format_version(0x0303), "1.2");
        assert_eq!(format_version(0x0304), "1.3");
    }

    #[test]
    fn a_tls12_server_hello_switches_to_the_12_branch() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        let record = Record {
            kind: ContentType::Handshake,
            payload: server_hello_12(0xc02f, &[]),
        };
        handshake
            .feed_record(&record)
            .expect("1.2 的 ServerHello 应当被接受");
        assert_eq!(handshake.version(), Some(Version::Tls12));
        assert_eq!(handshake.state(), State::WaitCertificate12);
    }

    #[test]
    fn a_tls11_peer_is_reported_as_unsupported() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        // legacy_version = 0x0302（TLS 1.1），且没有 supported_versions。
        let record = Record {
            kind: ContentType::Handshake,
            payload: server_hello_raw(0x0302, 0xc02f, &[]),
        };
        let error = handshake.feed_record(&record).expect_err("1.1 不受支持");
        let text = error.to_string();
        assert!(text.contains("TLS 1.1"), "实际文案：{text}");
    }

    #[test]
    fn an_unsupported_12_suite_is_named_in_the_error() {
        // CBC 套件（0xc013）不在支持范围：必须报「套件不支持」而不是「版本不支持」，
        // 否则会让人以为我们连 1.2 都没实现。
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        let record = Record {
            kind: ContentType::Handshake,
            payload: server_hello_12(0xc013, &[]),
        };
        let error = handshake.feed_record(&record).expect_err("CBC 套件不受支持");
        let text = error.to_string();
        assert!(text.contains("1.2 套件"), "实际文案：{text}");
        assert!(text.contains("0xc013"), "实际文案：{text}");
    }

    #[test]
    fn a_12_server_hello_records_alpn_and_extended_master_secret() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        let alpn = crate::client_hello::alpn(&["h2"]);
        let record = Record {
            kind: ContentType::Handshake,
            payload: server_hello_12(
                0xc02f,
                &[
                    (crate::client_hello::ext::EXTENDED_MASTER_SECRET, Vec::new()),
                    (crate::client_hello::ext::ALPN, alpn),
                ],
            ),
        };
        handshake.feed_record(&record).expect("应当接受");
        assert_eq!(handshake.alpn(), Some("h2"));
        assert!(handshake.tls12.as_ref().expect("1.2 状态").extended_master);
    }

    /// 造一条 1.2 的 ServerHello：没有 `supported_versions`，版本就是 `legacy_version`。
    fn server_hello_12(suite: u16, extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        server_hello_raw(0x0303, suite, extensions)
    }

    /// 造一条指定 legacy_version 与扩展的 ServerHello（含握手头）。
    fn server_hello_raw(legacy: u16, suite: u16, extensions: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut block = Vec::new();
        for (kind, data) in extensions {
            block.extend_from_slice(&kind.to_be_bytes());
            block.extend_from_slice(&(data.len() as u16).to_be_bytes());
            block.extend_from_slice(data);
        }
        let mut body = Vec::new();
        body.extend_from_slice(&legacy.to_be_bytes());
        body.extend_from_slice(&[0x5a; 32]);
        body.push(0);
        body.extend_from_slice(&suite.to_be_bytes());
        body.push(0);
        body.extend_from_slice(&(block.len() as u16).to_be_bytes());
        body.extend_from_slice(&block);
        let mut message = vec![hs::SERVER_HELLO];
        message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }

    /// 造一条要求改用 `group` 并带 cookie 的 HelloRetryRequest。
    fn hello_retry_request(group: u16, cookie: &[u8]) -> Vec<u8> {
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&crate::client_hello::ext::SUPPORTED_VERSIONS.to_be_bytes());
        extensions.extend_from_slice(&2u16.to_be_bytes());
        extensions.extend_from_slice(&0x0304u16.to_be_bytes());
        extensions.extend_from_slice(&crate::client_hello::ext::KEY_SHARE.to_be_bytes());
        extensions.extend_from_slice(&2u16.to_be_bytes());
        extensions.extend_from_slice(&group.to_be_bytes());
        if !cookie.is_empty() {
            extensions.extend_from_slice(&crate::client_hello::ext::COOKIE.to_be_bytes());
            extensions.extend_from_slice(&((cookie.len() + 2) as u16).to_be_bytes());
            extensions.extend_from_slice(&(cookie.len() as u16).to_be_bytes());
            extensions.extend_from_slice(cookie);
        }

        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&crate::handshake::HELLO_RETRY_REQUEST_RANDOM);
        body.push(0);
        body.extend_from_slice(&0x1301u16.to_be_bytes());
        body.push(0);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let mut message = vec![hs::SERVER_HELLO];
        message.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        message.extend_from_slice(&body);
        message
    }

    /// 取出重发的 ClientHello 体（驱动层要原样发出，这里直接检查它的内容）。
    fn retried_client_hello(handshake: &mut ClientHandshake) -> Vec<u8> {
        let record = handshake
            .take_retry_client_hello()
            .expect("HRR 之后应当有待发的 ClientHello");
        let (parsed, consumed) = crate::record::parse_record(&record).expect("应当能解析");
        assert_eq!(consumed, record.len());
        assert_eq!(parsed.kind, ContentType::Handshake);
        parsed.payload
    }

    #[test]
    fn a_hello_retry_request_for_p256_is_answered_with_a_p256_key_share() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        let record = Record {
            kind: ContentType::Handshake,
            payload: hello_retry_request(crate::client_hello::group::SECP256R1, &[]),
        };
        handshake.feed_record(&record).expect("HRR 应当被接受");

        let body = retried_client_hello(&mut handshake);
        let (spec, _, _) = crate::parse::parse_client_hello(&body).expect("应当能解析");
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::KEY_SHARE)
            .expect("应当有 key_share");
        // 恰好一个条目：组 0x0017 + 65 字节未压缩点。
        assert_eq!(u16::from_be_bytes([slot.data[2], slot.data[3]]), 0x0017);
        assert_eq!(
            u16::from_be_bytes([slot.data[4], slot.data[5]]) as usize,
            65,
            "P-256 的公钥应当是 65 字节"
        );
    }

    #[test]
    fn the_retried_client_hello_reuses_the_first_random_and_session_id() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        let first = handshake.start(&rng()).expect("生成");
        let (first_record, _) = crate::record::parse_record(&first).expect("解析");
        let (first_spec, first_random, first_session_id) =
            crate::parse::parse_client_hello(&first_record.payload).expect("解析");

        let record = Record {
            kind: ContentType::Handshake,
            payload: hello_retry_request(crate::client_hello::group::SECP256R1, &[0xab; 16]),
        };
        handshake.feed_record(&record).expect("HRR 应当被接受");

        let body = retried_client_hello(&mut handshake);
        let (spec, random, session_id) = crate::parse::parse_client_hello(&body).expect("解析");
        // 换掉 random 或 session_id 会被服务端判为 illegal_parameter。
        assert_eq!(random, first_random, "random 必须沿用第一个 ClientHello");
        assert_eq!(session_id, first_session_id, "session_id 必须沿用");
        assert_eq!(
            spec.cipher_suites, first_spec.cipher_suites,
            "密码套件顺序也要一致"
        );
    }

    #[test]
    fn a_hello_retry_request_cookie_is_echoed_back() {
        let mut handshake = ClientHandshake::new(Family::Chrome, "example.com");
        handshake.start(&rng()).expect("生成");
        let cookie = [0x5a; 24];
        let record = Record {
            kind: ContentType::Handshake,
            payload: hello_retry_request(crate::client_hello::group::SECP256R1, &cookie),
        };
        handshake.feed_record(&record).expect("HRR 应当被接受");

        let body = retried_client_hello(&mut handshake);
        let (spec, _, _) = crate::parse::parse_client_hello(&body).expect("解析");
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::COOKIE)
            .expect("重发的 ClientHello 必须回带 cookie");
        // cookie 扩展的数据是「2 字节长度 + 原始 cookie」。
        assert_eq!(&slot.data[2..], &cookie);
    }
}
