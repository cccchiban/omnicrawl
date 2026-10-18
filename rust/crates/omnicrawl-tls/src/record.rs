//! TLS 记录层：把握手消息切成记录，以及在 1.3 下加解密记录。
//!
//! TLS 1.3 的记录层与 1.2 有个关键差异：1.3 的内层明文总是握手消息（末字节是
//! `content_type`），外层记录类型一律是 `application_data`（23）。这让中间设备看到的
//! 记录类型分布与 1.2 不同，也是指纹的一部分——因此这一层按 1.3 的规则实现，
//! 不为兼容 1.2 做双轨。
//!
//! 记录长度上限固定 2^14（16384）字节明文；1.3 允许尾部填充，填充计入
//! `ciphertext` 长度，读取时按内层 `content_type` 剥离。

use crate::wire::{push_prefixed, EncodeError, LengthWidth};

/// 记录层内容类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ContentType {
    ChangeCipherSpec = 20,
    Alert = 21,
    Handshake = 22,
    ApplicationData = 23,
}

impl ContentType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            20 => Some(Self::ChangeCipherSpec),
            21 => Some(Self::Alert),
            22 => Some(Self::Handshake),
            23 => Some(Self::ApplicationData),
            _ => None,
        }
    }
}

/// 明文记录的最大载荷（TLS 1.3 固定 2^14）。
pub const MAX_PLAINTEXT: usize = 16_384;

/// TLS 1.3 的 `inner` 类型字节与 AEAD tag 各占 1 与 16 字节。
const INNER_TYPE_LEN: usize = 1;
/// AEAD 认证标签长度（AES-GCM 与 ChaCha20-Poly1305 均为 16）。
pub const TAG_LEN: usize = 16;

/// 一条记录：类型 + 版本 + 载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: ContentType,
    pub payload: Vec<u8>,
}

impl Record {
    /// 编码为线上字节。`legacy_version` 在 1.3 的记录头里固定 `0x0303`。
    pub fn encode(&self, legacy_version: u16) -> Result<Vec<u8>, EncodeError> {
        let mut out = Vec::with_capacity(self.payload.len() + 5);
        out.push(self.kind as u8);
        crate::wire::push_u16(&mut out, legacy_version);
        push_prefixed(&mut out, LengthWidth::U16, "记录载荷", &self.payload)?;
        Ok(out)
    }
}

/// 解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// 数据不足以构成一条完整记录。
    Incomplete { needed: usize, available: usize },
    /// 记录类型不认识。
    UnknownType(u8),
    /// 载荷超过 2^14。
    TooLarge(usize),
    /// 加密记录的载荷长度不合法（不足以容纳 tag 与内层类型）。
    Undersized(usize),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete { needed, available } => {
                write!(
                    formatter,
                    "记录不完整：需要 {needed} 字节，只有 {available}"
                )
            }
            Self::UnknownType(value) => write!(formatter, "未知记录类型 {value}"),
            Self::TooLarge(len) => write!(formatter, "记录载荷 {len} 字节超过 2^14"),
            Self::Undersized(len) => write!(formatter, "加密载荷 {len} 字节不足以容纳 tag"),
        }
    }
}

impl std::error::Error for RecordError {}

/// 从字节流头部解析一条明文记录；返回记录与消耗的字节数。
pub fn parse_record(data: &[u8]) -> Result<(Record, usize), RecordError> {
    if data.len() < 5 {
        return Err(RecordError::Incomplete {
            needed: 5,
            available: data.len(),
        });
    }
    let kind = ContentType::from_u8(data[0]).ok_or(RecordError::UnknownType(data[0]))?;
    let len = u16::from_be_bytes([data[3], data[4]]) as usize;
    if len > MAX_PLAINTEXT + TAG_LEN + INNER_TYPE_LEN {
        return Err(RecordError::TooLarge(len));
    }
    let total = 5 + len;
    if data.len() < total {
        return Err(RecordError::Incomplete {
            needed: total,
            available: data.len(),
        });
    }
    Ok((
        Record {
            kind,
            payload: data[5..total].to_vec(),
        },
        total,
    ))
}

/// 把一段数据切成若干记录（每片不超过 2^14）。
pub fn split_records(kind: ContentType, payload: &[u8]) -> Vec<Record> {
    if payload.is_empty() {
        return vec![Record {
            kind,
            payload: Vec::new(),
        }];
    }
    payload
        .chunks(MAX_PLAINTEXT)
        .map(|chunk| Record {
            kind,
            payload: chunk.to_vec(),
        })
        .collect()
}

/// 加密记录的序号与密钥。
///
/// 1.3 的 nonce 是「静态 IV 异或 64 位序号」，序号从 0 起、每发一条记录加一，
/// 且**两个方向各自独立计数**。
pub struct RecordKey {
    key: ring::aead::LessSafeKey,
    iv: [u8; 12],
    sequence: u64,
}

