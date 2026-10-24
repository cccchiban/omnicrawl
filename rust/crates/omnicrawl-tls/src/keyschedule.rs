//! TLS 1.3 的密钥调度（RFC 8446 §7.1）。
//!
//! 1.3 的密钥是从「握手转录哈希」逐级派生出来的，而不是像 1.2 那样直接从密钥交换
//! 结果派生。链条固定：
//!
//! ```text
//! early_secret     = HKDF-Extract(salt=0, ikm=0)
//! derived          = Derive-Secret(early_secret, "derived", "")
//! handshake_secret = HKDF-Extract(salt=derived, ikm=ECDHE)
//! client_hs_traffic = Derive-Secret(handshake_secret, "c hs traffic", Hash(CH..SH))
//! server_hs_traffic = Derive-Secret(handshake_secret, "s hs traffic", Hash(CH..SH))
//! derived2         = Derive-Secret(handshake_secret, "derived", "")
//! master_secret    = HKDF-Extract(salt=derived2, ikm=0)
//! client_ap_traffic = Derive-Secret(master_secret, "c ap traffic", Hash(..server Finished))
//! ```
//!
//! 关键实现约束：`ring` 的 `Prk` 不暴露内部字节，而下一轮 `HKDF-Extract` 需要把上一轮的
//! 派生结果当 salt。因此这里**全程持有字节**（派生结果先 `fill` 进 `Vec`），需要
//! `Prk` 时用 `new_less_safe` 重建。这样链条每一环都可独立断言，不必猜内部状态。

use ring::{digest, hkdf, hmac};

/// 握手用的密码套件（决定哈希与 AEAD）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherSuite {
    Aes128GcmSha256,
    Aes256GcmSha384,
    ChaCha20Poly1305Sha256,
}

impl CipherSuite {
    pub fn from_code(code: u16) -> Option<Self> {
        match code {
            0x1301 => Some(Self::Aes128GcmSha256),
            0x1302 => Some(Self::Aes256GcmSha384),
            0x1303 => Some(Self::ChaCha20Poly1305Sha256),
            _ => None,
        }
    }

    pub fn code(&self) -> u16 {
        match self {
            Self::Aes128GcmSha256 => 0x1301,
            Self::Aes256GcmSha384 => 0x1302,
            Self::ChaCha20Poly1305Sha256 => 0x1303,
        }
    }

    pub fn hash(&self) -> &'static digest::Algorithm {
        match self {
            Self::Aes128GcmSha256 | Self::ChaCha20Poly1305Sha256 => &digest::SHA256,
            Self::Aes256GcmSha384 => &digest::SHA384,
        }
    }

    /// `ring` 的 HKDF 算法常量（与哈希一一对应）。
    pub fn hkdf_algorithm(&self) -> hkdf::Algorithm {
        match self {
            Self::Aes128GcmSha256 | Self::ChaCha20Poly1305Sha256 => hkdf::HKDF_SHA256,
            Self::Aes256GcmSha384 => hkdf::HKDF_SHA384,
        }
    }

    /// `ring` 的 HMAC 算法（与哈希一一对应）。
    pub fn hmac_algorithm(&self) -> hmac::Algorithm {
        match self {
            Self::Aes128GcmSha256 | Self::ChaCha20Poly1305Sha256 => hmac::HMAC_SHA256,
            Self::Aes256GcmSha384 => hmac::HMAC_SHA384,
        }
    }

    pub fn aead(&self) -> &'static ring::aead::Algorithm {
        match self {
            Self::Aes128GcmSha256 => &ring::aead::AES_128_GCM,
            Self::Aes256GcmSha384 => &ring::aead::AES_256_GCM,
            Self::ChaCha20Poly1305Sha256 => &ring::aead::CHACHA20_POLY1305,
        }
    }

    /// 记录层密钥长度（字节）。
    pub fn key_len(&self) -> usize {
        match self {
            Self::Aes128GcmSha256 => 16,
            Self::Aes256GcmSha384 | Self::ChaCha20Poly1305Sha256 => 32,
        }
    }

    /// 该套件所用哈希的输出长度（字节）。
    pub fn hash_len(&self) -> usize {
        self.hash().output_len()
    }
}

/// 一组「密钥 + IV」，可直接交给记录层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficKeys {
    pub suite: CipherSuite,
    pub key: Vec<u8>,
    pub iv: [u8; 12],
}

/// 握手转录：按顺序累积哈希，可在任意时点取快照。
///
/// 1.3 需要在多个时点取「到目前为止」的哈希（CH..SH、CH..server Finished），
/// 因此用可克隆的增量摘要：取快照是 O(1)，不必缓存全部握手字节。
#[derive(Clone)]
pub struct Transcript {
    state: digest::Context,
}

