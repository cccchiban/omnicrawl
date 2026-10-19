//! 握手校验：Finished 的 verify_data、CertificateVerify 的签名，以及证书链的信任判定。
//!
//! 这三件事都属于「拒绝可疑对端」的判定，放在一起是因为它们共用同一份转录状态与
//! 同一套失败语义：任何一项不通过都必须终止握手，且错误信息要能指认是哪一环失败。
//!
//! 证书链的信任判定交给 `rustls-webpki`（已审计）。不自研：X.509 路径构建、名称约束、
//! 策略处理里的边界极多，写错一处就是可绕过验证，而这条链路恰恰是安全边界。

use ring::{hkdf, hmac};
use rustls_pki_types::{CertificateDer, ServerName, TrustAnchor, UnixTime};
use webpki::EndEntityCert;

use crate::keyschedule::{CipherSuite, Transcript};

/// 握手校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Finished 的 verify_data 长度不对。
    BadFinishedLength { expected: usize, actual: usize },
    /// Finished 的 verify_data 不匹配：对端没有同一份转录，握手必须中止。
    FinishedMismatch,
    /// CertificateVerify 的签名算法不被支持。
    UnsupportedSignatureScheme(u16),
    /// CertificateVerify 的签名校验失败。
    BadSignature,
    /// 证书链无法建立到受信根。
    UntrustedCertificate(String),
    /// 证书与本次连接的主机名不匹配。
    WrongHostName(String),
    /// 对端证书已过期或尚未生效。
    CertificateNotValidYet(String),
    /// 没有可用的叶证书。
    EmptyCertificateChain,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadFinishedLength { expected, actual } => {
                write!(
                    formatter,
                    "Finished 长度应为 {expected} 字节，实际 {actual}"
                )
            }
            Self::FinishedMismatch => write!(formatter, "Finished 校验失败：对端转录与本端不一致"),
            Self::UnsupportedSignatureScheme(scheme) => {
                write!(
                    formatter,
                    "CertificateVerify 的签名算法 {scheme:#06x} 不受支持"
                )
            }
            Self::BadSignature => write!(formatter, "CertificateVerify 签名校验失败"),
            Self::UntrustedCertificate(reason) => {
                write!(formatter, "证书链不可信：{reason}")
            }
            Self::WrongHostName(reason) => write!(formatter, "证书与主机名不匹配：{reason}"),
            Self::CertificateNotValidYet(reason) => {
                write!(formatter, "证书不在有效期内：{reason}")
            }
            Self::EmptyCertificateChain => write!(formatter, "对端未提供叶证书"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// 计算 Finished 消息的 verify_data（RFC 8446 §4.4.4）。
///
/// ```text
/// finished_key = HKDF-Expand-Label(base_key, "finished", "", Hash.length)
/// verify_data  = HMAC(finished_key, Transcript-Hash(Handshake Context, Certificate*))
/// ```
///
/// `base_key` 是**该方向的**握手阶段密钥（`traffic_keys` 的 secret，不是 key/iv）。
/// 注意它必须与 `traffic_keys` 用的同一个 secret 派生，不能拿 RecordKey 的 key 顶替。
pub fn finished_verify_data(
    suite: CipherSuite,
    base_key: &[u8],
    transcript: &Transcript,
) -> Result<Vec<u8>, String> {
    let prk = hkdf::Prk::new_less_safe(suite.hkdf_algorithm(), base_key);
    let mut finished_key = vec![0u8; suite.hash().output_len()];
    crate::keyschedule::expand_label(suite, &prk, "finished", &[], &mut finished_key)?;
    let key = hmac::Key::new(suite.hmac_algorithm(), &finished_key);
    let hash = transcript.hash();
    Ok(hmac::sign(&key, &hash).as_ref().to_vec())
}

/// 校验对端的 Finished。
pub fn verify_finished(
    suite: CipherSuite,
    base_key: &[u8],
    transcript: &Transcript,
    received: &[u8],
) -> Result<(), VerifyError> {
    let expected = finished_verify_data(suite, base_key, transcript)
        .map_err(|_| VerifyError::FinishedMismatch)?;
    if received.len() != expected.len() {
        return Err(VerifyError::BadFinishedLength {
            expected: expected.len(),
            actual: received.len(),
        });
    }
    // 常量时间比较：逐字节早退会把「前几字节正确」这一信息泄漏给对端。
    if constant_time_eq(&expected, received) {
        Ok(())
    } else {
        Err(VerifyError::FinishedMismatch)
    }
}

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

/// CertificateVerify 的签名上下文前缀（RFC 8446 §4.4.3）。
///
/// 全部是 0x20 空格、不含结尾 NUL。这个常量写错会让所有真证书校验失败。
const SERVER_SIGNATURE_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify";

/// 构造 CertificateVerify 被签名的原始消息。
///
/// ```text
/// 0x20 * 64 || context string || 0x00 || Transcript-Hash(..Certificate)
/// ```
pub fn certificate_verify_message(context: &[u8], transcript_hash: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(64 + context.len() + 1 + transcript_hash.len());
    message.extend_from_slice(&[0x20; 64]);
    message.extend_from_slice(context);
    message.push(0x00);
    message.extend_from_slice(transcript_hash);
    message
}

/// CertificateVerify 里服务端用的上下文串。
pub fn server_verify_context() -> &'static [u8] {
    SERVER_SIGNATURE_CONTEXT
}

