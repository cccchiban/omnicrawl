//! 四个浏览器家族的 ClientHello 档案。
//!
//! # 数值来源
//!
//! 扩展顺序、密码套件排列、各组取值都**取自真实抓包**，不是查资料抄的。
//! 抓法：让本机 Edge（Chromium 系）对一个监听端口发起握手，存下第一条记录，
//! 再按 RFC 8446 §4.1.2 的结构逐字段解出。脚本在
//! `.omnicrawl/.agent_tmp/scripts/capture_client_hello.py`。
//!
//! 校准后的 Chromium 档案与真实抓包逐项对齐（1701 字节的 ClientHello）：
//!
//! | 项 | 真实值 |
//! |---|---|
//! | 密码套件 | `8a8a 1301 1302 1303 c02b c02f c02c c030 cca9 cca8 c013 c014 009c 009d 002f 0035` |
//! | 扩展顺序 | GREASE, 0xfe0d, 0x0017, 0x0005, 0x0023, 0x001b, 0x44cd, 0xff01, 0x0033, 0x000a, 0x002b, 0x000b, 0x000d, 0x0012, 0x0010, 0x002d, GREASE |
//! | supported_groups | GREASE, 0x11ec, 0x001d, 0x0017, 0x0018 |
//! | signature_algorithms | 0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501, 0x0806, 0x0601 |
//! | supported_versions | GREASE, 0x0304, 0x0303 |
//! | key_share | GREASE, 0x11ec(1216B), 0x001d(32B) |
//!
//! # 与真实 Chromium 的已知差距
//!
//! `0x11ec`（X25519MLKEM768）在 `supported_groups` 里保留、但**不提供 key_share**：
//! 提供它需要 ML-KEM 实现，`ring` 没有，本 crate 也没写。这与真实 Chromium 不同，
//! 会让 JA4 的 groups 段有差异，但**不会导致 HRR**——服务端只在「列了该组却不给
//! key_share」时才可能要求重试，而我们可以显式选择不列该组（见 `include_pq_group`）。
//!
//! Firefox / Safari 没有对应版本的抓包，其顺序沿用公开资料，标注为「未校准」。
//! 宁可标注未校准，也不要写一组看起来像、实际对不上的数字。

use crate::client_hello::{
    alpn, ec_point_formats, group, key_share, psk_key_exchange_modes, server_name,
    signature_algorithms, suite, supported_groups, supported_versions, ClientHelloSpec,
    ExtensionSlot, GREASE_MARK,
};

/// 浏览器家族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Chrome,
    Firefox,
    Safari,
    Edge,
}

impl Family {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Firefox => "firefox",
            Self::Safari => "safari",
            Self::Edge => "edge",
        }
    }

    /// 与 `omnicrawl-host` 的 `IMPERSONATE_OPTIONS` 同口径的解析（前缀匹配、未知回退 Chrome）。
    pub fn parse(name: &str) -> Self {
        let name = name.trim().to_lowercase();
        if name.starts_with("firefox") {
            Self::Firefox
        } else if name.starts_with("safari") {
            Self::Safari
        } else if name.starts_with("edge") {
            Self::Edge
        } else {
            Self::Chrome
        }
    }

    /// 该家族是否在列表里放 GREASE（只有 Chromium 系）。
    pub fn emits_grease(&self) -> bool {
        matches!(self, Self::Chrome | Self::Edge)
    }

    /// 该家族默认的 ALPN 顺序。
    pub fn alpn_protocols(&self) -> &'static [&'static str] {
        &["h2", "http/1.1"]
    }

    /// 该家族是否把 `X25519MLKEM768` 列进 `supported_groups`。
    ///
    /// 恒为 `false`：列了却不给对应 `key_share` 会让部分服务端回 HelloRetryRequest，
    /// 而给得出 key_share 需要 ML-KEM 实现（`ring` 没有，本 crate 也没写）。
    ///
    /// 实测依据（2026-10，example.com / cloudflare.com / wikipedia.org 一致）：本 crate
    /// 早期版本在 `supported_groups` 里列了 `0x11ec` 却没给 key_share，三个站全部回
    /// HRR 要求改用该组，握手直接卡死。真实 Chrome 能列它，是因为它同时给了 1216 字节
    /// 的 key_share——抓包可见。
    fn supports_pq_group(&self) -> bool {
        false
    }
}