impl Transcript {
    pub fn new(suite: CipherSuite) -> Self {
        Self {
            state: digest::Context::new(suite.hash()),
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.state.update(data);
    }

    /// 按 RFC 8446 §4.4.1 放入 `message_hash`。
    ///
    /// 线上形状是一条完整的合成握手消息：
    ///
    /// ```text
    /// message_hash = 0xfe || 00 00 Hash.length || Hash(ClientHello1)
    /// ```
    ///
    /// 三个细节都不能省：类型字节是 254（不是 1）、长度是三字节、长度值为哈希长度
    /// 而不是 `2 + 哈希长度`。任何一处写错都会让 HRR 路径的转录哈希与服务端不一致，
    /// 而普通握手测不出来。
    pub fn push_message_hash(&mut self, hash: &[u8]) {
        self.state.update(&[0xfe]);
        self.state.update(&[0, 0, hash.len() as u8]);
        self.state.update(hash);
    }

    /// 当前转录摘要用的是什么哈希。
    ///
    /// 客户端一开始不知道会协商出哪个套件，只能先用一个占位套件开转录；收到
    /// ServerHello 后必须核对一次，不一致就用 [`Transcript::restarted`] 重建——
    /// 同一个哈希函数累加的结果与「先缓冲再一次性哈希」完全等价，因此重建只要拿到
    /// 已累积的明文即可。
    pub fn hash_len(&self) -> usize {
        self.hash().len()
    }

    /// 用一个已累积原始消息的缓冲按新套件重建转录。
    pub fn restarted(suite: CipherSuite, messages: &[u8]) -> Self {
        let mut transcript = Self::new(suite);
        transcript.push(messages);
        transcript
    }

    /// 当前转录哈希。
    pub fn hash(&self) -> Vec<u8> {
        self.state.clone().finish().as_ref().to_vec()
    }
}

/// `Expand-Label` 的 info 结构（RFC 8446 §7.1）：
///
/// ```text
/// struct {
///     uint16 length;                    // 输出长度
///     opaque label<7..255> = "tls13 " + Label;   // 1 字节长度 + 内容
///     opaque context<0..255>;           // 1 字节长度 + 上下文
/// } HkdfLabel;
/// ```
///
/// 那个标签长度字节覆盖 `"tls13 "` 前缀与标签本身，值为 `6 + label.len()`。
/// 漏掉它会让整条密钥调度全错，而且错得很隐蔽——自己两端对称的测试依然自洽通过。
fn label_info(label: &str, context: &[u8], out_len: usize) -> Result<Vec<u8>, String> {
    if context.len() > u8::MAX as usize {
        return Err("HKDF 上下文过长".to_string());
    }
    let full_label_len = 6 + label.len();
    if full_label_len > u8::MAX as usize {
        return Err("HKDF 标签过长".to_string());
    }
    let mut info = Vec::with_capacity(2 + 1 + full_label_len + 1 + context.len());
    info.extend_from_slice(&(out_len as u16).to_be_bytes());
    info.push(full_label_len as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);
    Ok(info)
}

/// `HKDF-Expand-Label`：TLS 1.3 独有的标签扩展。
///
/// `ring` 的 `Okm::fill` 要求输出长度恰好等于所请求 `KeyType` 的长度，而唯一可用的
/// `KeyType` 是 `hkdf::Algorithm`（长度 = 哈希长度）。因此这里按哈希长度为单位请求，
/// 需要更短输出时截取前缀——HKDF 的输出前缀性质保证这与直接请求短长度的结果一致。
pub fn expand_label(
    suite: CipherSuite,
    secret: &hkdf::Prk,
    label: &str,
    context: &[u8],
    out: &mut [u8],
) -> Result<(), String> {
    let unit = suite.hash_len();
    if out.is_empty() {
        return Ok(());
    }
    let blocks = out.len().div_ceil(unit);
    if blocks > 255 {
        return Err("HKDF-Expand 输出长度超限（超过 255 个哈希块）".to_string());
    }
    let info = label_info(label, context, out.len())?;
    let mut buffer = vec![0u8; blocks * unit];
    let algorithm = suite.hkdf_algorithm();
    secret
        .expand(&[&info], algorithm)
        .map_err(|_| "HKDF-Expand 请求失败".to_string())?
        .fill(&mut buffer)
        .map_err(|_| "HKDF-Expand 填充失败".to_string())?;
    out.copy_from_slice(&buffer[..out.len()]);
    Ok(())
}

/// `HKDF-Extract`：返回 PRK 字节（长度等于哈希长度）。
///
/// 按 RFC 5869 定义，`PRK = HMAC-Hash(salt, IKM)`。`ring` 的 `hkdf::Salt::extract`
/// 就是这个运算，但它返回的 `Prk` 不暴露字节；下一轮 `HKDF-Extract` 又需要把本轮
/// 结果当 salt，所以这里直接用 hmac 算出字节，等价且可断言。
pub fn extract(suite: CipherSuite, salt: &[u8], ikm: &[u8]) -> Vec<u8> {
    let key = ring::hmac::Key::new(suite.hkdf_algorithm().hmac_algorithm(), salt);
    let tag = ring::hmac::sign(&key, ikm);
    tag.as_ref().to_vec()
}

/// 空转录的哈希，即 `Transcript-Hash("")`。
///
/// `Derive-Secret` 的上下文永远是「消息的哈希」，因此当消息为空时必须传这个值，
/// 而不是空字节。传空字节会让所有派生出的密钥全错——但这个错误在自家两端对称的
/// 测试里看不出来，只有对接真实实现才会暴露。
pub fn empty_transcript_hash(suite: CipherSuite) -> Vec<u8> {
    let mut context = digest::Context::new(suite.hash());
    context.update(&[]);
    context.finish().as_ref().to_vec()
}

/// `Derive-Secret`：用转录哈希当上下文派生，返回 PRK 字节。
///
/// 注意 `transcript_hash` 是**已经哈希过的转录**。`Derive-Secret(Secret, Label, Messages)`
/// 的上下文是 `Transcript-Hash(Messages)`，因此传 `""`（空转录）时必须是
/// **空串的哈希**而不是空字节——见 [`empty_transcript_hash`]。
pub fn derive_secret(
    suite: CipherSuite,
    secret: &[u8],
    label: &str,
    transcript_hash: &[u8],
) -> Result<Vec<u8>, String> {
    let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), secret);
    let mut out = vec![0u8; suite.hash_len()];
    expand_label(suite, &prk, label, transcript_hash, &mut out)?;
    Ok(out)
}

