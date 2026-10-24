//! 驱动层的端到端用例：内存里模拟一个完整的 TLS 1.3 + HTTP/2 服务端。
//!
//! 这里的服务端不是在「假装握手」——它用与客户端同一套原语（同一份 HKDF、同一份记录层、
//! 同一份 HPACK）真的完成 1.3 握手，然后真的收发 HTTP/2 帧。因此这组用例覆盖的是
//! **驱动层的编排逻辑**：读记录的时机、写 ClientHello 的顺序、ALPN 检查、窗口回补、
//! 响应拼装。
//!
//! 关键设计：服务端在独立线程里跑，通过一对内存管道与客户端相连。这样
//! `Connection::connect` 里的阻塞读写是真实发生的，而不是被桩替换掉。

use std::thread;

use omnicrawl_tls::driver::{Application, Connection, DriverError, Response};
use omnicrawl_tls::handshake::hs;
use omnicrawl_tls::hpack::{Decoder, Encoder, Header};
use omnicrawl_tls::http2::{
    encode_frame, parse_frame, setting_id, ErrorCode, Frame, Setting, DEFAULT_MAX_FRAME_SIZE,
};
use omnicrawl_tls::keyschedule::{CipherSuite, Transcript};
use omnicrawl_tls::profile::Family;
use omnicrawl_tls::record::{parse_record, ContentType, Record, RecordKey};

fn rng() -> ring::rand::SystemRandom {
    ring::rand::SystemRandom::new()
}

const CA_DER: &[u8] = include_bytes!("fixtures/ca.der");
const LEAF_EC_DER: &[u8] = include_bytes!("fixtures/leaf-ec.der");
const LEAF_EC_KEY: &[u8] = include_bytes!("fixtures/leaf-ec-key.p8");

/// 一对通过回环地址相连的 socket。
struct Pipe;

impl Pipe {
    fn pair() -> (std::net::TcpStream, std::net::TcpStream) {
        // 用本地回环真实 socket：不引入额外依赖，且读写语义与真实网络一致。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
        let address = listener.local_addr().expect("取本地地址");
        let join = thread::spawn(move || {
            let stream = std::net::TcpStream::connect(address);
            stream.inspect(|stream| {
                stream
                    .set_nodelay(true)
                    .expect("禁用 Nagle，避免测试因合并小包而挂起");
            })
        });
        let (server_side, _) = listener.accept().expect("接受连接");
        server_side.set_nodelay(true).expect("禁用 Nagle");
        // 给两端都设读超时：死锁时宁可失败也不要挂住 CI。
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("服务端读超时");
        let client_side = join.join().expect("连接线程").expect("连接");
        client_side
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("客户端读超时");
        (client_side, server_side)
    }
}

fn kind_len(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn handshake_msg(kind: u8, body: &[u8]) -> Vec<u8> {
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

/// 从 ClientHello 里取出 X25519 公钥。
fn extract_x25519_public(client_hello: &[u8]) -> Vec<u8> {
    use omnicrawl_tls::{ext, group};
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
                if group == group::X25519 {
                    return client_hello[cursor + 4..cursor + 4 + key_len].to_vec();
                }
                cursor += 4 + key_len;
            }
        }
        offset = data_start + len;
    }
    panic!("ClientHello 里没有 X25519 的 key_share");
}

/// 服务端一侧的完整状态。
struct ServerTls {
    suite: CipherSuite,
    transcript: Transcript,
    handshake_secret: Vec<u8>,
    application_secrets: Option<omnicrawl_tls::keyschedule::ApplicationSecrets>,
    read: RecordKey,
    write: RecordKey,
    inbound: Vec<u8>,
}

impl ServerTls {
    /// 从 ClientHello 推进到服务端 Finished 已发出、等待客户端 Finished。
    fn accept(stream: &mut std::net::TcpStream) -> Self {
        Self::accept_with_alpn(stream, &["h2", "http/1.1"])
    }