/// 常见签名算法（`signature_algorithms` 的取值）。
const SIG_RSA_PSS_SHA256: u16 = 0x0804;
const SIG_RSA_PSS_SHA384: u16 = 0x0805;
const SIG_RSA_PSS_SHA512: u16 = 0x0806;
const SIG_ECDSA_P256_SHA256: u16 = 0x0403;
const SIG_ECDSA_P384_SHA384: u16 = 0x0503;
const SIG_ECDSA_P521_SHA512: u16 = 0x0601;
const SIG_RSA_PKCS1_SHA256: u16 = 0x0401;
const SIG_RSA_PKCS1_SHA384: u16 = 0x0501;

/// TLS 1.3 与 1.2 的版本号。
const TLS13: u16 = 0x0304;
const TLS12: u16 = 0x0303;

/// 按家族构造 ClientHello 档案。
///
/// `host` 用于填 SNI，`key_share_public` 是各组公钥（由调用方的密钥交换层生成：
/// X25519 32 字节、P-256 65 字节未压缩点）。这一层不生成密钥，保持纯函数。
pub fn spec_for(
    family: Family,
    host: &str,
    key_share_public: &[(u16, Vec<u8>)],
) -> ClientHelloSpec {
    match family {
        Family::Chrome | Family::Edge => chromium_spec(host, key_share_public),
        Family::Firefox => firefox_spec(host, key_share_public),
        Family::Safari => safari_spec(host, key_share_public),
    }
}

/// Chromium 系（Chrome / Edge）。扩展顺序取自真实抓包。
fn chromium_spec(host: &str, key_share_public: &[(u16, Vec<u8>)]) -> ClientHelloSpec {
    let mut extensions = Vec::with_capacity(20);

    // 1. 首位扩展类型是 GREASE，且无载荷。
    extensions.push(ExtensionSlot::new(GREASE_MARK, Vec::new()));
    // 2. server_name。真实抓包里 SNI 被 ECH 吸收了，但 ECH 尚未全站生效，
    //    不发 SNI 会让不支持 ECH 的服务端拿到空主机名（部分会直接握手失败）。
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SERVER_NAME,
        server_name(host),
    ));
    // 3. extended_master_secret
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EXTENDED_MASTER_SECRET,
        Vec::new(),
    ));
    // 5. status_request（OCSP）
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::STATUS_REQUEST,
        vec![0x01, 0x00, 0x00, 0x00, 0x00],
    ));
    // 6. session_ticket（空体）
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SESSION_TICKET,
        Vec::new(),
    ));
    // 7. compress_certificate 不发：声明它意味着接受 RFC 8879 的
    //    `compressed_certificate`（握手消息类型 25），而本实现不解压证书。
    //    声明了却不处理会让握手在 WaitCertificate 阶段收到类型 25 后失败。
    // 9. renegotiation_info
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::RENEGOTIATION_INFO,
        vec![0x00],
    ));
    // 10. key_share
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::KEY_SHARE,
        chromium_key_share(key_share_public),
    ));
    // 11. supported_groups
    let mut groups = vec![GREASE_MARK];
    if Family::Chrome.supports_pq_group() {
        groups.push(group::X25519_MLKEM768);
    }
    groups.push(group::X25519);
    groups.push(group::SECP256R1);
    groups.push(group::SECP384R1);
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_GROUPS,
        supported_groups(&groups),
    ));
    // 12. supported_versions：GREASE + 1.3 + 1.2
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_VERSIONS,
        supported_versions(&[GREASE_MARK, TLS13, TLS12]),
    ));
    // 13. ec_point_formats
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EC_POINT_FORMATS,
        ec_point_formats(),
    ));
    // 14. signature_algorithms（8 项，取自抓包）
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SIGNATURE_ALGORITHMS,
        signature_algorithms(&[
            SIG_ECDSA_P256_SHA256,
            SIG_RSA_PSS_SHA256,
            SIG_RSA_PKCS1_SHA256,
            SIG_ECDSA_P384_SHA384,
            SIG_RSA_PSS_SHA384,
            SIG_RSA_PKCS1_SHA384,
            SIG_RSA_PSS_SHA512,
            SIG_ECDSA_P521_SHA512,
        ]),
    ));
    // 15. signed_certificate_timestamp（空体）
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SIGNED_CERTIFICATE_TIMESTAMP,
        Vec::new(),
    ));
    // 16. ALPN
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::ALPN,
        alpn(&["h2", "http/1.1"]),
    ));
    // 17. psk_key_exchange_modes
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::PSK_KEY_EXCHANGE_MODES,
        psk_key_exchange_modes(&[0x01]),
    ));
    // 18. 末位扩展类型也是 GREASE。
    extensions.push(ExtensionSlot::new(GREASE_MARK, vec![0x00]));

    ClientHelloSpec {
        legacy_version: TLS12,
        // Chromium 发 32 字节会话 ID（兼容中间设备的遗留怪癖）。
        session_id_len: 32,
        cipher_suites: chromium_suites(),
        compression_methods: vec![0x00],
        extensions,
    }
}

