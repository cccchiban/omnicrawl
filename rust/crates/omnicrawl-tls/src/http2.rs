//! HTTP/2 帧层（RFC 9113，帧格式沿用 RFC 7540 §11.2 的注册表）。
//!
//! 这一层只做「字节 ↔ 帧」：帧头 9 字节、各类型的载荷结构、以及必须拒绝的畸形输入。
//! 流状态、流量控制、HEADERS/CONTINUATION 的拼接在 [`crate::h2conn`] 里。
//!
//! 帧头固定 9 字节：
//!
//! ```text
//! +-----------------------------------------------+
//! |                 Length (24)                   |
//! +---------------+---------------+---------------+
//! |   Type (8)    |   Flags (8)   |
//! +-+-------------+---------------+-------------------------------+
//! |R|                 Stream Identifier (31)                      |
//! +=+=============================================================+
//! |                   Frame Payload (..)                          |
//! +---------------------------------------------------------------+
//! ```
//!
//! 三个容易错的地方：
//!
//! 1. **`R` 位必须在发送时清零、接收时忽略**，不能当成标志位的一部分。
//! 2. **长度字段不含 9 字节帧头**。把帧头算进去会让所有后续帧错位。
//! 3. **PING 与 WINDOW_UPDATE 的长度是硬性的**（8 与 4），不满足是
//!    `FRAME_SIZE_ERROR`，不是「尽力解析」。

use crate::hpack::Header;

/// 帧头长度。
pub const HEADER_LEN: usize = 9;

/// 默认最大帧载荷（对端未通过 SETTINGS 放大时）。
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16_384;

/// 协议允许的最大帧载荷上限。
pub const MAX_MAX_FRAME_SIZE: usize = 16_777_215;

/// 默认初始流量控制窗口（2^16 - 1）。
pub const DEFAULT_WINDOW_SIZE: u32 = 65_535;

/// 默认 HPACK 动态表容量（RFC 9113 §6.5.2 的 `SETTINGS_HEADER_TABLE_SIZE` 默认值）。
pub const DEFAULT_HEADER_TABLE_SIZE: usize = 4096;

/// 帧类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Data = 0x0,
    Headers = 0x1,
    Priority = 0x2,
    RstStream = 0x3,
    Settings = 0x4,
    PushPromise = 0x5,
    Ping = 0x6,
    GoAway = 0x7,
    WindowUpdate = 0x8,
    Continuation = 0x9,
}

impl FrameType {
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x0 => Self::Data,
            0x1 => Self::Headers,
            0x2 => Self::Priority,
            0x3 => Self::RstStream,
            0x4 => Self::Settings,
            0x5 => Self::PushPromise,
            0x6 => Self::Ping,
            0x7 => Self::GoAway,
            0x8 => Self::WindowUpdate,
            0x9 => Self::Continuation,
            _ => return None,
        })
    }
}

/// HTTP/2 连接前言（RFC 9113 §3.4）。
///
/// 客户端在**任何帧之前**必须先发这 24 字节。它本质上是让服务端确认「对端真的是
/// HTTP/2 客户端」；漏掉它服务端会直接把后续字节当成错误输入并 GOAWAY。
pub const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// 各帧的标志位。
pub mod flags {
    /// DATA / HEADERS 的 END_STREAM。
    pub const END_STREAM: u8 = 0x1;
    /// HEADERS 的 END_HEADERS。
    pub const END_HEADERS: u8 = 0x4;
    /// HEADERS 的 PADDED。
    pub const PADDED: u8 = 0x8;
    /// HEADERS / CONTINUATION 的 PRIORITY（仅 HEADERS 有）。
    pub const PRIORITY: u8 = 0x20;
    /// SETTINGS / PING 的 ACK。
    pub const ACK: u8 = 0x1;
}

/// 错误码（RFC 7540 §11.4 的注册表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ErrorCode {
    NoError = 0x0,
    ProtocolError = 0x1,
    InternalError = 0x2,
    FlowControlError = 0x3,
    SettingsTimeout = 0x4,
    StreamClosed = 0x5,
    FrameSizeError = 0x6,
    RefusedStream = 0x7,
    Cancel = 0x8,
    CompressionError = 0x9,
    ConnectError = 0xa,
    EnhanceYourCalm = 0xb,
    InadequateSecurity = 0xc,
    Http11Required = 0xd,
}

