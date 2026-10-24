//! TLS 1.2 客户端侧：PRF、密钥块、ECDHE 的 ServerKeyExchange 解析、AEAD 记录层。
//!
//! 为什么要做这一层：真实世界仍有大量只支持 1.2 的站点（百度、QQ 等），而本 crate 的
//! ClientHello 本来就声明了 1.2（Chromium 档案的 `supported_versions` 与
//! `legacy_version` 都必须如此，否则 JA3/JA4 与真浏览器对不上）。缺的只是服务端选 1.2
//! 之后的整条通路。
//!
//! 刻意缩小范围，只做「现代 1.2」：
//!
//! - 只支持 **ECDHE + AEAD**：`ECDHE_{ECDSA,RSA}_AES_{128,256}_GCM` 与
//!   `ECDHE_{ECDSA,RSA}_CHACHA20_POLY1305`。CBC 套件与 RSA 密钥传输（需要客户端持有
//!   服务端公钥对应的私钥）一律明确拒绝——前者要把 MAC-then-Encrypt 的填充、隐式 IV
//!   与常量时间比较全部写对，后者在无客户端私钥时根本走不通。
//! - 不做会话恢复、不做重协商、不做客户端认证。收到 `CertificateRequest` 直接报错，
//!   而不是假装没看见然后卡在等 `ServerHelloDone`。
//! - 支持 extended master secret（RFC 7627）：档案里每个族都发了
//!   `extended_master_secret`，服务端会回带它。此时 master secret 必须用
//!   **session_hash** 而不是两个 random 派生，漏掉这一步会让 Finished 校验失败。

use ring::{aead, digest, hmac};

use crate::client_hello::ext;
use crate::record::{ContentType, Record, TAG_LEN};

/// TLS 1.2 的记录层版本号（也是 `legacy_version`）。
pub const TLS12_VERSION: u16 = 0x0303;

/// Finished 的 verify_data 长度（RFC 5246 §7.4.9 固定 12 字节）。
pub const FINISHED_LEN: usize = 12;

/// master secret 长度（固定 48 字节）。
pub const MASTER_LEN: usize = 48;

/// GCM 的 `nonce_explicit` 长度（RFC 5288 §3）。
const GCM_EXPLICIT_LEN: usize = 8;

/// 1.2 的 AEAD 套件（本项目支持的子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite12 {
    EcdheEcdsaAes128GcmSha256,
    EcdheRsaAes128GcmSha256,
    EcdheEcdsaAes256GcmSha384,
    EcdheRsaAes256GcmSha384,
    EcdheEcdsaChacha20Sha256,
    EcdheRsaChacha20Sha256,
}

impl Suite12 {
    pub fn from_code(code: u16) -> Option<Self> {
        match code {
            0xc02b => Some(Self::EcdheEcdsaAes128GcmSha256),
            0xc02f => Some(Self::EcdheRsaAes128GcmSha256),
            0xc02c => Some(Self::EcdheEcdsaAes256GcmSha384),
            0xc030 => Some(Self::EcdheRsaAes256GcmSha384),
            0xcca9 => Some(Self::EcdheEcdsaChacha20Sha256),
            0xcca8 => Some(Self::EcdheRsaChacha20Sha256),
            _ => None,
        }
    }

    pub fn code(&self) -> u16 {
        match self {
            Self::EcdheEcdsaAes128GcmSha256 => 0xc02b,
            Self::EcdheRsaAes128GcmSha256 => 0xc02f,
            Self::EcdheEcdsaAes256GcmSha384 => 0xc02c,
            Self::EcdheRsaAes256GcmSha384 => 0xc030,
            Self::EcdheEcdsaChacha20Sha256 => 0xcca9,
            Self::EcdheRsaChacha20Sha256 => 0xcca8,
        }
    }