/// Chromium 的密码套件顺序，取自抓包。
fn chromium_suites() -> Vec<u16> {
    vec![
        GREASE_MARK,
        suite::TLS_AES_128_GCM_SHA256,
        suite::TLS_AES_256_GCM_SHA384,
        suite::TLS_CHACHA20_POLY1305_SHA256,
        suite::TLS_ECDHE_ECDSA_AES128_GCM,
        suite::TLS_ECDHE_RSA_AES128_GCM,
        suite::TLS_ECDHE_ECDSA_AES256_GCM,
        suite::TLS_ECDHE_RSA_AES256_GCM,
        suite::TLS_ECDHE_ECDSA_CHACHA20,
        suite::TLS_ECDHE_RSA_CHACHA20,
        0xc013,
        0xc014,
        0x009c,
        0x009d,
        0x002f,
        0x0035,
    ]
}

/// Chromium 的 key_share：首项是 GREASE（1 字节占位），然后是各组公钥。
fn chromium_key_share(entries: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut all = Vec::with_capacity(entries.len() + 1);
    all.push((GREASE_MARK, vec![0x00]));
    all.extend_from_slice(entries);
    key_share(&all)
}

/// Firefox。未按抓包校准：顺序沿用公开资料。
fn firefox_spec(host: &str, key_share_public: &[(u16, Vec<u8>)]) -> ClientHelloSpec {
    let mut extensions = Vec::with_capacity(20);
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SERVER_NAME,
        server_name(host),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EXTENDED_MASTER_SECRET,
        Vec::new(),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::RENEGOTIATION_INFO,
        vec![0x00],
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_GROUPS,
        supported_groups(&[
            group::X25519,
            group::SECP256R1,
            group::SECP384R1,
            group::SECP521R1,
        ]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EC_POINT_FORMATS,
        ec_point_formats(),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SESSION_TICKET,
        Vec::new(),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::ALPN,
        alpn(&["h2", "http/1.1"]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::STATUS_REQUEST,
        vec![0x01, 0x00, 0x00, 0x00, 0x00],
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SIGNATURE_ALGORITHMS,
        signature_algorithms(&[
            SIG_ECDSA_P256_SHA256,
            SIG_ECDSA_P384_SHA384,
            SIG_ECDSA_P521_SHA512,
            SIG_RSA_PSS_SHA256,
            SIG_RSA_PSS_SHA384,
            SIG_RSA_PSS_SHA512,
            SIG_RSA_PKCS1_SHA256,
            SIG_RSA_PKCS1_SHA384,
        ]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_VERSIONS,
        supported_versions(&[TLS13, TLS12]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::KEY_SHARE,
        key_share(key_share_public),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::PSK_KEY_EXCHANGE_MODES,
        psk_key_exchange_modes(&[0x01]),
    ));

    ClientHelloSpec {
        legacy_version: TLS12,
        session_id_len: 0,
        cipher_suites: firefox_suites(),
        compression_methods: vec![0x00],
        extensions,
    }
}

fn firefox_suites() -> Vec<u16> {
    vec![
        suite::TLS_AES_128_GCM_SHA256,
        suite::TLS_CHACHA20_POLY1305_SHA256,
        suite::TLS_AES_256_GCM_SHA384,
        suite::TLS_ECDHE_ECDSA_AES128_GCM,
        suite::TLS_ECDHE_RSA_AES128_GCM,
        suite::TLS_ECDHE_ECDSA_CHACHA20,
        suite::TLS_ECDHE_RSA_CHACHA20,
        suite::TLS_ECDHE_ECDSA_AES256_GCM,
        suite::TLS_ECDHE_RSA_AES256_GCM,
    ]
}

/// Safari。未按抓包校准：顺序沿用公开资料。
fn safari_spec(host: &str, key_share_public: &[(u16, Vec<u8>)]) -> ClientHelloSpec {
    let mut extensions = Vec::with_capacity(20);
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SERVER_NAME,
        server_name(host),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EXTENDED_MASTER_SECRET,
        Vec::new(),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::RENEGOTIATION_INFO,
        vec![0x00],
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_GROUPS,
        supported_groups(&[
            group::X25519,
            group::SECP256R1,
            group::SECP384R1,
            group::SECP521R1,
        ]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::EC_POINT_FORMATS,
        ec_point_formats(),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::ALPN,
        alpn(&["h2", "http/1.1"]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::STATUS_REQUEST,
        vec![0x01, 0x00, 0x00, 0x00, 0x00],
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SIGNATURE_ALGORITHMS,
        signature_algorithms(&[
            SIG_ECDSA_P256_SHA256,
            SIG_RSA_PSS_SHA256,
            SIG_RSA_PSS_SHA384,
            SIG_RSA_PSS_SHA512,
            SIG_ECDSA_P384_SHA384,
            SIG_RSA_PKCS1_SHA256,
        ]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::SUPPORTED_VERSIONS,
        supported_versions(&[TLS13, TLS12]),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::KEY_SHARE,
        key_share(key_share_public),
    ));
    extensions.push(ExtensionSlot::new(
        crate::client_hello::ext::PSK_KEY_EXCHANGE_MODES,
        psk_key_exchange_modes(&[0x01]),
    ));

    ClientHelloSpec {
        legacy_version: TLS12,
        session_id_len: 0,
        cipher_suites: safari_suites(),
        compression_methods: vec![0x00],
        extensions,
    }
}

fn safari_suites() -> Vec<u16> {
    vec![
        suite::TLS_AES_128_GCM_SHA256,
        suite::TLS_AES_256_GCM_SHA384,
        suite::TLS_CHACHA20_POLY1305_SHA256,
        suite::TLS_ECDHE_ECDSA_AES128_GCM,
        suite::TLS_ECDHE_RSA_AES128_GCM,
        suite::TLS_ECDHE_ECDSA_AES256_GCM,
        suite::TLS_ECDHE_RSA_AES256_GCM,
        suite::TLS_ECDHE_ECDSA_CHACHA20,
        suite::TLS_ECDHE_RSA_CHACHA20,
    ]
}

/// HTTP/2 连接级指纹：SETTINGS 的内容、顺序与随后的 WINDOW_UPDATE。
///
/// # 数值来源
///
/// 取自**真实浏览器抓包**：本机 Edge 145 对一个带 ALPN `h2` 的本地 TLS 服务端发起连接，
/// 捕获其连接前言之后的字节（70 字节）。抓法见
/// `.omnicrawl/.agent_tmp/scripts/capture_h2_settings.py`。实测结果：
///
/// ```text
/// 帧 type=4 flags=0x00 sid=0 len=24
///   1 (HEADER_TABLE_SIZE)      = 65536
///   2 (ENABLE_PUSH)            = 0
///   4 (INITIAL_WINDOW_SIZE)    = 6291456
///   6 (MAX_HEADER_LIST_SIZE)   = 262144
/// 帧 type=8 flags=0x00 sid=0 len=4
///   连接级 WINDOW_UPDATE 增量 = 15663105
/// ```
///
/// 注意这**不是**「把常见库的档案抄过来」：同样一份 Chromium，`wreq-util` 的档案里
/// SETTINGS 顺序是八项且含 `MAX_CONCURRENT_STREAMS`、`MAX_FRAME_SIZE`、`ENABLE_CONNECT_PROTOCOL`
/// 与 `NO_RFC7540_PRIORITIES`，与这台 Edge 145 的实际输出不一致。以抓包为准。
///
/// # 为什么这一层重要
///
/// 只做 TLS 指纹是不够的：实测 `tls.browserleaks.com` 会对「SETTINGS 只有两项」的连接
/// 直接回 `GOAWAY NO_ERROR`（看起来像正常关闭）——它按 HTTP/2 指纹判断客户端不是浏览器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Http2Profile {
    /// SETTINGS 里的条目，**顺序即线上顺序**。
    pub settings: Vec<(u16, u32)>,
    /// SETTINGS 之后立刻发的连接级 WINDOW_UPDATE 增量（0 表示不发）。
    pub connection_window_update: u32,
    /// 本端接受的流级初始窗口（与 `settings` 里的 INITIAL_WINDOW_SIZE 一致）。
    pub local_initial_window: u32,
    /// 本端接受的最大帧载荷。
    pub local_max_frame_size: u32,
}

/// 各家族的 HTTP/2 连接指纹。
///
/// Firefox 与 Safari 没有抓包基准：它们的设置**沿用本实现既有的最小集合**并显式标注未
/// 校准——宁可少发也不要编一组看起来像的数字。真实浏览器对这两族发得更少或更多，但
/// 未校准的下限不会导致握手失败（HTTP/2 设置缺项时取规范默认值）。
pub fn http2_profile_for(family: Family) -> Http2Profile {
    match family {
        // 抓包基准：Edge 145 / Chromium 同源。
        Family::Chrome | Family::Edge => Http2Profile {
            settings: vec![
                (1, 65536),   // HEADER_TABLE_SIZE
                (2, 0),       // ENABLE_PUSH：Chromium 显式关推送
                (4, 6291456), // INITIAL_WINDOW_SIZE
                (6, 262144),  // MAX_HEADER_LIST_SIZE
            ],
            // 15728640 - 65535 = 15663105：Chromium 把连接窗口补到 15 MB。
            connection_window_update: 15_663_105,
            local_initial_window: 6_291_456,
            local_max_frame_size: 16_384,
        },
        // 未校准：Firefox 真实档案含 INITIAL_WINDOW_SIZE=131072 与
        // MAX_FRAME_SIZE=16384 等项，这里只保留能确认的那部分。
        Family::Firefox => Http2Profile {
            settings: vec![
                (1, 65536),   // HEADER_TABLE_SIZE
                (2, 0),       // ENABLE_PUSH
                (4, 131_072), // INITIAL_WINDOW_SIZE
            ],
            connection_window_update: 12_582_912 - 65_535,
            local_initial_window: 131_072,
            local_max_frame_size: 16_384,
        },
        // 未校准：Safari 的设置更少，且不发 ENABLE_PUSH（它从未实现推送）。
        Family::Safari => Http2Profile {
            settings: vec![
                (1, 4096),      // HEADER_TABLE_SIZE
                (4, 2_097_152), // INITIAL_WINDOW_SIZE
            ],
            connection_window_update: 0,
            local_initial_window: 2_097_152,
            local_max_frame_size: 16_384,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 抓包基准必须逐项对上：任何改动都要有新的抓包依据。
    #[test]
    fn chromium_http2_settings_match_the_captured_bytes() {
        let profile = http2_profile_for(Family::Chrome);
        assert_eq!(
            profile.settings,
            vec![(1, 65536), (2, 0), (4, 6_291_456), (6, 262_144)],
            "SETTINGS 的内容与顺序都必须与 Edge 145 的抓包一致"
        );
        assert_eq!(profile.connection_window_update, 15_663_105);
        // 窗口增量必须与 INITIAL_WINDOW_SIZE 讲同一件事。
        assert_eq!(profile.local_initial_window, 6_291_456);
        // Chromium 不发 MAX_FRAME_SIZE：缺席时按规范默认 16384。
        assert!(
            profile.settings.iter().all(|(id, _)| *id != 5),
            "Chromium 抓包里没有 MAX_FRAME_SIZE"
        );
        assert_eq!(profile.local_max_frame_size, 16_384);
    }

    #[test]
    fn every_family_yields_non_empty_settings() {
        for family in [
            Family::Chrome,
            Family::Firefox,
            Family::Safari,
            Family::Edge,
        ] {
            let profile = http2_profile_for(family);
            assert!(
                !profile.settings.is_empty(),
                "{} 不该没有设置",
                family.as_str()
            );
            // MAX_FRAME_SIZE 若出现，必须落在规范允许的区间内。
            for (id, value) in &profile.settings {
                if *id == 5 {
                    assert!(
                        (16_384..=16_777_215).contains(value),
                        "{} 的 MAX_FRAME_SIZE {value} 越界",
                        family.as_str()
                    );
                }
                if *id == 2 {
                    assert!(*value <= 1, "ENABLE_PUSH 只能是 0/1");
                }
            }
        }
    }

    fn public_key() -> Vec<(u16, Vec<u8>)> {
        vec![(group::X25519, vec![0x11; 32])]
    }

    #[test]
    fn family_names_match_the_host_options() {
        assert_eq!(Family::parse("Chrome124"), Family::Chrome);
        assert_eq!(Family::parse(" firefox "), Family::Firefox);
        assert_eq!(Family::parse("safari17"), Family::Safari);
        assert_eq!(Family::parse("edge"), Family::Edge);
        assert_eq!(Family::parse("unknown"), Family::Chrome);
        assert_eq!(Family::parse(""), Family::Chrome);
    }

    #[test]
    fn only_chromium_families_emit_grease() {
        assert!(Family::Chrome.emits_grease());
        assert!(Family::Edge.emits_grease());
        assert!(!Family::Firefox.emits_grease());
        assert!(!Family::Safari.emits_grease());
    }

    #[test]
    fn each_family_produces_a_parsable_hello() {
        for family in [
            Family::Chrome,
            Family::Firefox,
            Family::Safari,
            Family::Edge,
        ] {
            let spec = spec_for(family, "example.com", &public_key());
            let message = spec
                .encode(0x0a0a, &[0x22; 32], &spec.empty_session_id())
                .expect("应当能编码");
            assert!(
                crate::parse::round_trips(&spec, &message),
                "{} 的 ClientHello 应能原样解析回来",
                family.as_str()
            );
        }
    }

    #[test]
    fn chromium_cipher_suites_match_the_capture() {
        // 真实 Edge 抓包的前 16 项（首项是 GREASE，这里用标记占位）。
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        assert_eq!(
            spec.cipher_suites,
            vec![
                GREASE_MARK,
                0x1301,
                0x1302,
                0x1303,
                0xc02b,
                0xc02f,
                0xc02c,
                0xc030,
                0xcca9,
                0xcca8,
                0xc013,
                0xc014,
                0x009c,
                0x009d,
                0x002f,
                0x0035,
            ],
            "密码套件必须与抓包一致"
        );
    }

    #[test]
    fn chromium_extension_order_matches_the_capture() {
        // 真实 Edge 抓包的扩展顺序。三处刻意差异，都因为「声明了却做不到」：
        //
        // - SNI 显式保留（抓包里被 ECH 吸收，但不支持 ECH 的服务端需要它）
        // - 去掉 encrypted_client_hello(0xfe0d)：我们要发的是空体，而真实浏览器
        //   发 186 字节载荷；声明 ECH 却不给合法载荷没有收益。
        // - 去掉 compress_certificate(0x001b)：声明它意味着接受 RFC 8879 的
        //   `compressed_certificate`（握手消息 25），实测会让握手在
        //   WaitCertificate 阶段失败。
        // - 去掉 application_settings(0x44cd)：ALPS 是 Chromium 私有扩展，
        //   服务端可忽略，发它没有收益。
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let kinds: Vec<u16> = spec.extensions.iter().map(|slot| slot.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GREASE_MARK, // 首位 GREASE
                0x0000,      // server_name
                0x0017,      // extended_master_secret
                0x0005,      // status_request
                0x0023,      // session_ticket
                0xff01,      // renegotiation_info
                0x0033,      // key_share
                0x000a,      // supported_groups
                0x002b,      // supported_versions
                0x000b,      // ec_point_formats
                0x000d,      // signature_algorithms
                0x0012,      // signed_certificate_timestamp
                0x0010,      // ALPN
                0x002d,      // psk_key_exchange_modes
                GREASE_MARK, // 末位 GREASE
            ],
            "扩展顺序必须与抓包一致（差异见注释）"
        );
    }

    #[test]
    fn every_grease_position_gets_a_distinct_value() {
        // RFC 8701 §3：同一连接里不同位置用相同的 GREASE 值是协议错误。
        // 实测：两个 GREASE 扩展用同一个值发往 example.com 会被
        // alert decode_error(50) 直接拒掉。
        //
        // 注意 `parse_client_hello` 会把 GREASE 归一化回 `GREASE_MARK`（为了让往返
        // 比较稳定），所以这里必须看**原始字节**，不能看解析结果。
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let message = spec
            .encode(0x5a5a, &[0x22; 32], &spec.empty_session_id())
            .expect("应当编码");

        let mut cursor = 4 + 2 + 32;
        cursor += 1 + message[cursor] as usize;
        let suites = u16::from_be_bytes([message[cursor], message[cursor + 1]]) as usize;
        cursor += 2 + suites;
        cursor += 1 + message[cursor] as usize;
        let total = u16::from_be_bytes([message[cursor], message[cursor + 1]]) as usize;
        cursor += 2;
        let end = cursor + total;

        let mut grease = Vec::new();
        while cursor < end {
            let kind = u16::from_be_bytes([message[cursor], message[cursor + 1]]);
            cursor += 2;
            let length = u16::from_be_bytes([message[cursor], message[cursor + 1]]) as usize;
            cursor += 2 + length;
            if kind & 0x0f0f == 0x0a0a {
                grease.push(kind);
            }
        }
        assert_eq!(grease.len(), 2, "Chrome 档案有首尾两个 GREASE 扩展");
        assert_ne!(
            grease[0], grease[1],
            "两个 GREASE 扩展的值必须不同：{grease:04x?}"
        );
    }

    #[test]
    fn chromium_sends_a_server_name_so_non_ech_servers_can_pick_a_certificate() {
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::SERVER_NAME)
            .expect("必须有 SNI");
        // 结构：2 字节列表长度 + 1 字节类型 0 + 2 字节名字长度 + 名字
        assert_eq!(slot.data[0..2], [0x00, 0x0e], "列表长度 14");
        assert_eq!(slot.data[2], 0x00, "host_name 类型");
        assert_eq!(slot.data[3..5], [0x00, 0x0b], "名字长度 11");
        assert_eq!(&slot.data[5..16], b"example.com");
    }

    #[test]
    fn chromium_signature_algorithms_match_the_capture() {
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::SIGNATURE_ALGORITHMS)
            .expect("应有 signature_algorithms");
        // 数据段是 2 字节长度 + 值列表。
        let count = u16::from_be_bytes([slot.data[0], slot.data[1]]) as usize;
        let actual: Vec<u16> = slot.data[2..2 + count]
            .chunks(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            actual,
            vec![0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501, 0x0806, 0x0601],
            "签名算法顺序必须与抓包一致"
        );
    }

    #[test]
    fn chromium_key_share_starts_with_grease_then_x25519() {
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::KEY_SHARE)
            .expect("应有 key_share");
        // 列表长度字段只覆盖各项：GREASE(2+2+1=5) + X25519(2+2+32=36) = 41 = 0x29。
        assert_eq!(slot.data[0..2], [0x00, 0x29], "列表长度 41");
        // 首项：GREASE 标记 + 1 字节占位公钥。
        assert_eq!(&slot.data[2..4], &GREASE_MARK.to_be_bytes());
        assert_eq!(&slot.data[4..6], &[0x00, 0x01], "占位公钥长度 1");
        assert_eq!(slot.data[6], 0x00, "占位公钥内容");
        // 第二项：X25519。
        assert_eq!(&slot.data[7..9], &group::X25519.to_be_bytes());
        assert_eq!(&slot.data[9..11], &[0x00, 0x20], "32 字节公钥");
        assert_eq!(&slot.data[11..43], &[0x11u8; 32]);
    }

    #[test]
    fn chromium_does_not_advertise_an_unimplemented_pq_group() {
        // 列了 0x11ec 却不给 key_share 会触发服务端 HRR。这条断言防止回退。
        let spec = spec_for(Family::Chrome, "example.com", &public_key());
        let slot = spec
            .extensions
            .iter()
            .find(|slot| slot.kind == crate::client_hello::ext::SUPPORTED_GROUPS)
            .expect("应有 supported_groups");
        let count = u16::from_be_bytes([slot.data[0], slot.data[1]]) as usize;
        let groups: Vec<u16> = slot.data[2..2 + count]
            .chunks(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        assert!(
            !groups.contains(&group::X25519_MLKEM768),
            "实现不了就不要声明；声称支持却给不出 key_share 会让握手卡在 HRR"
        );
        assert!(groups.contains(&group::X25519));
    }

    #[test]
    fn ja3_string_lists_the_fingerprint_fields() {
        let spec = spec_for(Family::Firefox, "example.com", &public_key());
        let ja3 = spec.ja3_string();
        let fields: Vec<&str> = ja3.split(',').collect();
        assert_eq!(fields.len(), 5, "JA3 有版本/套件/扩展/曲线/点格式五段");
        assert_eq!(fields[0], "771", "0x0303 = 771");
        assert!(!fields[1].contains("65278"), "GREASE 不进 JA3");
        assert!(fields[3].contains("29"), "应有 X25519（29）");
        assert_eq!(fields[4], "0", "点格式只有一个 uncompressed");
    }

    #[test]
    fn session_id_length_differs_by_family() {
        assert_eq!(
            spec_for(Family::Chrome, "a", &public_key()).session_id_len,
            32
        );
        assert_eq!(
            spec_for(Family::Safari, "a", &public_key()).session_id_len,
            0
        );
    }

    #[test]
    fn firefox_and_safari_do_not_emit_sct() {
        for family in [Family::Firefox, Family::Safari] {
            let spec = spec_for(family, "example.com", &public_key());
            assert!(
                spec.extensions
                    .iter()
                    .all(|slot| slot.kind
                        != crate::client_hello::ext::SIGNED_CERTIFICATE_TIMESTAMP),
                "{} 不发 SCT",
                family.as_str()
            );
        }
    }
}
