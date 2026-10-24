//! 端到端：用真实字节流驱动「ClientHello → ServerHello → 握手密钥」这条链。
//!
//! 这些用例不跑网络，但把所有需要对齐的边界串起来了：
//! 档案生成 ClientHello → 解析对端 ServerHello → X25519 协商 → 转录哈希 → 密钥调度 →
//! 用派生出的密钥加解密一条记录。任何一环字段错位都会在这里暴露。
//!
//! 这组测试的价值是**接缝检查**：单元测试各自通过、但 A 生产的字节 B 读不懂，
//! 是自研协议栈最常见的失败形态。

use omnicrawl_tls::client_hello::{ext, group, pick_grease, suite, ClientHelloSpec};
use omnicrawl_tls::handshake::{hs, parse_server_hello, take_handshake_messages, X25519Exchange};
use omnicrawl_tls::keyschedule::{CipherSuite, KeySchedule, Transcript};
use omnicrawl_tls::profile::{spec_for, Family};
use omnicrawl_tls::record::{ContentType, RecordKey};

fn rng() -> ring::rand::SystemRandom {
    ring::rand::SystemRandom::new()
}

/// 构造一份对端会发出的 ServerHello（含 supported_versions 与 key_share）。
fn server_hello(suite_code: u16, server_public: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0303u16.to_be_bytes());
    body.extend_from_slice(&[0x5a; 32]);
    body.push(0);
    body.extend_from_slice(&suite_code.to_be_bytes());
    body.push(0);

    let mut extensions = Vec::new();
    extensions.extend_from_slice(&kind_len(ext::SUPPORTED_VERSIONS, &[0x03, 0x04]));
    let mut share = Vec::new();
    share.extend_from_slice(&group::X25519.to_be_bytes());
    share.extend_from_slice(&(server_public.len() as u16).to_be_bytes());
    share.extend_from_slice(server_public);
    extensions.extend_from_slice(&kind_len(ext::KEY_SHARE, &share));

    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);

    let mut message = vec![hs::SERVER_HELLO];
    message.push((body.len() >> 16) as u8);
    message.push((body.len() >> 8) as u8);
    message.push(body.len() as u8);
    message.extend_from_slice(&body);
    message
}

