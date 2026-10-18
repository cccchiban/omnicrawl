//! HTTP/2 的端到端用例：真实 TLS 记录层 + HPACK + 帧层 + 连接层。
//!
//! 这组用例与 `http2`/`h2conn` 内部的单测不同：它把 HTTP/2 帧真的塞进 TLS 1.3 的
//! 加密记录里，再在另一边解密出来。这样能抓住一类只在组合时出现的问题——
//! 例如帧被跨记录边界切开（HF 平均 16 KB 一帧、TLS 记录 2^14 上限，两者会错位）。
//!
//! 服务端侧是模拟的：它用同一套原语构造响应，因此任何「客户端自己跟自己自洽、
//! 但与真实服务端不兼容」的实现细节都会在这里露出来。

use omnicrawl_tls::h2conn::{ClientConnection, ConnectionError, Event, StreamState};
use omnicrawl_tls::hpack::{Decoder, Encoder, Header};
use omnicrawl_tls::http2::{encode_frame, flags, parse_frame, ErrorCode, Frame, DEFAULT_MAX_FRAME_SIZE};
use omnicrawl_tls::keyschedule::CipherSuite;
use omnicrawl_tls::record::{ContentType, Record, RecordKey};
use ring::rand::SecureRandom;

fn rng() -> ring::rand::SystemRandom {
    ring::rand::SystemRandom::new()
}

/// 一条模拟的 TLS 1.3 连接。
///
/// TLS 1.3 的两个方向各有独立的写密钥：客户端写 = 服务端读，反之亦然。
/// 用同一把密钥既写又读会立刻解不开——这既是正确性要求，也顺带验证了密钥方向确实分离。
struct Tunnel {
    /// 客户端 → 服务端的密钥（客户端写 / 服务端读）。
    client_to_server: RecordKey,
    /// 服务端 → 客户端的密钥（服务端写 / 客户端读）。
    server_to_client: RecordKey,
    /// 另一个方向上的同一把密钥，供对端解封。
    /// `RecordKey` 内部持有递增序号，因此每个方向需要独立的两份实例。
    read_at_server: RecordKey,
    read_at_client: RecordKey,
}

impl Tunnel {
    fn new() -> Self {
        let suite = CipherSuite::Aes128GcmSha256;
        let (c2s_key, c2s_iv) = random_key();
        let (s2c_key, s2c_iv) = random_key();
        Self {
            client_to_server: RecordKey::new(suite.aead(), &c2s_key, c2s_iv).expect("写密钥"),
            server_to_client: RecordKey::new(suite.aead(), &s2c_key, s2c_iv).expect("写密钥"),
            read_at_server: RecordKey::new(suite.aead(), &c2s_key, c2s_iv).expect("读密钥"),
            read_at_client: RecordKey::new(suite.aead(), &s2c_key, s2c_iv).expect("读密钥"),
        }
    }

    /// 客户端发送：返回服务端收到的明文记录。
    fn client_sends(&mut self, plaintext: &[u8]) -> Vec<Vec<u8>> {
        let records = split_and_seal(&mut self.client_to_server, plaintext);
        records
            .into_iter()
            .map(|payload| open_record(&mut self.read_at_server, &payload))
            .collect()
    }

    /// 服务端发送：返回客户端收到的明文记录。
    fn server_sends(&mut self, plaintext: &[u8]) -> Vec<Vec<u8>> {
        let records = split_and_seal(&mut self.server_to_client, plaintext);
        records
            .into_iter()
            .map(|payload| open_record(&mut self.read_at_client, &payload))
            .collect()
    }

    /// 单条记录：返回客户端一侧的写密钥封装结果（用于篡改测试）。
    fn seal_from_server(&mut self, plaintext: &[u8]) -> Vec<Vec<u8>> {
        split_and_seal(&mut self.server_to_client, plaintext)
    }

    /// 在客户端侧解封（用于篡改测试）。
    fn open_at_client(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let record = Record {
            kind: ContentType::ApplicationData,
            payload: payload.to_vec(),
        };
        let (kind, body) = self
            .read_at_client
            .open(&record)
            .map_err(|error| error.to_string())?;
        assert_eq!(kind, ContentType::ApplicationData);
        Ok(body)
    }
}

fn random_key() -> ([u8; 16], [u8; 12]) {
    let mut key = [0u8; 16];
    let mut iv = [0u8; 12];
    rng().fill(&mut key).expect("随机数");
    rng().fill(&mut iv).expect("随机数");
    (key, iv)
}