    /// 同上，但自定义 ALPN 列表（用于验证协商失败的处理）。
    fn accept_with_alpn(stream: &mut std::net::TcpStream, alpn: &[&str]) -> Self {
        let mut inbound = Vec::new();
        let client_hello = loop {
            match parse_record(&inbound) {
                Ok((record, used)) => {
                    inbound.drain(..used);
                    break record.payload;
                }
                Err(_) => {
                    let mut buffer = [0u8; 8192];
                    let read = std::io::Read::read(stream, &mut buffer).expect("读取 ClientHello");
                    assert!(read > 0, "客户端在发 ClientHello 之前就断了");
                    inbound.extend_from_slice(&buffer[..read]);
                }
            }
        };
        let client_public = extract_x25519_public(&client_hello);

        let suite = CipherSuite::Aes128GcmSha256;
        let mut transcript = Transcript::new(suite);
        transcript.push(&client_hello);

        let server_exchange = omnicrawl_tls::X25519Exchange::generate(&rng()).expect("服务端密钥");
        let server_public = server_exchange.public_key.clone();
        let shared = server_exchange.agree(&client_public).expect("协商");

        let mut body = Vec::new();
        body.extend_from_slice(&0x0303u16.to_be_bytes());
        body.extend_from_slice(&[0x5a; 32]);
        body.push(0);
        body.extend_from_slice(&suite_as_u16(suite).to_be_bytes());
        body.push(0);
        let mut extensions = Vec::new();
        extensions.extend_from_slice(&kind_len(0x002b, &[0x03, 0x04]));
        let mut share = Vec::new();
        share.extend_from_slice(&0x001du16.to_be_bytes());
        share.extend_from_slice(&(server_public.len() as u16).to_be_bytes());
        share.extend_from_slice(&server_public);
        extensions.extend_from_slice(&kind_len(0x0033, &share));
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let server_hello = handshake_msg(hs::SERVER_HELLO, &body);
        transcript.push(&server_hello);

        let schedule = omnicrawl_tls::keyschedule::KeySchedule::new(suite);
        let secrets = schedule
            .handshake_secrets(&shared, &transcript.hash())
            .expect("握手密钥");

        let client_write = RecordKey::new(suite.aead(), &secrets.client.key, secrets.client.iv)
            .expect("服务端读密钥");
        let server_write = RecordKey::new(suite.aead(), &secrets.server.key, secrets.server.iv)
            .expect("服务端写密钥");

        let mut server = Self {
            suite,
            transcript,
            handshake_secret: secrets.server_secret.clone(),
            application_secrets: None,
            read: client_write,
            write: server_write,
            inbound,
        };

        // ServerHello 是明文记录。
        let record = Record {
            kind: ContentType::Handshake,
            payload: server_hello.clone(),
        };
        let encoded = record.encode(0x0303).expect("编码");
        std::io::Write::write_all(stream, &encoded).expect("发送 ServerHello");

        // 加密部分：EncryptedExtensions / Certificate / CertificateVerify / Finished。
        let mut ee_extensions = Vec::new();
        let mut alpn_list = Vec::new();
        for protocol in alpn {
            alpn_list.push(protocol.len() as u8);
            alpn_list.extend_from_slice(protocol.as_bytes());
        }
        let mut alpn_data = Vec::new();
        alpn_data.extend_from_slice(&(alpn_list.len() as u16).to_be_bytes());
        alpn_data.extend_from_slice(&alpn_list);
        ee_extensions.extend_from_slice(&kind_len(0x0010, &alpn_data));
        let mut ee_body = Vec::new();
        ee_body.extend_from_slice(&(ee_extensions.len() as u16).to_be_bytes());
        ee_body.extend_from_slice(&ee_extensions);
        server.send_handshake(stream, hs::ENCRYPTED_EXTENSIONS, &ee_body);

        server.send_handshake(
            stream,
            hs::CERTIFICATE,
            &certificate_body(&[LEAF_EC_DER, CA_DER]),
        );

        let message = omnicrawl_tls::session::signed_message(&server.transcript.hash());
        let key = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            LEAF_EC_KEY,
            &rng(),
        )
        .expect("加载私钥");
        let signature = key.sign(&rng(), &message).expect("签名").as_ref().to_vec();
        let mut cv_body = Vec::new();
        cv_body.extend_from_slice(&0x0403u16.to_be_bytes());
        cv_body.extend_from_slice(&(signature.len() as u16).to_be_bytes());
        cv_body.extend_from_slice(&signature);
        server.send_handshake(stream, hs::CERTIFICATE_VERIFY, &cv_body);

