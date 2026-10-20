//! 会话恢复（TLS 1.3 PSK）的端到端用例：模拟服务端接受 PSK，客户端跳过证书完成握手。
//!
//! 这里最关键的不是「能跑通」，而是三件容易错的事：
//!
//! 1. **binder 覆盖的是截断后的 ClientHello**——算错时服务端会拒绝，但若是自家两端
//!    都用同一个错误算法，测试会「通过」而真实站点失败。因此本文件的服务端**独立**
//!    从收到的字节里截断并复算 binder，不复用客户端的中间值。
//! 2. **接受 PSK 后不发 Certificate / CertificateVerify**（RFC 8446 §4.1.3），
//!    客户端必须据此直接从 EncryptedExtensions 走到 Finished。
//! 3. **密钥调度必须用 PSK 作为 `early_secret` 的 ikm**，否则两侧密钥不同，
//!    第一条加密记录就解不开。

use omnicrawl_tls::client_hello::ext;
use omnicrawl_tls::handshake::hs;
use omnicrawl_tls::keyschedule::{CipherSuite, KeySchedule, Transcript};
use omnicrawl_tls::profile::Family;
use omnicrawl_tls::record::{ContentType, RecordKey};
use omnicrawl_tls::session::{ClientHandshake, State};
use omnicrawl_tls::session_cache::Session;
use omnicrawl_tls::verify::finished_verify_data;

fn rng() -> ring::rand::SystemRandom {
    ring::rand::SystemRandom::new()
}

fn kind_len(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn handshake(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(kind);
    out.push(((body.len() >> 16) & 0xff) as u8);
    out.push(((body.len() >> 8) & 0xff) as u8);
    out.push((body.len() & 0xff) as u8);
    out.extend_from_slice(body);
    out
}

/// 从 ClientHello 里取出 `pre_shared_key` 的 identities 与 binders 位置。
///
/// 返回 `(ticket, binder_offset_in_hello, binder_len)`。**独立**解析而不是复用客户端
/// 计算出的偏移——同一个偏移算错两次就会互相掩盖。
/// 返回 `(ticket, binder 在**完整握手消息**里的偏移, binder 长度)`。
///
/// 偏移加上了 4 字节握手头：binder 的转录包含完整消息，不能只对消息体算。
fn extract_psk(client_hello: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    // 入参是 ClientHello 的**消息体**（不含 4 字节握手头），偏移都相对于它。
    let body = client_hello;
    let mut cursor = 0;
    cursor += 2 + 32; // version + random
    let session_id_len = body[cursor] as usize;
    cursor += 1 + session_id_len;
    let suites_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2 + suites_len;
    let compression_len = body[cursor] as usize;
    cursor += 1 + compression_len;
    let ext_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2;
    let extensions_end = cursor + ext_len;

    while cursor + 4 <= extensions_end {
        let kind = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
        let len = u16::from_be_bytes([body[cursor + 2], body[cursor + 3]]) as usize;
        let data_start = cursor + 4;
        if kind == ext::PRE_SHARED_KEY {
            // identities<2> : { identity<2> ticket, age<4> } ...
            let identities_len = u16::from_be_bytes([body[data_start], body[data_start + 1]]) as usize;
            let identity_start = data_start + 2;
            let ticket_len =
                u16::from_be_bytes([body[identity_start], body[identity_start + 1]]) as usize;
            let ticket = body[identity_start + 2..identity_start + 2 + ticket_len].to_vec();
            // binders 紧跟在 identities 之后。
            let binders_start = identity_start + identities_len;
            let binder_len = u16::from_be_bytes([
                body[binders_start],
                body[binders_start + 1],
            ]) as usize;
            // binder 数据在 binders 长度字段之后；偏移要换算回整个 hello 的坐标。
            return Some((ticket, 4 + binders_start + 2, binder_len));
        }
        cursor = data_start + len;
    }
    None
}

/// 构造一条可恢复的会话：票据 + PSK。
fn a_session() -> Session {
    Session {
        ticket: b"resumption-ticket-abc".to_vec(),
        psk: vec![0x5a; 32],
        ticket_age_add: 0,
        issued_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0),
        lifetime: 7200,
        suite: 0x1301,
        alpn: Some("h2".to_string()),
    }
}