fn split_and_seal(key: &mut RecordKey, plaintext: &[u8]) -> Vec<Vec<u8>> {
    omnicrawl_tls::record::split_records(ContentType::ApplicationData, plaintext)
        .into_iter()
        .map(|record| {
            key.seal(ContentType::ApplicationData, &record.payload)
                .expect("封装")
                .payload
        })
        .collect()
}

fn open_record(key: &mut RecordKey, payload: &[u8]) -> Vec<u8> {
    let record = Record {
        kind: ContentType::ApplicationData,
        payload: payload.to_vec(),
    };
    let (kind, body) = key.open(&record).expect("解封");
    assert_eq!(kind, ContentType::ApplicationData);
    body
}

/// 一个自足的服务端，负责回 200 + 指定响应体。/// 一个自足的服务端，负责回 200 + 指定响应体。/// 一个自足的服务端，负责回 200 + 指定响应体。
struct FakeH2Server {
    encoder: Encoder,
    /// 解码器必须跨请求存活：HPACK 动态表是连接级状态，每次新建会立刻解不开
    /// 第二轮的索引引用（表现为 InvalidIndex）。
    decoder: Decoder,
}

impl FakeH2Server {
    fn new() -> Self {
        Self {
            encoder: Encoder::new(4096),
            decoder: Decoder::new(4096),
        }
    }

    /// 解析客户端发来的请求，返回 (流号, 请求头列表)。
    fn read_request(&mut self, mut bytes: &[u8]) -> (u32, Vec<Header>) {
        let mut stream_id = 0;
        let mut block = Vec::new();
        let mut end_headers = false;
        let mut saw_headers = false;
        while !bytes.is_empty() {
            let (frame, used) = parse_frame(bytes, DEFAULT_MAX_FRAME_SIZE).expect("解析帧");
            bytes = &bytes[used..];
            match frame {
                Frame::Settings { .. } | Frame::WindowUpdate { .. } => {}
                Frame::Headers {
                    stream_id: id,
                    end_headers: headers_done,
                    fragment,
                    ..
                } => {
                    stream_id = id;
                    block.extend_from_slice(&fragment);
                    end_headers = headers_done;
                    saw_headers = true;
                }
                Frame::Continuation {
                    end_headers: headers_done,
                    fragment,
                    ..
                } => {
                    block.extend_from_slice(&fragment);
                    end_headers = headers_done;
                }
                other => panic!("服务端不期望收到 {other:?}"),
            }
        }
        // 纯 SETTINGS 的调用没有请求头，不算错误。
        if !saw_headers {
            return (0, Vec::new());
        }
        assert!(end_headers, "请求头必须完整");
        (stream_id, self.decoder.decode(&block).expect("解请求头"))
    }

    /// 生成响应：HEADERS + DATA（如果响应体非空）。
    fn respond(&mut self, stream_id: u32, status: &str, body: &[u8]) -> Vec<u8> {
        let headers = vec![
            Header::new(":status", status),
            Header::new("content-type", "text/plain; charset=utf-8"),
            Header::new("content-length", body.len().to_string()),
            Header::new("server", "fake-h2"),
        ];
        let block = self.encoder.encode(&headers);
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
}

fn get_request(authority: &str, path: &str) -> Vec<Header> {
    vec![
        Header::new(":method", "GET"),
        Header::new(":scheme", "https"),
        Header::new(":authority", authority),
        Header::new(":path", path),
        Header::new("user-agent", "omnicrawl/1.0"),
        Header::new("accept", "*/*"),
        Header::new("accept-encoding", "gzip, deflate, br"),
    ]
}

#[test]
fn a_get_request_and_response_travel_through_tls_records() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let mut server = FakeH2Server::new();

    // 1) 连接前言（SETTINGS）经 TLS 送到服务端。
    let preamble = client.preamble();
    for plaintext in tunnel.client_sends(&preamble) {
        server.read_request(&plaintext);
    }

    // 2) 客户端发请求。
    let (stream_id, request) = client
        .send_request(&get_request("example.com", "/index.html"), true)
        .expect("发起请求");
    assert_eq!(stream_id, 1);
    let mut delivered = Vec::new();
    for plaintext in tunnel.client_sends(&request) {
        delivered.extend_from_slice(&plaintext);
    }
    let (server_stream, headers) = server.read_request(&delivered);
    assert_eq!(server_stream, stream_id);
    assert_eq!(
        headers
            .iter()
            .find(|header| header.name == ":path")
            .map(|header| header.value.as_str()),
        Some("/index.html")
    );
    assert_eq!(
        headers
            .iter()
            .find(|header| header.name == ":authority")
            .map(|header| header.value.as_str()),
        Some("example.com")
    );