impl RecordKey {
    /// `key` 与 `iv` 由密钥调度给出（`iv` 是 12 字节）。
    pub fn new(
        algorithm: &'static ring::aead::Algorithm,
        key: &[u8],
        iv: [u8; 12],
    ) -> Result<Self, String> {
        let key = ring::aead::UnboundKey::new(algorithm, key)
            .map_err(|_| "记录密钥长度与所选算法不符".to_string())?;
        Ok(Self {
            key: ring::aead::LessSafeKey::new(key),
            iv,
            sequence: 0,
        })
    }

    /// 当前序号。
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn nonce(&self) -> ring::aead::Nonce {
        let mut nonce = self.iv;
        let sequence = self.sequence.to_be_bytes();
        for (index, byte) in sequence.iter().enumerate() {
            nonce[4 + index] ^= byte;
        }
        ring::aead::Nonce::assume_unique_for_key(nonce)
    }

    /// 加密一段内层明文，附加真实内容类型后封装成 `application_data` 记录。
    ///
    /// 1.3 的规则：`inner = plaintext || content_type`，AEAD 的附加认证数据就是记录头
    /// （`type=23`、版本、长度）。长度在加密前必须已确定，因此这里先算再加密。
    pub fn seal(&mut self, inner_type: ContentType, plaintext: &[u8]) -> Result<Record, String> {
        let mut buffer = plaintext.to_vec();
        buffer.push(inner_type as u8);
        let ciphertext_len = buffer.len() + TAG_LEN;
        let header = record_header(ContentType::ApplicationData, ciphertext_len);
        let mut in_out = buffer;
        self.key
            .seal_in_place_append_tag(self.nonce(), ring::aead::Aad::from(&header), &mut in_out)
            .map_err(|_| "记录加密失败".to_string())?;
        self.sequence += 1;
        Ok(Record {
            kind: ContentType::ApplicationData,
            payload: in_out,
        })
    }

    /// 解密一条记录，返回「内层类型 + 明文」。
    pub fn open(&mut self, record: &Record) -> Result<(ContentType, Vec<u8>), String> {
        if record.kind != ContentType::ApplicationData {
            return Err(format!(
                "1.3 的加密记录类型应为 23，收到 {}",
                record.kind as u8
            ));
        }
        if record.payload.len() < TAG_LEN + INNER_TYPE_LEN {
            return Err(format!(
                "加密载荷 {} 字节不足以容纳 tag",
                record.payload.len()
            ));
        }
        let header = record_header(ContentType::ApplicationData, record.payload.len());
        let mut in_out = record.payload.clone();
        let plaintext = self
            .key
            .open_in_place(self.nonce(), ring::aead::Aad::from(&header), &mut in_out)
            .map_err(|_| "记录解密失败（密钥、序号或附加数据不匹配）".to_string())?;
        self.sequence += 1;
        // 内层明文的末字节是真实内容类型，其前的一切都是载荷。
        //
        // 这里**不能**把尾部的零字节当 TLS 填充剥掉：1.3 的 `TLSInnerPlaintext`
        // 里，零填充位于 `content_type` 之后而不是之前，末字节定位法已经把填充排除在外。
        // 剥掉载荷自身的尾部零字节会静默改坏上层数据——HTTP/2 的 PADDED 帧恰好全是
        // 零，曾经因此把填充长度字段后面的内容整段吃掉，表现为「帧莫名缺了 7 字节」。
        let inner_type = *plaintext.last().ok_or_else(|| "解密结果为空".to_string())?;
        let kind = ContentType::from_u8(inner_type)
            .ok_or_else(|| format!("内层类型 {inner_type} 不认识"))?;
        Ok((kind, plaintext[..plaintext.len() - 1].to_vec()))
    }
}