impl ErrorCode {
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0x0 => Self::NoError,
            0x1 => Self::ProtocolError,
            0x2 => Self::InternalError,
            0x3 => Self::FlowControlError,
            0x4 => Self::SettingsTimeout,
            0x5 => Self::StreamClosed,
            0x6 => Self::FrameSizeError,
            0x7 => Self::RefusedStream,
            0x8 => Self::Cancel,
            0x9 => Self::CompressionError,
            0xa => Self::ConnectError,
            0xb => Self::EnhanceYourCalm,
            0xc => Self::InadequateSecurity,
            0xd => Self::Http11Required,
            _ => return None,
        })
    }
}

/// 一条 SETTINGS 项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    pub id: u16,
    pub value: u32,
}

/// SETTINGS 的标识符。
pub mod setting_id {
    /// 头部表大小（HPACK 动态表）。
    pub const HEADER_TABLE_SIZE: u16 = 0x1;
    /// 是否允许服务端推送。
    pub const ENABLE_PUSH: u16 = 0x2;
    /// 并发流上限。
    pub const MAX_CONCURRENT_STREAMS: u16 = 0x3;
    /// 初始流量控制窗口。
    pub const INITIAL_WINDOW_SIZE: u16 = 0x4;
    /// 最大帧载荷。
    pub const MAX_FRAME_SIZE: u16 = 0x5;
    /// 最大头部列表字节数。
    pub const MAX_HEADER_LIST_SIZE: u16 = 0x6;
}

/// 解析出来的帧类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// DATA：流号、END_STREAM 标志、数据内容。
    Data {
        stream_id: u32,
        end_stream: bool,
        data: Vec<u8>,
    },
    /// HEADERS：流号、END_STREAM / END_HEADERS、头部块片段。
    Headers {
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        /// 头部块（HPACK 字节）。若 `end_headers` 为假，需继续收 CONTINUATION。
        fragment: Vec<u8>,
    },
    /// PRIORITY：流依赖权重信息（本实现解析但不使用）。
    Priority {
        stream_id: u32,
        dependency: u32,
        exclusive: bool,
        weight: u8,
    },
    /// RST_STREAM：流号与错误码。
    RstStream { stream_id: u32, code: u32 },
    /// SETTINGS：`ack` 为真时没有载荷项。
    Settings { ack: bool, settings: Vec<Setting> },
    /// PING。
    Ping { ack: bool, payload: [u8; 8] },
    /// GOAWAY：最后处理的流号与错误码。
    GoAway {
        last_stream_id: u32,
        code: u32,
        debug: Vec<u8>,
    },
    /// WINDOW_UPDATE：流号（0 表示连接级）与增量。
    WindowUpdate { stream_id: u32, increment: u32 },
    /// CONTINUATION：头部块续片。
    Continuation {
        stream_id: u32,
        end_headers: bool,
        fragment: Vec<u8>,
    },
    /// 未知类型：按 RFC 9113 §4.1 必须忽略，但要保留原始字节以便诊断。
    Unknown {
        kind: u8,
        stream_id: u32,
        payload: Vec<u8>,
    },
    /// PUSH_PROMISE：本实现作为客户端会拒绝服务端推送，但需要能解析以便回 RST。
    PushPromise {
        stream_id: u32,
        promised_stream_id: u32,
        fragment: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// 数据不足以构成一个完整帧。
    Incomplete { needed: usize, available: usize },
    /// 载荷超过本端宣告的最大帧大小。
    FrameTooLarge { len: usize, max: usize },
    /// 载荷长度不符合该帧类型的硬性要求。
    BadLength { frame: &'static str, len: usize },
    /// SETTINGS 项长度不是 6 的倍数。
    BadSettingsLength(usize),
    /// SETTINGS ACK 却带了载荷。
    SettingsAckWithPayload,
    /// 流号不符合要求（例如 DATA 出现在流 0 上）。
    BadStreamId { frame: &'static str, stream_id: u32 },
    /// 固定长度字段的载荷不足。
    Truncated(&'static str),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete { needed, available } => {
                write!(formatter, "帧不完整：需要 {needed} 字节，只有 {available}")
            }
            Self::FrameTooLarge { len, max } => {
                write!(formatter, "帧载荷 {len} 超过上限 {max}")
            }
            Self::BadLength { frame, len } => {
                write!(formatter, "{frame} 的载荷长度 {len} 不合法")
            }
            Self::BadSettingsLength(len) => {
                write!(formatter, "SETTINGS 载荷长度 {len} 不是 6 的倍数")
            }
            Self::SettingsAckWithPayload => formatter.write_str("SETTINGS ACK 不得携带载荷"),
            Self::BadStreamId { frame, stream_id } => {
                write!(formatter, "{frame} 不得出现在流 {stream_id} 上")
            }
            Self::Truncated(what) => write!(formatter, "{what} 的载荷不足"),
        }
    }
}