    // 3) 服务端回响应，经 TLS 送回客户端。
    let response = server.respond(stream_id, "200", b"<html>hello</html>");
    let mut received = Vec::new();
    for plaintext in tunnel.server_sends(&response) {
        received.extend_from_slice(&plaintext);
    }
    let feed = client.feed(&received).expect("客户端应能处理响应");
    assert_eq!(feed.consumed, received.len());
    assert!(feed
        .events
        .iter()
        .any(|event| matches!(event, Event::Headers { .. })));

    assert_eq!(client.status(stream_id), Some(200));
    assert_eq!(
        client.response_body(stream_id),
        Some(&b"<html>hello</html>"[..])
    );
    assert_eq!(client.stream_state(stream_id), Some(StreamState::Closed));
}

#[test]
fn a_frame_split_across_tls_records_is_reassembled() {
    // 这是两层组合时最容易出的问题：一个 HTTP/2 帧被切成两条 TLS 记录，
    // 客户端必须以「未消费任何字节」的方式等下一段。
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/"), true)
        .expect("请求");

    let mut encoder = Encoder::new(4096);
    let block = encoder.encode(&[Header::new(":status", "204")]);
    let response = encode_frame(&Frame::Headers {
        stream_id,
        end_stream: true,
        end_headers: true,
        fragment: block,
    })
    .expect("HEADERS");

    // 先在帧中间切断，仅前 5 字节（连帧头都不完整）：不产生事件，但字节被接手。
    let feed = client.feed(&response[..5]).expect("应当容忍");
    assert!(feed.events.is_empty(), "半帧不应产生事件");
    assert_eq!(feed.consumed, 5, "残帧由内部缓冲，调用方总能丢弃已喂字节");

    // 再从帧头处切断。
    let feed = client.feed(&response[5..9]).expect("应当容忍");
    assert!(feed.events.is_empty());
    assert_eq!(client.status(stream_id), None, "帧还没收全");

    // 补齐剩余部分：内部缓冲会与这次输入拼接。
    let feed = client.feed(&response[9..]).expect("应当处理");
    assert_eq!(feed.consumed, response.len() - 9);
    assert_eq!(client.status(stream_id), Some(204));
}

#[test]
fn a_large_body_is_delivered_across_many_frames_and_records() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let mut server = FakeH2Server::new();

    let (stream_id, request) = client
        .send_request(&get_request("example.com", "/big"), true)
        .expect("请求");
    let mut delivered = Vec::new();
    for plaintext in tunnel.client_sends(&request) {
        delivered.extend_from_slice(&plaintext);
    }
    server.read_request(&delivered);

    // 100 KB 响应体：会被切成多条 DATA 帧，再被切成多条 TLS 记录。
    let body: Vec<u8> = (0..100_000).map(|index| (index % 251) as u8).collect();
    let response = server.respond(stream_id, "200", &body);

    // 逐条 TLS 记录喂入，模拟真实的流式到达。
    let mut total_consumed = 0;
    for plaintext in tunnel.server_sends(&response) {
        // 一次喂一整个记录里的明文即可：跨帧的残片由连接内部缓冲接续。
        let feed = client.feed(&plaintext).expect("处理");
        assert_eq!(feed.consumed, plaintext.len());
        total_consumed += feed.consumed;
    }
    assert_eq!(client.response_body(stream_id), Some(&body[..]));
    assert_eq!(client.stream_state(stream_id), Some(StreamState::Closed));
    // 补窗口的 WINDOW_UPDATE 会随每批数据产出，总量应该覆盖全部响应体。
    assert_eq!(total_consumed, response.len());
}