fn kind_len(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// 完整走一遍：档案 → ClientHello 字节 → 解析对端 → 协商 → 密钥调度。
#[test]
fn a_full_handshake_prefix_produces_usable_record_keys() {
    let exchange = X25519Exchange::generate(&rng()).expect("生成密钥对");
    let key_share = [(group::X25519, exchange.public_key.clone())];
    let spec = spec_for(Family::Chrome, "example.com", &key_share);

    // 1) 生成 ClientHello，并把它当作转录的第一段。
    let grease = pick_grease(0x0a);
    let session_id = spec.empty_session_id();
    let client_hello_body = spec
        .encode(grease, &[0x33; 32], &session_id)
        .expect("应当能编码");
    let mut transcript = Transcript::new(CipherSuite::Aes128GcmSha256);
    transcript.push(&client_hello_body);

    // 2) 对端回 ServerHello，用我们自己的另一把密钥模拟服务端。
    let server = X25519Exchange::generate(&rng()).expect("服务端密钥对");
    let server_public = server.public_key.clone();
    let hello_bytes = server_hello(suite::TLS_AES_128_GCM_SHA256, &server_public);
    let (messages, consumed) = take_handshake_messages(&hello_bytes);
    assert_eq!(messages.len(), 1);
    assert_eq!(consumed, hello_bytes.len());
    assert_eq!(messages[0].kind, hs::SERVER_HELLO);

    // 3) 解析并校验对端选择。
    let parsed = parse_server_hello(&messages[0].body).expect("应当解析成功");
    assert_eq!(parsed.cipher_suite, suite::TLS_AES_128_GCM_SHA256);
    assert_eq!(parsed.selected_version, 0x0304);
    assert_eq!(parsed.key_share_group, group::X25519);
    transcript.push(&messages[0].raw);

    // 4) 双方各自算出共享密钥，必须一致。
    let server_private_side = server.agree(&exchange.public_key).expect("服务端协商");
    let client_side = exchange.agree(&parsed.key_share).expect("客户端协商");
    assert_eq!(client_side, server_private_side);

    // 5) 密钥调度：转录哈希取 CH..SH 之后。
    let suite_enum = CipherSuite::from_code(parsed.cipher_suite).expect("应是 1.3 套件");
    let schedule = KeySchedule::new(suite_enum);
    let transcript_hash = transcript.hash();
    let secrets = schedule
        .handshake_secrets(&client_side, &transcript_hash)
        .expect("应当派生握手密钥");

    // 6) 拿派生出的密钥真的加解密一条记录：客户端写入、服务端读回。
    let mut writer = RecordKey::new(suite_enum.aead(), &secrets.client.key, secrets.client.iv)
        .expect("客户端记录密钥");
    let mut reader = RecordKey::new(suite_enum.aead(), &secrets.client.key, secrets.client.iv)
        .expect("服务端记录密钥");
    let sealed = writer
        .seal(ContentType::Handshake, b"client finished")
        .expect("加密");
    let (kind, body) = reader.open(&sealed).expect("解密");
    assert_eq!(kind, ContentType::Handshake);
    assert_eq!(body, b"client finished");
}

/// 四个家族的 ClientHello 都能走通同一条链（档案差异不影响后续阶段）。
#[test]
fn every_family_reaches_the_same_key_schedule_stage() {
    for family in [
        Family::Chrome,
        Family::Firefox,
        Family::Safari,
        Family::Edge,
    ] {
        let exchange = X25519Exchange::generate(&rng()).expect("生成");
        let spec: ClientHelloSpec = spec_for(
            family,
            "example.com",
            &[(group::X25519, exchange.public_key.clone())],
        );
        let grease = if family.emits_grease() {
            pick_grease(0x1a)
        } else {
            0
        };
        let session_id = spec.empty_session_id();
        let encoded = spec.encode(grease, &[0x44; 32], &session_id).expect("编码");
        let mut transcript = Transcript::new(CipherSuite::Aes128GcmSha256);
        transcript.push(&encoded);

        let server = X25519Exchange::generate(&rng()).expect("服务端");
        let shared = server.agree(&exchange.public_key).expect("协商");
        let schedule = KeySchedule::new(CipherSuite::Aes128GcmSha256);
        let secrets = schedule
            .handshake_secrets(&shared, &transcript.hash())
            .expect("派生");
        assert_eq!(secrets.client.key.len(), 16, "{}", family.as_str());
        assert_ne!(
            secrets.client.key,
            secrets.server.key,
            "{} 的两个方向必须不同",
            family.as_str()
        );
    }
}

/// Chromium 系的 GREASE 值必须每次都落在合法集合里，且真的进入线上字节。
#[test]
fn grease_values_from_the_profile_are_legal_and_present() {
    let exchange = X25519Exchange::generate(&rng()).expect("生成");
    let spec = spec_for(
        Family::Chrome,
        "example.com",
        &[(group::X25519, exchange.public_key.clone())],
    );
    for seed in [0u8, 1, 7, 15, 200] {
        let grease = pick_grease(seed);
        assert!(
            omnicrawl_tls::GREASE_VALUES.contains(&grease),
            "{grease:#06x} 必须是 RFC 8701 的合法值"
        );
        let session_id = spec.empty_session_id();
        let encoded = spec.encode(grease, &[0x55; 32], &session_id).expect("编码");
        let needle = grease.to_be_bytes();
        assert!(
            encoded.windows(2).any(|pair| pair == needle),
            "GREASE 值 {grease:#06x} 必须出现在线上字节里"
        );
    }
}

/// 档案里 GREASE 占位符不能在最终字节里残留。
#[test]
fn the_grease_placeholder_never_reaches_the_wire() {
    let exchange = X25519Exchange::generate(&rng()).expect("生成");
    let spec = spec_for(
        Family::Chrome,
        "example.com",
        &[(group::X25519, exchange.public_key.clone())],
    );
    let session_id = spec.empty_session_id();
    let encoded = spec
        .encode(pick_grease(3), &[0x66; 32], &session_id)
        .expect("编码");
    let placeholder = omnicrawl_tls::GREASE_MARK.to_be_bytes();
    assert!(
        !encoded.windows(2).any(|pair| pair == placeholder),
        "0xFEFE 只是一处内部标记，不能出现在线上字节里"
    );
}
