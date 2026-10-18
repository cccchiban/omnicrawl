//! HTTP/2 连接层：把帧层与 HPACK 串成可以真正发起请求的状态机。
//!
//! 职责边界：帧层只管「字节 ↔ 帧」，这一层管**顺序与状态**——流号分配、流状态迁移、
//! 流量控制窗口、HEADERS + CONTINUATION 的拼接、SETTINGS 协商结果。
//!
//! 三条必须守住的规则：
//!
//! 1. **一个头部块必须连续发送完成。** HEADERS 之后、END_HEADERS 之前，连接上不允许
//!    插入任何其他帧（RFC 9113 §6.2）。因此发送侧会一次性排出 HEADERS + 全部
//!    CONTINUATION，接收侧则在拼接期间拒绝其他流的帧。
//! 2. **流号必须单调递增，且客户端用奇数。** 复用或倒退都是 `PROTOCOL_ERROR`。
//! 3. **两个窗口都要扣：连接级与流级。** 只扣一个会在对大响应体时死锁——服务端会
//!    因为连接窗口耗尽而暂停所有流。

use std::collections::HashMap;

use crate::hpack::{Decoder, Encoder, Header, HpackError};
use crate::http2::{
    encode_frame, header_value, parse_frame, setting_id, ErrorCode, Frame, FrameError, Setting,
    DEFAULT_MAX_FRAME_SIZE, DEFAULT_WINDOW_SIZE, MAX_MAX_FRAME_SIZE,
};

/// 流状态（只保留客户端需要区分的几个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// 已发 HEADERS、等响应头。
    Open,
    /// 收到响应头且未结束（还可能有 DATA）。
    HalfClosedRemote,
    /// 已收到 END_STREAM。
    Closed,
    /// 任一侧发了 RST_STREAM。
    Reset,
}

/// 一条流上的进展。
#[derive(Debug, Clone)]
struct Stream {
    state: StreamState,
    /// 本端还能发送的字节数。
    send_window: i64,
    /// 对端还能发送给本端的字节数。
    recv_window: i64,
    /// 已收到的头部块片段。
    header_fragments: Vec<u8>,
    /// 已收到的响应头。
    headers: Vec<Header>,
    /// 已收到的响应体。
    body: Vec<u8>,
    /// 对端有没有宣告过 END_STREAM。
    remote_closed: bool,
}

impl Stream {
    fn new(send_window: i64, recv_window: i64) -> Self {
        Self {
            state: StreamState::Open,
            send_window,
            recv_window,
            header_fragments: Vec::new(),
            headers: Vec::new(),
            body: Vec::new(),
            remote_closed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionError {
    Frame(FrameError),
    Hpack(HpackError),
    /// 收到 GOAWAY；`last_stream_id` 之前的流可能已完成。
    GoAway {
        last_stream_id: u32,
        code: u32,
    },
    /// 对端重置了流。
    StreamReset {
        stream_id: u32,
        code: u32,
    },
    /// 对端拒绝推送（我们作为客户端本来就不接受）。
    UnexpectedPush {
        promised_stream_id: u32,
    },
    /// 协议违规，需以该错误码发 GOAWAY。
    Protocol {
        code: ErrorCode,
        detail: String,
    },
    /// 操作与当前状态不符（本端错误，不发 GOAWAY）。
    State(String),
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Frame(error) => write!(formatter, "帧层错误：{error}"),
            Self::Hpack(error) => write!(formatter, "头部压缩错误：{error}"),
            Self::GoAway {
                last_stream_id,
                code,
            } => write!(
                formatter,
                "对端发来 GOAWAY：last_stream_id={last_stream_id} code={code}"
            ),
            Self::StreamReset { stream_id, code } => {
                write!(formatter, "流 {stream_id} 被重置：code={code}")
            }
            Self::UnexpectedPush { promised_stream_id } => {
                write!(
                    formatter,
                    "未启用推送却收到 PUSH_PROMISE（流 {promised_stream_id}）"
                )
            }
            Self::Protocol { code, detail } => write!(formatter, "协议违规（{code:?}）：{detail}"),
            Self::State(detail) => write!(formatter, "状态错误：{detail}"),
        }
    }
}

impl std::error::Error for ConnectionError {}

impl From<FrameError> for ConnectionError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error)
    }
}

impl From<HpackError> for ConnectionError {
    fn from(error: HpackError) -> Self {
        Self::Hpack(error)
    }
}

/// 客户端连接。只实现客户端一侧：不处理服务端推送、不做服务端流控协商。
pub struct ClientConnection {
    encoder: Encoder,
    decoder: Decoder,
    streams: HashMap<u32, Stream>,
    /// 下一个可用的客户端流号（奇数，从 1 开始）。
    next_stream_id: u32,
    /// 连接级发送窗口。
    connection_send_window: i64,
    /// 连接级接收窗口。
    connection_recv_window: i64,
    /// 对端 SETTINGS 允许的最大帧载荷。
    peer_max_frame_size: usize,
    /// 本端宣告的最大帧载荷。
    local_max_frame_size: usize,
    /// 本端宣告的 `MAX_HEADER_LIST_SIZE`：解码后的头部列表上限（RFC 9113 §6.5.2）。
    ///
    /// `None` 表示不设限制。声明了就必须真的执行——声明一个上限却照单全收，
    /// 等于把「我们不会被巨大的头打爆」这句话说给对端听，自己却不兑现。
    local_max_header_list_size: Option<usize>,
    /// 对端 SETTINGS 给出的初始窗口。
    peer_initial_window: i64,
    /// 本端 SETTINGS 给出的初始窗口。
    local_initial_window: i64,
    /// 本端 SETTINGS 的条目与顺序（HTTP/2 指纹的一部分）。
    settings: Vec<(u16, u32)>,
    /// SETTINGS 之后要补发的连接级 WINDOW_UPDATE 增量（0 表示不发）。
    connection_window_update: u32,
    /// 正在拼接的头部块所属流号；非空时其他帧一律拒绝。
    pending_headers: Option<u32>,
    /// 已收到的 GOAWAY：`(last_stream_id, code, debug)`。
    ///
    /// `debug` 必须留着：不少服务端用 `NO_ERROR` 关连接表达「我不想按你这套协议谈」，
    /// 真正的理由只写在这个字段里。丢掉它，排障时就只剩一个看起来像「正常关闭」的错误码。
    goaway: Option<(u32, u32, Vec<u8>)>,
    /// 需要回复的 SETTINGS ACK 计数。
    pending_settings_ack: bool,
    /// 跨 `feed` 调用残留的不完整帧字节。
    ///
    /// 必须有这个缓冲：TLS 记录的边界与 HTTP/2 帧的边界毫无关系，一个帧被切在两条
    /// 甚至三条记录里是常态。若不在这里缓存，调用方丢掉「未消费」的尾巴就等于丢帧。
    partial_frame: Vec<u8>,
}

