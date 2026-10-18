//! 一个为浏览器指纹而写的 TLS 客户端实现。
//!
//! 动机：`JA3`/`JA4` 的主要指纹字段是「密码套件的原始排列」与「扩展出现的顺序」。
//! `rustls` 把这两者都写死在代码里，且不支持 `GREASE`，因此无法用它伪装成 Chrome。
//! 原本靠 `wreq` + `btls` + `BoringSSL` 解决，代价是整个工作区引入 C/C++ 工具链、
//! CMake、NASM 与 libclang，且交叉编译处处受限。
//!
//! 分层与当前进度：
//!
//! | 模块 | 职责 | 状态 |
//! | --- | --- | --- |
//! | [`wire`] | 线格式编码原语（长度前缀、扩展、握手消息） | 完成 |
//! | [`client_hello`] | ClientHello 构造：顺序 / GREASE / 套件排列完全可控 | 完成 |
//! | [`profile`] | chrome / firefox / safari / edge 四份档案 | 骨架（数值待校准） |
//! | [`parse`] | ClientHello 反序列化，用于往返验证与抓包对照 | 完成 |
//! | [`record`] | 记录层：分片、1.3 加解密、序号与 nonce | 完成 |
//! | [`keyschedule`] | HKDF-Extract / Expand-Label / Derive-Secret，握手与应用两阶段密钥 | 完成 |
//! | [`handshake`] | X25519 协商、ServerHello / EncryptedExtensions / Certificate 解析 | 完成 |
//! | [`verify`] | Finished 的 verify_data、CertificateVerify 签名校验、证书链信任判定 | 完成 |
//! | [`session`] | 客户端握手状态机：Start → Connected，含客户端 Finished | 完成 |
//! | [`hpack`] | HPACK 头部压缩：静态表、动态表、Huffman、前缀整数 | 完成 |
//! | [`http2`] | HTTP/2 帧层：帧头与各类型帧的编解码与校验 | 完成 |
//! | [`h2conn`] | HTTP/2 连接层：流状态、流量控制、HEADERS/CONTINUATION 拼接 | 完成 |
//! | [`http1`] | HTTP/1.1 客户端编解码：请求行、响应头、三种响应体定界 | 完成 |
//!
//! 应用层由 ALPN 决定：协商出 `h2` 走 HTTP/2，协商出 `http/1.1` 在同一套 TLS 记录层上
//! 退到 HTTP/1.1 报文。只支持这两种——服务端选别的会如实报
//! [`driver::DriverError::UnsupportedAlpn`]。
//!
//! 尚未完成：`profile` 里 Firefox / Safari 的逐字节校准、证书吊销检查、
//! 服务端推送（刻意不支持）、0-RTT、会话恢复。
//!
//! 明确的能力边界，避免误用：
//!
//! - [`session::PeerPolicy::Inspect`] 只校验 CertificateVerify 的签名，**不验证书链**。
//!   它证明对端持有叶证书私钥，不证明那张证书可信。要信任对端，用
//!   [`session::ClientHandshake::with_trust`] 切到 [`session::PeerPolicy::VerifyChain`]。
//! - 不支持 0-RTT、客户端认证、会话恢复；HelloRetryRequest 仅在换了组的情况下重试一次。
//! - [`h2conn`] 只实现**客户端**一侧：不发 PUSH_PROMISE、不处理服务端流量控制策略；
//!   它在 SETTINGS 里固定关闭服务端推送，收到 PUSH_PROMISE 即报错。
//!
//! 设计取舍：密码学原语交给 `ring`（已审计、自带预生成汇编、不需要外部 C 工具链），
//! X.509 路径判定交给 `rustls-webpki`；自研面收敛在「线格式 + 握手编排 + 档案」——
//! 那才是 `BoringSSL` 无法替代的部分。这一层不持有随机源：密钥与随机数由调用方提供。

pub mod client_hello;
pub mod driver;
pub mod h2conn;
pub mod handshake;
pub mod http2;
pub mod keyschedule;
pub mod parse;
pub mod profile;
pub mod record;
pub mod session;
pub mod verify;
pub mod wire;

pub use client_hello::{
    ext, group, pick_grease, suite, ClientHelloSpec, ExtensionSlot, GREASE_MARK, GREASE_VALUES,
};
pub use driver::{Application, Connection, DriverError, Response, Transport};
pub use h2conn::{ClientConnection, ConnectionError, Event, Feed, StreamState};
pub use handshake::{
    negotiated_alpn, parse_certificate_chain, parse_certificate_leaf, parse_certificate_verify,
    parse_encrypted_extensions, parse_server_hello, take_handshake_messages, CertificateVerify,
    HandshakeMessage, ServerHello, X25519Exchange,
};
pub use hpack::{Decoder, DynamicTable, Encoder, Header, HpackError};
pub use http1::{BodyKind, Head, Http1Error};
pub use http2::{
    encode_frame, parse_frame, ErrorCode, Frame, FrameError, FrameType, Setting,
    DEFAULT_MAX_FRAME_SIZE, DEFAULT_WINDOW_SIZE,
};
pub use keyschedule::{
    derive_secret, expand_label, traffic_keys, ApplicationSecrets, CipherSuite, HandshakeSecrets,
    KeySchedule, TrafficKeys, Transcript,
};
pub use parse::{parse_client_hello, ParseError};
pub use profile::{spec_for, Family};
pub use record::{ContentType, Record, RecordError, RecordKey, MAX_PLAINTEXT, TAG_LEN};
pub use session::{ClientHandshake, HandshakeError, PeerPolicy, State};
pub use verify::{
    certificate_verify_message, finished_verify_data, signature_algorithm_for_scheme,
    verify_certificate_verify, verify_finished, verify_server_chain, TrustAnchors, VerifyError,
};
pub use wire::{EncodeError, Extension};

pub mod hpack;
pub mod http1;