    pub fn aead(&self) -> &'static aead::Algorithm {
        match self {
            Self::EcdheEcdsaAes128GcmSha256 | Self::EcdheRsaAes128GcmSha256 => &aead::AES_128_GCM,
            Self::EcdheEcdsaAes256GcmSha384 | Self::EcdheRsaAes256GcmSha384 => &aead::AES_256_GCM,
            Self::EcdheEcdsaChacha20Sha256 | Self::EcdheRsaChacha20Sha256 => {
                &aead::CHACHA20_POLY1305
            }
        }
    }

    /// 记录层密钥长度（字节）。
    pub fn key_len(&self) -> usize {
        match self {
            Self::EcdheEcdsaAes128GcmSha256 | Self::EcdheRsaAes128GcmSha256 => 16,
            _ => 32,
        }
    }

    /// `key_block` 里 `*_write_IV` 的长度。
    ///
    /// GCM 只要 4 字节固定部分（另 8 字节是每条记录自带、随记录发送的 explicit nonce）；
    /// ChaCha20-Poly1305（RFC 7905）要 12 字节，且不发 explicit nonce。
    pub fn fixed_iv_len(&self) -> usize {
        match self {
            Self::EcdheEcdsaAes128GcmSha256
            | Self::EcdheRsaAes128GcmSha256
            | Self::EcdheEcdsaAes256GcmSha384
            | Self::EcdheRsaAes256GcmSha384 => 4,
            Self::EcdheEcdsaChacha20Sha256 | Self::EcdheRsaChacha20Sha256 => 12,
        }
    }

    /// 是否把 explicit nonce 写进记录（GCM 写，ChaCha 不写）。
    pub fn has_explicit_nonce(&self) -> bool {
        self.fixed_iv_len() == 4
    }

    pub fn prf_hash(&self) -> PrfHash {
        match self {
            Self::EcdheEcdsaAes256GcmSha384 | Self::EcdheRsaAes256GcmSha384 => PrfHash::Sha384,
            _ => PrfHash::Sha256,
        }
    }
}

/// 1.2 的 PRF 用哪个哈希。
///
/// 注意它与「记录层 AEAD」是两个独立的选择：`0xc02c` 用 AES-256-GCM，PRF 就是 SHA-384。
/// 这也是 1.2 比 1.3 容易写错的地方——密钥块、Finished、EMS 的 session_hash 全都要按
/// 这个哈希算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrfHash {
    Sha256,
    Sha384,
}

impl PrfHash {
    pub fn digest(&self) -> &'static digest::Algorithm {
        match self {
            Self::Sha256 => &digest::SHA256,
            Self::Sha384 => &digest::SHA384,
        }
    }

    pub fn hmac(&self) -> hmac::Algorithm {
        match self {
            Self::Sha256 => hmac::HMAC_SHA256,
            Self::Sha384 => hmac::HMAC_SHA384,
        }
    }

    pub fn output_len(&self) -> usize {
        self.digest().output_len()
    }
}

/// TLS 1.2 的 P_hash：`A(0)=seed`，`A(i)=HMAC(secret, A(i-1))`，逐块 `HMAC(secret, A(i)+seed)`。
pub fn prf(hash: PrfHash, secret: &[u8], label: &str, seed: &[u8], out: &mut [u8]) {
    if out.is_empty() {
        return;
    }
    let key = hmac::Key::new(hash.hmac(), secret);
    let mut label_seed = Vec::with_capacity(label.len() + seed.len());
    label_seed.extend_from_slice(label.as_bytes());
    label_seed.extend_from_slice(seed);

    let mut a = hmac::sign(&key, &label_seed).as_ref().to_vec();
    let mut written = 0;
    while written < out.len() {
        let mut input = Vec::with_capacity(a.len() + label_seed.len());
        input.extend_from_slice(&a);
        input.extend_from_slice(&label_seed);
        let block = hmac::sign(&key, &input);
        let block = block.as_ref();
        let take = (out.len() - written).min(block.len());
        out[written..written + take].copy_from_slice(&block[..take]);
        written += take;
        a = hmac::sign(&key, &a).as_ref().to_vec();
    }
}

/// 1.2 的转录哈希。与 1.3 的 [`crate::keyschedule::Transcript`] 不共用：1.2 只需要
/// 「到目前为止」的摘要，不需要按协商套件中途重建。
#[derive(Clone)]
pub struct Transcript12 {
    state: digest::Context,
    hash: PrfHash,
}