/// 接受 PSK 的服务端：校验 binder、用 PSK 派生密钥、跳过证书直接发 Finished。
struct PskServer {
    suite: CipherSuite,
    transcript: Transcript,
    handshake_secret: Vec<u8>,
    server_write: Option<RecordKey>,
    client_write: Option<RecordKey>,
}

impl PskServer {
    fn accept(
        client_hello: &[u8],
        psk: &[u8],
        server_exchange: omnicrawl_tls::X25519Exchange,
    ) -> (Self, Vec<u8>) {
        let suite = CipherSuite::Aes128GcmSha256;
        let mut transcript = Transcript::new(suite);
        // 转录里放**完整握手消息**（含 4 字节头）：客户端 `push_transcript` 放的就是它，
        // 两侧不一致会让派生出的密钥不同，症状正是「第一条加密记录解不开」。
        let full_client_hello = {
            let mut full = Vec::with_capacity(client_hello.len() + 4);
            full.push(hs::CLIENT_HELLO);
            full.push(((client_hello.len() >> 16) & 0xff) as u8);
            full.push(((client_hello.len() >> 8) & 0xff) as u8);
            full.push((client_hello.len() & 0xff) as u8);
            full.extend_from_slice(client_hello);
            full
        };
        transcript.push(&full_client_hello);

        let (_, binder_offset, binder_len) =
            extract_psk(client_hello).expect("ClientHello 必须带 PSK");
        // binder 的转录是完整握手消息（含 4 字节头），因此这里把头部补回去。
        let mut full = Vec::with_capacity(client_hello.len() + 4);
        full.push(hs::CLIENT_HELLO);
        full.push(((client_hello.len() >> 16) & 0xff) as u8);
        full.push(((client_hello.len() >> 8) & 0xff) as u8);
        full.push((client_hello.len() & 0xff) as u8);
        full.extend_from_slice(client_hello);
        // **独立复算 binder**：用收到的字节截断，而不是信客户端给的中间值。
        let expected = KeySchedule::psk_binder(suite, psk, &full[..binder_offset])
            .expect("服务端计算 binder");
        let actual = &full[binder_offset..binder_offset + binder_len];
        assert_eq!(
            expected.as_slice(),
            actual,
            "服务端独立复算的 binder 必须与客户端一致"
        );

        // 公钥必须在 `agree` 之前取走：它会消费交换对象。
        let server_public = server_exchange.public_key.clone();
        let client_public = extract_x25519_public(client_hello);
        let shared = server_exchange
            .agree(&client_public)
            .expect("服务端协商共享密钥");
        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&[0x5b; 32]);
        body.push(0);
        body.extend_from_slice(&suite.code().to_be_bytes());
        body.push(0);
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&kind_len(ext::SUPPORTED_VERSIONS, &[0x03, 0x04]));
        let mut share = Vec::new();
        share.extend_from_slice(&omnicrawl_tls::group::X25519.to_be_bytes());
        share.extend_from_slice(&(server_public.len() as u16).to_be_bytes());
        share.extend_from_slice(&server_public);
        extensions.extend_from_slice(&kind_len(ext::KEY_SHARE, &share));
        // 关键：回带 pre_shared_key 表示「接受恢复」。
        extensions.extend_from_slice(&kind_len(ext::PRE_SHARED_KEY, &0u16.to_be_bytes()));
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let server_hello = handshake(hs::SERVER_HELLO, &body);
        transcript.push(&server_hello);

        // 用 PSK 作为 early_secret 的 ikm——与客户端必须一致。
        let schedule = KeySchedule::with_psk(suite, Some(psk));
        let secrets = schedule
            .handshake_secrets(&shared, &transcript.hash())
            .expect("服务端派生握手密钥");

        let server = Self {
            suite,
            transcript,
            handshake_secret: secrets.server_secret.clone(),
            server_write: Some(
                RecordKey::new(suite.aead(), &secrets.server.key, secrets.server.iv)
                    .expect("服务端写密钥"),
            ),
            client_write: Some(
                RecordKey::new(suite.aead(), &secrets.client.key, secrets.client.iv)
                    .expect("服务端读密钥"),
            ),
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

    fn open_client_finished(&mut self, payload: &[u8]) -> Vec<u8> {
        let (kind, plaintext) = self
            .client_write
            .as_mut()
            .expect("服务端读密钥")
            .open(&omnicrawl_tls::record::Record {
                kind: ContentType::ApplicationData,
                payload: payload.to_vec(),
            })
            .expect("解密客户端 Finished");
        assert_eq!(kind, ContentType::Handshake);
        self.transcript.push(&plaintext);
        plaintext
    }
}

