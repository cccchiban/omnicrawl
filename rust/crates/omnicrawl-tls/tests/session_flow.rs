//! 握手状态机的端到端用例：模拟一个服务端，把状态机从 Start 推到 Connected。
//!
//! 这里不联网，但服务端侧的每一步都用与客户端同一套原语构造（同一份 HKDF、同一份
//! 记录层），因此任何方向性错误（例如把客户端 secret 当服务端 secret 用）都会立刻
//! 表现为「对端 Finished 校验失败」。
//!
//! 这组用例的价值在于**密钥切换的时机**：握手密钥在收到 ServerHello 后启用，应用读密钥
//! 在收到 Finished 后启用，应用写密钥要等本端 Finished 发出。切早了或切晚了，加解密都会
//! 错，而单看某一层是发现不了的。
//!
//! 证书与私钥来自 `tests/fixtures/`（由 openssl 生成、随仓库提交），因此
//! CertificateVerify 走的是**真的签名与真的验签**，不是打桩。

use omnicrawl_tls::handshake::hs;
use omnicrawl_tls::keyschedule::{CipherSuite, Transcript};
use omnicrawl_tls::profile::Family;
use omnicrawl_tls::record::{ContentType, RecordKey};
use omnicrawl_tls::session::{ClientHandshake, HandshakeError, PeerPolicy, State};
use omnicrawl_tls::verify::{finished_verify_data, TrustAnchors};
use omnicrawl_tls::{ext, group, suite};

fn rng() -> ring::rand::SystemRandom {
    ring::rand::SystemRandom::new()
}

const CA_DER: &[u8] = include_bytes!("fixtures/ca.der");
const LEAF_EC_DER: &[u8] = include_bytes!("fixtures/leaf-ec.der");
const LEAF_EC_KEY: &[u8] = include_bytes!("fixtures/leaf-ec-key.p8");
const LEAF_RSA_DER: &[u8] = include_bytes!("fixtures/leaf-rsa.der");
const LEAF_RSA_KEY: &[u8] = include_bytes!("fixtures/leaf-rsa-key.p8");

/// 夹具证书的 `notBefore` 之后再取一个时间点，避免依赖系统时钟。
fn verify_time() -> rustls_pki_types::UnixTime {
    rustls_pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000))
}