        let verify_data = omnicrawl_tls::verify::finished_verify_data(
            suite,
            &server.handshake_secret,
            &server.transcript,
        )
        .expect("服务端 Finished");
        let finished = handshake_msg(hs::FINISHED, &verify_data);
        server.write_record(stream, &finished);
        server.transcript.push(&finished);

        // 派生应用密钥。注意：**读密钥此刻不能切**——客户端 Finished 仍用握手密钥
        // 加密，要等 `finish_handshake` 收到它之后才能切。写密钥可以立刻切，因为
        // 服务端接下来发的都是应用数据。
        let application = secrets
            .application_secrets(&server.transcript.hash())
            .expect("应用密钥");
        server.application_secrets = Some(application);
        let application = server.application_secrets.as_ref().expect("刚设置");
        server.write = RecordKey::new(suite.aead(), &application.server.key, application.server.iv)
            .expect("服务端应用写密钥");
        server
    }

    fn send_handshake(&mut self, stream: &mut std::net::TcpStream, kind: u8, body: &[u8]) {
        let raw = handshake_msg(kind, body);
        self.transcript.push(&raw);
        self.write_record(stream, &raw);
    }

    fn write_record(&mut self, stream: &mut std::net::TcpStream, plaintext: &[u8]) {
        let sealed = self
            .write
            .seal(ContentType::Handshake, plaintext)
            .expect("封装");
        let encoded = sealed.encode(0x0303).expect("编码");
        std::io::Write::write_all(stream, &encoded).expect("发送记录");
    }

    /// 把明文 HTTP/2 字节切片后发出。
    fn write_h2(&mut self, stream: &mut std::net::TcpStream, plaintext: &[u8]) {
        for chunk in plaintext.chunks(16_384) {
            let sealed = self
                .write
                .seal(ContentType::ApplicationData, chunk)
                .expect("封装");
            let encoded = sealed.encode(0x0303).expect("编码");
            std::io::Write::write_all(stream, &encoded).expect("发送应用数据");
        }
    }

    /// 读一条记录并解密成内层明文。
    fn read_plaintext(&mut self, stream: &mut std::net::TcpStream) -> Vec<u8> {
        loop {
            match parse_record(&self.inbound) {
                Ok((record, used)) => {
                    self.inbound.drain(..used);
                    if record.kind == ContentType::ChangeCipherSpec {
                        continue;
                    }
                    let (_kind, body) = self.read.open(&record).expect("解密");
                    return body;
                }
                Err(_) => {
                    let mut buffer = [0u8; 8192];
                    let read = std::io::Read::read(stream, &mut buffer).expect("读取");
                    assert!(read > 0, "客户端关闭了连接");
                    self.inbound.extend_from_slice(&buffer[..read]);
                }
            }
        }
    }

    /// 尝试读一条记录；对端关闭时返回 `None`。
    ///
    /// 与 [`Self::read_plaintext`] 的区别只在于「关闭」是正常退出而不是错误：
    /// 服务端主循环需要它来判断何时结束。
    fn try_read_plaintext(&mut self, stream: &mut std::net::TcpStream) -> Option<Vec<u8>> {
        loop {
            match parse_record(&self.inbound) {
                Ok((record, used)) => {
                    self.inbound.drain(..used);
                    if record.kind == ContentType::ChangeCipherSpec {
                        continue;
                    }
                    let (_kind, body) = self.read.open(&record).ok()?;
                    return Some(body);
                }
                Err(_) => {
                    let mut buffer = [0u8; 8192];
                    match std::io::Read::read(stream, &mut buffer) {
                        Ok(0) => return None,
                        Ok(read) => self.inbound.extend_from_slice(&buffer[..read]),
                        Err(_) => return None,
                    }
                }
            }
        }
    }

    /// 校验客户端的 Finished，完成握手。
    ///
    /// 这里只校验结构：客户端的 verify_data 由握手阶段的 `client_secret` 派生，
    /// 而本假服务端在切到应用密钥后没有保留它——真实服务端当然会校验，
    /// 本用例的目的是验证驱动层的编排，不是复验 TLS 的密码学。
    fn finish_handshake(&mut self, stream: &mut std::net::TcpStream) {
        let plaintext = self.read_plaintext(stream);
        assert_eq!(plaintext[0], hs::FINISHED);
        assert!(
            plaintext.len() == 4 + 32,
            "客户端 Finished 长度应为 4+32，实际 {}",
            plaintext.len()
        );
        self.transcript.push(&plaintext);
        // 收到客户端 Finished 之后才切读密钥——此前它一直是握手读密钥。
        let application = self.application_secrets.as_ref().expect("应用密钥已派生");
        self.read = RecordKey::new(
            self.suite.aead(),
            &application.client.key,
            application.client.iv,
        )
        .expect("服务端应用读密钥");
    }
}