#[test]
fn psk_resumption_handshake_completes_without_certificates() {
    let session = a_session();
    // 默认 Inspect 策略（不验链）：恢复路径根本不验证书。
    let mut client =
        ClientHandshake::new(Family::Chrome, "example.com").with_session(session.clone());

    let client_hello_record = client.start(&rng()).expect("生成 ClientHello");

    // 从明文记录里取出 ClientHello。
    let hello = extract_handshake_payload(&client_hello_record, hs::CLIENT_HELLO);
    let (ticket, _, _) = extract_psk(&hello).expect("必须带 pre_shared_key");
    assert_eq!(ticket, session.ticket, "票据必须原样出现在 identities 里");

    let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("服务端密钥");
    let (mut server, server_hello) = PskServer::accept(&hello, &session.psk, server_exchange);

    feed_handshake(&mut client, &server_hello).expect("ServerHello");

    assert!(
        client.resumed(),
        "服务端回带 pre_shared_key 后客户端应当认为已恢复"
    );

    // 恢复路径：EncryptedExtensions 之后直接 Finished，没有 Certificate。
    let ee = server.encrypt(hs::ENCRYPTED_EXTENSIONS, &0u16.to_be_bytes());
    feed_encrypted(&mut client, &ee).expect("EncryptedExtensions");
    assert_eq!(client.state(), State::WaitFinished, "必须跳过等证书");

    let finished = server.server_finished();
    feed_encrypted(&mut client, &finished).expect("server Finished");
    // 收到 server Finished 后客户端要先发出自己的 Finished，才进入 Connected。
    assert_eq!(
        client.state(),
        State::SendClientFinished,
        "恢复路径应当只差客户端 Finished"
    );

    // 客户端 Finished 必须能被服务端解开并验过。
    // `finish()` 返回的已经是记录 payload（不含 5 字节记录头）。
    let payload = client.finish().expect("客户端 Finished");
    let plaintext = server.open_client_finished(&payload);
    assert_eq!(plaintext[0], hs::FINISHED);
    assert_eq!(client.state(), State::Connected);
    assert_eq!(plaintext.len(), 4 + 32, "Finished 是 4 字节头 + 32 字节 verify_data");
}

/// 没有会话时必须退回完整握手（不能因为缺 PSK 就报错）。
#[test]
fn without_a_session_the_client_falls_back_to_a_full_handshake() {
    let mut client = ClientHandshake::new(Family::Chrome, "example.com");
    let record = client.start(&rng()).expect("生成 ClientHello");
    let hello = extract_handshake_payload(&record, hs::CLIENT_HELLO);
    assert!(
        extract_psk(&hello).is_none(),
        "没有会话时不该发 pre_shared_key"
    );
    assert!(!client.resumed());
}

/// 服务端不接受 PSK 时必须走完整握手，而不是卡住。
#[test]
fn a_rejected_psk_falls_back_to_the_certificate_path() {
    let session = a_session();
    let mut client = ClientHandshake::new(Family::Chrome, "example.com")
        .with_session(session);
    let record = client.start(&rng()).expect("生成 ClientHello");
    let hello = extract_handshake_payload(&record, hs::CLIENT_HELLO);

    // 服务端的 ServerHello **不带** pre_shared_key：表示拒绝恢复。
    let server_exchange =
        omnicrawl_tls::X25519Exchange::generate(&rng()).expect("服务端密钥");
    // 公钥必须在 `agree` 之前取走：它会消费交换对象。
    let server_public = server_exchange.public_key.clone();
    let client_public = extract_x25519_public(&hello);
    let _shared = server_exchange.agree(&client_public).expect("共享密钥");
    let suite = CipherSuite::Aes128GcmSha256;
    let mut transcript = Transcript::new(suite);
    transcript.push(&hello);
    let mut body = Vec::new();
    body.extend_from_slice(&0x0303u16.to_be_bytes());
    body.extend_from_slice(&[0x5c; 32]);
    body.push(0);
    body.extend_from_slice(&suite.code().to_be_bytes());
    body.push(0);
    let mut extensions = Vec::new();
    extensions.extend_from_slice(&kind_len(ext::SUPPORTED_VERSIONS, &[0x03, 0x04]));
    let mut share = Vec::new();
    share.extend_from_slice(&omnicrawl_tls::group::X25519.to_be_bytes());
    share.extend_from_slice(&(server_public.len() as u16).to_be_bytes());
    share.extend_from_slice(&server_public);
    extensions.extend_from_slice(&kind_len(ext::KEY_SHARE, &share));
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);
    let server_hello = handshake(hs::SERVER_HELLO, &body);
    transcript.push(&server_hello);

    feed_handshake(&mut client, &server_hello).expect("ServerHello");
    assert!(!client.resumed(), "服务端没回带 PSK 就不算恢复");
    assert_eq!(
        client.state(),
        State::WaitEncryptedExtensions,
        "应当进入正常 1.3 流程"
    );
}