/// 把 TLS 1.3 的 `SignatureScheme` 映射到 webpki 的验签算法。
///
/// 每个 scheme 只给**一个**算法，不做候选放宽：1.3 的 `ecdsa_secp256r1_sha256`
/// 已经把曲线钉死在 P-256 上，允许退到 P-384 会让「换个曲线」这类降级溜过去。
///
/// 不支持 `ecdsa_secp521r1_sha512`（`ring` 没有 P-521）、`ed448`，也不支持
/// `rsa_pss_pss_*`：webpki 0.103 只提供针对 `rsaEncryption` SPKI 的 PSS 算法，
/// 拿它验 PSS 专用 SPKI 会以「签名不匹配」的错误露面，那等于把「不支持」伪装成
/// 「被伪造」，宁可在这里明确拒绝。
pub fn signature_algorithm_for_scheme(
    scheme: u16,
) -> Option<&'static dyn rustls_pki_types::SignatureVerificationAlgorithm> {
    Some(match scheme {
        0x0403 => webpki::ring::ECDSA_P256_SHA256,
        0x0503 => webpki::ring::ECDSA_P384_SHA384,
        0x0804 => webpki::ring::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
        0x0805 => webpki::ring::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
        0x0806 => webpki::ring::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
        0x0807 => webpki::ring::ED25519,
        _ => return None,
    })
}

/// 校验 CertificateVerify 的签名（RFC 8446 §4.4.3）。
///
/// 被签名的消息是 `0x20×64 || 上下文串 || 0x00 || Transcript-Hash(CH..Certificate)`，
/// 签名由叶证书的公钥验证。这一步只证明「对端持有叶证书私钥」，
/// **不证明证书本身可信**——那要另外调 [`verify_server_chain`]。
pub fn verify_certificate_verify(
    leaf: &CertificateDer<'_>,
    scheme: u16,
    transcript_hash: &[u8],
    signature: &[u8],
) -> Result<(), VerifyError> {
    let algorithm = signature_algorithm_for_scheme(scheme)
        .ok_or(VerifyError::UnsupportedSignatureScheme(scheme))?;
    let cert = EndEntityCert::try_from(leaf)
        .map_err(|error| VerifyError::UntrustedCertificate(format!("叶证书无法解析：{error}")))?;
    let message = certificate_verify_message(SERVER_SIGNATURE_CONTEXT, transcript_hash);
    cert.verify_signature(algorithm, &message, signature)
        .map_err(|_| VerifyError::BadSignature)
}

/// 用「已拼好的原始消息」验签，供 TLS 1.2 的 `ServerKeyExchange` 使用。
///
/// 1.2 与 1.3 的签名方案取值相同（`0x0403` 等），但有一个关键差别：1.2 的 RSA
/// 可以配 `rsa_pkcs1_*`（PKCS#1 v1.5），而 1.3 强制 PSS。因此这里不能复用
/// [`signature_algorithm_for_scheme`]——它把 `0x0401`/`0x0501` 视为不支持。
///
/// 被签名的内容由调用方拼好（1.2 是 `client_random || server_random || params`），
/// 与 1.3 的 `0x20×64` 前缀完全不同，这也是不能共用同一个函数的原因。
pub fn verify_raw_signature(
    leaf: &CertificateDer<'_>,
    scheme: u16,
    message: &[u8],
    signature: &[u8],
) -> Result<(), VerifyError> {
    let algorithm = tls12_signature_algorithm(scheme)
        .ok_or(VerifyError::UnsupportedSignatureScheme(scheme))?;
    let cert = EndEntityCert::try_from(leaf)
        .map_err(|error| VerifyError::UntrustedCertificate(format!("叶证书无法解析：{error}")))?;
    cert.verify_signature(algorithm, message, signature)
        .map_err(|_| VerifyError::BadSignature)
}