fn suite_as_u16(suite: CipherSuite) -> u16 {
    match suite {
        CipherSuite::Aes128GcmSha256 => 0x1301,
        CipherSuite::Aes256GcmSha384 => 0x1302,
        CipherSuite::ChaCha20Poly1305Sha256 => 0x1303,
    }
}

/// 服务端主循环：握手之后按请求回响应。
///
/// 刻意把「读一条记录得到明文」与「解析明文里的帧」分开成两个缓冲：
/// 混在一个 `Vec` 里会把密文字节当明文帧解析，进而死循环。
fn serve(stream: &mut std::net::TcpStream, mut respond: impl FnMut(u32, &[Header]) -> Vec<u8>) {
    let mut tls = ServerTls::accept(stream);
    tls.finish_handshake(stream);

    let mut decoder = Decoder::new(4096);
    let mut encoder = Encoder::new(4096);
    // 明文侧：可能停在半个帧上。
    let mut plain: Vec<u8> = Vec::new();
    let mut preface_skipped = false;

    loop {
        // 先尽量把 plain 里的完整帧处理掉。
        let mut offset = 0;
        let mut requests = Vec::new();
        while offset < plain.len() {
            match parse_frame(&plain[offset..], DEFAULT_MAX_FRAME_SIZE) {
                Ok((frame, used)) => {
                    offset += used;
                    match frame {
                        Frame::Settings { ack, .. } => {
                            if !ack {
                                let settings = encode_frame(&Frame::Settings {
                                    ack: false,
                                    settings: Vec::new(),
                                })
                                .expect("编码 SETTINGS");
                                tls.write_h2(stream, &settings);
                                let ack_frame = encode_frame(&Frame::Settings {
                                    ack: true,
                                    settings: Vec::new(),
                                })
                                .expect("编码 ACK");
                                tls.write_h2(stream, &ack_frame);
                            }
                        }
                        Frame::Headers {
                            stream_id,
                            fragment,
                            end_headers,
                            ..
                        } => {
                            assert!(end_headers, "本测试不切分头部块");
                            let headers = decoder.decode(&fragment).expect("解请求头");
                            requests.push((stream_id, headers));
                        }
                        Frame::WindowUpdate { .. } => {}
                        Frame::Data { .. } => {}
                        other => panic!("服务端不期望收到 {other:?}"),
                    }
                }
                // 半个帧：等更多字节。
                Err(_) => break,
            }
        }
        plain.drain(..offset);

        for (stream_id, headers) in requests {
            let payload = respond(stream_id, &headers);
            let _ = &mut encoder;
            tls.write_h2(stream, &payload);
        }

        // 再读一条记录补充明文。读不到就重试：客户端可能还在写。
        let chunk = match tls.try_read_plaintext(stream) {
            Some(chunk) => chunk,
            None => return,
        };
        if chunk.is_empty() {
            continue;
        }
        if !preface_skipped {
            // 前言必须在进入 `plain` **之前**剥掉：留在里面会让 parse_frame
            // 永远从 "PRI " 开始，一个帧都解析不出来。
            let skip = if chunk.starts_with(b"PRI * HTTP/2.0") {
                24
            } else {
                0
            };
            preface_skipped = true;
            plain.extend_from_slice(&chunk[skip..]);
        } else {
            plain.extend_from_slice(&chunk);
        }
    }
}