impl std::error::Error for FrameError {}

/// 把一条帧编码成线上字节，同时校验它是否允许出现在这个流号上。
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    let mut payload = Vec::new();
    let (kind, stream_id, flags) = match frame {
        Frame::Data {
            stream_id,
            end_stream,
            data,
        } => {
            require_nonzero("DATA", *stream_id)?;
            let mut flags = 0u8;
            if *end_stream {
                flags |= flags::END_STREAM;
            }
            payload.extend_from_slice(data);
            (FrameType::Data, *stream_id, flags)
        }
        Frame::Headers {
            stream_id,
            end_stream,
            end_headers,
            fragment,
        } => {
            require_nonzero("HEADERS", *stream_id)?;
            let mut flags = 0u8;
            if *end_stream {
                flags |= flags::END_STREAM;
            }
            if *end_headers {
                flags |= flags::END_HEADERS;
            }
            payload.extend_from_slice(fragment);
            (FrameType::Headers, *stream_id, flags)
        }
        Frame::Priority {
            stream_id,
            dependency,
            exclusive,
            weight,
        } => {
            require_nonzero("PRIORITY", *stream_id)?;
            let mut dependency = *dependency;
            if *exclusive {
                dependency |= 0x8000_0000;
            }
            payload.extend_from_slice(&dependency.to_be_bytes());
            payload.push(weight.saturating_sub(1));
            (FrameType::Priority, *stream_id, 0)
        }
        Frame::RstStream { stream_id, code } => {
            require_nonzero("RST_STREAM", *stream_id)?;
            payload.extend_from_slice(&code.to_be_bytes());
            (FrameType::RstStream, *stream_id, 0)
        }
        Frame::Settings { ack, settings } => {
            let flags = if *ack { flags::ACK } else { 0 };
            if !ack {
                for setting in settings {
                    payload.extend_from_slice(&setting.id.to_be_bytes());
                    payload.extend_from_slice(&setting.value.to_be_bytes());
                }
            }
            (FrameType::Settings, 0, flags)
        }
        Frame::Ping { ack, payload: data } => {
            let flags = if *ack { flags::ACK } else { 0 };
            payload.extend_from_slice(data);
            (FrameType::Ping, 0, flags)
        }
        Frame::GoAway {
            last_stream_id,
            code,
            debug,
        } => {
            payload.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
            payload.extend_from_slice(&code.to_be_bytes());
            payload.extend_from_slice(debug);
            (FrameType::GoAway, 0, 0)
        }
        Frame::WindowUpdate {
            stream_id,
            increment,
        } => {
            payload.extend_from_slice(&(increment & 0x7fff_ffff).to_be_bytes());
            (FrameType::WindowUpdate, *stream_id, 0)
        }
        Frame::Continuation {
            stream_id,
            end_headers,
            fragment,
        } => {
            require_nonzero("CONTINUATION", *stream_id)?;
            let flags = if *end_headers { flags::END_HEADERS } else { 0 };
            payload.extend_from_slice(fragment);
            (FrameType::Continuation, *stream_id, flags)
        }
        Frame::PushPromise {
            stream_id,
            promised_stream_id,
            fragment,
        } => {
            require_nonzero("PUSH_PROMISE", *stream_id)?;
            payload.extend_from_slice(&(promised_stream_id & 0x7fff_ffff).to_be_bytes());
            payload.extend_from_slice(fragment);
            (FrameType::PushPromise, *stream_id, flags::END_HEADERS)
        }
        Frame::Unknown {
            kind,
            stream_id,
            payload: data,
        } => {
            payload.extend_from_slice(data);
            // 未知类型用原始类型字节；这里复用 `Unknown` 的 kind 字段。
            return Ok(build_header_and_body(*kind, 0, *stream_id, &payload));
        }
    };
    Ok(build_header_and_body(
        kind as u8, flags, stream_id, &payload,
    ))
}