#[test]
fn flow_control_windows_are_replenished_so_large_bodies_do_not_stall() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/stream"), true)
        .expect("请求");

    // 每次只喂一帧（=16384），观察连接接收窗口是否保持不回退。
    let mut encoder = Encoder::new(4096);
    let block = encoder.encode(&[Header::new(":status", "200")]);
    let mut frames = encode_frame(&Frame::Headers {
        stream_id,
        end_stream: false,
        end_headers: true,
        fragment: block,
    })
    .expect("HEADERS");
    client.feed(&frames).expect("处理响应头");
    frames.clear();

    let chunk = vec![0xab; DEFAULT_MAX_FRAME_SIZE];
    for index in 0..4 {
        let last = index == 3;
        let data = encode_frame(&Frame::Data {
            stream_id,
            end_stream: last,
            data: chunk.clone(),
        })
        .expect("DATA");
        let feed = client.feed(&data).expect("处理 DATA");
        // 每批数据之后都必须产出 WINDOW_UPDATE，否则窗口会单调下降。
        assert!(
            !feed.outbound.is_empty(),
            "第 {index} 批数据之后必须有窗口补充"
        );
        // 窗口补充要真的能穿过 TLS 记录层。
        for plaintext in tunnel.client_sends(&feed.outbound) {
            assert!(!plaintext.is_empty());
        }
    }
    assert_eq!(client.stream_state(stream_id), Some(StreamState::Closed));
    assert_eq!(
        client.response_body(stream_id).map(|body| body.len()),
        Some(4 * DEFAULT_MAX_FRAME_SIZE)
    );
}

#[test]
fn pipelined_requests_reuse_the_hpack_dynamic_table() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let mut server = FakeH2Server::new();

    let mut sizes = Vec::new();
    for path in ["/a", "/b", "/c"] {
        let (stream_id, request) = client
            .send_request(&get_request("example.com", path), true)
            .expect("请求");
        sizes.push(request.len());
        let mut delivered = Vec::new();
        for plaintext in tunnel.client_sends(&request) {
            delivered.extend_from_slice(&plaintext);
        }
        let (server_stream, headers) = server.read_request(&delivered);
        assert_eq!(server_stream, stream_id);
        assert_eq!(
            headers
                .iter()
                .find(|header| header.name == ":path")
                .map(|header| header.value.as_str()),
            Some(path)
        );
    }
    // 第二个起 :authority / user-agent / accept 都命中动态表，帧应明显更小。
    assert!(
        sizes[1] < sizes[0],
        "第二轮应更小：{:?}",
        sizes
    );
    assert!(sizes[2] < sizes[0]);
}

#[test]
fn a_reset_stream_surfaces_as_an_event() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/cancel"), true)
        .expect("请求");

    let reset = encode_frame(&Frame::RstStream {
        stream_id,
        code: ErrorCode::Cancel as u32,
    })
    .expect("RST_STREAM");
    let mut received = Vec::new();
    for plaintext in tunnel.server_sends(&reset) {
        received.extend_from_slice(&plaintext);
    }
    let feed = client.feed(&received).expect("处理");
    assert!(feed.events.iter().any(|event| matches!(
        event,
        Event::StreamReset { stream_id: id, code } if *id == stream_id && *code == ErrorCode::Cancel as u32
    )));
    assert_eq!(client.stream_state(stream_id), Some(StreamState::Reset));
}

#[test]
fn a_goaway_stops_the_connection_from_opening_new_streams() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/"), true)
        .expect("请求");

    let goaway = encode_frame(&Frame::GoAway {
        last_stream_id: stream_id,
        code: ErrorCode::NoError as u32,
        debug: b"server shutting down".to_vec(),
    })
    .expect("GOAWAY");
    let mut received = Vec::new();
    for plaintext in tunnel.server_sends(&goaway) {
        received.extend_from_slice(&plaintext);
    }
    let feed = client.feed(&received).expect("处理");
    assert!(feed
        .events
        .iter()
        .any(|event| matches!(event, Event::GoAway { .. })));

    let error = client
        .send_request(&get_request("example.com", "/next"), true)
        .expect_err("GOAWAY 之后不得再开新流");
    assert!(matches!(error, ConnectionError::GoAway { .. }), "{error:?}");
}