fn build_response(encoder: &mut Encoder, stream_id: u32, status: &str, body: &[u8]) -> Vec<u8> {
    let headers = vec![
        Header::new(":status", status),
        Header::new("content-type", "text/plain"),
        Header::new("content-length", body.len().to_string()),
    ];
    let block = encoder.encode(&headers);
    let mut out = encode_frame(&Frame::Headers {
        stream_id,
        end_stream: body.is_empty(),
        end_headers: true,
        fragment: block,
    })
    .expect("HEADERS");
    if !body.is_empty() {
        for (index, chunk) in body.chunks(DEFAULT_MAX_FRAME_SIZE).enumerate() {
            let last = (index + 1) * DEFAULT_MAX_FRAME_SIZE >= body.len();
            out.extend_from_slice(
                &encode_frame(&Frame::Data {
                    stream_id,
                    end_stream: last,
                    data: chunk.to_vec(),
                })
                .expect("DATA"),
            );
        }
    }
    out
}

#[test]
fn a_full_tls_and_http2_request_round_trips_through_real_sockets() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        serve(&mut server_side, |stream_id, headers| {
            let path = headers
                .iter()
                .find(|header| header.name == ":path")
                .map(|header| header.value.clone())
                .unwrap_or_default();
            let mut encoder = Encoder::new(4096);
            let body = format!("path was {path}");
            build_response(&mut encoder, stream_id, "200", body.as_bytes())
        });
    });

    let mut connection = Connection::connect(client_side, Family::Chrome, "example.com", None)
        .expect("握手应当完成");
    assert_eq!(connection.alpn(), Some("h2"), "必须协商出 h2");
    assert!(connection.peer_certificate().is_some(), "应当拿到叶证书");

    let response = connection.get("/hello").expect("请求应当成功");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"path was /hello");
    assert_eq!(response.header("content-type"), Some("text/plain"));

    drop(connection);
    let _ = server.join();
}

#[test]
fn requests_on_the_same_connection_reuse_the_session() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut counter = 0u32;
        serve(&mut server_side, |stream_id, _headers| {
            counter += 1;
            let mut encoder = Encoder::new(4096);
            let body = format!("response #{counter}");
            build_response(&mut encoder, stream_id, "200", body.as_bytes())
        });
    });

    let mut connection =
        Connection::connect(client_side, Family::Firefox, "example.com", None).expect("握手");
    let first = connection.get("/one").expect("第一个请求");
    assert_eq!(first.body, b"response #1");
    let second = connection.get("/two").expect("第二个请求");
    assert_eq!(second.body, b"response #2");

    drop(connection);
    let _ = server.join();
}

#[test]
fn a_large_response_body_is_reassembled_across_records() {
    let (client_side, mut server_side) = Pipe::pair();
    let body: Vec<u8> = (0..120_000).map(|index| (index % 251) as u8).collect();
    let expected = body.clone();
    let server = thread::spawn(move || {
        serve(&mut server_side, move |stream_id, _headers| {
            let mut encoder = Encoder::new(4096);
            build_response(&mut encoder, stream_id, "200", &body)
        });
    });

    let mut connection =
        Connection::connect(client_side, Family::Safari, "example.com", None).expect("握手");
    let response = connection.get("/big").expect("请求");
    assert_eq!(response.status, 200);
    assert_eq!(response.body.len(), expected.len(), "长度必须一致");
    assert_eq!(response.body, expected, "内容必须逐字节一致");

    drop(connection);
    let _ = server.join();
}

#[test]
fn a_post_with_a_body_reaches_the_server() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        serve(&mut server_side, |stream_id, headers| {
            let method = headers
                .iter()
                .find(|header| header.name == ":method")
                .map(|header| header.value.clone())
                .unwrap_or_default();
            let mut encoder = Encoder::new(4096);
            build_response(&mut encoder, stream_id, "201", method.as_bytes())
        });
    });

    let mut connection =
        Connection::connect(client_side, Family::Chrome, "example.com", None).expect("握手");
    let response = connection
        .request("POST", "/submit", Some(b"payload"))
        .expect("POST 应当成功");
    assert_eq!(response.status, 201);
    assert_eq!(response.body, b"POST", "服务端应看到 POST");

    drop(connection);
    let _ = server.join();
}