/// 1.2 的 `signature_algorithms` 取值到 webpki 算法：在 1.3 的基础上补上 PKCS#1 v1.5。
fn tls12_signature_algorithm(
    scheme: u16,
) -> Option<&'static dyn rustls_pki_types::SignatureVerificationAlgorithm> {
    Some(match scheme {
        0x0401 => webpki::ring::RSA_PKCS1_2048_8192_SHA256,
        0x0501 => webpki::ring::RSA_PKCS1_2048_8192_SHA384,
        0x0601 => webpki::ring::RSA_PKCS1_2048_8192_SHA512,
        other => return signature_algorithm_for_scheme(other),
    })
}

/// 受信根证书集合。
pub struct TrustAnchors {
    anchors: Vec<TrustAnchor<'static>>,
}

impl TrustAnchors {
    /// 用 `webpki-roots` 的内置根（Mozilla 根证书库的快照）。
    ///
    /// `TLS_SERVER_ROOTS` 本身就是 `TrustAnchor` 列表，直接转成拥有所有权即可。
    pub fn webpki_roots() -> Self {
        Self {
            anchors: webpki_roots::TLS_SERVER_ROOTS
                .iter()
                .map(|anchor| TrustAnchor {
                    subject: anchor.subject.to_vec().into(),
                    subject_public_key_info: anchor.subject_public_key_info.to_vec().into(),
                    name_constraints: anchor
                        .name_constraints
                        .as_ref()
                        .map(|constraints| rustls_pki_types::Der::from(constraints.to_vec())),
                })
                .collect(),
        }
    }