impl ClientConnection {
    pub fn new() -> Self {
        Self {
            encoder: Encoder::new(4096),
            decoder: Decoder::new(4096),
            streams: HashMap::new(),
            next_stream_id: 1,
            connection_send_window: DEFAULT_WINDOW_SIZE as i64,
            connection_recv_window: DEFAULT_WINDOW_SIZE as i64,
            peer_max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            local_max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            local_max_header_list_size: None,
            peer_initial_window: DEFAULT_WINDOW_SIZE as i64,
            local_initial_window: DEFAULT_WINDOW_SIZE as i64,
            // 默认就按 Chromium 抓包发：SETTINGS 是 HTTP/2 指纹的一部分，空设置会让
            // 做指纹校验的站点直接 GOAWAY（实测 tls.browserleaks.com）。
            settings: crate::profile::http2_profile_for(crate::profile::Family::Chrome).settings,
            connection_window_update: crate::profile::http2_profile_for(
                crate::profile::Family::Chrome,
            )
            .connection_window_update,
            pending_headers: None,
            goaway: None,
            pending_settings_ack: false,
            partial_frame: Vec::new(),
        }
    }

    /// 覆盖本端的 HTTP/2 连接指纹。
    ///
    /// 必须在 [`Self::preamble`] 之前调用：SETTINGS 的内容与顺序就是指纹本身，发出去之后
    /// 不能改。默认值取 Chromium 抓包，见 [`crate::profile::http2_profile_for`]。
    ///
    /// 这里同时调整本端的接收窗口与帧上限，让「声明」与「实际接受」一致——声明了
    /// `INITIAL_WINDOW_SIZE` 却不按它做流控，会在收到大响应时提前拒帧。
    pub fn apply_profile(&mut self, profile: &crate::profile::Http2Profile) {
        self.local_initial_window = profile.local_initial_window as i64;
        self.local_max_frame_size = profile.local_max_frame_size as usize;
        self.settings = profile.settings.clone();
        self.connection_window_update = profile.connection_window_update;
        // 我们随后会发一帧 WINDOW_UPDATE 把连接窗口放大这么多，本端的记账必须同步加上，
        // 否则「已经告诉对端可以多发 15 MB，自己却只肯收 64 KB」——一旦对端真按声明发，
        // 这里就会误判 FlowControlError。声明与执行必须是同一件事。
        self.connection_recv_window =
            crate::http2::DEFAULT_WINDOW_SIZE as i64 + profile.connection_window_update as i64;
        // 声明了多少动态表容量，解码端就必须真的能建那么大。
        //
        // 这一条曾经错了：SETTINGS 里声明的 `HEADER_TABLE_SIZE` 是抓包值 65536，而解码器
        // 仍以 4096 为上限。对端按 65536 填表时，它算出的索引在我们这边超出表范围，
        // 报错是「索引超出范围」——看起来像 HPACK 实现有 bug，实际是容量没跟着声明走。
        let table_size = profile
            .settings
            .iter()
            .find(|(id, _)| *id == setting_id::HEADER_TABLE_SIZE)
            .map(|(_, value)| *value as usize)
            .unwrap_or(crate::http2::DEFAULT_HEADER_TABLE_SIZE);
        self.decoder = Decoder::new(table_size);
        // 声明了就执行：解码后若超过这个上限，按协议错误处理而不是把超大头列表塞给上层。
        self.local_max_header_list_size = profile
            .settings
            .iter()
            .find(|(id, _)| *id == setting_id::MAX_HEADER_LIST_SIZE)
            .map(|(_, value)| *value as usize);
    }

    /// 连接建立后的第一件事：发送本端 SETTINGS（以及必要时的连接窗口放大）。
    ///
    /// 顺序不能省：RFC 9113 §3.4 要求连接前言（24 字节固定串 `PRI * HTTP/2.0` 加两个空行与 `SM`）之后
    /// 立刻发 SETTINGS，否则服务端会等超时。
    ///
    /// SETTINGS 之后若连接窗口需要放大，紧接着发一帧连接级 WINDOW_UPDATE：真实 Chromium
    /// 就是这么做的（本机 Edge 145 抓包，见 profile 的 `Http2Profile` 文档）。不少站点按
    /// 这套字节序列判断客户端是不是浏览器。
    pub fn preamble(&self) -> Vec<u8> {
        let settings = Frame::Settings {
            ack: false,
            settings: self
                .settings
                .iter()
                .map(|(id, value)| Setting {
                    id: *id,
                    value: *value,
                })
                .collect(),
        };
        let mut out = encode_frame(&settings).expect("SETTINGS 一定合法");
        if self.connection_window_update > 0 {
            out.extend_from_slice(
                &encode_frame(&Frame::WindowUpdate {
                    stream_id: 0,
                    increment: self.connection_window_update,
                })
                .expect("WINDOW_UPDATE 一定合法"),
            );
        }
        out
    }

    pub fn goaway(&self, code: ErrorCode) -> Vec<u8> {
        let frame = Frame::GoAway {
            last_stream_id: self.next_stream_id.saturating_sub(2),
            code: code as u32,
            debug: Vec::new(),
        };
        encode_frame(&frame).expect("GOAWAY 一定合法")
    }

    /// 发起一个请求，返回要发送的字节（HEADERS + 可能的 CONTINUATION）。
    ///
    /// `end_stream` 为真时表示没有请求体，HEADERS 直接带 END_STREAM。
    pub fn send_request(
        &mut self,
        headers: &[Header],
        end_stream: bool,
    ) -> Result<(u32, Vec<u8>), ConnectionError> {
        if let Some((last, _, _)) = self.goaway {
            return Err(ConnectionError::GoAway {
                last_stream_id: last,
                code: 0,
            });
        }
        if self.next_stream_id > 0x7fff_ffff {
            return Err(ConnectionError::State("客户端流号耗尽".to_string()));
        }
        let stream_id = self.next_stream_id;
        self.next_stream_id += 2;
        self.streams.insert(
            stream_id,
            Stream::new(self.peer_initial_window, self.local_initial_window),
        );
        let block = self.encoder.encode(headers);
        let bytes = self.frame_header_block(stream_id, &block, end_stream)?;
        Ok((stream_id, bytes))
    }

