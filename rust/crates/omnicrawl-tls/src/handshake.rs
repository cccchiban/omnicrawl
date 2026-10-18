//! 密钥交换与握手消息解析：把「收到 ServerHello」推进到「已算出握手密钥」。
//!
//! 支持的组只有 X25519：四个浏览器档案的首选都是它，且 `ring` 提供已审计实现。
//! 刻意不支持 P-256/384——需要时再加，现在加只会扩大未验证的面积。
//!
//! 这一层的职责边界：解析 ServerHello 与 EncryptedExtensions、Certificate、
//! CertificateVerify、Finished 这几条消息的结构，并把它们按 1.3 的规则推进转录哈希。
//! 证书链的信任判定不在这里（交给 `rustls-webpki`）。

use ring::agreement;

use crate::client_hello::ext;
use crate::wire::LengthWidth;

/// 密钥交换错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyExchangeError {
    /// 对端选的组不受支持。
    UnsupportedGroup(u16),
    /// 公钥长度不对。
    BadKeyLength { expected: usize, actual: usize },
    /// 密钥协商失败（`ring` 拒绝了公钥）。
    AgreementFailed,
}

impl std::fmt::Display for KeyExchangeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedGroup(value) => {
                write!(
                    formatter,
                    "对端选择的组 {value:#06x} 不受支持（仅支持 X25519）"
                )
            }
            Self::BadKeyLength { expected, actual } => {
                write!(formatter, "公钥长度应为 {expected} 字节，实际 {actual}")
            }
            Self::AgreementFailed => write!(formatter, "X25519 协商失败（公钥不合法）"),
        }
    }
}

impl std::error::Error for KeyExchangeError {}

/// 一次 X25519 密钥交换的客户端侧状态。
pub struct X25519Exchange {
    private: agreement::EphemeralPrivateKey,
    /// 待放进 `key_share` 扩展的公钥（32 字节）。
    pub public_key: Vec<u8>,
}

impl X25519Exchange {
    /// 用调用方给的随机源生成密钥对。
    ///
    /// 随机源由外部提供而不是内置 `SystemRandom`：这一层的测试要可复现，
    /// 而生产调用方本来就该自己决定随机源（并保证其质量）。
    pub fn generate(rng: &dyn ring::rand::SecureRandom) -> Result<Self, KeyExchangeError> {
        let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng)
            .map_err(|_| KeyExchangeError::AgreementFailed)?;
        let public_key = private
            .compute_public_key()
            .map_err(|_| KeyExchangeError::AgreementFailed)?
            .as_ref()
            .to_vec();
        Ok(Self {
            private,
            public_key,
        })
    }

    /// 用对端的公钥算出共享密钥（`ECDHE` 的结果，作为 `HKDF-Extract` 的 IKM）。
    pub fn agree(self, peer_public: &[u8]) -> Result<Vec<u8>, KeyExchangeError> {
        if peer_public.len() != 32 {
            return Err(KeyExchangeError::BadKeyLength {
                expected: 32,
                actual: peer_public.len(),
            });
        }
        let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, peer_public);
        agreement::agree_ephemeral(self.private, &peer, |material| material.to_vec())
            .map_err(|_| KeyExchangeError::AgreementFailed)
    }
}

/// 解析后的 ServerHello 关键字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub legacy_version: u16,
    pub random: [u8; 32],
    pub session_id: Vec<u8>,
    pub cipher_suite: u16,
    /// 对端选的密钥共享组。
    pub key_share_group: u16,
    /// 对端给的公钥。
    pub key_share: Vec<u8>,
    /// 协商出的 TLS 版本（来自 `supported_versions`）。
    pub selected_version: u16,
}

/// 一个握手消息：类型 + 体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeMessage {
    pub kind: u8,
    pub body: Vec<u8>,
    /// 完整的线上字节（含 4 字节头），转录哈希要用它。
    pub raw: Vec<u8>,
}

/// 握手消息类型。
pub mod hs {
    pub const CLIENT_HELLO: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    pub const ENCRYPTED_EXTENSIONS: u8 = 8;
    pub const CERTIFICATE: u8 = 11;
    pub const CERTIFICATE_VERIFY: u8 = 15;
    pub const FINISHED: u8 = 20;
}