/// 从 PRK 字节切出记录层需要的 `key` 与 `iv`。
pub fn traffic_keys(suite: CipherSuite, secret: &[u8]) -> Result<TrafficKeys, String> {
    let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), secret);
    let mut key = vec![0u8; suite.key_len()];
    expand_label(suite, &prk, "key", &[], &mut key)?;
    let mut iv = [0u8; 12];
    expand_label(suite, &prk, "iv", &[], &mut iv)?;
    Ok(TrafficKeys { suite, key, iv })
}

/// 握手密钥调度，全程以字节表示中间密钥。
pub struct KeySchedule {
    suite: CipherSuite,
    early_secret: Vec<u8>,
    /// 是否用 PSK 构造。`HKDF-Extract(0, 0)` 不是全零，无法从 `early_secret` 反推。
    resumption: bool,
}

impl KeySchedule {
    pub fn new(suite: CipherSuite) -> Self {
        Self::with_psk(suite, None)
    }

    /// 带 PSK 的构造：会话恢复时 `early_secret = HKDF-Extract(0, PSK)`。
    ///
    /// 没有 PSK 时传 `None`，退回全文握手的 `HKDF-Extract(0, 0)`。这两个值一旦用错，
    /// 后续所有密钥全错——而对称的自家测试看不出来，只有对接真实服务端才会暴露。
    pub fn with_psk(suite: CipherSuite, psk: Option<&[u8]>) -> Self {
        let zeros = vec![0u8; suite.hash_len()];
        let ikm = psk.unwrap_or(&zeros);
        Self {
            suite,
            early_secret: extract(suite, &zeros, ikm),
            // 必须显式记录：`HKDF-Extract(0, 0)` 的结果本身不是全零，
            // 因此不能靠比较 `early_secret` 反推是否用了 PSK。
            resumption: psk.is_some(),
        }
    }

    /// `early_secret` 本身：派生 binder key 与 client early traffic secret 的起点。
    pub fn early_secret(&self) -> &[u8] {
        &self.early_secret
    }

    /// 这条调度是否基于 PSK（会话恢复）。
    pub fn is_resumption(&self) -> bool {
        self.resumption
    }

    /// `resumption_master_secret`：派发 NewSessionTicket 里 PSK 的根。
    ///
    /// 必须在 client Finished 之后调用：上下文是 `Hash(..client Finished)`。
    /// 这一步用到的 `master_secret` 原先算完就丢，因此 [`ApplicationSecrets`]
    /// 现在把它带出来。
    pub fn resumption_master_secret(
        suite: CipherSuite,
        master_secret: &[u8],
        transcript_hash_after_client_finished: &[u8],
    ) -> Result<Vec<u8>, String> {
        derive_secret(
            suite,
            master_secret,
            "res master",
            transcript_hash_after_client_finished,
        )
    }