    /// 用调用方提供的根证书（测试与私有 CA 场景）。
    pub fn from_der(roots: &[CertificateDer<'static>]) -> Result<Self, VerifyError> {
        let anchors = roots
            .iter()
            .map(|der| {
                webpki::anchor_from_trusted_cert(der)
                    .map(|anchor| anchor.to_owned())
                    .map_err(|error| VerifyError::UntrustedCertificate(format!("{error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { anchors })
    }
}

/// 校验服务端证书链：建链、有效期、主机名。
///
/// 只做「这条链能不能信」与「它是不是给这个主机名的」，不做吊销检查
/// （OCSP/CRL 按 1.3 的可选扩展处理，当前实现不发 `status_request`）。
pub fn verify_server_chain(
    anchors: &TrustAnchors,
    chain: &[CertificateDer<'static>],
    server_name: &ServerName<'_>,
    now: UnixTime,
) -> Result<(), VerifyError> {
    let leaf = chain.first().ok_or(VerifyError::EmptyCertificateChain)?;
    let intermediates: Vec<CertificateDer<'_>> = chain[1..]
        .iter()
        .map(|der| der.clone().into_owned())
        .collect();
    let cert = EndEntityCert::try_from(leaf)
        .map_err(|error| VerifyError::UntrustedCertificate(format!("叶证书无法解析：{error}")))?;
    let usage = webpki::KeyUsage::server_auth();
    cert.verify_for_usage(
        // webpki 要求显式给出可接受的签名算法；这里只开放现代 TLS 常用的几种，
        // 包含 SHA-1 的旧算法一律不给，避免为了「兼容」而降低验证强度。
        &[
            webpki::ring::ECDSA_P256_SHA256,
            webpki::ring::ECDSA_P256_SHA384,
            webpki::ring::ECDSA_P384_SHA256,
            webpki::ring::ECDSA_P384_SHA384,
            webpki::ring::ED25519,
            webpki::ring::RSA_PKCS1_2048_8192_SHA256,
            webpki::ring::RSA_PKCS1_2048_8192_SHA384,
            webpki::ring::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
            webpki::ring::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
        ],
        &anchors.anchors,
        &intermediates,
        now,
        usage,
        None,
        None,
    )
    .map_err(|error| match error {
        webpki::Error::CertNotValidYet { .. } => {
            VerifyError::CertificateNotValidYet(format!("{error}"))
        }
        webpki::Error::CertExpired { .. } => {
            VerifyError::CertificateNotValidYet(format!("已过期：{error}"))
        }
        other => VerifyError::UntrustedCertificate(format!("{other}")),
    })?;
    // 主机名单独校验：`verify_for_usage` 只验链，不验名字。
    cert.verify_is_valid_for_subject_name(server_name)
        .map_err(|error| VerifyError::WrongHostName(format!("{error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_length_follows_the_hash() {
        for (suite, expected) in [
            (CipherSuite::Aes128GcmSha256, 32),
            (CipherSuite::Aes256GcmSha384, 48),
            (CipherSuite::ChaCha20Poly1305Sha256, 32),
        ] {
            let transcript = Transcript::new(suite);
            let data = finished_verify_data(suite, &[0x11; 32], &transcript).expect("应当计算");
            assert_eq!(data.len(), expected, "{suite:?}");
        }
    }

    #[test]
    fn finished_depends_on_both_key_and_transcript() {
        let suite = CipherSuite::Aes128GcmSha256;
        let mut first = Transcript::new(suite);
        first.push(b"handshake");
        let mut second = Transcript::new(suite);
        second.push(b"different");

        let with_key_one = finished_verify_data(suite, &[0x01; 32], &first).expect("计算");
        let with_key_two = finished_verify_data(suite, &[0x02; 32], &first).expect("计算");
        let other_transcript = finished_verify_data(suite, &[0x01; 32], &second).expect("计算");
        assert_ne!(with_key_one, with_key_two, "换了密钥必须变");
        assert_ne!(with_key_one, other_transcript, "换了转录必须变");
    }

    #[test]
    fn verifying_the_same_finished_succeeds() {
        let suite = CipherSuite::Aes128GcmSha256;
        let mut transcript = Transcript::new(suite);
        transcript.push(b"messages");
        let data = finished_verify_data(suite, &[0x33; 32], &transcript).expect("计算");
        assert_eq!(
            verify_finished(suite, &[0x33; 32], &transcript, &data),
            Ok(())
        );
    }

    #[test]
    fn a_tampered_finished_is_rejected() {
        let suite = CipherSuite::Aes128GcmSha256;
        let transcript = Transcript::new(suite);
        let mut data = finished_verify_data(suite, &[0x33; 32], &transcript).expect("计算");
        data[0] ^= 0xff;
        assert_eq!(
            verify_finished(suite, &[0x33; 32], &transcript, &data),
            Err(VerifyError::FinishedMismatch)
        );
    }

    #[test]
    fn a_wrong_length_finished_is_reported_distinctly() {
        let suite = CipherSuite::Aes128GcmSha256;
        let transcript = Transcript::new(suite);
        assert_eq!(
            verify_finished(suite, &[0x33; 32], &transcript, &[0u8; 31]),
            Err(VerifyError::BadFinishedLength {
                expected: 32,
                actual: 31
            })
        );
    }

    #[test]
    fn certificate_verify_message_has_the_specified_layout() {
        let hash = [0xab; 32];
        let message = certificate_verify_message(server_verify_context(), &hash);
        assert_eq!(message.len(), 64 + 33 + 1 + 32);
        assert!(message[..64].iter().all(|byte| *byte == 0x20), "64 个空格");
        assert_eq!(&message[64..97], b"TLS 1.3, server CertificateVerify");
        assert_eq!(message[97], 0x00, "分隔符");
        assert_eq!(&message[98..], &hash);
    }

    #[test]
    fn the_context_string_has_no_trailing_nul() {
        let context = server_verify_context();
        assert_eq!(context.len(), 33);
        assert_ne!(context.last(), Some(&0));
        assert!(context.starts_with(b"TLS 1.3, server"));
    }

    #[test]
    fn constant_time_eq_behaves_like_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn an_empty_chain_is_reported_as_such() {
        let anchors = TrustAnchors::webpki_roots();
        let name = ServerName::try_from("example.com").expect("主机名合法");
        assert_eq!(
            verify_server_chain(
                &anchors,
                &[],
                &name,
                UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_700_000_000))
            ),
            Err(VerifyError::EmptyCertificateChain)
        );
    }

    #[test]
    fn garbage_leaf_certificate_is_rejected() {
        let anchors = TrustAnchors::webpki_roots();
        let name = ServerName::try_from("example.com").expect("主机名合法");
        let chain = vec![CertificateDer::from(vec![0xde, 0xad, 0xbe, 0xef])];
        let result = verify_server_chain(
            &anchors,
            &chain,
            &name,
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_700_000_000)),
        );
        assert!(
            matches!(result, Err(VerifyError::UntrustedCertificate(_))),
            "{result:?}"
        );
    }

    #[test]
    fn webpki_roots_load_without_panicking() {
        let anchors = TrustAnchors::webpki_roots();
        // 只要构造成功即可：根的数量由依赖决定，不写死具体值。
        let name = ServerName::try_from("example.com").expect("主机名合法");
        assert!(!anchors.anchors.is_empty());
        let _ = name;
    }
}