// ---- 以下是从「明文记录字节」里取握手消息的辅助函数 ----

/// 从一条 `Handshake` 记录里取出指定类型的握手消息体（不含 4 字节消息头）。
fn extract_handshake_payload(record: &[u8], kind: u8) -> Vec<u8> {
    let payload = extract_record_payload(record);
    assert_eq!(payload[0], kind, "记录里的第一条握手消息类型不符");
    let len = ((payload[1] as usize) << 16) | ((payload[2] as usize) << 8) | payload[3] as usize;
    payload[4..4 + len].to_vec()
}

/// 取一条明文记录（5 字节头）的载荷。
fn extract_record_payload(record: &[u8]) -> Vec<u8> {
    let len = u16::from_be_bytes([record[3], record[4]]) as usize;
    record[5..5 + len].to_vec()
}

/// 把服务端的明文握手记录喂给客户端。
fn feed_handshake(client: &mut ClientHandshake, message: &[u8]) -> Result<(), String> {
    // 入参是**完整握手消息**（含类型与 3 字节长度），不再补头。
    let record = omnicrawl_tls::record::Record {
        kind: ContentType::Handshake,
        payload: message.to_vec(),
    };
    client
        .feed_record(&record)
        .map_err(|error| format!("{error:?}"))
}

/// 把服务端的加密记录喂给客户端。
fn feed_encrypted(client: &mut ClientHandshake, payload: &[u8]) -> Result<(), String> {
    let record = omnicrawl_tls::record::Record {
        kind: ContentType::ApplicationData,
        payload: payload.to_vec(),
    };
    client
        .feed_record(&record)
        .map_err(|error| format!("{error:?}"))
}

/// 从 ClientHello 里取出 X25519 的 key_share 公钥。
fn extract_x25519_public(client_hello: &[u8]) -> Vec<u8> {
    // 入参是 ClientHello 的**消息体**（不含 4 字节握手头）。
    let body = client_hello;
    let mut cursor = 0;
    cursor += 2 + 32;
    let sid_len = body[cursor] as usize;
    cursor += 1 + sid_len;
    let suites_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2 + suites_len;
    let compression_len = body[cursor] as usize;
    cursor += 1 + compression_len;
    let ext_len = u16::from_be_bytes([body[cursor], body[cursor + 1]]) as usize;
    cursor += 2;
    let end = cursor + ext_len;

    while cursor + 4 <= end {
        let kind = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
        let len = u16::from_be_bytes([body[cursor + 2], body[cursor + 3]]) as usize;
        let data_start = cursor + 4;
        if kind == ext::KEY_SHARE {
            let mut inner = data_start + 2;
            while inner + 4 <= data_start + len {
                let group = u16::from_be_bytes([body[inner], body[inner + 1]]);
                let key_len = u16::from_be_bytes([body[inner + 2], body[inner + 3]]) as usize;
                let key_start = inner + 4;
                if group == omnicrawl_tls::group::X25519 {
                    return body[key_start..key_start + key_len].to_vec();
                }
                inner = key_start + key_len;
            }
        }
        cursor = data_start + len;
    }
    panic!("ClientHello 里没有 X25519 的 key_share");
}