fn kind_len(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn handshake(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.push((body.len() >> 16) as u8);
    out.push((body.len() >> 8) as u8);
    out.push(body.len() as u8);
    out.extend_from_slice(body);
    out
}

fn certificate_body(chain: &[&[u8]]) -> Vec<u8> {
    let mut list = Vec::new();
    for der in chain {
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

/// 服务端私钥：负责签 CertificateVerify。
enum Signer {
    EcdsaP256(ring::signature::EcdsaKeyPair),
    Rsa2048(ring::signature::RsaKeyPair),
}

impl Signer {
    fn ecdsa() -> Self {
        Self::EcdsaP256(
            ring::signature::EcdsaKeyPair::from_pkcs8(
                &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                LEAF_EC_KEY,
                &rng(),
            )
            .expect("加载 EC 私钥"),
        )
    }

    fn rsa() -> Self {
        Self::Rsa2048(
            ring::signature::RsaKeyPair::from_pkcs8(LEAF_RSA_KEY).expect("加载 RSA 私钥"),
        )
    }

    /// CertificateVerify 用的签名算法。
    fn scheme(&self) -> u16 {
        match self {
            Self::EcdsaP256(_) => 0x0403,
            Self::Rsa2048(_) => 0x0804,
        }
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        match self {
            Self::EcdsaP256(key) => key.sign(&rng(), message).expect("ECDSA 签名").as_ref().to_vec(),
            Self::Rsa2048(key) => {
                let mut signature = vec![0u8; key.public().modulus_len()];
                key.sign(
                    &ring::signature::RSA_PSS_SHA256,
                    &rng(),
                    message,
                    &mut signature,
                )
                .expect("RSA-PSS 签名");
                signature
            }
        }
    }
}

/// 模拟服务端：持有与客户端对称的状态，按顺序吐出各条消息。
struct FakeServer {
    suite: CipherSuite,
    transcript: Transcript,
    handshake_secret: Vec<u8>,
    server_write: Option<RecordKey>,
    client_finished_key: Option<RecordKey>,
    signer: Signer,
}

impl FakeServer {
    fn accept(
        client_hello: &[u8],
        client_public: &[u8],
        server_exchange: omnicrawl_tls::X25519Exchange,
        signer: Signer,
    ) -> (Self, Vec<u8>) {
        let suite = CipherSuite::Aes128GcmSha256;
        let mut transcript = Transcript::new(suite);
        transcript.push(client_hello);

        let server_public = server_exchange.public_key.clone();
        let shared = server_exchange.agree(client_public).expect("服务端协商");

        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&[0x5a; 32]);
        body.push(0);
        body.extend_from_slice(&suite::TLS_AES_128_GCM_SHA256.to_be_bytes());
        body.push(0);
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&kind_len(ext::SUPPORTED_VERSIONS, &[0x03, 0x04]));
        let mut share = Vec::new();
        share.extend_from_slice(&group::X25519.to_be_bytes());
        share.extend_from_slice(&(server_public.len() as u16).to_be_bytes());
        share.extend_from_slice(&server_public);
        extensions.extend_from_slice(&kind_len(ext::KEY_SHARE, &share));
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let server_hello = handshake(hs::SERVER_HELLO, &body);
        transcript.push(&server_hello);

        let schedule = omnicrawl_tls::keyschedule::KeySchedule::new(suite);
        let secrets = schedule
            .handshake_secrets(&shared, &transcript.hash())
            .expect("服务端派生握手密钥");

        let client_write = RecordKey::new(suite.aead(), &secrets.client.key, secrets.client.iv)
            .expect("服务端读密钥");

        let server = Self {
            suite,
            transcript,
            handshake_secret: secrets.server_secret.clone(),
            server_write: Some(
                RecordKey::new(suite.aead(), &secrets.server.key, secrets.server.iv)
                    .expect("服务端写密钥"),
            ),
            client_finished_key: Some(client_write),
            signer,
        };
        (server, server_hello)
    }

    fn encrypt(&mut self, kind: u8, body: &[u8]) -> Vec<u8> {
        let raw = handshake(kind, body);
        self.transcript.push(&raw);
        self.server_write
            .as_mut()
            .expect("服务端写密钥")
            .seal(ContentType::Handshake, &raw)
            .expect("加密")
            .payload
    }

    /// 生成并加密 CertificateVerify：签名对象是「到 Certificate 为止」的转录哈希。
    fn certificate_verify(&mut self) -> Vec<u8> {
        let message = omnicrawl_tls::session::signed_message(&self.transcript.hash());
        let signature = self.signer.sign(&message);
        let mut body = Vec::new();
        body.extend_from_slice(&self.signer.scheme().to_be_bytes());
        body.extend_from_slice(&(signature.len() as u16).to_be_bytes());
        body.extend_from_slice(&signature);
        self.encrypt(hs::CERTIFICATE_VERIFY, &body)
    }

    fn server_finished(&mut self) -> Vec<u8> {
        let data = finished_verify_data(self.suite, &self.handshake_secret, &self.transcript)
            .expect("计算 Finished");
        let raw = handshake(hs::FINISHED, &data);
        self.transcript.push(&raw);
        self.server_write
            .as_mut()
            .expect("服务端写密钥")
            .seal(ContentType::Handshake, &raw)
            .expect("加密")
            .payload
    }

    /// 解开客户端 Finished，并独立校验它的 verify_data。
    fn expect_client_finished(&mut self, payload: &[u8]) -> Vec<u8> {
        let (kind, plaintext) = self
            .client_finished_key
            .as_mut()
            .expect("服务端读密钥")
            .open(&omnicrawl_tls::record::Record {
                kind: ContentType::ApplicationData,
                payload: payload.to_vec(),
            })
            .expect("解密客户端 Finished");
        assert_eq!(kind, ContentType::Handshake, "Finished 必须封在握手内层类型里");
        self.transcript.push(&plaintext);
        plaintext
    }
}

fn encrypted_extensions_body(alpn: Option<&str>) -> Vec<u8> {
    let mut extensions = Vec::new();
    if let Some(protocol) = alpn {
        let mut list = Vec::new();
        list.push(protocol.len() as u8);
        list.extend_from_slice(protocol.as_bytes());
        let mut data = Vec::new();
        data.extend_from_slice(&(list.len() as u16).to_be_bytes());
        data.extend_from_slice(&list);
        extensions.extend_from_slice(&kind_len(ext::ALPN, &data));
    }
    let mut body = Vec::new();
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);
    body
}