/// 从字节流里切出若干握手消息（不足一条时返回已切出的部分）。
pub fn take_handshake_messages(data: &[u8]) -> (Vec<HandshakeMessage>, usize) {
    let mut messages = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let kind = data[offset];
        let len = ((data[offset + 1] as usize) << 16)
            | ((data[offset + 2] as usize) << 8)
            | data[offset + 3] as usize;
        let end = offset + 4 + len;
        if end > data.len() {
            break;
        }
        messages.push(HandshakeMessage {
            kind,
            body: data[offset + 4..end].to_vec(),
            raw: data[offset..end].to_vec(),
        });
        offset = end;
    }
    (messages, offset)
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.remaining() < count {
            return None;
        }
        let slice = &self.data[self.position..self.position + count];
        self.position += count;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|slice| slice[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2)
            .map(|slice| u16::from_be_bytes([slice[0], slice[1]]))
    }

    fn u24(&mut self) -> Option<usize> {
        self.take(3).map(|slice| {
            ((slice[0] as usize) << 16) | ((slice[1] as usize) << 8) | slice[2] as usize
        })
    }

    fn block(&mut self, width: LengthWidth) -> Option<&'a [u8]> {
        let len = match width {
            LengthWidth::U8 => self.u8()? as usize,
            LengthWidth::U16 => self.u16()? as usize,
            LengthWidth::U24 => self.u24()?,
        };
        self.take(len)
    }
}

/// HelloRetryRequest 的固定 random（RFC 8446 §4.1.3）。
///
/// 这个值就是为了让「HRR 长得像 ServerHello」这件事可被识别出来：兼容性考虑要求
/// 它用 ServerHello 的线上格式，服务端只能靠 random 表明自己是 HRR。
pub const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// 判断一个已解析的 ServerHello 实际上是 HelloRetryRequest。
///
/// HRR 与 ServerHello 结构完全一致，唯一可靠的区分就是 random 等于固定值。
pub fn is_hello_retry_request(hello: &ServerHello) -> bool {
    hello.random == HELLO_RETRY_REQUEST_RANDOM
}

/// 解析 ServerHello 的体。
///
/// 对 HRR 也适用：此时 `key_share` 扩展只含「服务端要求我们改用的组」，
/// 不带公钥，因此这里允许公钥为空，由调用方用 [`is_hello_retry_request`] 区分。
pub fn parse_server_hello(body: &[u8]) -> Result<ServerHello, String> {
    let mut reader = Reader::new(body);
    let legacy_version = reader.u16().ok_or("ServerHello 缺少版本")?;
    let random_slice = reader.take(32).ok_or("ServerHello 缺少 random")?;
    let mut random = [0u8; 32];
    random.copy_from_slice(random_slice);
    let session_id = reader
        .block(LengthWidth::U8)
        .ok_or("ServerHello 会话 ID 截断")?
        .to_vec();
    let cipher_suite = reader.u16().ok_or("ServerHello 缺少密码套件")?;
    let _compression = reader.u8().ok_or("ServerHello 缺少压缩方法")?;
    let extensions_block = reader
        .block(LengthWidth::U16)
        .ok_or("ServerHello 扩展列表截断")?;

    let mut selected_version = legacy_version;
    let mut key_share_group = 0u16;
    let mut key_share = Vec::new();
    let mut extensions = Reader::new(extensions_block);
    while extensions.remaining() >= 4 {
        let kind = extensions.u16().ok_or("扩展类型截断")?;
        let data = extensions.block(LengthWidth::U16).ok_or("扩展数据截断")?;
        match kind {
            ext::SUPPORTED_VERSIONS => {
                if data.len() < 2 {
                    return Err("supported_versions 数据过短".to_string());
                }
                selected_version = u16::from_be_bytes([data[0], data[1]]);
            }
            ext::KEY_SHARE => {
                // ServerHello 里是「组 + 公钥」；HRR 里只有「选中的组」，没有公钥。
                let mut share = Reader::new(data);
                key_share_group = share.u16().ok_or("key_share 缺少组")?;
                // 剩余为空说明这是 HRR 的「只指定组」形式，不是错误。
                if share.remaining() > 0 {
                    key_share = share
                        .block(LengthWidth::U16)
                        .ok_or("key_share 公钥长度截断")?
                        .to_vec();
                }
            }
            _ => {}
        }
    }

    // 组校验交给调用方：HRR 也会给出组但不给公钥，在这里拒绝会把重试路径堵死。
    Ok(ServerHello {
        legacy_version,
        random,
        session_id,
        cipher_suite,
        key_share_group,
        key_share,
        selected_version,
    })
}