impl Transcript12 {
    pub fn new(hash: PrfHash) -> Self {
        Self {
            state: digest::Context::new(hash.digest()),
            hash,
        }
    }

    /// 用一个已累积全部握手原文的缓冲重建（与「逐条 push」等价）。
    pub fn restarted(hash: PrfHash, messages: &[u8]) -> Self {
        let mut transcript = Self::new(hash);
        transcript.push(messages);
        transcript
    }

    pub fn push(&mut self, data: &[u8]) {
        self.state.update(data);
    }

    pub fn hash(&self) -> Vec<u8> {
        self.state.clone().finish().as_ref().to_vec()
    }

    pub fn prf_hash(&self) -> PrfHash {
        self.hash
    }
}

/// `master_secret = PRF(pre_master_secret, "master secret", client_random + server_random)`。
pub fn master_secret(
    hash: PrfHash,
    pre_master: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
) -> [u8; MASTER_LEN] {
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(client_random);
    seed.extend_from_slice(server_random);
    let mut master = [0u8; MASTER_LEN];
    prf(hash, pre_master, "master secret", &seed, &mut master);
    master
}

/// RFC 7627 的 extended master secret：
/// `PRF(pre_master_secret, "extended master secret", session_hash)`。
///
/// 服务端回带 `extended_master_secret` 时**必须**走这条；用两个 random 派生会被
/// Finished 校验挡下，表现为「密钥全错」而不是「扩展没实现」。
pub fn extended_master_secret(
    hash: PrfHash,
    pre_master: &[u8],
    session_hash: &[u8],
) -> [u8; MASTER_LEN] {
    let mut master = [0u8; MASTER_LEN];
    prf(
        hash,
        pre_master,
        "extended master secret",
        session_hash,
        &mut master,
    );
    master
}

/// 一个方向的记录密钥与固定 IV。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficKeys12 {
    pub key: Vec<u8>,
    pub fixed_iv: Vec<u8>,
}

/// `key_block` 切出来的四个值（AEAD 套件没有 MAC key）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBlock12 {
    pub client: TrafficKeys12,
    pub server: TrafficKeys12,
}

/// `key_block = PRF(master_secret, "key expansion", server_random + client_random)`。
///
/// AEAD 套件的字段顺序是 **client_write_key、server_write_key、client_write_IV、
/// server_write_IV**（RFC 5288 §3）——四个字段按「先两个 key、再两个 IV」排，
/// 不是按「每个方向一组」。按方向交替切会让两侧各自用错 IV，症状是服务端回
/// `bad_record_mac`。
pub fn key_block(
    hash: PrfHash,
    suite: Suite12,
    master: &[u8],
    client_random: &[u8; 32],
    server_random: &[u8; 32],
) -> KeyBlock12 {
    let key_len = suite.key_len();
    let iv_len = suite.fixed_iv_len();
    let mut seed = Vec::with_capacity(64);
    seed.extend_from_slice(server_random);
    seed.extend_from_slice(client_random);
    let mut block = vec![0u8; key_len * 2 + iv_len * 2];
    prf(hash, master, "key expansion", &seed, &mut block);

    let mut offset = 0;
    let mut take = |width: usize| {
        let slice = block[offset..offset + width].to_vec();
        offset += width;
        slice
    };
    let client_key = take(key_len);
    let server_key = take(key_len);
    let client_iv = take(iv_len);
    let server_iv = take(iv_len);
    KeyBlock12 {
        client: TrafficKeys12 {
            key: client_key,
            fixed_iv: client_iv,
        },
        server: TrafficKeys12 {
            key: server_key,
            fixed_iv: server_iv,
        },
    }
}

/// Finished 的 verify_data（RFC 5246 §7.4.9）：
/// `PRF(master_secret, finished_label, Hash(handshake_messages))[0..12]`。
pub fn finished_verify_data(
    hash: PrfHash,
    master: &[u8],
    label: &str,
    transcript_hash: &[u8],
) -> Vec<u8> {
    let mut out = vec![0u8; FINISHED_LEN];
    prf(hash, master, label, transcript_hash, &mut out);
    out
}