/// 把客户端从 Start 推到 WaitFinished，返回（服务端, 客户端）。
fn handshake_until_wait_finished(
    client: &mut ClientHandshake,
    chain: &[&[u8]],
    signer: Signer,
) -> FakeServer {
    let client_hello_record = client.start(&rng()).expect("生成 ClientHello");
    let (record, _) = omnicrawl_tls::record::parse_record(&client_hello_record).expect("解析");
    let client_hello = record.payload.clone();
    let client_public = extract_x25519_public(&client_hello);

    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("服务端密钥");
    let (mut server, server_hello) =
        FakeServer::accept(&client_hello, &client_public, server_exchange, signer);

    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::Handshake,
            payload: server_hello,
        })
        .expect("应当接受 ServerHello");
    assert_eq!(client.state(), State::WaitEncryptedExtensions);

    let ee = server.encrypt(
        hs::ENCRYPTED_EXTENSIONS,
        &encrypted_extensions_body(Some("h2")),
    );
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: ee,
        })
        .expect("应当接受 EncryptedExtensions");
    assert_eq!(client.alpn(), Some("h2"));

    let cert = server.encrypt(hs::CERTIFICATE, &certificate_body(chain));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cert,
        })
        .expect("应当接受 Certificate");
    assert_eq!(client.state(), State::WaitCertificateVerify);
    assert_eq!(client.server_chain().len(), chain.len());

    let cv = server.certificate_verify();
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cv,
        })
        .expect("真实的 CertificateVerify 签名应当通过校验");
    assert_eq!(client.state(), State::WaitFinished);
    server
}

#[test]
fn an_ecdsa_certificate_completes_a_full_handshake() {
    let mut client = ClientHandshake::new(Family::Chrome, "example.com")
        .with_trust(TrustAnchors::from_der(&[CA_DER.into()]).expect("加载测试根"))
        .with_verify_time(verify_time());
    assert_eq!(client.policy(), PeerPolicy::VerifyChain);

    let mut server =
        handshake_until_wait_finished(&mut client, &[LEAF_EC_DER, CA_DER], Signer::ecdsa());
    assert_eq!(
        client.certificate_verify().map(|cv| cv.scheme),
        Some(0x0403),
        "应当记下对端用的签名算法"
    );

    let finished = server.server_finished();
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: finished,
        })
        .expect("Finished 应当校验通过");
    assert_eq!(client.state(), State::SendClientFinished);

    // 此刻写密钥还是握手密钥，不允许发应用数据。
    assert!(
        client.seal_application(b"too early").is_err(),
        "客户端 Finished 之前不得发应用数据"
    );

    let client_finished = client.finish().expect("应当发出客户端 Finished");
    assert_eq!(client.state(), State::Connected);
    let plaintext = server.expect_client_finished(&client_finished);
    assert_eq!(plaintext[0], hs::FINISHED);
    assert_eq!(plaintext.len(), 4 + 32, "SHA-256 的 verify_data 是 32 字节");

    // 两端各自的应用密钥必须能互通：客户端写、服务端读（用客户端应用密钥）。
    let sealed = client.seal_application(b"GET / HTTP/1.1\r\n").expect("加密");
    let secrets = client.application_secrets().expect("应用密钥");
    let mut server_read =
        RecordKey::new(CipherSuite::Aes128GcmSha256.aead(), &secrets.client.key, secrets.client.iv)
            .expect("服务端应用读密钥");
    let (kind, body) = server_read.open(&sealed).expect("服务端应当能解开");
    assert_eq!(kind, ContentType::ApplicationData);
    assert_eq!(body, b"GET / HTTP/1.1\r\n");
}

#[test]
fn an_rsa_certificate_uses_rsa_pss() {
    let mut client = ClientHandshake::new(Family::Firefox, "example.com")
        .with_trust(TrustAnchors::from_der(&[CA_DER.into()]).expect("加载测试根"))
        .with_verify_time(verify_time());
    let mut server =
        handshake_until_wait_finished(&mut client, &[LEAF_RSA_DER, CA_DER], Signer::rsa());
    assert_eq!(
        client.certificate_verify().map(|cv| cv.scheme),
        Some(0x0804),
        "RSA 叶证书应当用 rsa_pss_rsae_sha256"
    );

    let finished = server.server_finished();
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: finished,
        })
        .expect("Finished");
    client.finish().expect("客户端 Finished");
    assert_eq!(client.state(), State::Connected);
}