/// 解析 EncryptedExtensions：只需确认它存在，并取回扩展类型列表（ALPN 用）。
pub fn parse_encrypted_extensions(body: &[u8]) -> Result<Vec<(u16, Vec<u8>)>, String> {
    let mut reader = Reader::new(body);
    let block = reader
        .block(LengthWidth::U16)
        .ok_or("EncryptedExtensions 扩展列表截断")?;
    let mut extensions = Reader::new(block);
    let mut out = Vec::new();
    while extensions.remaining() >= 4 {
        let kind = extensions.u16().ok_or("扩展类型截断")?;
        let data = extensions
            .block(LengthWidth::U16)
            .ok_or("扩展数据截断")?
            .to_vec();
        out.push((kind, data));
    }
    Ok(out)
}

/// 从 EncryptedExtensions 里取 ALPN 协商结果。
pub fn negotiated_alpn(extensions: &[(u16, Vec<u8>)]) -> Option<String> {
    let data = extensions
        .iter()
        .find(|(kind, _)| *kind == ext::ALPN)
        .map(|(_, data)| data)?;
    let mut reader = Reader::new(data);
    let block = reader.block(LengthWidth::U16)?;
    let mut inner = Reader::new(block);
    let protocol = inner.block(LengthWidth::U8)?;
    String::from_utf8(protocol.to_vec()).ok()
}

/// 解析 Certificate 消息：返回整条证书链（第一张是叶证书）。
///
/// 1.3 里每张证书后面还跟一个 `CertificateEntry` 扩展块，验链用不到，但必须
/// 跳过——否则后续解析会被扩展字节污染，看起来像「证书 truncation」。
pub fn parse_certificate_chain(body: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut reader = Reader::new(body);
    let _context = reader
        .block(LengthWidth::U8)
        .ok_or("Certificate 缺少请求上下文")?;
    let list = reader
        .block(LengthWidth::U24)
        .ok_or("Certificate 证书列表截断")?;
    let mut list_reader = Reader::new(list);
    let mut chain = Vec::new();
    while list_reader.remaining() > 0 {
        let der = list_reader
            .block(LengthWidth::U24)
            .ok_or("Certificate 链中某张证书截断")?;
        let _entry_extensions = list_reader
            .block(LengthWidth::U16)
            .ok_or("Certificate 条目扩展截断")?;
        chain.push(der.to_vec());
    }
    if chain.is_empty() {
        return Err("Certificate 消息里没有证书".to_string());
    }
    Ok(chain)
}

/// 解析 Certificate 消息：只返回叶证书的 DER 字节。
pub fn parse_certificate_leaf(body: &[u8]) -> Result<Vec<u8>, String> {
    parse_certificate_chain(body)?
        .into_iter()
        .next()
        .ok_or_else(|| "Certificate 消息里没有证书".to_string())
}

/// 解析后的 CertificateVerify。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateVerify {
    /// TLS 1.3 的 `SignatureScheme`（如 `0x0403` = ecdsa_secp256r1_sha256）。
    pub scheme: u16,
    /// 签名原始字节（ECDSA 是 DER 编码的 `SEQUENCE`，RSA 是定长模长字节）。
    pub signature: Vec<u8>,
}