fn record_header(kind: ContentType, len: usize) -> [u8; 5] {
    let mut header = [0u8; 5];
    header[0] = kind as u8;
    header[1] = 0x03;
    header[2] = 0x03;
    header[3] = (len >> 8) as u8;
    header[4] = len as u8;
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_bytes() {
        let record = Record {
            kind: ContentType::Handshake,
            payload: vec![1, 2, 3],
        };
        let bytes = record.encode(0x0303).expect("应当能编码");
        assert_eq!(bytes, vec![22, 0x03, 0x03, 0x00, 0x03, 1, 2, 3]);
        let (parsed, consumed) = parse_record(&bytes).expect("应当能解析");
        assert_eq!(parsed, record);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn incomplete_input_reports_what_is_missing() {
        let error = parse_record(&[22, 0x03, 0x03, 0x00]).expect_err("不足 5 字节");
        assert_eq!(
            error,
            RecordError::Incomplete {
                needed: 5,
                available: 4
            }
        );
        let error = parse_record(&[22, 0x03, 0x03, 0x00, 0x09, 1, 2]).expect_err("载荷不足");
        assert_eq!(
            error,
            RecordError::Incomplete {
                needed: 14,
                available: 7
            }
        );
    }

    #[test]
    fn unknown_content_type_is_rejected() {
        assert_eq!(
            parse_record(&[0x99, 0x03, 0x03, 0x00, 0x00]).expect_err("类型不对"),
            RecordError::UnknownType(0x99)
        );
    }

    #[test]
    fn oversized_payload_is_rejected_before_reading() {
        let mut data = vec![22, 0x03, 0x03];
        data.extend_from_slice(&0xffffu16.to_be_bytes());
        let error = parse_record(&data).expect_err("超长必须报错");
        assert!(matches!(error, RecordError::TooLarge(0xffff)));
    }

    #[test]
    fn payload_is_split_at_the_plaintext_limit() {
        let payload = vec![0xab; MAX_PLAINTEXT * 2 + 10];
        let records = split_records(ContentType::ApplicationData, &payload);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].payload.len(), MAX_PLAINTEXT);
        assert_eq!(records[2].payload.len(), 10);
    }

    #[test]
    fn empty_payload_still_produces_one_record() {
        let records = split_records(ContentType::Handshake, &[]);
        assert_eq!(records.len(), 1);
        assert!(records[0].payload.is_empty());
    }

    #[test]
    fn sealed_records_open_back_to_the_same_plaintext() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 12];
        let algorithm = &ring::aead::AES_128_GCM;
        let mut sender = RecordKey::new(algorithm, &key, iv).expect("应当接受密钥");
        let mut receiver = RecordKey::new(algorithm, &key, iv).expect("应当接受密钥");

        let sealed = sender
            .seal(ContentType::Handshake, b"hello")
            .expect("应当能加密");
        assert_eq!(sealed.kind, ContentType::ApplicationData);
        assert_eq!(sealed.payload.len(), 5 + TAG_LEN + INNER_TYPE_LEN);

        let (kind, body) = receiver.open(&sealed).expect("应当能解密");
        assert_eq!(kind, ContentType::Handshake);
        assert_eq!(body, b"hello");
        assert_eq!(sender.sequence(), 1, "序号每条递增");
        assert_eq!(receiver.sequence(), 1);
    }

    #[test]
    fn sequence_numbers_change_the_nonce_so_records_differ() {
        let key = [0x33u8; 16];
        let iv = [0x44u8; 12];
        let mut sender = RecordKey::new(&ring::aead::AES_128_GCM, &key, iv).expect("密钥");
        let first = sender.seal(ContentType::Handshake, b"same").expect("加密");
        let second = sender.seal(ContentType::Handshake, b"same").expect("加密");
        assert_ne!(
            first.payload, second.payload,
            "相同的明文在不同序号下必须产出不同密文"
        );
    }

    #[test]
    fn a_tampered_record_fails_authentication() {
        let key = [0x55u8; 16];
        let iv = [0x66u8; 12];
        let mut sender = RecordKey::new(&ring::aead::AES_128_GCM, &key, iv).expect("密钥");
        let mut receiver = RecordKey::new(&ring::aead::AES_128_GCM, &key, iv).expect("密钥");
        let mut sealed = sender
            .seal(ContentType::Handshake, b"payload")
            .expect("加密");
        sealed.payload[0] ^= 0xff;
        assert!(receiver.open(&sealed).is_err(), "篡改必须被拒绝");
    }

    #[test]
    fn out_of_order_records_fail_because_the_nonce_differs() {
        let key = [0x77u8; 16];
        let iv = [0x88u8; 12];
        let mut sender = RecordKey::new(&ring::aead::AES_128_GCM, &key, iv).expect("密钥");
        let mut receiver = RecordKey::new(&ring::aead::AES_128_GCM, &key, iv).expect("密钥");
        let first = sender.seal(ContentType::Handshake, b"one").expect("加密");
        let second = sender.seal(ContentType::Handshake, b"two").expect("加密");
        // 先收第二条：接收侧序号仍是 0，nonce 对不上。
        assert!(receiver.open(&second).is_err());
        assert!(receiver.open(&first).is_ok());
    }

    #[test]
    fn chacha_records_work_as_well() {
        let key = [0x99u8; 32];
        let iv = [0xaau8; 12];
        let mut sender = RecordKey::new(&ring::aead::CHACHA20_POLY1305, &key, iv).expect("密钥");
        let mut receiver = RecordKey::new(&ring::aead::CHACHA20_POLY1305, &key, iv).expect("密钥");
        let sealed = sender.seal(ContentType::Alert, b"x").expect("加密");
        let (kind, body) = receiver.open(&sealed).expect("解密");
        assert_eq!(kind, ContentType::Alert);
        assert_eq!(body, b"x");
    }
}