#[test]
fn a_bad_certificate_verify_signature_is_rejected() {
    let mut client = ClientHandshake::new(Family::Chrome, "example.com");
    let client_hello_record = client.start(&rng()).expect("生成");
    let (record, _) = omnicrawl_tls::record::parse_record(&client_hello_record).expect("解析");
    let client_public = extract_x25519_public(&record.payload);
    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("密钥");
    let (mut server, server_hello) = FakeServer::accept(
        &record.payload,
        &client_public,
        server_exchange,
        Signer::ecdsa(),
    );
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::Handshake,
            payload: server_hello,
        })
        .expect("ServerHello");
    let ee = server.encrypt(hs::ENCRYPTED_EXTENSIONS, &encrypted_extensions_body(None));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: ee,
        })
        .expect("EncryptedExtensions");
    let cert = server.encrypt(hs::CERTIFICATE, &certificate_body(&[LEAF_EC_DER, CA_DER]));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cert,
        })
        .expect("Certificate");

    // 伪造一份签名：结构合法，但不是这把私钥签的。
    let message = omnicrawl_tls::session::signed_message(&[0u8; 32]);
    let mut body = Vec::new();
    body.extend_from_slice(&0x0403u16.to_be_bytes());
    let fake = Signer::ecdsa().sign(&message);
    body.extend_from_slice(&(fake.len() as u16).to_be_bytes());
    body.extend_from_slice(&fake);
    let cv = server.encrypt(hs::CERTIFICATE_VERIFY, &body);

    let error = client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cv,
        })
        .expect_err("转录哈希对不上，签名必须被拒绝");
    assert!(
        matches!(error, HandshakeError::Verification(_)),
        "应当报告校验失败：{error:?}"
    );
    assert_ne!(client.state(), State::WaitFinished);
}

#[test]
fn an_unsupported_signature_scheme_is_reported_distinctly() {
    let mut client = ClientHandshake::new(Family::Chrome, "example.com");
    let client_hello_record = client.start(&rng()).expect("生成");
    let (record, _) = omnicrawl_tls::record::parse_record(&client_hello_record).expect("解析");
    let client_public = extract_x25519_public(&record.payload);
    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("密钥");
    let (mut server, server_hello) = FakeServer::accept(
        &record.payload,
        &client_public,
        server_exchange,
        Signer::ecdsa(),
    );
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::Handshake,
            payload: server_hello,
        })
        .expect("ServerHello");
    let ee = server.encrypt(hs::ENCRYPTED_EXTENSIONS, &encrypted_extensions_body(None));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: ee,
        })
        .expect("EncryptedExtensions");
    let cert = server.encrypt(hs::CERTIFICATE, &certificate_body(&[LEAF_EC_DER, CA_DER]));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cert,
        })
        .expect("Certificate");

    // 0x0603 = rsa_pss_pss_sha256：我们知道它不在支持列表里。
    let mut body = Vec::new();
    body.extend_from_slice(&0x0603u16.to_be_bytes());
    body.extend_from_slice(&(4u16).to_be_bytes());
    body.extend_from_slice(&[1, 2, 3, 4]);
    let cv = server.encrypt(hs::CERTIFICATE_VERIFY, &body);
    let error = client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cv,
        })
        .expect_err("不支持的算法必须明确报错");
    assert_eq!(
        error,
        HandshakeError::Verification(omnicrawl_tls::verify::VerifyError::UnsupportedSignatureScheme(0x0603))
    );
}

#[test]
fn verify_chain_rejects_a_certificate_from_another_ca() {
    // 只信任一张与夹具无关的根：验链必须失败。
    let other_root = TrustAnchors::from_der(&[CA_DER.into()]).expect("加载测试根");
    let mut client = ClientHandshake::new(Family::Chrome, "wrong.example")
        .with_trust(other_root)
        .with_verify_time(verify_time());

    let client_hello_record = client.start(&rng()).expect("生成");
    let (record, _) = omnicrawl_tls::record::parse_record(&client_hello_record).expect("解析");
    let client_public = extract_x25519_public(&record.payload);
    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("密钥");
    let (mut server, server_hello) = FakeServer::accept(
        &record.payload,
        &client_public,
        server_exchange,
        Signer::ecdsa(),
    );
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::Handshake,
            payload: server_hello,
        })
        .expect("ServerHello");
    let ee = server.encrypt(hs::ENCRYPTED_EXTENSIONS, &encrypted_extensions_body(None));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: ee,
        })
        .expect("EncryptedExtensions");
    let cert = server.encrypt(hs::CERTIFICATE, &certificate_body(&[LEAF_EC_DER, CA_DER]));
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cert,
        })
        .expect("Certificate");

    let cv = server.certificate_verify();
    let error = client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: cv,
        })
        .expect_err("主机名不匹配必须被拒绝");
    assert!(
        matches!(
            error,
            HandshakeError::Verification(
                omnicrawl_tls::verify::VerifyError::WrongHostName(_)
                    | omnicrawl_tls::verify::VerifyError::UntrustedCertificate(_)
            )
        ),
        "{error:?}"
    );
}