/// 1.2 的 AEAD 记录层。
///
/// 与 1.3 的三个差异都要照顾到：
/// - 记录头里的类型是**真实**类型（1.3 一律是 23，真类型藏在加密体末尾）；
/// - AAD 是 `seq || type || 0x0303 || length`，其中 seq 是**每方向独立**的 64 位计数器；
/// - GCM 的 nonce 由 4 字节固定 IV 加 8 字节随记录发送的 explicit nonce 拼成。
pub struct RecordKey12 {
    key: aead::LessSafeKey,
    fixed_iv: Vec<u8>,
    sequence: u64,
    explicit_nonce: bool,
}

impl RecordKey12 {
    pub fn new(suite: Suite12, keys: &TrafficKeys12) -> Result<Self, String> {
        let key = aead::UnboundKey::new(suite.aead(), &keys.key)
            .map_err(|_| "1.2 记录密钥长度与所选算法不符".to_string())?;
        Ok(Self {
            key: aead::LessSafeKey::new(key),
            fixed_iv: keys.fixed_iv.clone(),
            sequence: 0,
            explicit_nonce: suite.has_explicit_nonce(),
        })
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn aad(&self, kind: ContentType, plaintext_len: usize) -> [u8; 13] {
        let mut aad = [0u8; 13];
        aad[..8].copy_from_slice(&self.sequence.to_be_bytes());
        aad[8] = kind as u8;
        aad[9] = 0x03;
        aad[10] = TLS12_VERSION as u8;
        aad[11] = (plaintext_len >> 8) as u8;
        aad[12] = plaintext_len as u8;
        aad
    }

    fn nonce(&self, explicit: &[u8]) -> Result<aead::Nonce, String> {
        let mut nonce = [0u8; 12];
        if self.explicit_nonce {
            if self.fixed_iv.len() != 4 || explicit.len() != GCM_EXPLICIT_LEN {
                return Err("GCM 的 IV 应为 4 字节固定部分加 8 字节显式部分".to_string());
            }
            nonce[..4].copy_from_slice(&self.fixed_iv);
            nonce[4..].copy_from_slice(explicit);
        } else {
            // RFC 7905 §2：nonce = fixed_iv XOR（左补零到 12 字节的 seq）。
            if self.fixed_iv.len() != 12 {
                return Err("ChaCha20-Poly1305 的 IV 应为 12 字节".to_string());
            }
            nonce.copy_from_slice(&self.fixed_iv);
            let sequence = self.sequence.to_be_bytes();
            for (index, byte) in sequence.iter().enumerate() {
                nonce[4 + index] ^= byte;
            }
        }
        Ok(aead::Nonce::assume_unique_for_key(nonce))
    }

    pub fn seal(&mut self, kind: ContentType, plaintext: &[u8]) -> Result<Record, String> {
        let aad = self.aad(kind, plaintext.len());
        let mut body = plaintext.to_vec();
        let nonce = if self.explicit_nonce {
            self.nonce(&self.sequence.to_be_bytes())?
        } else {
            self.nonce(&[])?
        };
        self.key
            .seal_in_place_append_tag(nonce, aead::Aad::from(&aad), &mut body)
            .map_err(|_| "1.2 记录加密失败".to_string())?;
        self.sequence += 1;

        let payload = if self.explicit_nonce {
            let mut out = Vec::with_capacity(GCM_EXPLICIT_LEN + body.len());
            out.extend_from_slice(&(self.sequence - 1).to_be_bytes());
            out.extend_from_slice(&body);
            out
        } else {
            body
        };
        Ok(Record { kind, payload })
    }

    pub fn open(&mut self, record: &Record) -> Result<Vec<u8>, String> {
        let (explicit, ciphertext) = if self.explicit_nonce {
            if record.payload.len() < GCM_EXPLICIT_LEN + TAG_LEN {
                return Err(format!(
                    "1.2 加密载荷 {} 字节不足以容纳显式 nonce 与 tag",
                    record.payload.len()
                ));
            }
            record.payload.split_at(GCM_EXPLICIT_LEN)
        } else {
            if record.payload.len() < TAG_LEN {
                return Err(format!(
                    "1.2 加密载荷 {} 字节不足以容纳 tag",
                    record.payload.len()
                ));
            }
            (&[][..], &record.payload[..])
        };
        let plaintext_len = ciphertext.len() - TAG_LEN;
        let aad = self.aad(record.kind, plaintext_len);
        let nonce = if self.explicit_nonce {
            self.nonce(explicit)?
        } else {
            self.nonce(&[])?
        };
        let mut in_out = ciphertext.to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, aead::Aad::from(&aad), &mut in_out)
            .map_err(|_| "1.2 记录解密失败（密钥、序号或附加数据不匹配）".to_string())?;
        let out = plaintext.to_vec();
        self.sequence += 1;
        Ok(out)
    }
}