#[test]
fn a_peer_that_only_offers_http11_is_served_over_http11() {
    // 服务端完整跑完握手，但 ALPN 只提供 http/1.1。以前这里必须拒绝；现在同一套
    // TLS 记录层会退到 HTTP/1.1 报文，指纹照样生效。
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept_with_alpn(&mut server_side, &["http/1.1"]);
        tls.finish_handshake(&mut server_side);
        let request = tls.read_plaintext(&mut server_side);
        let text = String::from_utf8(request).expect("请求应当是 UTF-8");
        assert!(
            text.starts_with("GET /only11 HTTP/1.1\r\n"),
            "应当收到 1.1 请求行：{text}"
        );
        assert!(text.contains("Host: example.com\r\n"), "{text}");
        assert!(text.contains("Connection: close\r\n"), "{text}");
        let body = "hello over 1.1";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        tls.write_h2(&mut server_side, response.as_bytes());
        // 等客户端读到响应后关闭（客户端不再发请求）。
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 64];
        let _ = std::io::Read::read(&mut server_side, &mut sink);
    });

    let mut connection = Connection::connect(client_side, Family::Chrome, "example.com", None)
        .expect("握手应当完成");
    assert_eq!(connection.alpn(), Some("http/1.1"));
    assert_eq!(connection.protocol(), Application::Http1);

    let response = connection.get("/only11").expect("1.1 请求应当成功");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello over 1.1");
    assert_eq!(response.header("content-type"), Some("text/plain"));

    drop(connection);
    let _ = server.join();
}

/// 1.1 通路要处理的三种定界方式里最容易漏的是分块，以及「没有长度、读到关闭」。
#[test]
fn http11_chunked_and_until_eof_bodies_are_decoded() {
    for (label, response, expected) in [
        (
            "chunked",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            "hello world",
        ),
        (
            "until-eof",
            "HTTP/1.1 200 OK\r\n\r\nno length here",
            "no length here",
        ),
        // 1xx 是中间响应，必须跳过继续等真正的头。
        (
            "early-hints",
            "HTTP/1.1 103 Early Hints\r\nLink: </a.css>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
            "ok",
        ),
    ] {
        let (client_side, mut server_side) = Pipe::pair();
        let server = thread::spawn(move || {
            let mut tls = ServerTls::accept_with_alpn(&mut server_side, &["http/1.1"]);
            tls.finish_handshake(&mut server_side);
            let _request = tls.read_plaintext(&mut server_side);
            tls.write_h2(&mut server_side, response.as_bytes());
            server_side
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("设置读超时");
            let mut sink = [0u8; 64];
            let _ = std::io::Read::read(&mut server_side, &mut sink);
        });

        let mut connection = Connection::connect(client_side, Family::Chrome, "example.com", None)
            .expect("握手");
        let response = connection.get("/body").unwrap_or_else(|error| {
            panic!("{label} 应当成功：{error}")
        });
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            expected,
            "{label} 的响应体不对"
        );
        drop(connection);
        let _ = server.join();
    }
}

#[test]
fn a_peer_without_alpn_is_reported_as_unsupported() {
    // 完全没有 ALPN 时无法判断协议：既不能猜 h2 也不能猜 1.1。
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept_with_alpn(&mut server_side, &[]);
        tls.finish_handshake(&mut server_side);
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 256];
        let _ = std::io::Read::read(&mut server_side, &mut sink);
    });

    match Connection::connect(client_side, Family::Chrome, "example.com", None) {
        Err(DriverError::UnsupportedAlpn(None)) => {}
        Err(other) => panic!("应当报「没有协商 ALPN」，得到 {other:?}"),
        Ok(_) => panic!("没有 ALPN 时不应该成功"),
    }
    let _ = server.join();
}