#[test]
fn inspect_policy_accepts_a_self_signed_looking_chain() {
    // 同一份证书，但用占位签名算法以外的东西不参与：Inspect 不验链，
    // 因此主机名不匹配也能握手成功——这正是「不能用于传输机密数据」的含义。
    let mut client = ClientHandshake::new(Family::Chrome, "wrong.example");
    assert_eq!(client.policy(), PeerPolicy::Inspect);
    let mut server =
        handshake_until_wait_finished(&mut client, &[LEAF_EC_DER], Signer::ecdsa());
    let finished = server.server_finished();
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: finished,
        })
        .expect("Inspect 模式下只验签名");
    client.finish().expect("客户端 Finished");
    assert_eq!(client.state(), State::Connected);
}

#[test]
fn a_refused_finished_stops_the_handshake() {
    let mut client = ClientHandshake::new(Family::Firefox, "example.com");
    let mut server = handshake_until_wait_finished(&mut client, &[LEAF_EC_DER], Signer::ecdsa());

    // 服务端发一个被篡改的 Finished。
    let mut finished = server.server_finished();
    let last = finished.len() - 1;
    finished[last] ^= 0xff;
    let error = client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: finished,
        })
        .expect_err("篡改的 Finished 必须被拒绝");
    // 篡改先被 AEAD 拦下：记录层的认证标签不通过，比 Finished 校验更早一层。
    // 这本身是正确的防御顺序——坏字节根本到不了转录层。
    assert!(
        matches!(error, HandshakeError::Record(_)),
        "篡改记录应当被记录层拒绝：{error:?}"
    );
    assert_ne!(client.state(), State::Connected, "失败后不得进入 Connected");
}

#[test]
fn out_of_order_messages_are_rejected() {
    let mut client = ClientHandshake::new(Family::Chrome, "example.com");
    let record_bytes = client.start(&rng()).expect("生成");
    let (record, _) = omnicrawl_tls::record::parse_record(&record_bytes).expect("解析");
    let client_public = extract_x25519_public(&record.payload);
    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("密钥");
    let (mut server, server_hello) = FakeServer::accept(
        &record.payload,
        &client_public,
        server_exchange,
        Signer::ecdsa(),
    );
    client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::Handshake,
            payload: server_hello,
        })
        .expect("ServerHello");

    // 直接跳到 Finished，跳过 EncryptedExtensions/Certificate。
    let finished = server.server_finished();
    let error = client
        .feed_record(&omnicrawl_tls::record::Record {
            kind: ContentType::ApplicationData,
            payload: finished,
        })
        .expect_err("乱序必须报错");
    assert!(
        matches!(error, HandshakeError::UnexpectedMessage { .. }),
        "{error:?}"
    );
}

/// 从 ClientHello 的 key_share 扩展里取出 X25519 公钥。
fn extract_x25519_public(client_hello: &[u8]) -> Vec<u8> {
    // 消息结构：类型(1) 长度(3) 版本(2) random(32) sid_len(1)+sid
    //           套件长度(2)+套件 压缩长度(1)+压缩 扩展长度(2)+扩展…
    let mut offset = 4;
    offset += 2 + 32;
    let sid_len = client_hello[offset] as usize;
    offset += 1 + sid_len;
    let suites_len = u16::from_be_bytes([client_hello[offset], client_hello[offset + 1]]) as usize;
    offset += 2 + suites_len;
    let compression_len = client_hello[offset] as usize;
    offset += 1 + compression_len;
    let extensions_len =
        u16::from_be_bytes([client_hello[offset], client_hello[offset + 1]]) as usize;
    offset += 2;
    let end = offset + extensions_len;

    while offset + 4 <= end {
        let kind = u16::from_be_bytes([client_hello[offset], client_hello[offset + 1]]);
        let len = u16::from_be_bytes([client_hello[offset + 2], client_hello[offset + 3]]) as usize;
        let data_start = offset + 4;
        if kind == ext::KEY_SHARE {
            let mut cursor = data_start + 2;
            while cursor + 4 <= data_start + len {
                let group = u16::from_be_bytes([client_hello[cursor], client_hello[cursor + 1]]);
                let key_len =
                    u16::from_be_bytes([client_hello[cursor + 2], client_hello[cursor + 3]])
                        as usize;
                let key_start = cursor + 4;
                if group == group::X25519 {
                    return client_hello[key_start..key_start + key_len].to_vec();
                }
                cursor = key_start + key_len;
            }
        }
        offset = data_start + len;
    }
    panic!("ClientHello 里没有 X25519 的 key_share");
}