    /// 把头部块拆成 HEADERS + CONTINUATION。
    ///
    /// 首帧带 END_STREAM（如果请求无体），末帧带 END_HEADERS。
    fn frame_header_block(
        &self,
        stream_id: u32,
        block: &[u8],
        end_stream: bool,
    ) -> Result<Vec<u8>, ConnectionError> {
        let max = self.peer_max_frame_size;
        let mut out = Vec::new();
        if block.len() <= max {
            out.extend_from_slice(&encode_frame(&Frame::Headers {
                stream_id,
                end_stream,
                end_headers: true,
                fragment: block.to_vec(),
            })?);
            return Ok(out);
        }
        let chunks: Vec<&[u8]> = block.chunks(max).collect();
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index == 0 {
                out.extend_from_slice(&encode_frame(&Frame::Headers {
                    stream_id,
                    end_stream,
                    end_headers: false,
                    fragment: chunk.to_vec(),
                })?);
            } else {
                out.extend_from_slice(&encode_frame(&Frame::Continuation {
                    stream_id,
                    end_headers: index == last,
                    fragment: chunk.to_vec(),
                })?);
            }
        }
        Ok(out)
    }

    /// 发送请求体，按流量控制与最大帧载荷切片。
    ///
    /// 返回 `(要发送的字节, 实际排入的载荷字节数)`。两者长度不同：帧头不计入窗口，
    /// 因此调用方必须用后者推进自己的缓冲区游标。
    ///
    /// 一次调用会**填满当前可用窗口**并自动切成多条 DATA 帧，而不是每帧一次调用——
    /// 只按帧上限切片会让每次只能走 16 KB，对大请求体是几十倍的往返开销。
    /// 窗口不足时返回的载荷数会小于 `data.len()`，调用方需等 WINDOW_UPDATE 再来。
    pub fn send_body(
        &mut self,
        stream_id: u32,
        data: &[u8],
        end_stream: bool,
    ) -> Result<(Vec<u8>, usize), ConnectionError> {
        let stream = self
            .streams
            .get(&stream_id)
            .ok_or_else(|| ConnectionError::State(format!("流 {stream_id} 不存在")))?;
        if stream.state == StreamState::Closed || stream.state == StreamState::Reset {
            return Err(ConnectionError::State(format!("流 {stream_id} 已关闭")));
        }
        // 上限取「两个窗口的较小者」，与单帧载荷无关：帧上限只决定切成几片。
        let allowed = self.connection_send_window.min(stream.send_window).max(0) as usize;
        let sendable = allowed.min(data.len());
        if sendable == 0 {
            return Ok((Vec::new(), 0));
        }
        let frame_limit = self.peer_max_frame_size;
        // 只有「本次把 data 全部发完」且调用方要求结束时，末帧才带 END_STREAM。
        // 否则后续还有数据，提前带 END_STREAM 会让对端认为请求体已完整。
        let finish = end_stream && sendable == data.len();
        let chunks: Vec<&[u8]> = data[..sendable].chunks(frame_limit).collect();
        let last = chunks.len() - 1;
        let mut out = Vec::new();
        for (index, chunk) in chunks.into_iter().enumerate() {
            out.extend_from_slice(&encode_frame(&Frame::Data {
                stream_id,
                end_stream: finish && index == last,
                data: chunk.to_vec(),
            })?);
        }
        self.connection_send_window -= sendable as i64;
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.send_window -= sendable as i64;
        }
        Ok((out, sendable))
    }

    /// 喂入一段收到的字节，返回所有产出的事件与需要发回的字节。
    ///
    /// `bytes` 可以是任意切片边界（TCP 流或 TLS 记录）。不完整的尾部帧会被内部缓存到
    /// 下一次调用，因此 `consumed` 恒等于本次传入的字节数——调用方**总是**可以丢弃
    /// 已喂入的部分。这一点很重要：TLS 记录边界与 HTTP/2 帧边界无关，一个帧被切在
    /// 多条记录里是常态，若让调用方负责保留尾巴，稍不注意就丢帧。
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Feed, ConnectionError> {
        let mut events = Vec::new();
        let mut out = Vec::new();
        // 先把上次残留的半帧与本次输入接起来，帧边界不必与记录边界对齐。
        let mut buffer = std::mem::take(&mut self.partial_frame);
        buffer.extend_from_slice(bytes);
        let mut offset = 0;
        loop {
            let remaining = &buffer[offset..];
            if remaining.is_empty() {
                break;
            }
            let (frame, consumed) = match parse_frame(remaining, self.local_max_frame_size) {
                Ok(parsed) => parsed,
                // 帧还没收全：留在缓冲里等下一批字节。
                Err(FrameError::Incomplete { .. }) => break,
                Err(error) => {
                    // 协议错误不可恢复，残留字节也不必保留。
                    self.partial_frame.clear();
                    return Err(error.into());
                }
            };
            offset += consumed;
            self.handle_frame(frame, &mut events, &mut out)?;
        }
        // 未解析完的尾巴留到下次。
        self.partial_frame = buffer[offset..].to_vec();
        Ok(Feed {
            events,
            outbound: out,
            consumed: bytes.len(),
        })
    }

    fn handle_frame(
        &mut self,
        frame: Frame,
        events: &mut Vec<Event>,
        out: &mut Vec<u8>,
    ) -> Result<(), ConnectionError> {
        // 头部块拼接期间，只允许同流的 CONTINUATION。
        if let Some(pending) = self.pending_headers {
            match &frame {
                Frame::Continuation {
                    stream_id,
                    end_headers,
                    fragment,
                } => {
                    if *stream_id != pending {
                        return Err(ConnectionError::Protocol {
                            code: ErrorCode::ProtocolError,
                            detail: format!(
                                "头部块拼接中收到流 {stream_id} 的 CONTINUATION，期望 {pending}"
                            ),
                        });
                    }
                    self.append_header_fragment(pending, fragment.clone(), *end_headers, events)?;
                    return Ok(());
                }
                other => {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::ProtocolError,
                        detail: format!(
                            "头部块拼接中收到 {:?}，只允许 CONTINUATION",
                            std::mem::discriminant(other)
                        ),
                    });
                }
            }
        }

        match frame {
            Frame::Settings { ack, settings } => {
                if ack {
                    self.pending_settings_ack = false;
                    events.push(Event::SettingsAcknowledged);
                } else {
                    self.apply_settings(&settings)?;
                    self.pending_settings_ack = true;
                    out.extend_from_slice(
                        &encode_frame(&Frame::Settings {
                            ack: true,
                            settings: Vec::new(),
                        })
                        .expect("ACK 一定合法"),
                    );
                    events.push(Event::PeerSettings(settings));
                }
            }
            Frame::Headers {
                stream_id,
                end_stream,
                end_headers,
                fragment,
            } => {
                let stream =
                    self.streams
                        .get_mut(&stream_id)
                        .ok_or_else(|| ConnectionError::Protocol {
                            code: ErrorCode::ProtocolError,
                            detail: format!("收到未知流 {stream_id} 的 HEADERS"),
                        })?;
                if stream.remote_closed {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::StreamClosed,
                        detail: format!("流 {stream_id} 已结束却仍收到 HEADERS"),
                    });
                }
                stream.header_fragments.clear();
                stream.header_fragments.extend_from_slice(&fragment);
                if end_stream {
                    stream.remote_closed = true;
                }
                if end_headers {
                    self.finish_headers(stream_id, events)?;
                } else {
                    self.pending_headers = Some(stream_id);
                }
            }
            Frame::Continuation { stream_id, .. } => {
                // 能走到这里说明 pending_headers 为空：无主的 CONTINUATION 是协议违规。
                return Err(ConnectionError::Protocol {
                    code: ErrorCode::ProtocolError,
                    detail: format!("流 {stream_id} 收到无主的 CONTINUATION"),
                });
            }
            Frame::Data {
                stream_id,
                end_stream,
                data,
            } => {
                let length = data.len() as i64;
                let stream =
                    self.streams
                        .get_mut(&stream_id)
                        .ok_or_else(|| ConnectionError::Protocol {
                            code: ErrorCode::ProtocolError,
                            detail: format!("收到未知流 {stream_id} 的 DATA"),
                        })?;
                if stream.remote_closed {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::StreamClosed,
                        detail: format!("流 {stream_id} 已结束却仍收到 DATA"),
                    });
                }
                self.connection_recv_window -= length;
                stream.recv_window -= length;
                if self.connection_recv_window < 0 || stream.recv_window < 0 {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::FlowControlError,
                        detail: format!("流 {stream_id} 的 DATA 超出窗口"),
                    });
                }
                stream.body.extend_from_slice(&data);
                if stream.state == StreamState::Open {
                    stream.state = StreamState::HalfClosedRemote;
                }
                if end_stream {
                    stream.remote_closed = true;
                    stream.state = StreamState::Closed;
                }
                // 立刻补窗口：否则大响应体会卡在窗口耗尽上。
                self.replenish(stream_id, length, out)?;
                events.push(Event::Data {
                    stream_id,
                    data,
                    end_stream,
                });
            }
            Frame::RstStream { stream_id, code } => {
                if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.state = StreamState::Reset;
                    stream.remote_closed = true;
                }
                events.push(Event::StreamReset { stream_id, code });
            }
            Frame::Ping { ack, payload } => {
                if !ack {
                    out.extend_from_slice(
                        &encode_frame(&Frame::Ping { ack: true, payload })
                            .expect("PING ACK 一定合法"),
                    );
                }
                events.push(Event::Ping { ack });
            }
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => {
                if increment == 0 {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::ProtocolError,
                        detail: "WINDOW_UPDATE 的增量为 0".to_string(),
                    });
                }
                if stream_id == 0 {
                    self.connection_send_window += increment as i64;
                } else if let Some(stream) = self.streams.get_mut(&stream_id) {
                    stream.send_window += increment as i64;
                }
                events.push(Event::WindowUpdate {
                    stream_id,
                    increment,
                });
            }
            Frame::GoAway {
                last_stream_id,
                code,
                debug,
            } => {
                self.goaway = Some((last_stream_id, code, debug.clone()));
                events.push(Event::GoAway {
                    last_stream_id,
                    code,
                    debug,
                });
            }
            Frame::PushPromise {
                promised_stream_id, ..
            } => {
                // 我们在 SETTINGS 里把 ENABLE_PUSH 设为 0，对端不该推。
                return Err(ConnectionError::UnexpectedPush { promised_stream_id });
            }
            Frame::Priority { .. } => {}
            Frame::Unknown { .. } => {}
        }
        Ok(())
    }

    fn append_header_fragment(
        &mut self,
        stream_id: u32,
        fragment: Vec<u8>,
        end_headers: bool,
        events: &mut Vec<Event>,
    ) -> Result<(), ConnectionError> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or_else(|| ConnectionError::State(format!("流 {stream_id} 不存在")))?;
        stream.header_fragments.extend_from_slice(&fragment);
        if end_headers {
            self.pending_headers = None;
            self.finish_headers(stream_id, events)?;
        }
        Ok(())
    }

    fn finish_headers(
        &mut self,
        stream_id: u32,
        events: &mut Vec<Event>,
    ) -> Result<(), ConnectionError> {
        let block = {
            let stream = self
                .streams
                .get_mut(&stream_id)
                .ok_or_else(|| ConnectionError::State(format!("流 {stream_id} 不存在")))?;
            std::mem::take(&mut stream.header_fragments)
        };
        let headers = self.decoder.decode(&block)?;
        if let Some(limit) = self.local_max_header_list_size {
            // 口径与 HPACK 表项大小一致：名字 + 值 + 32 字节固定开销（RFC 7541 §4.1）。
            let total: usize = headers
                .iter()
                .map(|header| header.name.len() + header.value.len() + 32)
                .sum();
            if total > limit {
                return Err(ConnectionError::Protocol {
                    code: ErrorCode::EnhanceYourCalm,
                    detail: format!("头部列表 {total} 字节超过本端声明的上限 {limit}"),
                });
            }
        }
        let stream = self.streams.get_mut(&stream_id).expect("刚检查过");
        stream.headers = headers.clone();
        if stream.remote_closed {
            stream.state = StreamState::Closed;
        } else {
            stream.state = StreamState::HalfClosedRemote;
        }
        events.push(Event::Headers { stream_id, headers });
        Ok(())
    }

    /// 收到 DATA 后立刻把窗口补回去。
    fn replenish(
        &mut self,
        stream_id: u32,
        length: i64,
        out: &mut Vec<u8>,
    ) -> Result<(), ConnectionError> {
        if length == 0 {
            return Ok(());
        }
        let increment = length as u32;
        self.connection_recv_window += length;
        out.extend_from_slice(
            &encode_frame(&Frame::WindowUpdate {
                stream_id: 0,
                increment,
            })
            .expect("连接级 WINDOW_UPDATE 一定合法"),
        );
        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.recv_window += length;
        }
        // 流已关闭时不必再补流级窗口。
        if self
            .streams
            .get(&stream_id)
            .is_some_and(|stream| stream.state != StreamState::Closed)
        {
            out.extend_from_slice(
                &encode_frame(&Frame::WindowUpdate {
                    stream_id,
                    increment,
                })
                .expect("流级 WINDOW_UPDATE 一定合法"),
            );
        }
        Ok(())
    }

    /// 应用对端 SETTINGS。
    fn apply_settings(&mut self, settings: &[Setting]) -> Result<(), ConnectionError> {
        for setting in settings {
            match setting.id {
                setting_id::HEADER_TABLE_SIZE => {
                    self.encoder.set_peer_limit(setting.value as usize);
                }
                setting_id::MAX_FRAME_SIZE => {
                    let value = setting.value as usize;
                    if !(16_384..=MAX_MAX_FRAME_SIZE).contains(&value) {
                        return Err(ConnectionError::Protocol {
                            code: ErrorCode::ProtocolError,
                            detail: format!("SETTINGS_MAX_FRAME_SIZE {value} 越界"),
                        });
                    }
                    self.peer_max_frame_size = value;
                }
                setting_id::INITIAL_WINDOW_SIZE => {
                    let value = setting.value as i64;
                    if value > 0x7fff_ffff {
                        return Err(ConnectionError::Protocol {
                            code: ErrorCode::FlowControlError,
                            detail: format!("SETTINGS_INITIAL_WINDOW_SIZE {value} 越界"),
                        });
                    }
                    // 窗口变化量要施加到所有已存在的流上。
                    let delta = value - self.peer_initial_window;
                    self.peer_initial_window = value;
                    for stream in self.streams.values_mut() {
                        stream.send_window += delta;
                    }
                }
                // ENABLE_PUSH 只允许 0/1；服务端发它本身是协议违规（只有客户端配发），
                // 但那只在服务端实现里才需要报错，客户端这里只校验取值。
                setting_id::ENABLE_PUSH if setting.value > 1 => {
                    return Err(ConnectionError::Protocol {
                        code: ErrorCode::ProtocolError,
                        detail: format!("SETTINGS_ENABLE_PUSH {} 只能是 0/1", setting.value),
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 查询一条流的当前状态。
    pub fn stream_state(&self, stream_id: u32) -> Option<StreamState> {
        self.streams.get(&stream_id).map(|stream| stream.state)
    }

    /// 某条流上已收齐的响应头。
    pub fn response_headers(&self, stream_id: u32) -> Option<&[Header]> {
        self.streams
            .get(&stream_id)
            .filter(|stream| !stream.headers.is_empty())
            .map(|stream| stream.headers.as_slice())
    }

    /// 某条流上已收齐的响应体。
    pub fn response_body(&self, stream_id: u32) -> Option<&[u8]> {
        self.streams
            .get(&stream_id)
            .map(|stream| stream.body.as_slice())
    }

    /// 便捷读取响应状态码。
    pub fn status(&self, stream_id: u32) -> Option<u16> {
        let headers = self.response_headers(stream_id)?;
        header_value(headers, ":status")?.parse().ok()
    }

    pub fn peer_max_frame_size(&self) -> usize {
        self.peer_max_frame_size
    }

    pub fn connection_send_window(&self) -> i64 {
        self.connection_send_window
    }

    pub fn goaway_received(&self) -> Option<(u32, u32)> {
        self.goaway.as_ref().map(|(last, code, _)| (*last, *code))
    }

    /// 已收到的 GOAWAY 的 `debug` 载荷（通常是服务端的诊断文本）。
    pub fn goaway_debug(&self) -> Option<&[u8]> {
        self.goaway
            .as_ref()
            .map(|(_, _, debug)| debug.as_slice())
            .filter(|debug| !debug.is_empty())
    }
}

impl Default for ClientConnection {
    fn default() -> Self {
        Self::new()
    }
}

/// [`ClientConnection::feed`] 的返回值。
#[derive(Debug, Clone)]
pub struct Feed {
    /// 本次解析出的语义事件。
    pub events: Vec<Event>,
    /// 需要写回对端的字节（SETTINGS ACK、PING ACK、WINDOW_UPDATE）。
    pub outbound: Vec<u8>,
    /// 已消耗的输入字节数；其余留给下次。
    pub consumed: usize,
}

/// 由收到的帧派生的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Headers {
        stream_id: u32,
        headers: Vec<Header>,
    },
    Data {
        stream_id: u32,
        data: Vec<u8>,
        end_stream: bool,
    },
    StreamReset {
        stream_id: u32,
        code: u32,
    },
    GoAway {
        last_stream_id: u32,
        code: u32,
        debug: Vec<u8>,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    Ping {
        ack: bool,
    },
    PeerSettings(Vec<Setting>),
    SettingsAcknowledged,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_headers(path: &str) -> Vec<Header> {
        vec![
            Header::new(":method", "GET"),
            Header::new(":scheme", "https"),
            Header::new(":authority", "example.com"),
            Header::new(":path", path),
            Header::new("user-agent", "omnicrawl/1.0"),
        ]
    }

    /// 模拟服务端：解出请求头，回一个 200 与若干 DATA。
    fn server_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut encoder = Encoder::new(4096);
        let headers = vec![
            Header::new(":status", status),
            Header::new("content-type", "text/plain"),
            Header::new("content-length", body.len().to_string()),
        ];
        let block = encoder.encode(&headers);
        let mut out = Vec::new();
        out.extend_from_slice(
            &encode_frame(&Frame::Headers {
                stream_id: 1,
                end_stream: body.is_empty(),
                end_headers: true,
                fragment: block,
            })
            .expect("编码 HEADERS"),
        );
        if !body.is_empty() {
            out.extend_from_slice(
                &encode_frame(&Frame::Data {
                    stream_id: 1,
                    end_stream: true,
                    data: body.to_vec(),
                })
                .expect("编码 DATA"),
            );
        }
        out
    }

    /// 抓包基准：SETTINGS 的内容与顺序必须与真实 Chromium 一致，并紧跟一帧
    /// 连接级 WINDOW_UPDATE。这两帧加起来就构成 HTTP/2 连接指纹。
    #[test]
    fn the_preamble_reproduces_the_captured_chromium_settings() {
        let connection = ClientConnection::new();
        let bytes = connection.preamble();

        let (frame, used) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("解析 SETTINGS");
        let Frame::Settings { ack, settings } = frame else {
            panic!("第一帧应为 SETTINGS");
        };
        assert!(!ack, "首帧不是 ACK");
        let ids: Vec<u16> = settings.iter().map(|setting| setting.id).collect();
        assert_eq!(
            ids,
            vec![
                setting_id::HEADER_TABLE_SIZE,
                setting_id::ENABLE_PUSH,
                setting_id::INITIAL_WINDOW_SIZE,
                setting_id::MAX_HEADER_LIST_SIZE,
            ],
            "顺序即指纹，必须与 Edge 145 抓包一致"
        );
        assert_eq!(settings[0].value, 65536);
        assert_eq!(settings[1].value, 0, "Chromium 显式关闭推送");
        assert_eq!(settings[2].value, 6_291_456);
        assert_eq!(settings[3].value, 262_144);

        let (frame, _) =
            parse_frame(&bytes[used..], DEFAULT_MAX_FRAME_SIZE).expect("解析 WINDOW_UPDATE");
        match frame {
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => {
                assert_eq!(stream_id, 0, "必须是连接级窗口");
                assert_eq!(increment, 15_663_105, "与抓包一致");
            }
            other => panic!("第二帧应为 WINDOW_UPDATE，得到 {other:?}"),
        }
    }

    /// 换族必须真的换掉线上字节，否则 `impersonate` 只是个摆设。
    #[test]
    fn switching_family_changes_the_preamble_bytes() {
        let mut chrome = ClientConnection::new();
        let mut safari = ClientConnection::new();
        safari.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Safari,
        ));
        chrome.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Chrome,
        ));
        assert_ne!(
            chrome.preamble(),
            safari.preamble(),
            "chrome 与 safari 的前言不该相同"
        );
    }

    /// 声明多少动态表容量，解码端就必须真的能建那么大。
    ///
    /// 这个用例让对端按我们声明的 65536 填满表（每项约 1 KB，六项就超过默认的 4096），
    /// 再用索引引用**最早插入**的那一项。若解码表仍按 4096 裁切，那一项早被挤掉，
    /// 报错会是「索引超出范围」——看起来像 HPACK 实现有 bug，实际是容量没跟着声明走。
    #[test]
    fn the_declared_header_table_size_is_actually_accepted() {
        // 每一项的值约 1000 字节，加上名字与 32 字节固定开销。
        let value = "v".repeat(1000);
        let headers: Vec<Header> = (0..6)
            .map(|index| Header::new(format!("x-probe-{index}"), value.clone()))
            .collect();

        // 编码端按 65536 建表，先填满，再只发一次「引用最早那项」的块。
        let mut encoder = Encoder::new(65_536);
        encoder.set_peer_limit(65_536);
        let _ = encoder.encode(&headers);
        assert!(
            encoder.table().size() > 4096,
            "构造的表必须超过默认 4096，实际 {}",
            encoder.table().size()
        );
        // 最早插入的项此刻在动态表最深处；索引 = 静态表 61 项之后。
        let oldest_index = 61 + encoder.table().len();

        let mut connection = ClientConnection::new();
        connection.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Chrome,
        ));
        let (stream_id, _) = connection
            .send_request(&[Header::new(":method", "GET")], false)
            .expect("建流");
        // 同一个头部块要分两次喂：先填表，再引用。所以用一个连接喂两轮。
        let fill_block = {
            let mut encoder = Encoder::new(65_536);
            encoder.set_peer_limit(65_536);
            encoder.encode(&headers)
        };
        connection
            .feed(
                &encode_frame(&Frame::Headers {
                    stream_id,
                    end_stream: false,
                    end_headers: true,
                    fragment: fill_block,
                })
                .expect("编码 HEADERS"),
            )
            .unwrap_or_else(|error| panic!("按声明容量填表不该失败：{error}"));

        // 第二轮：只发一个索引，指向最早插入的那一项。
        let mut reference = Vec::new();
        crate::hpack::encode_integer(oldest_index, 7, 0x80, &mut reference);
        let feed = connection
            .feed(
                &encode_frame(&Frame::Headers {
                    stream_id,
                    end_stream: true,
                    end_headers: true,
                    fragment: reference,
                })
                .expect("编码 HEADERS"),
            )
            .unwrap_or_else(|error| panic!("引用深表项不该失败：{error}"));
        assert!(!feed.events.is_empty(), "应当产出事件，实际没有");
    }

    /// 我们告诉对端「连接窗口有 15 MB」，本端记账就必须也是 15 MB。
    ///
    /// 否则对端按声明发来 1 MB，我们会在 64 KB 处判 `FlowControlError`——声明与执行
    /// 不一致，错误信息还会指向「超出窗口」，看不出真正原因。
    #[test]
    fn the_announced_connection_window_is_the_one_we_track() {
        let mut connection = ClientConnection::new();
        connection.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Chrome,
        ));
        let profile = crate::profile::http2_profile_for(crate::profile::Family::Chrome);
        let expected =
            crate::http2::DEFAULT_WINDOW_SIZE as i64 + profile.connection_window_update as i64;
        assert_eq!(
            connection.connection_recv_window, expected,
            "本端记账的连接窗口必须等于「默认 + 已宣告的增量」"
        );
        // 而宣告的增量正是抓包值。
        assert_eq!(profile.connection_window_update, 15_663_105);
    }

    /// `MAX_HEADER_LIST_SIZE` 声明了就必须执行：超限按协议错误拒绝，而不是照单全收。
    #[test]
    fn the_declared_header_list_limit_is_enforced() {
        let mut connection = ClientConnection::new();
        // Safari 档案是最小的设置集合，不含 MAX_HEADER_LIST_SIZE，正好用来对照「未声明=不限制」。
        connection.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Safari,
        ));
        assert_eq!(
            connection.local_max_header_list_size, None,
            "Safari 档案没有声明上限，就不该凭空限制"
        );

        let mut connection = ClientConnection::new();
        connection.apply_profile(&crate::profile::http2_profile_for(
            crate::profile::Family::Chrome,
        ));
        assert_eq!(
            connection.local_max_header_list_size,
            Some(262_144),
            "Chromium 抓包里声明了 262144"
        );

        // 造一个超过上限的头部块。
        let huge = "v".repeat(300_000);
        let headers = vec![Header::new("x-huge", huge)];
        let mut encoder = Encoder::new(65_536);
        encoder.set_peer_limit(65_536);
        let block = encoder.encode(&headers);

        let (stream_id, _) = connection
            .send_request(&[Header::new(":method", "GET")], false)
            .expect("建流");
        // 头部块远超单帧上限：按真实节奏切成 HEADERS + CONTINUATION，并**逐帧**喂进去。
        // 一次把拼接好的多帧字节丢进去会让帧层先报「帧载荷超限」，掩盖真正要测的上限。
        let chunks: Vec<&[u8]> = block.chunks(DEFAULT_MAX_FRAME_SIZE).collect();
        assert!(chunks.len() > 1, "头部块应当跨多帧，实际只有一帧");
        let last = chunks.len() - 1;
        let mut error = None;
        for (index, chunk) in chunks.into_iter().enumerate() {
            let frame = if index == 0 {
                Frame::Headers {
                    stream_id,
                    end_stream: false,
                    end_headers: index == last,
                    fragment: chunk.to_vec(),
                }
            } else {
                Frame::Continuation {
                    stream_id,
                    end_headers: index == last,
                    fragment: chunk.to_vec(),
                }
            };
            match connection.feed(&encode_frame(&frame).expect("编码")) {
                Ok(_) => {}
                Err(caught) => {
                    error = Some(caught);
                    break;
                }
            }
        }
        match error {
            Some(ConnectionError::Protocol { code, detail }) => {
                assert_eq!(code, ErrorCode::EnhanceYourCalm);
                assert!(detail.contains("超过本端声明的上限"), "{detail}");
            }
            Some(other) => panic!("应当是头部列表超限，得到 {other:?}"),
            None => panic!("超大头部列表没有被拒绝"),
        }
    }

    #[test]
    fn the_connection_preface_is_the_fixed_24_byte_string() {
        // 漏掉这对服务端来说就是「对端不是 HTTP/2 客户端」，会直接 GOAWAY。
        assert_eq!(crate::http2::CONNECTION_PREFACE.len(), 24);
        assert_eq!(
            crate::http2::CONNECTION_PREFACE,
            b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
        );
    }

    #[test]
    fn requests_use_odd_increasing_stream_ids() {
        let mut connection = ClientConnection::new();
        let (first, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("第一个请求");
        let (second, _) = connection
            .send_request(&request_headers("/next"), true)
            .expect("第二个请求");
        assert_eq!(first, 1);
        assert_eq!(second, 3);
        assert!(first % 2 == 1, "客户端流号必须是奇数");
    }

    #[test]
    fn a_request_headers_frame_is_decodable_by_the_peer() {
        let mut connection = ClientConnection::new();
        let (stream_id, bytes) = connection
            .send_request(&request_headers("/hello"), true)
            .expect("请求");
        let (frame, _) = parse_frame(&bytes, DEFAULT_MAX_FRAME_SIZE).expect("解析");
        match frame {
            Frame::Headers {
                stream_id: parsed_id,
                end_stream,
                end_headers,
                fragment,
            } => {
                assert_eq!(parsed_id, stream_id);
                assert!(end_stream, "无请求体时应带 END_STREAM");
                assert!(end_headers);
                let mut decoder = Decoder::new(4096);
                let headers = decoder.decode(&fragment).expect("解头部");
                assert_eq!(header_value(&headers, ":path"), Some("/hello"));
            }
            other => panic!("应为 HEADERS，得到 {other:?}"),
        }
    }

    #[test]
    fn a_full_response_cycle_delivers_headers_and_body() {
        let mut connection = ClientConnection::new();
        let (stream_id, _request) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");

        let response = server_response("200", b"hello world");
        let feed = connection.feed(&response).expect("应当处理响应");
        assert_eq!(feed.consumed, response.len());
        assert!(feed.events.iter().any(|event| matches!(
            event,
            Event::Headers { stream_id: id, .. } if *id == stream_id
        )));
        assert_eq!(connection.status(stream_id), Some(200));
        assert_eq!(
            connection.response_body(stream_id),
            Some(&b"hello world"[..])
        );
        assert_eq!(
            connection.stream_state(stream_id),
            Some(StreamState::Closed)
        );
    }

    #[test]
    fn receiving_settings_triggers_an_ack() {
        let mut connection = ClientConnection::new();
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: vec![Setting {
                id: setting_id::MAX_FRAME_SIZE,
                value: 32_768,
            }],
        })
        .expect("编码");
        let feed = connection.feed(&settings).expect("应当处理");
        assert_eq!(connection.peer_max_frame_size(), 32_768);
        assert!(!feed.outbound.is_empty(), "必须回 ACK");
        let (ack, _) = parse_frame(&feed.outbound, DEFAULT_MAX_FRAME_SIZE).expect("解析 ACK");
        assert_eq!(
            ack,
            Frame::Settings {
                ack: true,
                settings: Vec::new()
            }
        );
    }

    #[test]
    fn receiving_a_ping_echoes_it_back_with_ack() {
        let mut connection = ClientConnection::new();
        let ping = encode_frame(&Frame::Ping {
            ack: false,
            payload: *b"pingpong",
        })
        .expect("编码");
        let feed = connection.feed(&ping).expect("应当处理");
        let (ack, _) = parse_frame(&feed.outbound, DEFAULT_MAX_FRAME_SIZE).expect("解析");
        match ack {
            Frame::Ping { ack, payload } => {
                assert!(ack);
                assert_eq!(&payload, b"pingpong", "PING 载荷必须原样回显");
            }
            other => panic!("应为 PING，得到 {other:?}"),
        }
    }

    #[test]
    fn data_delivery_replenishes_both_windows() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        let before = connection.connection_recv_window;
        let response = server_response("200", b"0123456789");
        let feed = connection.feed(&response).expect("处理");
        assert_eq!(
            connection.connection_recv_window, before,
            "补过窗口后应回到原值"
        );
        // 至少一个连接级与一个流级 WINDOW_UPDATE。
        let mut updates = 0;
        let mut offset = 0;
        while offset < feed.outbound.len() {
            let (frame, used) =
                parse_frame(&feed.outbound[offset..], DEFAULT_MAX_FRAME_SIZE).expect("解析");
            if let Frame::WindowUpdate { stream_id: id, .. } = frame {
                assert!(
                    id == 0 || id == stream_id,
                    "WINDOW_UPDATE 的流号只可能是 0 或该流"
                );
                updates += 1;
            }
            offset += used;
        }
        // 该响应带 END_STREAM，流在收到 DATA 时已被标记关闭，
        // 因此只补连接级窗口——对已关闭的流发 WINDOW_UPDATE 是多余的。
        assert_eq!(updates, 1, "流已关闭时只补连接级窗口");
    }

    #[test]
    fn send_body_respects_the_peer_window() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/upload"), false)
            .expect("请求");
        // 默认窗口 65535，发 100000 字节时分两次。
        let payload = vec![9u8; 100_000];
        let (first, sent) = connection
            .send_body(stream_id, &payload, true)
            .expect("第一次发送");
        assert!(!first.is_empty());
        assert_eq!(sent, 65_535, "应一次填满整个窗口");
        assert_eq!(connection.connection_send_window(), 0, "窗口应被扣到 0");
        // 窗口耗尽后发不出任何字节。
        let (second, sent_again) = connection
            .send_body(stream_id, &payload[65_535..], true)
            .expect("第二次发送");
        assert!(second.is_empty(), "窗口为 0 时不应发出数据");
        assert_eq!(sent_again, 0);
        // 补窗口后可以继续。**两个窗口都要补**：只补连接级，流窗口仍是 0，
        // send_body 依旧一片都发不出去——这正是「两个窗口必须都扣」的另一面。
        let mut updates = encode_frame(&Frame::WindowUpdate {
            stream_id: 0,
            increment: 65_535,
        })
        .expect("编码");
        updates.extend_from_slice(
            &encode_frame(&Frame::WindowUpdate {
                stream_id,
                increment: 65_535,
            })
            .expect("编码"),
        );
        connection.feed(&updates).expect("处理 WINDOW_UPDATE");
        let (third, sent_third) = connection
            .send_body(stream_id, &payload[65_535..], true)
            .expect("第三次发送");
        assert!(!third.is_empty(), "补过窗口后应能继续发送");
        assert_eq!(sent_third, 34_465, "剩余部分一次发完");
        // 第三次是真正发完，末帧必须带 END_STREAM。
        let mut offset = 0;
        let mut last_end_stream = false;
        while offset < third.len() {
            let (frame, used) =
                parse_frame(&third[offset..], DEFAULT_MAX_FRAME_SIZE).expect("解析");
            if let Frame::Data { end_stream, .. } = frame {
                last_end_stream = end_stream;
            }
            offset += used;
        }
        assert!(last_end_stream, "全部发完时末帧必须带 END_STREAM");
    }

    #[test]
    fn a_large_header_block_is_split_into_continuations() {
        let mut connection = ClientConnection::new();
        // 先把对端最大帧压到最小 16384，再用一个超长头部触发分片。
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: vec![Setting {
                id: setting_id::MAX_FRAME_SIZE,
                value: 16_384,
            }],
        })
        .expect("编码");
        connection.feed(&settings).expect("处理 SETTINGS");
        let mut headers = request_headers("/");
        headers.push(Header::new("x-big", "z".repeat(40_000)));
        let (_stream_id, bytes) = connection.send_request(&headers, true).expect("请求");

        let mut offsets = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let (frame, used) = parse_frame(&bytes[offset..], 16_384).expect("解析");
            offsets.push(std::mem::discriminant(&frame));
            offset += used;
        }
        assert!(offsets.len() >= 3, "40000 字节头部块应至少切成 3 帧");
        // 首帧必须是 HEADERS，其余是 CONTINUATION。
        let (first, first_used) = parse_frame(&bytes, 16_384).expect("首帧");
        assert!(matches!(first, Frame::Headers { .. }));
        let (second, _) = parse_frame(&bytes[first_used..], 16_384).expect("次帧");
        assert!(matches!(second, Frame::Continuation { .. }));
    }

    #[test]
    fn a_split_header_block_is_assembled_on_receipt() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");

        // 手工构造一个被切开的头部块：HEADERS(无 END_HEADERS) + CONTINUATION。
        let mut encoder = Encoder::new(4096);
        let block = encoder.encode(&[
            Header::new(":status", "200"),
            Header::new("x-note", "split"),
        ]);
        let (head, tail) = block.split_at(2);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            &encode_frame(&Frame::Headers {
                stream_id,
                end_stream: false,
                end_headers: false,
                fragment: head.to_vec(),
            })
            .expect("HEADERS"),
        );
        bytes.extend_from_slice(
            &encode_frame(&Frame::Continuation {
                stream_id,
                end_headers: true,
                fragment: tail.to_vec(),
            })
            .expect("CONTINUATION"),
        );

        let feed = connection.feed(&bytes).expect("应当拼接");
        let headers = connection.response_headers(stream_id).expect("应有响应头");
        assert_eq!(header_value(headers, "x-note"), Some("split"));
        assert!(feed
            .events
            .iter()
            .any(|event| matches!(event, Event::Headers { .. })));
    }

    #[test]
    fn an_interleaved_frame_during_header_assembly_is_rejected() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        let mut encoder = Encoder::new(4096);
        let block = encoder.encode(&[Header::new(":status", "200")]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            &encode_frame(&Frame::Headers {
                stream_id,
                end_stream: false,
                end_headers: false,
                fragment: block.clone(),
            })
            .expect("HEADERS"),
        );
        // 中间插一个 PING：这违反 §6.2 的连续性要求。
        bytes.extend_from_slice(
            &encode_frame(&Frame::Ping {
                ack: false,
                payload: *b"12345678",
            })
            .expect("PING"),
        );
        let error = connection.feed(&bytes).expect_err("必须拒绝");
        assert!(
            matches!(
                error,
                ConnectionError::Protocol {
                    code: ErrorCode::ProtocolError,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn an_orphan_continuation_is_rejected() {
        let mut connection = ClientConnection::new();
        let bytes = encode_frame(&Frame::Continuation {
            stream_id: 1,
            end_headers: true,
            fragment: vec![0x82],
        })
        .expect("编码");
        let error = connection
            .feed(&bytes)
            .expect_err("没有 HEADERS 的 CONTINUATION 非法");
        assert!(
            matches!(error, ConnectionError::Protocol { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn headers_on_an_unknown_stream_are_rejected() {
        let mut connection = ClientConnection::new();
        let mut encoder = Encoder::new(4096);
        let block = encoder.encode(&[Header::new(":status", "200")]);
        let bytes = encode_frame(&Frame::Headers {
            stream_id: 99,
            end_stream: true,
            end_headers: true,
            fragment: block,
        })
        .expect("编码");
        let error = connection.feed(&bytes).expect_err("未知流必须拒绝");
        assert!(
            matches!(error, ConnectionError::Protocol { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_reset_stream_marks_the_stream_reset() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        let bytes = encode_frame(&Frame::RstStream {
            stream_id,
            code: ErrorCode::Cancel as u32,
        })
        .expect("编码");
        let feed = connection.feed(&bytes).expect("处理");
        assert_eq!(connection.stream_state(stream_id), Some(StreamState::Reset));
        assert!(feed
            .events
            .iter()
            .any(|event| matches!(event, Event::StreamReset { .. })));
    }

    #[test]
    fn goaway_stops_new_requests() {
        let mut connection = ClientConnection::new();
        let bytes = encode_frame(&Frame::GoAway {
            last_stream_id: 1,
            code: ErrorCode::NoError as u32,
            debug: b"done".to_vec(),
        })
        .expect("编码");
        connection.feed(&bytes).expect("处理 GOAWAY");
        assert_eq!(connection.goaway_received(), Some((1, 0)));
        assert_eq!(
            connection.goaway_debug(),
            Some(b"done".as_slice()),
            "debug 载荷不能丢"
        );
        let error = connection
            .send_request(&request_headers("/"), true)
            .expect_err("GOAWAY 之后不得再发请求");
        assert!(matches!(error, ConnectionError::GoAway { .. }), "{error:?}");
    }

    #[test]
    fn a_push_promise_is_rejected_since_we_disabled_push() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        let bytes = encode_frame(&Frame::PushPromise {
            stream_id,
            promised_stream_id: 2,
            fragment: vec![0x82],
        })
        .expect("编码");
        let error = connection.feed(&bytes).expect_err("必须拒绝推送");
        assert_eq!(
            error,
            ConnectionError::UnexpectedPush {
                promised_stream_id: 2
            }
        );
    }

    #[test]
    fn a_partial_frame_leaves_the_remainder_unconsumed() {
        let mut connection = ClientConnection::new();
        let (_stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        let response = server_response("204", b"");
        // 只喂前 5 字节：一个帧都解析不出来，但字节由内部缓冲接手。
        let feed = connection.feed(&response[..5]).expect("应当容忍");
        assert_eq!(feed.consumed, 5, "残帧字节由内部缓冲，调用方总能丢弃");
        assert!(feed.events.is_empty());
        // 只补剩余部分：内部缓冲会把两段拼成一个完整帧。
        let feed = connection.feed(&response[5..]).expect("应当处理");
        assert_eq!(feed.consumed, response.len() - 5);
        assert!(!feed.events.is_empty(), "拼接后应产出事件");
    }

    #[test]
    fn a_zero_increment_window_update_is_rejected() {
        let mut connection = ClientConnection::new();
        let bytes = encode_frame(&Frame::WindowUpdate {
            stream_id: 0,
            increment: 1,
        })
        .expect("编码");
        // 把增量改成 0。
        let mut broken = bytes.clone();
        let last = broken.len() - 4;
        broken[last..].fill(0);
        let error = connection.feed(&broken).expect_err("增量 0 非法");
        assert!(
            matches!(error, ConnectionError::Protocol { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn initial_window_setting_shifts_all_existing_streams() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), false)
            .expect("请求");
        // 把初始窗口放大到 100000。
        let settings = encode_frame(&Frame::Settings {
            ack: false,
            settings: vec![Setting {
                id: setting_id::INITIAL_WINDOW_SIZE,
                value: 100_000,
            }],
        })
        .expect("编码");
        connection.feed(&settings).expect("处理");
        let stream = connection.streams.get(&stream_id).expect("流存在");
        assert_eq!(stream.send_window, 100_000, "已存在的流也要跟着调整");
    }

    #[test]
    fn data_exceeding_the_window_is_a_flow_control_error() {
        let mut connection = ClientConnection::new();
        let (stream_id, _) = connection
            .send_request(&request_headers("/"), true)
            .expect("请求");
        // 手工把流窗口压成很小，再灌大块数据。
        if let Some(stream) = connection.streams.get_mut(&stream_id) {
            stream.recv_window = 1;
        }
        let bytes = encode_frame(&Frame::Data {
            stream_id,
            end_stream: false,
            data: vec![0u8; 100],
        })
        .expect("编码");
        let error = connection.feed(&bytes).expect_err("超窗口必须报错");
        assert!(
            matches!(
                error,
                ConnectionError::Protocol {
                    code: ErrorCode::FlowControlError,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn multiple_streams_are_tracked_independently() {
        let mut connection = ClientConnection::new();
        let (a, _) = connection
            .send_request(&request_headers("/a"), true)
            .expect("请求 A");
        let (b, _) = connection
            .send_request(&request_headers("/b"), true)
            .expect("请求 B");
        assert_ne!(a, b);
        // 给 A 回一个响应。
        let mut encoder = Encoder::new(4096);
        let block = encoder.encode(&[Header::new(":status", "200")]);
        let bytes = encode_frame(&Frame::Headers {
            stream_id: a,
            end_stream: true,
            end_headers: true,
            fragment: block,
        })
        .expect("编码");
        connection.feed(&bytes).expect("处理");
        assert_eq!(connection.status(a), Some(200));
        assert_eq!(connection.status(b), None, "B 还没响应");
        assert_eq!(connection.stream_state(b), Some(StreamState::Open));
    }
}