fn build_header_and_body(kind: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.push((payload.len() >> 16) as u8);
    out.push((payload.len() >> 8) as u8);
    out.push(payload.len() as u8);
    out.push(kind);
    out.push(flags);
    // R 位清零，流号只保留 31 位。
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn require_nonzero(frame: &'static str, stream_id: u32) -> Result<(), FrameError> {
    if stream_id == 0 {
        return Err(FrameError::BadStreamId { frame, stream_id });
    }
    Ok(())
}

/// 解析一条帧。返回 `(帧, 消耗字节数)`。
pub fn parse_frame(data: &[u8], max_frame_size: usize) -> Result<(Frame, usize), FrameError> {
    if data.len() < HEADER_LEN {
        return Err(FrameError::Incomplete {
            needed: HEADER_LEN,
            available: data.len(),
        });
    }
    let length = ((data[0] as usize) << 16) | ((data[1] as usize) << 8) | data[2] as usize;
    if length > max_frame_size {
        return Err(FrameError::FrameTooLarge {
            len: length,
            max: max_frame_size,
        });
    }
    let total = HEADER_LEN + length;
    if data.len() < total {
        return Err(FrameError::Incomplete {
            needed: total,
            available: data.len(),
        });
    }
    let kind = data[3];
    let frame_flags = data[4];
    // R 位必须忽略。
    let stream_id = u32::from_be_bytes([data[5], data[6], data[7], data[8]]) & 0x7fff_ffff;
    let payload = &data[HEADER_LEN..total];

    let frame = match FrameType::from_u8(kind) {
        Some(FrameType::Data) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "DATA",
                    stream_id,
                });
            }
            Frame::Data {
                stream_id,
                end_stream: frame_flags & flags::END_STREAM != 0,
                data: payload.to_vec(),
            }
        }
        Some(FrameType::Headers) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "HEADERS",
                    stream_id,
                });
            }
            let mut body = payload;
            if frame_flags & flags::PADDED != 0 {
                body = strip_padding(body, "HEADERS")?;
            }
            if frame_flags & flags::PRIORITY != 0 {
                if body.len() < 5 {
                    return Err(FrameError::Truncated("HEADERS 的优先级字段"));
                }
                body = &body[5..];
            }
            Frame::Headers {
                stream_id,
                end_stream: frame_flags & flags::END_STREAM != 0,
                end_headers: frame_flags & flags::END_HEADERS != 0,
                fragment: body.to_vec(),
            }
        }
        Some(FrameType::Priority) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "PRIORITY",
                    stream_id,
                });
            }
            if payload.len() != 5 {
                return Err(FrameError::BadLength {
                    frame: "PRIORITY",
                    len: payload.len(),
                });
            }
            let dependency = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
            Frame::Priority {
                stream_id,
                dependency: dependency & 0x7fff_ffff,
                exclusive: dependency & 0x8000_0000 != 0,
                weight: payload[4].saturating_add(1),
            }
        }
        Some(FrameType::RstStream) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "RST_STREAM",
                    stream_id,
                });
            }
            if payload.len() != 4 {
                return Err(FrameError::BadLength {
                    frame: "RST_STREAM",
                    len: payload.len(),
                });
            }
            Frame::RstStream {
                stream_id,
                code: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
            }
        }
        Some(FrameType::Settings) => {
            if stream_id != 0 {
                return Err(FrameError::BadStreamId {
                    frame: "SETTINGS",
                    stream_id,
                });
            }
            let ack = frame_flags & flags::ACK != 0;
            if ack {
                if !payload.is_empty() {
                    return Err(FrameError::SettingsAckWithPayload);
                }
                Frame::Settings {
                    ack: true,
                    settings: Vec::new(),
                }
            } else {
                if !payload.len().is_multiple_of(6) {
                    return Err(FrameError::BadSettingsLength(payload.len()));
                }
                let settings = payload
                    .chunks_exact(6)
                    .map(|chunk| Setting {
                        id: u16::from_be_bytes([chunk[0], chunk[1]]),
                        value: u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]),
                    })
                    .collect();
                Frame::Settings {
                    ack: false,
                    settings,
                }
            }
        }
        Some(FrameType::Ping) => {
            if stream_id != 0 {
                return Err(FrameError::BadStreamId {
                    frame: "PING",
                    stream_id,
                });
            }
            if payload.len() != 8 {
                return Err(FrameError::BadLength {
                    frame: "PING",
                    len: payload.len(),
                });
            }
            let mut data = [0u8; 8];
            data.copy_from_slice(payload);
            Frame::Ping {
                ack: frame_flags & flags::ACK != 0,
                payload: data,
            }
        }
        Some(FrameType::GoAway) => {
            if stream_id != 0 {
                return Err(FrameError::BadStreamId {
                    frame: "GOAWAY",
                    stream_id,
                });
            }
            if payload.len() < 8 {
                return Err(FrameError::Truncated("GOAWAY"));
            }
            Frame::GoAway {
                last_stream_id: u32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]) & 0x7fff_ffff,
                code: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
                debug: payload[8..].to_vec(),
            }
        }
        Some(FrameType::WindowUpdate) => {
            if payload.len() != 4 {
                return Err(FrameError::BadLength {
                    frame: "WINDOW_UPDATE",
                    len: payload.len(),
                });
            }
            Frame::WindowUpdate {
                stream_id,
                increment: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]])
                    & 0x7fff_ffff,
            }
        }
        Some(FrameType::Continuation) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "CONTINUATION",
                    stream_id,
                });
            }
            Frame::Continuation {
                stream_id,
                end_headers: frame_flags & flags::END_HEADERS != 0,
                fragment: payload.to_vec(),
            }
        }
        Some(FrameType::PushPromise) => {
            if stream_id == 0 {
                return Err(FrameError::BadStreamId {
                    frame: "PUSH_PROMISE",
                    stream_id,
                });
            }
            let mut body = payload;
            if frame_flags & flags::PADDED != 0 {
                body = strip_padding(body, "PUSH_PROMISE")?;
            }
            if body.len() < 4 {
                return Err(FrameError::Truncated("PUSH_PROMISE"));
            }
            Frame::PushPromise {
                stream_id,
                promised_stream_id: u32::from_be_bytes([body[0], body[1], body[2], body[3]])
                    & 0x7fff_ffff,
                fragment: body[4..].to_vec(),
            }
        }
        None => Frame::Unknown {
            kind,
            stream_id,
            payload: payload.to_vec(),
        },
    };
    Ok((frame, total))
}