    /// 从 `resumption_master_secret` 与票据 nonce 派生出这条票据的 PSK。
    ///
    /// `nonce` 来自 NewSessionTicket；`label` 是 `resumption`。
    pub fn resumption_psk(
        suite: CipherSuite,
        resumption_master_secret: &[u8],
        nonce: &[u8],
    ) -> Result<Vec<u8>, String> {
        let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), resumption_master_secret);
        let mut out = vec![0u8; suite.hash_len()];
        expand_label(suite, &prk, "resumption", nonce, &mut out)?;
        Ok(out)
    }

    /// 计算 ClientHello 的 PSK binder。
    ///
    /// RFC 8446 §4.2.11.2：binder 覆盖的是「**不含 binder 本身**的 ClientHello」的转录哈希，
    /// 经 `binder_key` 派生后做 Finished 那样的 HMAC。因此调用方必须传入
    /// 「截断到 binder 之前」的 ClientHello 字节——把完整 CH 传进来会算出错误的值。
    pub fn psk_binder(
        suite: CipherSuite,
        psk: &[u8],
        truncated_client_hello: &[u8],
    ) -> Result<Vec<u8>, String> {
        let zeros = vec![0u8; suite.hash_len()];
        let early_secret = extract(suite, &zeros, psk);
        let binder_key = derive_secret(
            suite,
            &early_secret,
            "res binder",
            &empty_transcript_hash(suite),
        )?;
        let finished_key = {
            let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), &binder_key);
            let mut out = vec![0u8; suite.hash_len()];
            expand_label(suite, &prk, "finished", &[], &mut out)?;
            out
        };
        let mut context = digest::Context::new(suite.hash());
        context.update(truncated_client_hello);
        let transcript_hash = context.finish().as_ref().to_vec();
        let key = ring::hmac::Key::new(suite.hkdf_algorithm().hmac_algorithm(), &finished_key);
        Ok(ring::hmac::sign(&key, &transcript_hash).as_ref().to_vec())
    }

    /// 握手阶段密钥（`transcript_hash` 取 CH..SH 的哈希）。
    pub fn handshake_secrets(
        &self,
        shared_secret: &[u8],
        transcript_hash: &[u8],
    ) -> Result<HandshakeSecrets, String> {
        let suite = self.suite;
        let derived = derive_secret(
            suite,
            &self.early_secret,
            "derived",
            &empty_transcript_hash(suite),
        )?;
        let handshake_secret = extract(suite, &derived, shared_secret);
        // 两个方向的 secret 必须留下来：Finished 的 verify_data 由它派生，
        // 无法从记录层的 key/iv 反推。
        let client_secret =
            derive_secret(suite, &handshake_secret, "c hs traffic", transcript_hash)?;
        let server_secret =
            derive_secret(suite, &handshake_secret, "s hs traffic", transcript_hash)?;
        Ok(HandshakeSecrets {
            suite,
            client: traffic_keys(suite, &client_secret)?,
            server: traffic_keys(suite, &server_secret)?,
            client_secret,
            server_secret,
            handshake_secret,
        })
    }
}

/// 握手阶段的密钥组合。
///
/// 除记录层要用的 `key`/`iv` 外还保留两个方向的 `secret`：Finished 的 verify_data 必须
/// 由 secret 经 `HKDF-Expand-Label(..,"finished",..)` 派生，无法从记录层 key 反推。
pub struct HandshakeSecrets {
    pub suite: CipherSuite,
    pub client: TrafficKeys,
    pub server: TrafficKeys,
    /// 客户端方向的握手 secret（计算客户端 Finished 用）。
    pub client_secret: Vec<u8>,
    /// 服务端方向的握手 secret（校验服务端 Finished 用）。
    pub server_secret: Vec<u8>,
    handshake_secret: Vec<u8>,
}

impl HandshakeSecrets {
    /// 应用阶段密钥：`transcript_hash_after_finished` 取 server Finished 之后的哈希。
    pub fn application_secrets(
        &self,
        transcript_hash_after_finished: &[u8],
    ) -> Result<ApplicationSecrets, String> {
        let suite = self.suite;
        let derived = derive_secret(
            suite,
            &self.handshake_secret,
            "derived",
            &empty_transcript_hash(suite),
        )?;
        let zeros = vec![0u8; suite.hash_len()];
        let master_secret = extract(suite, &derived, &zeros);
        Ok(ApplicationSecrets {
            suite,
            client: traffic_keys(
                suite,
                &derive_secret(
                    suite,
                    &master_secret,
                    "c ap traffic",
                    transcript_hash_after_finished,
                )?,
            )?,
            server: traffic_keys(
                suite,
                &derive_secret(
                    suite,
                    &master_secret,
                    "s ap traffic",
                    transcript_hash_after_finished,
                )?,
            )?,
            exporter_master: traffic_keys(
                suite,
                &derive_secret(
                    suite,
                    &master_secret,
                    "exp master",
                    transcript_hash_after_finished,
                )?,
            )?,
            // 会话恢复要在 client Finished 之后再派生 `res master`，因此它不能算完就丢。
            master_secret,
        })
    }
}