/// 解析 CertificateVerify 的体。
pub fn parse_certificate_verify(body: &[u8]) -> Result<CertificateVerify, String> {
    let mut reader = Reader::new(body);
    let scheme = reader.u16().ok_or("CertificateVerify 缺少签名算法")?;
    let signature = reader
        .block(LengthWidth::U16)
        .ok_or("CertificateVerify 签名截断")?;
    if signature.is_empty() {
        return Err("CertificateVerify 的签名为空".to_string());
    }
    Ok(CertificateVerify {
        scheme,
        signature: signature.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_hello::group;

    fn rng() -> ring::rand::SystemRandom {
        ring::rand::SystemRandom::new()
    }

    #[test]
    fn x25519_agreement_is_symmetric() {
        let alice = X25519Exchange::generate(&rng()).expect("生成");
        let bob = X25519Exchange::generate(&rng()).expect("生成");
        assert_eq!(alice.public_key.len(), 32);
        let alice_public = alice.public_key.clone();
        let bob_public = bob.public_key.clone();
        let from_alice = alice.agree(&bob_public).expect("协商");
        let from_bob = bob.agree(&alice_public).expect("协商");
        assert_eq!(from_alice, from_bob, "双方必须得到同一个共享密钥");
        assert_eq!(from_alice.len(), 32);
    }

    #[test]
    fn a_wrong_length_public_key_is_rejected() {
        let exchange = X25519Exchange::generate(&rng()).expect("生成");
        let error = exchange.agree(&[0u8; 31]).expect_err("长度不对");
        assert_eq!(
            error,
            KeyExchangeError::BadKeyLength {
                expected: 32,
                actual: 31
            }
        );
    }

    #[test]
    fn different_peers_give_different_secrets() {
        let alice = X25519Exchange::generate(&rng()).expect("生成");
        let bob = X25519Exchange::generate(&rng()).expect("生成");
        let carol = X25519Exchange::generate(&rng()).expect("生成");
        let bob_public = bob.public_key.clone();
        let carol_public = carol.public_key.clone();
        let with_bob = alice.agree(&bob_public).expect("协商");
        let alice2 = X25519Exchange::generate(&rng()).expect("生成");
        let with_carol = alice2.agree(&carol_public).expect("协商");
        assert_ne!(with_bob, with_carol);
    }

    /// 构造一份最小的 ServerHello 以便测试解析路径。
    fn server_hello_bytes(suite: u16, group_id: u16, key: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&[0x11; 32]);
        body.push(0); // 会话 ID 为空
        body.extend_from_slice(&suite.to_be_bytes());
        body.push(0); // null 压缩

        let mut ext = Vec::new();
        // supported_versions = 0x0304
        ext.extend_from_slice(&ext_kind_len(ext::SUPPORTED_VERSIONS, &[0x03, 0x04]));
        let mut share = Vec::new();
        share.extend_from_slice(&group_id.to_be_bytes());
        share.extend_from_slice(&(key.len() as u16).to_be_bytes());
        share.extend_from_slice(key);
        ext.extend_from_slice(&ext_kind_len(ext::KEY_SHARE, &share));

        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        body
    }

    fn ext_kind_len(kind: u16, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn server_hello_extracts_the_key_share() {
        let body = server_hello_bytes(0x1301, group::X25519, &[0x77; 32]);
        let parsed = parse_server_hello(&body).expect("应当解析成功");
        assert_eq!(parsed.cipher_suite, 0x1301);
        assert_eq!(parsed.selected_version, 0x0304);
        assert_eq!(parsed.key_share_group, group::X25519);
        assert_eq!(parsed.key_share, vec![0x77; 32]);
        assert!(parsed.session_id.is_empty());
    }

    #[test]
    fn a_non_x25519_group_parses_so_the_session_can_report_it() {
        // 组校验已移到会话层：HRR 也会给出组但不带公钥，在解析层拒绝会把重试路径堵死。
        // 这里只保证解析能通过，拒绝的责任在 `ClientHandshake`。
        let body = server_hello_bytes(0x1301, group::SECP256R1, &[0x04; 65]);
        let parsed = parse_server_hello(&body).expect("解析层不拦组");
        assert_eq!(parsed.key_share_group, group::SECP256R1);
        assert_eq!(parsed.key_share.len(), 65);
    }

    #[test]
    fn a_key_share_without_a_public_key_parses_as_a_retry_signal() {
        // HelloRetryRequest 的 key_share 只有组，连公钥长度都不带。
        // `server_hello_bytes` 总会写长度，所以这里手工构造扩展。
        let mut share = Vec::new();
        share.extend_from_slice(&group::X25519.to_be_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&[0x22; 32]);
        body.push(0);
        body.extend_from_slice(&0x1301u16.to_be_bytes());
        body.push(0);
        let extensions = ext_kind_len(ext::KEY_SHARE, &share);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let parsed = parse_server_hello(&body).expect("HRR 形式应能解析");
        assert_eq!(parsed.key_share_group, group::X25519);
        assert!(parsed.key_share.is_empty(), "HRR 不带公钥");
    }

    #[test]
    fn the_hello_retry_request_random_is_recognized() {
        let mut share = Vec::new();
        share.extend_from_slice(&group::X25519.to_be_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&HELLO_RETRY_REQUEST_RANDOM);
        body.push(0);
        body.extend_from_slice(&0x1301u16.to_be_bytes());
        body.push(0);
        let extensions = ext_kind_len(ext::KEY_SHARE, &share);
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);

        let parsed = parse_server_hello(&body).expect("解析");
        assert!(
            is_hello_retry_request(&parsed),
            "固定 random 应被识别为 HRR"
        );

        // 换成普通 random 就不是 HRR。
        let normal = server_hello_bytes(0x1301, group::X25519, &[0x11; 32]);
        let parsed = parse_server_hello(&normal).expect("解析");
        assert!(!is_hello_retry_request(&parsed));
    }

    #[test]
    fn handshake_messages_are_split_on_boundaries() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&handshake_bytes(hs::SERVER_HELLO, &[1, 2]));
        stream.extend_from_slice(&handshake_bytes(hs::ENCRYPTED_EXTENSIONS, &[3]));
        let (messages, consumed) = take_handshake_messages(&stream);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].kind, hs::SERVER_HELLO);
        assert_eq!(messages[0].body, vec![1, 2]);
        assert_eq!(messages[1].kind, hs::ENCRYPTED_EXTENSIONS);
        assert_eq!(consumed, stream.len());
        assert_eq!(messages[0].raw.len(), 4 + 2, "raw 含 4 字节头");
    }

    #[test]
    fn a_partial_handshake_message_is_left_unconsumed() {
        let complete = handshake_bytes(hs::SERVER_HELLO, &[1, 2, 3]);
        let mut stream = complete.clone();
        stream.extend_from_slice(&[hs::FINISHED, 0x00]); // 只有半截
        let (messages, consumed) = take_handshake_messages(&stream);
        assert_eq!(messages.len(), 1);
        assert_eq!(consumed, complete.len(), "半截消息不应被消费");
    }

    fn handshake_bytes(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.push((body.len() >> 16) as u8);
        out.push((body.len() >> 8) as u8);
        out.push(body.len() as u8);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn alpn_is_read_from_encrypted_extensions() {
        let mut alpn_data = Vec::new();
        alpn_data.extend_from_slice(&3u16.to_be_bytes());
        alpn_data.push(2);
        alpn_data.extend_from_slice(b"h2");
        let mut ext = Vec::new();
        ext.extend_from_slice(&ext_kind_len(ext::ALPN, &alpn_data));
        let mut body = Vec::new();
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);

        let extensions = parse_encrypted_extensions(&body).expect("应当解析");
        assert_eq!(negotiated_alpn(&extensions).as_deref(), Some("h2"));
    }

    #[test]
    fn leaf_certificate_is_extracted_from_the_chain() {
        let leaf = vec![0xAA; 10];
        let mut list = Vec::new();
        list.extend_from_slice(&(leaf.len() as u32).to_be_bytes()[1..]);
        list.extend_from_slice(&leaf);
        list.extend_from_slice(&0u16.to_be_bytes()); // 空的证书条目扩展
        let mut body = Vec::new();
        body.push(0); // 请求上下文为空
        body.extend_from_slice(&(list.len() as u32).to_be_bytes()[1..]);
        body.extend_from_slice(&list);

        let parsed = parse_certificate_leaf(&body).expect("应当解析");
        assert_eq!(parsed, leaf);
    }

    #[test]
    fn a_full_chain_is_returned_in_order() {
        let body = certificate_body(&[vec![0x01; 4], vec![0x02; 5], vec![0x03; 6]]);
        let chain = parse_certificate_chain(&body).expect("应当解析");
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], vec![0x01; 4], "第一张必须是叶证书");
        assert_eq!(chain[2], vec![0x03; 6]);
    }

    #[test]
    fn an_empty_certificate_list_is_rejected() {
        let body = certificate_body(&[]);
        assert!(parse_certificate_chain(&body).is_err());
    }

    fn certificate_body(certs: &[Vec<u8>]) -> Vec<u8> {
        let mut list = Vec::new();
        for der in certs {
            list.extend_from_slice(&(der.len() as u32).to_be_bytes()[1..]);
            list.extend_from_slice(der);
            list.extend_from_slice(&0u16.to_be_bytes());
        }
        let mut body = Vec::new();
        body.push(0);
        body.extend_from_slice(&(list.len() as u32).to_be_bytes()[1..]);
        body.extend_from_slice(&list);
        body
    }

    #[test]
    fn certificate_verify_carries_scheme_and_signature() {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0403u16.to_be_bytes());
        body.extend_from_slice(&(4u16).to_be_bytes());
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let parsed = parse_certificate_verify(&body).expect("应当解析");
        assert_eq!(parsed.scheme, 0x0403);
        assert_eq!(parsed.signature, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn an_empty_signature_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&0x0403u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        assert!(parse_certificate_verify(&body).is_err());
        assert!(
            parse_certificate_verify(&[0x04]).is_err(),
            "只有半个 scheme"
        );
    }

    #[test]
    fn truncated_structures_report_errors_instead_of_panicking() {
        assert!(parse_server_hello(&[]).is_err());
        assert!(parse_server_hello(&[0x03, 0x03]).is_err());
        assert!(parse_encrypted_extensions(&[]).is_err());
        assert!(parse_certificate_leaf(&[]).is_err());
        assert!(parse_certificate_chain(&[]).is_err());
        assert!(parse_certificate_verify(&[]).is_err());
        let (messages, consumed) = take_handshake_messages(&[0x02, 0xff, 0xff, 0xff]);
        assert!(messages.is_empty());
        assert_eq!(consumed, 0);
    }
}