#[test]
fn settings_negotiation_changes_the_encoder_table_size() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();

    // 服务端把 HEADER_TABLE_SIZE 压到 0：客户端之后不能再往动态表里塞东西。
    let settings = encode_frame(&Frame::Settings {
        ack: false,
        settings: vec![omnicrawl_tls::http2::Setting {
            id: omnicrawl_tls::http2::setting_id::HEADER_TABLE_SIZE,
            value: 0,
        }],
    })
    .expect("SETTINGS");
    let mut received = Vec::new();
    for plaintext in tunnel.server_sends(&settings) {
        received.extend_from_slice(&plaintext);
    }
    let feed = client.feed(&received).expect("处理 SETTINGS");
    assert!(!feed.outbound.is_empty(), "必须回 SETTINGS ACK");

    // 之后的请求头仍必须能被对端解出（即使不能进动态表）。
    let (stream_id, request) = client
        .send_request(&get_request("example.com", "/after-settings"), true)
        .expect("请求");
    let mut delivered = Vec::new();
    for plaintext in tunnel.client_sends(&request) {
        delivered.extend_from_slice(&plaintext);
    }
    let mut decoder = Decoder::new(0);
    let mut block = Vec::new();
    let mut bytes = &delivered[..];
    while !bytes.is_empty() {
        let (frame, used) = parse_frame(bytes, DEFAULT_MAX_FRAME_SIZE).expect("解析");
        bytes = &bytes[used..];
        if let Frame::Headers { fragment, .. } = frame {
            block.extend_from_slice(&fragment);
        }
    }
    let headers = decoder.decode(&block).expect("上限 0 时仍应可解");
    assert_eq!(
        headers
            .iter()
            .find(|header| header.name == ":path")
            .map(|header| header.value.as_str()),
        Some("/after-settings")
    );
    assert_eq!(stream_id, 1);
}

#[test]
fn a_corrupted_record_is_rejected_by_the_record_layer_not_the_h2_layer() {
    // 分层检查：TLS 记录的 AEAD 标签先拦下篡改，HTTP/2 层根本看不到坏帧。
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/"), true)
        .expect("请求");

    let mut encoder = Encoder::new(4096);
    let block = encoder.encode(&[Header::new(":status", "200")]);
    let response = encode_frame(&Frame::Headers {
        stream_id,
        end_stream: true,
        end_headers: true,
        fragment: block,
    })
    .expect("HEADERS");
    let mut records = tunnel.seal_from_server(&response);
    assert_eq!(records.len(), 1);
    let last = records[0].len() - 1;
    records[0][last] ^= 0xff;

    assert!(
        tunnel.open_at_client(&records[0]).is_err(),
        "篡改的记录必须被 AEAD 拒绝"
    );
}

#[test]
fn the_end_stream_flag_is_only_on_the_final_data_frame() {
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/upload"), false)
        .expect("请求（有请求体）");

    // 40 KB 请求体：会被切成 3 条 DATA（16384 + 16384 + 7232）。
    let payload = vec![0x11u8; 40_000];
    let (bytes, sent) = client.send_body(stream_id, &payload, true).expect("发送");
    assert_eq!(sent, 40_000);

    let mut frames = Vec::new();
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let (frame, used) = parse_frame(rest, DEFAULT_MAX_FRAME_SIZE).expect("解析");
        rest = &rest[used..];
        frames.push(frame);
    }
    let data_frames: Vec<_> = frames
        .iter()
        .filter(|frame| matches!(frame, Frame::Data { .. }))
        .collect();
    assert_eq!(data_frames.len(), 3);
    for (index, frame) in data_frames.iter().enumerate() {
        let end_stream = match frame {
            Frame::Data { end_stream, .. } => *end_stream,
            _ => unreachable!(),
        };
        assert_eq!(
            end_stream,
            index == data_frames.len() - 1,
            "只有末帧带 END_STREAM"
        );
    }

    // 这些字节也要能穿过 TLS 记录层。
    for plaintext in tunnel.client_sends(&bytes) {
        assert!(!plaintext.is_empty());
    }
}

#[test]
fn headers_with_padding_from_a_real_server_are_accepted() {
    // 有些服务端会给 HEADERS 加填充。帧层必须剥掉填充再交给 HPACK。
    let mut tunnel = Tunnel::new();
    let mut client = ClientConnection::new();
    let (stream_id, _) = client
        .send_request(&get_request("example.com", "/"), true)
        .expect("请求");

    let mut encoder = Encoder::new(4096);
    let block = encoder.encode(&[Header::new(":status", "418")]);
    let padding = 7usize;
    let mut payload = vec![padding as u8];
    payload.extend_from_slice(&block);
    payload.extend_from_slice(&vec![0u8; padding]);

    let mut bytes = vec![
        (payload.len() >> 16) as u8,
        (payload.len() >> 8) as u8,
        payload.len() as u8,
        0x1,
        flags::PADDED | flags::END_HEADERS | flags::END_STREAM,
    ];
    bytes.extend_from_slice(&stream_id.to_be_bytes());
    bytes.extend_from_slice(&payload);

    let mut received = Vec::new();
    for plaintext in tunnel.server_sends(&bytes) {
        received.extend_from_slice(&plaintext);
    }
    client.feed(&received).expect("应当接受带填充的 HEADERS");
    assert_eq!(client.status(stream_id), Some(418));
}