/// 剥离 PADDED 帧的填充：首字节是填充长度。
fn strip_padding<'a>(body: &'a [u8], frame: &'static str) -> Result<&'a [u8], FrameError> {
    let (&pad, rest) = body.split_first().ok_or(FrameError::Truncated(frame))?;
    let pad = pad as usize;
    // 填充长度必须严格小于剩余长度，否则连自己也装不下。
    if pad >= rest.len() {
        return Err(FrameError::BadLength {
            frame,
            len: rest.len(),
        });
    }
    Ok(&rest[..rest.len() - pad])
}

/// 把任意帧切成不超过 `max_frame_size` 的多条同类型帧。
///
/// 只有 DATA 与 CONTINUATION 需要分片；HEADERS 的分片要求首帧带 END_STREAM、
/// 末帧带 END_HEADERS，因此由 [`crate::h2conn`] 负责。
pub fn split_data(stream_id: u32, data: &[u8], max_frame_size: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if data.is_empty() {
        let frame = Frame::Data {
            stream_id,
            end_stream: true,
            data: Vec::new(),
        };
        out.push(encode_frame(&frame).expect("空 DATA 一定合法"));
        return out;
    }
    let chunks: Vec<&[u8]> = data.chunks(max_frame_size).collect();
    let last = chunks.len() - 1;
    for (index, chunk) in chunks.into_iter().enumerate() {
        let frame = Frame::Data {
            stream_id,
            end_stream: index == last,
            data: chunk.to_vec(),
        };
        out.push(encode_frame(&frame).expect("分片一定合法"));
    }
    out
}