/// 应用阶段密钥组合。
pub struct ApplicationSecrets {
    pub suite: CipherSuite,
    pub client: TrafficKeys,
    pub server: TrafficKeys,
    /// 导出用主密钥（1.3 的 `exporter_master_secret`）。
    pub exporter_master: TrafficKeys,
    /// 主密钥本身：派生 `res master` 需要它，因此必须留到 client Finished 之后。
    pub master_secret: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8448 §3：`res master` 的公式（含上下文是 client Finished 之后的转录哈希）。
    #[test]
    fn resumption_master_secret_matches_the_rfc8448_vector() {
        let master = from_hex("18df06843d13a08bf2a449844c5f8a478001bc4d4c627984d5a41da8d0402919");
        let transcript =
            from_hex("209145a96ee8e2a122ff810047cc952684658d6049e86429426db87c54ad143d");
        let res = KeySchedule::resumption_master_secret(
            CipherSuite::Aes128GcmSha256,
            &master,
            &transcript,
        )
        .expect("派生 res master");
        assert_eq!(
            hex(&res),
            "7df235f2031d2a051287d02b0241b0bfdaf86cc856231f2d5aba46c434ec196c",
            "res master 必须与 RFC 8448 一致"
        );
    }

    /// RFC 8448 §3 的权威向量：从 `res master` 到票据 PSK 的派生必须逐字节吻合。
    ///
    /// 这条覆盖的是「第二次握手能不能成功」的关键一环：PSK 算错时第一次连接完全正常，
    /// 只有第二次被服务端以 decrypt_error(51) 拒绝——排查成本很高，因此必须有外部锚点。
    #[test]
    fn resumption_psk_matches_the_rfc8448_vector() {
        // RFC 8448 §3 的 `resumption_master_secret`。
        let res_master =
            from_hex("7df235f2031d2a051287d02b0241b0bfdaf86cc856231f2d5aba46c434ec196c");
        // NewSessionTicket 里的 nonce（2 字节）。
        let nonce = [0x00u8, 0x00];
        let psk = KeySchedule::resumption_psk(CipherSuite::Aes128GcmSha256, &res_master, &nonce)
            .expect("派生 PSK");
        assert_eq!(
            hex(&psk),
            "4ecd0eb6ec3b4d87f5d6028f922ca4c5851a277fd41311c9e62d2c9492e1c4f3",
            "PSK 必须与 RFC 8448 一致"
        );
    }