/// 1.2 的 `ServerKeyExchange`（ECDHE 形式）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerKeyExchange12 {
    /// `curve_type`：只接受 `named_curve`（3）。
    pub curve_type: u8,
    pub group: u16,
    /// 服务端的 ECDHE 公钥（X25519 是 32 字节，P-256 是 65 字节未压缩点）。
    pub point: Vec<u8>,
    /// `SignatureAndHashAlgorithm` 的两字节（hash, signature），与 1.3 的 scheme 同值。
    pub scheme: u16,
    pub signature: Vec<u8>,
    /// 被签名的那一段原始参数（`curve_params`），验签时要与两个 random 拼接。
    pub params: Vec<u8>,
}

/// 解析 ECDHE 的 `ServerKeyExchange`。
pub fn parse_server_key_exchange(body: &[u8]) -> Result<ServerKeyExchange12, String> {
    if body.len() < 4 {
        return Err("ServerKeyExchange 过短".to_string());
    }
    let curve_type = body[0];
    if curve_type != 0x03 {
        return Err(format!(
            "ServerKeyExchange 的 curve_type 为 {curve_type}，只支持 named_curve（3）"
        ));
    }
    let group = u16::from_be_bytes([body[1], body[2]]);
    let point_len = body[3] as usize;
    if body.len() < 4 + point_len + 2 {
        return Err("ServerKeyExchange 的公钥截断".to_string());
    }
    let point = body[4..4 + point_len].to_vec();
    let params_end = 4 + point_len;
    let scheme = u16::from_be_bytes([body[params_end], body[params_end + 1]]);
    let mut offset = params_end + 2;
    if body.len() < offset + 2 {
        return Err("ServerKeyExchange 的签名长度截断".to_string());
    }
    let signature_len = u16::from_be_bytes([body[offset], body[offset + 1]]) as usize;
    offset += 2;
    if body.len() < offset + signature_len {
        return Err("ServerKeyExchange 的签名截断".to_string());
    }
    Ok(ServerKeyExchange12 {
        curve_type,
        group,
        point,
        scheme,
        signature: body[offset..offset + signature_len].to_vec(),
        params: body[..params_end].to_vec(),
    })
}

/// `ServerKeyExchange` 被签名的原始消息：`client_random || server_random || curve_params`。
pub fn signed_key_exchange_message(
    client_random: &[u8; 32],
    server_random: &[u8; 32],
    params: &[u8],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(64 + params.len());
    message.extend_from_slice(client_random);
    message.extend_from_slice(server_random);
    message.extend_from_slice(params);
    message
}

/// `ClientKeyExchange` 的体：`ECPoint`（1 字节长度 + 公钥）。
pub fn client_key_exchange_body(public_key: &[u8]) -> Result<Vec<u8>, String> {
    if public_key.len() > u8::MAX as usize {
        return Err("客户端公钥过长".to_string());
    }
    let mut body = Vec::with_capacity(1 + public_key.len());
    body.push(public_key.len() as u8);
    body.extend_from_slice(public_key);
    Ok(body)
}