/// 请求/响应共用的伪头部辅助：把 HPACK 头部列表转成便于阅读的形式。
pub fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name == name)
        .map(|header| header.value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_type_codes_match_the_registry() {
        assert_eq!(FrameType::Data as u8, 0x0);
        assert_eq!(FrameType::Headers as u8, 0x1);
        assert_eq!(FrameType::Priority as u8, 0x2);
        assert_eq!(FrameType::RstStream as u8, 0x3);
        assert_eq!(FrameType::Settings as u8, 0x4);
        assert_eq!(FrameType::PushPromise as u8, 0x5);
        assert_eq!(FrameType::Ping as u8, 0x6);
        assert_eq!(FrameType::GoAway as u8, 0x7);
        assert_eq!(FrameType::WindowUpdate as u8, 0x8);
        assert_eq!(FrameType::Continuation as u8, 0x9);
    }

    #[test]
    fn error_codes_match_the_registry() {
        assert_eq!(ErrorCode::NoError as u32, 0x0);
        assert_eq!(ErrorCode::ProtocolError as u32, 0x1);
        assert_eq!(ErrorCode::FlowControlError as u32, 0x3);
        assert_eq!(ErrorCode::FrameSizeError as u32, 0x6);
        assert_eq!(ErrorCode::CompressionError as u32, 0x9);
        assert_eq!(ErrorCode::EnhanceYourCalm as u32, 0xb);
        assert_eq!(ErrorCode::Http11Required as u32, 0xd);
    }

    #[test]
    fn a_frame_header_is_nine_bytes_and_length_excludes_it() {
        let frame = Frame::Data {
            stream_id: 1,
            end_stream: false,
            data: b"hi".to_vec(),
        };
        let bytes = encode_frame(&frame).expect("应当编码");
        assert_eq!(bytes.len(), HEADER_LEN + 2);
        assert_eq!(bytes[0..3], [0, 0, 2], "长度只算载荷");
        assert_eq!(bytes[3], 0x0, "类型 DATA");
        assert_eq!(bytes[4], 0x0, "无标志");
        assert_eq!(bytes[5..9], [0, 0, 0, 1], "流号 1");
    }

    #[test]
    fn the_reserved_bit_is_cleared_when_sending() {
        let frame = Frame::WindowUpdate {
            stream_id: 0x8000_0001,
            increment: 100,
        };
        let bytes = encode_frame(&frame).expect("应当编码");
        assert_eq!(bytes[5] & 0x80, 0, "R 位必须清零");
        assert_eq!(&bytes[5..9], &[0, 0, 0, 1]);
    }

    #[test]
    fn the_reserved_bit_is_ignored_when_receiving() {
        let mut bytes = encode_frame(&Frame::Settings {
            ack: true,
            settings: Vec::new(),
        })
        .expect("编码");
        // 手工把 R 位打上，解析时应当仍得到流号 0。
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
        assert_eq!(
            frame,
            Frame::Settings {
                ack: true,
                settings: Vec::new()
            }
        );
        bytes.clear();
    }

    #[test]
    fn every_frame_kind_roundtrips() {
        let frames = vec![
            Frame::Data {
                stream_id: 1,
                end_stream: true,
                data: b"body".to_vec(),
            },
            Frame::Headers {
                stream_id: 3,
                end_stream: false,
                end_headers: true,
                fragment: vec![0x82, 0x86],
            },
            Frame::Priority {
                stream_id: 5,
                dependency: 1,
                exclusive: true,
                weight: 200,
            },
            Frame::RstStream {
                stream_id: 7,
                code: ErrorCode::Cancel as u32,
            },
            Frame::Settings {
                ack: false,
                settings: vec![
                    Setting {
                        id: setting_id::MAX_FRAME_SIZE,
                        value: 16384,
                    },
                    Setting {
                        id: setting_id::INITIAL_WINDOW_SIZE,
                        value: 65535,
                    },
                ],
            },
            Frame::Ping {
                ack: true,
                payload: *b"12345678",
            },
            Frame::GoAway {
                last_stream_id: 9,
                code: ErrorCode::NoError as u32,
                debug: b"bye".to_vec(),
            },
            Frame::WindowUpdate {
                stream_id: 0,
                increment: 1024,
            },
            Frame::Continuation {
                stream_id: 11,
                end_headers: false,
                fragment: vec![0xaa; 3],
            },
            Frame::PushPromise {
                stream_id: 13,
                promised_stream_id: 2,
                fragment: vec![0xbb],
            },
        ];
        for frame in frames {
            let bytes = encode_frame(&frame).expect("应当编码");
            let (parsed, consumed) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
            assert_eq!(consumed, bytes.len());
            assert_eq!(parsed, frame, "往返失败：{frame:?}");
        }
    }

    #[test]
    fn a_multi_frame_buffer_parses_one_at_a_time() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(
            &encode_frame(&Frame::Settings {
                ack: true,
                settings: Vec::new(),
            })
            .expect("编码"),
        );
        buffer.extend_from_slice(
            &encode_frame(&Frame::Ping {
                ack: false,
                payload: *b"abcdefgh",
            })
            .expect("编码"),
        );
        let (first, used) = parse_frame(&buffer, DEFAULT_MAX_FRAME_SIZE).expect("第一帧");
        assert!(matches!(first, Frame::Settings { ack: true, .. }));
        let (second, used2) = parse_frame(&buffer[used..], DEFAULT_MAX_FRAME_SIZE).expect("第二帧");
        assert!(matches!(second, Frame::Ping { ack: false, .. }));
        assert_eq!(used + used2, buffer.len());
    }

    #[test]
    fn an_incomplete_frame_reports_how_much_is_missing() {
        let bytes = vec![0, 0, 4, 0x0, 0, 0, 0, 0, 1, b'a'];
        assert_eq!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::Incomplete {
                needed: 13,
                available: 10
            })
        );
        assert_eq!(
            parse_frame(&[0, 0], DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::Incomplete {
                needed: 9,
                available: 2
            })
        );
    }

    #[test]
    fn an_oversized_frame_is_rejected_before_buffering_the_body() {
        let bytes = vec![0xff, 0xff, 0xff, 0x0, 0, 0, 0, 0, 1];
        assert_eq!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::FrameTooLarge {
                len: 0xff_ffff,
                max: DEFAULT_MAX_FRAME_SIZE
            })
        );
    }

    #[test]
    fn fixed_length_frames_reject_wrong_sizes() {
        // PING 长度 7。
        let mut ping = vec![0, 0, 7, 0x6, 0, 0, 0, 0, 0];
        ping.extend_from_slice(&[0; 7]);
        assert_eq!(
            parse_frame(&ping, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadLength {
                frame: "PING",
                len: 7
            })
        );
        // WINDOW_UPDATE 长度 5。
        let mut wu = vec![0, 0, 5, 0x8, 0, 0, 0, 0, 0];
        wu.extend_from_slice(&[0; 5]);
        assert!(matches!(
            parse_frame(&wu, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadLength {
                frame: "WINDOW_UPDATE",
                ..
            })
        ));
        // RST_STREAM 长度 3。
        let mut rst = vec![0, 0, 3, 0x3, 0, 0, 0, 0, 1];
        rst.extend_from_slice(&[0; 3]);
        assert!(matches!(
            parse_frame(&rst, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadLength {
                frame: "RST_STREAM",
                ..
            })
        ));
    }

    #[test]
    fn settings_payload_must_be_a_multiple_of_six() {
        let mut bytes = vec![0, 0, 7, 0x4, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&[0; 7]);
        assert_eq!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadSettingsLength(7))
        );
    }

    #[test]
    fn a_settings_ack_with_payload_is_rejected() {
        let mut bytes = vec![0, 0, 6, 0x4, 0x1, 0, 0, 0, 0];
        bytes.extend_from_slice(&[0; 6]);
        assert_eq!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::SettingsAckWithPayload)
        );
    }

    #[test]
    fn frames_that_require_a_stream_reject_stream_zero() {
        let frame = Frame::Data {
            stream_id: 0,
            end_stream: false,
            data: Vec::new(),
        };
        assert!(matches!(
            encode_frame(&frame),
            Err(FrameError::BadStreamId { .. })
        ));
        // 接收侧同样拒绝。
        let bytes = vec![0, 0, 0, 0x0, 0, 0, 0, 0, 0];
        assert!(matches!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadStreamId { .. })
        ));
    }

    #[test]
    fn connection_frames_reject_nonzero_stream() {
        let bytes = vec![0, 0, 0, 0x4, 0, 0, 0, 0, 1];
        assert!(matches!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadStreamId { .. })
        ));
    }

    #[test]
    fn padded_headers_have_their_padding_stripped() {
        let mut payload = vec![2u8]; // 填充长度 2
        payload.extend_from_slice(&[0x82, 0x86]);
        payload.extend_from_slice(&[0, 0]);
        let mut bytes = vec![
            0,
            0,
            payload.len() as u8,
            0x1,
            flags::PADDED | flags::END_HEADERS,
        ];
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(&payload);
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
        match frame {
            Frame::Headers { fragment, .. } => assert_eq!(fragment, vec![0x82, 0x86]),
            other => panic!("应为 HEADERS，得到 {other:?}"),
        }
    }

    #[test]
    fn padding_longer_than_the_payload_is_rejected() {
        let mut payload = vec![9u8];
        payload.extend_from_slice(&[0x82]);
        let mut bytes = vec![0, 0, payload.len() as u8, 0x1, flags::PADDED, 0, 0, 0, 1];
        bytes.extend_from_slice(&payload);
        assert!(matches!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::BadLength {
                frame: "HEADERS",
                ..
            })
        ));
    }

    #[test]
    fn headers_with_priority_have_the_five_bytes_skipped() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0, 0, 0, 0]); // 依赖
        payload.push(15); // 权重
        payload.extend_from_slice(&[0x82]);
        let mut bytes = vec![0, 0, payload.len() as u8, 0x1, flags::PRIORITY, 0, 0, 0, 3];
        bytes.extend_from_slice(&payload);
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
        match frame {
            Frame::Headers { fragment, .. } => assert_eq!(fragment, vec![0x82]),
            other => panic!("应为 HEADERS，得到 {other:?}"),
        }
    }

    #[test]
    fn unknown_frame_types_are_preserved_not_rejected() {
        // RFC 9113 §4.1：未知类型必须忽略而不是报错。
        let mut bytes = vec![0, 0, 3, 0xee, 0x0, 0, 0, 0, 1];
        bytes.extend_from_slice(&[1, 2, 3]);
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当忽略");
        match frame {
            Frame::Unknown { kind, payload, .. } => {
                assert_eq!(kind, 0xee);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("应为 Unknown，得到 {other:?}"),
        }
    }

    #[test]
    fn data_is_split_into_bounded_frames_with_end_stream_on_the_last() {
        let data = vec![7u8; 40_000];
        let frames = split_data(1, &data, DEFAULT_MAX_FRAME_SIZE);
        assert_eq!(frames.len(), 3, "40000 字节需要 3 帧");
        let mut collected = Vec::new();
        for (index, bytes) in frames.iter().enumerate() {
            let (frame, _) = parse_frame(bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
            match frame {
                Frame::Data {
                    end_stream, data, ..
                } => {
                    assert_eq!(
                        end_stream,
                        index == frames.len() - 1,
                        "只有末帧带 END_STREAM"
                    );
                    collected.extend_from_slice(&data);
                }
                other => panic!("应为 DATA，得到 {other:?}"),
            }
        }
        assert_eq!(collected, data);
    }

    #[test]
    fn an_empty_data_frame_still_carries_end_stream() {
        let frames = split_data(3, &[], DEFAULT_MAX_FRAME_SIZE);
        assert_eq!(frames.len(), 1);
        let (frame, _) = parse_frame(&frames[0], DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
        match frame {
            Frame::Data {
                end_stream, data, ..
            } => {
                assert!(end_stream);
                assert!(data.is_empty());
            }
            other => panic!("应为 DATA，得到 {other:?}"),
        }
    }

    #[test]
    fn goaway_without_debug_data_parses() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&5u32.to_be_bytes());
        payload.extend_from_slice(&(ErrorCode::ProtocolError as u32).to_be_bytes());
        let mut bytes = vec![0, 0, payload.len() as u8, 0x7, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&payload);
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("应当解析");
        assert_eq!(
            frame,
            Frame::GoAway {
                last_stream_id: 5,
                code: 1,
                debug: Vec::new()
            }
        );
    }

    #[test]
    fn a_truncated_goaway_is_rejected() {
        let bytes = vec![0, 0, 7, 0x7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE),
            Err(FrameError::Truncated("GOAWAY"))
        );
    }
}