    /// RFC 8448 §4 的权威向量：binder 必须逐字节吻合。
    ///
    /// 这条是外部权威值，不是自家两端互相印证——自家两端用同一个错误算法也能「通过」，
    /// 只有对照 RFC 才能发现 binder 算错。真实症状是服务端直接 alert decode_error(50)。
    #[test]
    fn psk_binder_matches_the_rfc8448_vector() {
        // RFC 8448 §4：截断后的 ClientHello（477 字节，长度字段已按 binder 存在写好）。
        const TRUNCATED_CLIENT_HELLO: &[u8] = &[
            0x01, 0x00, 0x01, 0xfc, 0x03, 0x03, 0x1b, 0xc3, 0xce, 0xb6, 0xbb, 0xe3, 0x9c, 0xff,
            0x93, 0x83, 0x55, 0xb5, 0xa5, 0x0a, 0xdb, 0x6d, 0xb2, 0x1b, 0x7a, 0x6a, 0xf6, 0x49,
            0xd7, 0xb4, 0xbc, 0x41, 0x9d, 0x78, 0x76, 0x48, 0x7d, 0x95, 0x00, 0x00, 0x06, 0x13,
            0x01, 0x13, 0x03, 0x13, 0x02, 0x01, 0x00, 0x01, 0xcd, 0x00, 0x00, 0x00, 0x0b, 0x00,
            0x09, 0x00, 0x00, 0x06, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0xff, 0x01, 0x00, 0x01,
            0x00, 0x00, 0x0a, 0x00, 0x14, 0x00, 0x12, 0x00, 0x1d, 0x00, 0x17, 0x00, 0x18, 0x00,
            0x19, 0x01, 0x00, 0x01, 0x01, 0x01, 0x02, 0x01, 0x03, 0x01, 0x04, 0x00, 0x33, 0x00,
            0x26, 0x00, 0x24, 0x00, 0x1d, 0x00, 0x20, 0xe4, 0xff, 0xb6, 0x8a, 0xc0, 0x5f, 0x8d,
            0x96, 0xc9, 0x9d, 0xa2, 0x66, 0x98, 0x34, 0x6c, 0x6b, 0xe1, 0x64, 0x82, 0xba, 0xdd,
            0xda, 0xfe, 0x05, 0x1a, 0x66, 0xb4, 0xf1, 0x8d, 0x66, 0x8f, 0x0b, 0x00, 0x2a, 0x00,
            0x00, 0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x04, 0x00, 0x0d, 0x00, 0x20, 0x00, 0x1e,
            0x04, 0x03, 0x05, 0x03, 0x06, 0x03, 0x02, 0x03, 0x08, 0x04, 0x08, 0x05, 0x08, 0x06,
            0x04, 0x01, 0x05, 0x01, 0x06, 0x01, 0x02, 0x01, 0x04, 0x02, 0x05, 0x02, 0x06, 0x02,
            0x02, 0x02, 0x00, 0x2d, 0x00, 0x02, 0x01, 0x01, 0x00, 0x1c, 0x00, 0x02, 0x40, 0x01,
            0x00, 0x15, 0x00, 0x57, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x29, 0x00, 0xdd, 0x00, 0xb8, 0x00,
            0xb2, 0x2c, 0x03, 0x5d, 0x82, 0x93, 0x59, 0xee, 0x5f, 0xf7, 0xaf, 0x4e, 0xc9, 0x00,
            0x00, 0x00, 0x00, 0x26, 0x2a, 0x64, 0x94, 0xdc, 0x48, 0x6d, 0x2c, 0x8a, 0x34, 0xcb,
            0x33, 0xfa, 0x90, 0xbf, 0x1b, 0x00, 0x70, 0xad, 0x3c, 0x49, 0x88, 0x83, 0xc9, 0x36,
            0x7c, 0x09, 0xa2, 0xbe, 0x78, 0x5a, 0xbc, 0x55, 0xcd, 0x22, 0x60, 0x97, 0xa3, 0xa9,
            0x82, 0x11, 0x72, 0x83, 0xf8, 0x2a, 0x03, 0xa1, 0x43, 0xef, 0xd3, 0xff, 0x5d, 0xd3,
            0x6d, 0x64, 0xe8, 0x61, 0xbe, 0x7f, 0xd6, 0x1d, 0x28, 0x27, 0xdb, 0x27, 0x9c, 0xce,
            0x14, 0x50, 0x77, 0xd4, 0x54, 0xa3, 0x66, 0x4d, 0x4e, 0x6d, 0xa4, 0xd2, 0x9e, 0xe0,
            0x37, 0x25, 0xa6, 0xa4, 0xda, 0xfc, 0xd0, 0xfc, 0x67, 0xd2, 0xae, 0xa7, 0x05, 0x29,
            0x51, 0x3e, 0x3d, 0xa2, 0x67, 0x7f, 0xa5, 0x90, 0x6c, 0x5b, 0x3f, 0x7d, 0x8f, 0x92,
            0xf2, 0x28, 0xbd, 0xa4, 0x0d, 0xda, 0x72, 0x14, 0x70, 0xf9, 0xfb, 0xf2, 0x97, 0xb5,
            0xae, 0xa6, 0x17, 0x64, 0x6f, 0xac, 0x5c, 0x03, 0x27, 0x2e, 0x97, 0x07, 0x27, 0xc6,
            0x21, 0xa7, 0x91, 0x41, 0xef, 0x5f, 0x7d, 0xe6, 0x50, 0x5e, 0x5b, 0xfb, 0xc3, 0x88,
            0xe9, 0x33, 0x43, 0x69, 0x40, 0x93, 0x93, 0x4a, 0xe4, 0xd3, 0x57, 0xfa, 0xd6, 0xaa,
            0xcb,
        ];
        // 票据对应的 PSK（RFC 标注为 "IKM"）。
        const PSK: [u8; 32] = [
            0x4e, 0xcd, 0x0e, 0xb6, 0xec, 0x3b, 0x4d, 0x87, 0xf5, 0xd6, 0x02, 0x8f, 0x92, 0x2c,
            0xa4, 0xc5, 0x85, 0x1a, 0x27, 0x7f, 0xd4, 0x13, 0x11, 0xc9, 0xe6, 0x2d, 0x2c, 0x94,
            0x92, 0xe1, 0xc4, 0xf3,
        ];
        let binder =
            KeySchedule::psk_binder(CipherSuite::Aes128GcmSha256, &PSK, TRUNCATED_CLIENT_HELLO)
                .expect("计算 binder");
        assert_eq!(
            hex(&binder),
            "3add4fb2d8fdf822a0ca3cf7678ef5e88dae990141c5924d57bb6fa31b9e5f9d",
            "binder 必须与 RFC 8448 一致"
        );
    }

    /// PSK 会改变 `early_secret`，这是会话恢复的地基。
    #[test]
    fn psk_changes_the_early_secret() {
        let suite = CipherSuite::Aes128GcmSha256;
        let without = KeySchedule::with_psk(suite, None);
        let with = KeySchedule::with_psk(suite, Some(&[0x01; 32]));
        assert_ne!(without.early_secret(), with.early_secret());
        assert!(!without.is_resumption());
        assert!(with.is_resumption());
    }

    /// 没有 PSK 时必须与旧行为逐字节一致（回归保护）。
    #[test]
    fn the_no_psk_path_still_matches_the_rfc_early_secret() {
        let suite = CipherSuite::Aes128GcmSha256;
        let zeros = vec![0u8; 32];
        assert_eq!(
            KeySchedule::new(suite).early_secret(),
            extract(suite, &zeros, &zeros).as_slice()
        );
    }