/// 服务端把响应与 GOAWAY(NO_ERROR) 一起发来是合法的收尾（RFC 9113 §6.8）。
///
/// 这里的顺序是刻意的：GOAWAY 放在响应**之前**的同一个 TCP 段里。若驱动层一看到
/// GOAWAY 就判失败，就会「明明拿到了 200 却报错」。真实站点这么干过
/// （tls.browserleaks.com 的 HTTP/2 端点）。
#[test]
fn a_goaway_together_with_the_response_does_not_lose_the_response() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept(&mut server_side);
        tls.finish_handshake(&mut server_side);
        let _preamble = tls.read_plaintext(&mut server_side);
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: Vec::new(),
        })
        .expect("编码 SETTINGS");
        tls.write_h2(&mut server_side, &settings);
        // 等客户端把请求发过来。
        let mut seen_headers = false;
        while !seen_headers {
            let plaintext = tls.read_plaintext(&mut server_side);
            let mut offset = 0;
            while offset < plaintext.len() {
                let (frame, used) = parse_frame(&plaintext[offset..], DEFAULT_MAX_FRAME_SIZE)
                    .expect("解析客户端帧");
                offset += used;
                if matches!(frame, Frame::Headers { .. }) {
                    seen_headers = true;
                }
            }
        }
        let mut encoder = Encoder::new(4096);
        let mut payload = build_response(&mut encoder, 1, "200", b"body survives goaway");
        // 关键：GOAWAY 先于响应到达，且 last_stream_id 覆盖流 1。
        payload.splice(
            0..0,
            encode_frame(&Frame::GoAway {
                last_stream_id: 1,
                code: ErrorCode::NoError as u32,
                debug: Vec::new(),
            })
            .expect("编码 GOAWAY"),
        );
        tls.write_h2(&mut server_side, &payload);
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 64];
        let _ = std::io::Read::read(&mut server_side, &mut sink);
    });

    let mut connection =
        Connection::connect(client_side, Family::Chrome, "example.com", None).expect("握手");
    let response = connection.get("/").expect("GOAWAY 不该吞掉已完成的流");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"body survives goaway");
    let _ = server.join();
}

/// 但 `last_stream_id` 之外的新流必须报错：那才是真正的「对端没处理」。
#[test]
fn a_goaway_that_excludes_the_stream_is_an_error() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept(&mut server_side);
        tls.finish_handshake(&mut server_side);
        let _preamble = tls.read_plaintext(&mut server_side);
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: Vec::new(),
        })
        .expect("编码 SETTINGS");
        tls.write_h2(&mut server_side, &settings);
        let mut seen_headers = false;
        while !seen_headers {
            let plaintext = tls.read_plaintext(&mut server_side);
            let mut offset = 0;
            while offset < plaintext.len() {
                let (frame, used) = parse_frame(&plaintext[offset..], DEFAULT_MAX_FRAME_SIZE)
                    .expect("解析客户端帧");
                offset += used;
                if matches!(frame, Frame::Headers { .. }) {
                    seen_headers = true;
                }
            }
        }
        // last_stream_id = 0：流 1 不在处理范围内。
        let goaway = encode_frame(&Frame::GoAway {
            last_stream_id: 0,
            code: ErrorCode::NoError as u32,
            debug: b"nothing processed".to_vec(),
        })
        .expect("编码 GOAWAY");
        tls.write_h2(&mut server_side, &goaway);
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 64];
        let _ = std::io::Read::read(&mut server_side, &mut sink);
    });

    let mut connection =
        Connection::connect(client_side, Family::Chrome, "example.com", None).expect("握手");
    match connection.get("/") {
        Err(DriverError::BadResponse(detail)) => {
            assert!(detail.contains("GOAWAY"), "{detail}");
            assert!(
                detail.contains("nothing processed"),
                "debug 载荷要带上：{detail}"
            );
        }
        other => panic!("应当报 GOAWAY 错误，得到 {other:?}"),
    }
    let _ = server.join();
}

/// SETTINGS 与 GOAWAY 出现在同一批读取里时，GOAWAY 不能被吞掉。
///
/// 设置交换阶段只关心「收到 PeerSettings 了吗」，若顺手把同批的 GOAWAY 丢掉，连接
/// 看起来是活的，后面的请求却会撞上「对端已关闭」，错误出现在完全无关的地方。
#[test]
fn a_goaway_during_the_settings_exchange_fails_the_connection() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept(&mut server_side);
        tls.finish_handshake(&mut server_side);
        let _preamble = tls.read_plaintext(&mut server_side);
        let mut batch = encode_frame(&Frame::Settings {
            ack: false,
            settings: Vec::new(),
        })
        .expect("编码 SETTINGS");
        batch.extend_from_slice(
            &encode_frame(&Frame::GoAway {
                last_stream_id: 0,
                code: ErrorCode::NoError as u32,
                debug: b"not for you".to_vec(),
            })
            .expect("编码 GOAWAY"),
        );
        tls.write_h2(&mut server_side, &batch);
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 64];
        let _ = std::io::Read::read(&mut server_side, &mut sink);
    });

    match Connection::connect(client_side, Family::Chrome, "example.com", None) {
        Err(DriverError::BadResponse(detail)) => {
            assert!(detail.contains("GOAWAY"), "{detail}");
            assert!(detail.contains("not for you"), "debug 载荷要带上：{detail}");
        }
        Ok(_) => panic!("设置交换阶段的 GOAWAY 应当让连接失败"),
        Err(other) => panic!("应当是 GOAWAY 错误，得到 {other:?}"),
    }
    let _ = server.join();
}

