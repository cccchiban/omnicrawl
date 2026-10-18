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

    fn hash_len(&self) -> usize {
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

    /// 按 RFC 8446 §4.4.1 放入 `message_hash`：`1 字节哈希类型 + 1 字节长度 + 哈希值`。
    ///
    /// HelloRetryRequest 之后，转录里的 ClientHello1 必须换成它的哈希，而不是保留原字节。
    /// 保留了会算出不同的转录哈希，服务端的 Finished 就永远对不上——这个错误只在
    /// HRR 路径上出现，普通握手测不出来。
    pub fn push_message_hash(&mut self, hash: &[u8]) {
        self.state.update(&[0x01]);
        self.state.update(&[hash.len() as u8]);
        self.state.update(hash);
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
}

impl KeySchedule {
    pub fn new(suite: CipherSuite) -> Self {
        let zeros = vec![0u8; suite.hash_len()];
        Self {
            suite,
            early_secret: extract(suite, &zeros, &zeros),
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