    /// binder 必须对「截断到 binders 之前」的 CH 计算：多一个字节就完全不同。
    #[test]
    fn psk_binder_depends_on_the_exact_truncated_client_hello() {
        let suite = CipherSuite::Aes128GcmSha256;
        let psk = [0x42u8; 32];
        let hello = b"\x01\x00\x00\x2fclienthello".as_slice();
        let first = KeySchedule::psk_binder(suite, &psk, hello).expect("binder");
        // 确定性：同样的输入必须得到同样的值。
        assert_eq!(
            first,
            KeySchedule::psk_binder(suite, &psk, hello).expect("binder")
        );
        assert_eq!(first.len(), 32, "binder 长度等于哈希长度");
        // 转录里多一个字节就不同。
        let mut longer = hello.to_vec();
        longer.push(0x00);
        assert_ne!(
            first,
            KeySchedule::psk_binder(suite, &psk, &longer).expect("binder")
        );
        // 换 PSK 也必须不同。
        assert_ne!(
            first,
            KeySchedule::psk_binder(suite, &[0x43u8; 32], hello).expect("binder")
        );
    }

    /// `resumption_psk` 按票据 nonce 派生：nonce 不同则 PSK 不同。
    #[test]
    fn resumption_psk_is_bound_to_the_ticket_nonce() {
        let suite = CipherSuite::Aes128GcmSha256;
        let master = [0x11u8; 32];
        let first = KeySchedule::resumption_psk(suite, &master, b"nonce-a").expect("psk");
        let second = KeySchedule::resumption_psk(suite, &master, b"nonce-b").expect("psk");
        assert_eq!(first.len(), 32);
        assert_ne!(first, second, "nonce 必须参与派生");
        assert_eq!(
            first,
            KeySchedule::resumption_psk(suite, &master, b"nonce-a").expect("psk")
        );
    }

    /// `res master` 的上下文是 client Finished 之后的转录哈希。
    #[test]
    fn resumption_master_secret_uses_the_client_finished_transcript() {
        let suite = CipherSuite::Aes128GcmSha256;
        let master = [0x22u8; 32];
        let first =
            KeySchedule::resumption_master_secret(suite, &master, &[0xaa; 32]).expect("res master");
        let second =
            KeySchedule::resumption_master_secret(suite, &master, &[0xbb; 32]).expect("res master");
        assert_ne!(first, second);
    }

    /// 应用密钥必须把 `master_secret` 带出来，否则派生不出 `res master`。
    #[test]
    fn application_secrets_expose_the_master_secret() {
        let suite = CipherSuite::Aes128GcmSha256;
        let schedule = KeySchedule::new(suite);
        let shared = [0x33u8; 32];
        let mut transcript = digest::Context::new(suite.hash());
        transcript.update(b"ch..sh");
        let handshake = schedule
            .handshake_secrets(&shared, transcript.finish().as_ref())
            .expect("握手密钥");
        let mut after = digest::Context::new(suite.hash());
        after.update(b"..server finished");
        let application = handshake
            .application_secrets(after.finish().as_ref())
            .expect("应用密钥");
        assert_eq!(application.master_secret.len(), 32);
        assert!(KeySchedule::resumption_master_secret(
            suite,
            &application.master_secret,
            &[0xcc; 32]
        )
        .is_ok());
    }

    #[test]
    fn cipher_suites_map_to_codes_and_algorithms() {
        for (code, suite) in [
            (0x1301, CipherSuite::Aes128GcmSha256),
            (0x1302, CipherSuite::Aes256GcmSha384),
            (0x1303, CipherSuite::ChaCha20Poly1305Sha256),
        ] {
            assert_eq!(CipherSuite::from_code(code), Some(suite));
            assert_eq!(suite.code(), code);
        }
        assert_eq!(CipherSuite::from_code(0xc02b), None, "1.2 套件不是 1.3 的");
        assert_eq!(CipherSuite::Aes128GcmSha256.key_len(), 16);
        assert_eq!(CipherSuite::Aes256GcmSha384.key_len(), 32);
        assert_eq!(CipherSuite::Aes128GcmSha256.hash().output_len(), 32);
        assert_eq!(CipherSuite::Aes256GcmSha384.hash().output_len(), 48);
    }

    /// RFC 8446 §7.1 的第一个派生值：`early_secret = HKDF-Extract(0, 0)`。
    #[test]
    fn early_secret_matches_the_rfc_example() {
        let suite = CipherSuite::Aes128GcmSha256;
        let zeros = vec![0u8; 32];
        let early = extract(suite, &zeros, &zeros);
        assert_eq!(
            hex(&early),
            "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a"
        );
    }