#[test]
fn connecting_to_a_closed_port_reports_an_io_error() {
    // 绑一个端口再立刻释放，随后连接必然失败。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定");
    let address = listener.local_addr().expect("地址");
    drop(listener);
    let stream = std::net::TcpStream::connect(address).expect_err("端口已释放");
    let error: DriverError = stream.into();
    assert!(matches!(error, DriverError::Io(_)), "{error:?}");
}

#[test]
fn a_server_reset_mid_request_surfaces_as_an_error() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        // 握手完成后立刻发 RST_STREAM 然后关闭。
        let mut tls = ServerTls::accept(&mut server_side);
        tls.finish_handshake(&mut server_side);
        let _preamble = tls.read_plaintext(&mut server_side);
        // 先完成设置交换，客户端才会结束 connect。
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: Vec::new(),
        })
        .expect("编码 SETTINGS");
        tls.write_h2(&mut server_side, &settings);
        let _client_settings = tls.read_plaintext(&mut server_side);
        let reset = encode_frame(&Frame::RstStream {
            stream_id: 1,
            code: ErrorCode::InternalError as u32,
        })
        .expect("编码");
        tls.write_h2(&mut server_side, &reset);
        // 发完 RST 后保持连接存活，否则客户端先看到的是 TCP 中止而不是 RST 帧。
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 256];
        for _ in 0..8 {
            match std::io::Read::read(&mut server_side, &mut sink) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });

    let mut connection = Connection::connect(client_side, Family::Chrome, "example.com", None)
        .expect("握手应当完成");
    let result = connection.get("/");
    match result {
        Err(DriverError::BadResponse(detail)) => {
            assert!(
                detail.contains("重置") || detail.contains("GOAWAY"),
                "{detail}"
            )
        }
        Err(other) => panic!("应当是响应错误，得到 {other:?}"),
        Ok(_) => panic!("被重置的流不该返回成功"),
    }
    let _ = server.join();
}

#[test]
fn settings_from_the_server_change_the_local_frame_limit() {
    let (client_side, mut server_side) = Pipe::pair();
    let server = thread::spawn(move || {
        let mut tls = ServerTls::accept(&mut server_side);
        tls.finish_handshake(&mut server_side);
        let _preamble = tls.read_plaintext(&mut server_side);
        // 发一个把 HEADER_TABLE_SIZE 压到 0 的 SETTINGS，再正常回一个响应。
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: vec![Setting {
                id: setting_id::HEADER_TABLE_SIZE,
                value: 0,
            }],
        })
        .expect("编码");
        tls.write_h2(&mut server_side, &settings);
        // 读客户端回的控制帧（可能既有 SETTINGS ACK 也有请求），直到看到 HEADERS。
        loop {
            let plaintext = tls.read_plaintext(&mut server_side);
            let mut offset = 0;
            let mut saw_headers = false;
            while offset < plaintext.len() {
                let (frame, used) = parse_frame(&plaintext[offset..], DEFAULT_MAX_FRAME_SIZE)
                    .expect("解析客户端帧");
                offset += used;
                if matches!(frame, Frame::Headers { .. }) {
                    saw_headers = true;
                }
            }
            if saw_headers {
                break;
            }
        }
        let mut encoder = Encoder::new(0);
        let payload = build_response(&mut encoder, 1, "200", b"after settings");
        tls.write_h2(&mut server_side, &payload);
        // 保持存活直到客户端主动断开（带超时，避免测试悬挂）。
        server_side
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("设置读超时");
        let mut sink = [0u8; 256];
        while let Ok(read) = std::io::Read::read(&mut server_side, &mut sink) {
            if read == 0 {
                break;
            }
        }
    });

    let mut connection =
        Connection::connect(client_side, Family::Chrome, "example.com", None).expect("握手");
    let response: Response = connection.get("/").expect("请求");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"after settings");
    let _ = server.join();
}