/// 解析 1.2 的 `Certificate` 消息。
///
/// 与 1.3 的结构不同：没有请求上下文字节，每张证书后面也**没有** `CertificateEntry`
/// 扩展块。用 1.3 的解析器读它会把扩展长度当成证书长度，表现为「证书链截断」。
pub fn parse_certificate_chain(body: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if body.len() < 3 {
        return Err("Certificate 过短".to_string());
    }
    let total = ((body[0] as usize) << 16) | ((body[1] as usize) << 8) | body[2] as usize;
    if body.len() < 3 + total {
        return Err("Certificate 链截断".to_string());
    }
    let list = &body[3..3 + total];
    let mut offset = 0;
    let mut chain = Vec::new();
    while offset + 3 <= list.len() {
        let len = ((list[offset] as usize) << 16)
            | ((list[offset + 1] as usize) << 8)
            | list[offset + 2] as usize;
        offset += 3;
        if offset + len > list.len() {
            return Err("Certificate 链中某张证书截断".to_string());
        }
        chain.push(list[offset..offset + len].to_vec());
        offset += len;
    }
    if chain.is_empty() {
        return Err("Certificate 消息里没有证书".to_string());
    }
    Ok(chain)
}

/// ServerHello 的扩展里是否有 `extended_master_secret`。
pub fn has_extended_master_secret(extensions: &[(u16, Vec<u8>)]) -> bool {
    extensions
        .iter()
        .any(|(kind, _)| *kind == ext::EXTENDED_MASTER_SECRET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suites_map_to_codes_and_key_sizes() {
        for (code, key_len, iv_len, hash, explicit) in [
            (0xc02b, 16, 4, PrfHash::Sha256, true),
            (0xc02f, 16, 4, PrfHash::Sha256, true),
            (0xc02c, 32, 4, PrfHash::Sha384, true),
            (0xc030, 32, 4, PrfHash::Sha384, true),
            (0xcca9, 32, 12, PrfHash::Sha256, false),
            (0xcca8, 32, 12, PrfHash::Sha256, false),
        ] {
            let suite = Suite12::from_code(code).expect("应当认识");
            assert_eq!(suite.code(), code);
            assert_eq!(suite.key_len(), key_len, "{code:#06x}");
            assert_eq!(suite.fixed_iv_len(), iv_len, "{code:#06x}");
            assert_eq!(suite.prf_hash(), hash, "{code:#06x}");
            assert_eq!(suite.has_explicit_nonce(), explicit, "{code:#06x}");
        }
        // CBC 与 RSA 密钥传输不在支持范围：静默接受会让握手在更晚的地方以密钥错误失败。
        for code in [0xc013u16, 0xc014, 0x009c, 0x009d, 0x002f, 0x0035] {
            assert_eq!(Suite12::from_code(code), None, "{code:#06x}");
        }
    }

    #[test]
    fn prf_fills_the_whole_output_deterministically() {
        let mut first = [0u8; 96];
        prf(PrfHash::Sha256, b"secret", "label", b"seed", &mut first);
        let mut second = [0u8; 96];
        prf(PrfHash::Sha256, b"secret", "label", b"seed", &mut second);
        assert_eq!(first, second, "PRF 是确定性的");
        assert!(
            first[..32] != first[32..64],
            "不同块必须不同，否则说明 A(i) 没有推进"
        );
        assert!(first.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn prf_output_prefix_is_stable_across_lengths() {
        // 短输出必须是长输出的前缀（P_hash 的块拼接性质）。若把 A(i) 的推进写错，
        // 这条会失败，而自洽的握手测试看不出来。
        let mut long = [0u8; 80];
        prf(PrfHash::Sha256, b"k", "l", b"s", &mut long);
        let mut short = [0u8; 20];
        prf(PrfHash::Sha256, b"k", "l", b"s", &mut short);
        assert_eq!(&long[..20], &short[..]);
    }

    #[test]
    fn sha384_prf_differs_from_sha256() {
        let mut sha256 = [0u8; 48];
        let mut sha384 = [0u8; 48];
        prf(
            PrfHash::Sha256,
            b"secret",
            "master secret",
            b"seed",
            &mut sha256,
        );
        prf(
            PrfHash::Sha384,
            b"secret",
            "master secret",
            b"seed",
            &mut sha384,
        );
        assert_ne!(sha256, sha384);
    }

    #[test]
    fn key_block_has_the_expected_field_order_and_sizes() {
        let suite = Suite12::EcdheRsaAes128GcmSha256;
        let block = key_block(
            suite.prf_hash(),
            suite,
            &[0x11; 48],
            &[0x22; 32],
            &[0x33; 32],
        );
        assert_eq!(block.client.key.len(), 16);
        assert_eq!(block.server.key.len(), 16);
        assert_eq!(block.client.fixed_iv.len(), 4);
        assert_ne!(block.client.key, block.server.key);
        assert_ne!(block.client.fixed_iv, block.server.fixed_iv);

        let suite = Suite12::EcdheRsaAes256GcmSha384;
        let block = key_block(
            suite.prf_hash(),
            suite,
            &[0x11; 48],
            &[0x22; 32],
            &[0x33; 32],
        );
        assert_eq!(block.client.key.len(), 32);
        assert_eq!(block.client.fixed_iv.len(), 4);
    }

    #[test]
    fn gcm_records_round_trip_and_carry_an_explicit_nonce() {
        let suite = Suite12::EcdheRsaAes128GcmSha256;
        let traffic = TrafficKeys12 {
            key: vec![0x42; 16],
            fixed_iv: vec![0x24; 4],
        };
        let mut sender = RecordKey12::new(suite, &traffic).expect("密钥");
        let mut receiver = RecordKey12::new(suite, &traffic).expect("密钥");
        let sealed = sender
            .seal(ContentType::Handshake, b"finished")
            .expect("加密");
        assert_eq!(sealed.kind, ContentType::Handshake, "1.2 保留真实记录类型");
        assert_eq!(sealed.payload.len(), 8 + 8 + 16);
        assert_eq!(
            &sealed.payload[..8],
            &0u64.to_be_bytes(),
            "首条记录的显式 nonce 是 0"
        );
        let plaintext = receiver.open(&sealed).expect("解密");
        assert_eq!(plaintext, b"finished");
        assert_eq!(sender.sequence(), 1);
        assert_eq!(receiver.sequence(), 1);
    }

    #[test]
    fn chacha_records_omit_the_explicit_nonce() {
        let suite = Suite12::EcdheRsaChacha20Sha256;
        let traffic = TrafficKeys12 {
            key: vec![0x42; 32],
            fixed_iv: vec![0x24; 12],
        };
        let mut sender = RecordKey12::new(suite, &traffic).expect("密钥");
        let mut receiver = RecordKey12::new(suite, &traffic).expect("密钥");
        let sealed = sender
            .seal(ContentType::ApplicationData, b"body")
            .expect("加密");
        assert_eq!(sealed.payload.len(), 4 + 16, "没有显式 nonce");
        assert_eq!(receiver.open(&sealed).expect("解密"), b"body");
    }

    #[test]
    fn a_tampered_12_record_fails_authentication() {
        let suite = Suite12::EcdheRsaAes128GcmSha256;
        let traffic = TrafficKeys12 {
            key: vec![0x42; 16],
            fixed_iv: vec![0x24; 4],
        };
        let mut sender = RecordKey12::new(suite, &traffic).expect("密钥");
        let mut receiver = RecordKey12::new(suite, &traffic).expect("密钥");
        let mut sealed = sender
            .seal(ContentType::ApplicationData, b"payload")
            .expect("加密");
        sealed.payload[10] ^= 0xff;
        assert!(receiver.open(&sealed).is_err(), "篡改必须被拒绝");
    }

    #[test]
    fn the_record_type_is_authenticated_in_12() {
        // 1.2 把类型放进 AAD：改掉类型必须导致认证失败，否则「把握手记录伪装成
        // 应用数据」这类攻击就能通过。
        let suite = Suite12::EcdheRsaAes128GcmSha256;
        let traffic = TrafficKeys12 {
            key: vec![0x42; 16],
            fixed_iv: vec![0x24; 4],
        };
        let mut sender = RecordKey12::new(suite, &traffic).expect("密钥");
        let mut receiver = RecordKey12::new(suite, &traffic).expect("密钥");
        let sealed = sender
            .seal(ContentType::Handshake, b"payload")
            .expect("加密");
        let forged = Record {
            kind: ContentType::ApplicationData,
            payload: sealed.payload,
        };
        assert!(receiver.open(&forged).is_err());
    }

    #[test]
    fn server_key_exchange_parses_params_and_signature() {
        let point = vec![0x04; 65];
        let mut body = Vec::new();
        body.push(0x03);
        body.extend_from_slice(&0x0017u16.to_be_bytes());
        body.push(point.len() as u8);
        body.extend_from_slice(&point);
        body.extend_from_slice(&0x0403u16.to_be_bytes());
        body.extend_from_slice(&3u16.to_be_bytes());
        body.extend_from_slice(&[0xaa, 0xbb, 0xcc]);

        let parsed = parse_server_key_exchange(&body).expect("应当解析");
        assert_eq!(parsed.curve_type, 3);
        assert_eq!(parsed.group, 0x0017);
        assert_eq!(parsed.point, point);
        assert_eq!(parsed.scheme, 0x0403);
        assert_eq!(parsed.signature, vec![0xaa, 0xbb, 0xcc]);
        assert_eq!(
            parsed.params.len(),
            4 + point.len(),
            "被签名的部分不含签名算法"
        );
    }

    #[test]
    fn a_truncated_server_key_exchange_is_rejected() {
        assert!(parse_server_key_exchange(&[0x03, 0x00]).is_err());
        assert!(parse_server_key_exchange(&[0x03, 0x00, 0x17, 65, 1, 2]).is_err());
        let mut body = vec![0x03, 0x00, 0x17, 1, 0x04];
        body.extend_from_slice(&0x0403u16.to_be_bytes());
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(&[1, 2]);
        assert!(parse_server_key_exchange(&body).is_err(), "签名长度超界");
        assert!(
            parse_server_key_exchange(&[0x01, 0x00, 0x17, 0]).is_err(),
            "curve_type 必须是 named_curve"
        );
    }

    #[test]
    fn a_12_certificate_chain_has_no_entry_extensions() {
        // 1.2：3 字节总长 + (3 字节证书长 + DER)*，没有 1.3 的 per-entry 扩展。
        let first = vec![0x30, 0x01, 0x02];
        let second = vec![0x30, 0x03];
        let mut list = Vec::new();
        for der in [&first, &second] {
            list.push((der.len() >> 16) as u8);
            list.push((der.len() >> 8) as u8);
            list.push(der.len() as u8);
            list.extend_from_slice(der);
        }
        let mut body = Vec::new();
        body.push((list.len() >> 16) as u8);
        body.push((list.len() >> 8) as u8);
        body.push(list.len() as u8);
        body.extend_from_slice(&list);

        let chain = parse_certificate_chain(&body).expect("应当解析");
        assert_eq!(chain, vec![first, second]);
    }

    #[test]
    fn finished_is_twelve_bytes_and_depends_on_the_label() {
        let client =
            finished_verify_data(PrfHash::Sha256, &[0x11; 48], "client finished", &[0x22; 32]);
        let server =
            finished_verify_data(PrfHash::Sha256, &[0x11; 48], "server finished", &[0x22; 32]);
        assert_eq!(client.len(), FINISHED_LEN);
        assert_ne!(client, server);
    }

    #[test]
    fn extended_master_secret_differs_from_the_classic_derivation() {
        let hash = PrfHash::Sha256;
        let classic = master_secret(hash, &[0x01; 32], &[0x02; 32], &[0x03; 32]);
        let extended = extended_master_secret(hash, &[0x01; 32], &[0x04; 32]);
        assert_eq!(classic.len(), MASTER_LEN);
        assert_ne!(classic, extended);
    }
}