    #[test]
    fn expand_label_encodes_length_label_and_context() {
        let suite = CipherSuite::Aes128GcmSha256;
        let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), &[1u8; 32]);
        let mut first = [0u8; 16];
        let mut second = [0u8; 16];
        expand_label(suite, &prk, "key", &[], &mut first).expect("扩展");
        expand_label(suite, &prk, "iv", &[], &mut second).expect("扩展");
        assert_ne!(first, second, "不同标签必须得到不同输出");

        // 相同输入必须可复现（HKDF 是确定性的）。
        let mut again = [0u8; 16];
        expand_label(suite, &prk, "key", &[], &mut again).expect("扩展");
        assert_eq!(first, again);
    }

    #[test]
    fn label_info_has_the_specified_layout() {
        let info = label_info("key", &[0xaa], 16).expect("应当构造");
        assert_eq!(&info[0..2], &[0x00, 0x10], "输出长度 16");
        assert_eq!(info[2], 9, "标签长度覆盖 \"tls13 \" 前缀：6 + 3");
        assert_eq!(&info[3..12], b"tls13 key");
        assert_eq!(info[12], 1, "上下文长度");
        assert_eq!(info[13], 0xaa);
        assert_eq!(info.len(), 14);
    }

    #[test]
    fn label_info_matches_the_rfc_8448_derived_vector() {
        // RFC 8448 §3 给出 `tls13 derived` 的完整 info（49 字节）。
        // 这个断言的价值在于它是外部权威值：之前少了标签长度字节，
        // 自家测试却断言了错误布局并通过。
        let empty_hash =
            from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        let info = label_info("derived", &empty_hash, 32).expect("构造");
        assert_eq!(
            hex(&info),
            "00200d746c733133206465726976656420e3b0c44298fc1c149afbf4c8996fb924\
             27ae41e4649b934ca495991b7852b855"
                .replace([' ', '\n'], ""),
            "info 必须与 RFC 8448 逐字节一致"
        );
        assert_eq!(info.len(), 49, "RFC 给出的长度是 49");
    }

    fn from_hex(text: &str) -> Vec<u8> {
        let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        cleaned
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex")
            })
            .collect()
    }

    #[test]
    fn derived_secret_is_sensitive_to_the_transcript_hash() {
        let suite = CipherSuite::Aes128GcmSha256;
        let secret = [2u8; 32];
        let first = derive_secret(suite, &secret, "c hs traffic", &[0xaa; 32]).expect("派生");
        let second = derive_secret(suite, &secret, "c hs traffic", &[0xbb; 32]).expect("派生");
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
    }

    #[test]
    fn transcript_is_snapshottable_without_losing_state() {
        let mut transcript = Transcript::new(CipherSuite::Aes128GcmSha256);
        transcript.push(b"first");
        let after_first = transcript.hash();
        transcript.push(b"second");
        let after_second = transcript.hash();
        assert_ne!(after_first, after_second);
        assert_eq!(transcript.hash(), after_second, "取快照不应破坏累积");
        assert_eq!(after_first.len(), 32);
    }

    #[test]
    fn transcript_hash_matches_a_direct_digest() {
        let mut transcript = Transcript::new(CipherSuite::Aes128GcmSha256);
        transcript.push(b"abc");
        let direct = digest::digest(&digest::SHA256, b"abc");
        assert_eq!(transcript.hash(), direct.as_ref());
    }

    #[test]
    fn traffic_keys_have_the_right_lengths() {
        let suite = CipherSuite::Aes256GcmSha384;
        let keys = traffic_keys(suite, &[3u8; 48]).expect("派生");
        assert_eq!(keys.key.len(), 32, "AES-256 密钥 32 字节");
        assert_eq!(keys.iv.len(), 12, "IV 固定 12 字节");
        assert_eq!(keys.suite, suite);
    }

    #[test]
    fn handshake_and_application_secrets_differ() {
        let suite = CipherSuite::Aes128GcmSha256;
        let schedule = KeySchedule::new(suite);
        let shared = [0x42u8; 32];
        let transcript_hash = [0x11u8; 32];
        let handshake = schedule
            .handshake_secrets(&shared, &transcript_hash)
            .expect("握手密钥");
        assert_ne!(
            handshake.client.key, handshake.server.key,
            "两个方向必须不同"
        );

        let application = handshake
            .application_secrets(&transcript_hash)
            .expect("应用密钥");
        assert_ne!(application.client.key, handshake.client.key);
        assert_ne!(application.client.key, application.server.key);
        assert_eq!(application.exporter_master.key.len(), 16);
    }

    #[test]
    fn different_shared_secrets_give_different_handshake_keys() {
        let suite = CipherSuite::Aes128GcmSha256;
        let schedule = KeySchedule::new(suite);
        let hash = [0x11u8; 32];
        let first = schedule
            .handshake_secrets(&[0x01; 32], &hash)
            .expect("密钥");
        let second = schedule
            .handshake_secrets(&[0x02; 32], &hash)
            .expect("密钥");
        assert_ne!(
            first.client.key, second.client.key,
            "ECDHE 结果必须影响密钥"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
