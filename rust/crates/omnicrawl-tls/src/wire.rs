//! TLS 线格式的编码原语。
//!
//! 只做「把数字写成协议要求的字节」，不含任何密码学。所有写入都返回 `Result`，
//! 越界即报错而不是静默截断：ClientHello 的每一字节都要与真实浏览器逐字节一致，
//! 静默截断会让指纹偏移一个字节而看不出原因。
//!
//! 长度前缀统一按三段式处理（TLS 的惯例）：先占位写长度，内容写完回填。这样
//! 嵌套结构的书写顺序与人类阅读顺序一致，不必自底向上计算长度。

use std::fmt;

/// 编码错误：只有长度溢出一种，单独定义是为了给出可定位的上下文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError {
    pub what: &'static str,
    pub len: usize,
    pub max: usize,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} 长度 {} 超出上限 {}",
            self.what, self.len, self.max
        )
    }
}

impl std::error::Error for EncodeError {}

/// 长度前缀的位宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthWidth {
    /// 单字节，上限 255（TLS 的若干 `uint8` 长度）。
    U8,
    /// 两字节，上限 65535（扩展、密码套件、签名算法列表）。
    U16,
    /// 三字节，上限 16777215（握手体、记录）。
    U24,
}

impl LengthWidth {
    pub fn max(self) -> usize {
        match self {
            Self::U8 => u8::MAX as usize,
            Self::U16 => u16::MAX as usize,
            Self::U24 => 0x00ff_ffff,
        }
    }

    fn write(self, out: &mut Vec<u8>, len: usize) {
        match self {
            Self::U8 => out.push(len as u8),
            Self::U16 => out.extend_from_slice(&(len as u16).to_be_bytes()),
            Self::U24 => {
                out.push((len >> 16) as u8);
                out.push((len >> 8) as u8);
                out.push(len as u8);
            }
        }
    }
}

/// 追加一个带长度前缀的块。
pub fn push_prefixed(
    out: &mut Vec<u8>,
    width: LengthWidth,
    what: &'static str,
    body: &[u8],
) -> Result<(), EncodeError> {
    if body.len() > width.max() {
        return Err(EncodeError {
            what,
            len: body.len(),
            max: width.max(),
        });
    }
    width.write(out, body.len());
    out.extend_from_slice(body);
    Ok(())
}

/// 用闭包填充一个带长度前缀的块。
///
/// 长度在闭包写完后才回填，因此内部还能继续嵌套 `push_prefixed`。
pub fn push_prefixed_with(
    out: &mut Vec<u8>,
    width: LengthWidth,
    what: &'static str,
    fill: impl FnOnce(&mut Vec<u8>) -> Result<(), EncodeError>,
) -> Result<(), EncodeError> {
    let start = out.len();
    width.write_at_placeholder(out, width);
    let body_start = out.len();
    // 闭包失败时把占位符一起回滚：否则调用方拿到的是一段前缀错位的字节，
    // 而错误信息指向内层，排查时会误以为是别处的问题。
    if let Err(error) = fill(out) {
        out.truncate(start);
        return Err(error);
    }
    let len = out.len() - body_start;
    if len > width.max() {
        out.truncate(start);
        return Err(EncodeError {
            what,
            len,
            max: width.max(),
        });
    }
    width.patch(out, start, len);
    Ok(())
}

impl LengthWidth {
    fn write_at_placeholder(self, out: &mut Vec<u8>, width: Self) {
        for _ in 0..width.byte_len() {
            out.push(0);
        }
    }

    fn byte_len(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U24 => 3,
        }
    }

    fn patch(self, out: &mut [u8], at: usize, len: usize) {
        match self {
            Self::U8 => out[at] = len as u8,
            Self::U16 => out[at..at + 2].copy_from_slice(&(len as u16).to_be_bytes()),
            Self::U24 => {
                out[at] = (len >> 16) as u8;
                out[at + 1] = (len >> 8) as u8;
                out[at + 2] = len as u8;
            }
        }
    }
}

/// 追加一个 16 位整数。
pub fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// 追加一组 16 位整数（密码套件、签名算法等）。
pub fn push_u16_list(out: &mut Vec<u8>, values: &[u16]) -> Result<(), EncodeError> {
    let mut body = Vec::with_capacity(values.len() * 2);
    for value in values {
        push_u16(&mut body, *value);
    }
    push_prefixed(out, LengthWidth::U16, "uint16 列表", &body)
}

/// 一个扩展：类型 + 数据。
///
/// 顺序由调用方按档案给定时序保留，序列化层不做任何重排——扩展顺序正是 JA3/JA4 的
/// 主要指纹字段，任何「顺手排序」都会让整份指纹失效。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub kind: u16,
    pub data: Vec<u8>,
}

impl Extension {
    pub fn new(kind: u16, data: Vec<u8>) -> Self {
        Self { kind, data }
    }

    pub fn write(&self, out: &mut Vec<u8>) -> Result<(), EncodeError> {
        push_u16(out, self.kind);
        push_prefixed(out, LengthWidth::U16, "扩展数据", &self.data)
    }
}

/// 追加扩展列表（含两字节总长）。
pub fn push_extensions(out: &mut Vec<u8>, extensions: &[Extension]) -> Result<(), EncodeError> {
    push_prefixed_with(out, LengthWidth::U16, "扩展列表", |body| {
        for extension in extensions {
            extension.write(body)?;
        }
        Ok(())
    })
}

/// 序列化一个完整握手消息（类型 + 三字节长度 + 体）。
pub fn handshake_message(kind: u8, body: &[u8]) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(kind);
    push_prefixed(&mut out, LengthWidth::U24, "握手体", body)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixed_blocks_carry_their_length() {
        let mut out = Vec::new();
        push_prefixed(&mut out, LengthWidth::U16, "test", &[1, 2, 3]).expect("应当写入");
        assert_eq!(out, vec![0x00, 0x03, 1, 2, 3]);
    }

    #[test]
    fn nested_lengths_are_backfilled_after_the_body() {
        let mut out = Vec::new();
        push_prefixed_with(&mut out, LengthWidth::U8, "outer", |body| {
            push_prefixed(body, LengthWidth::U8, "inner", &[9, 9])
        })
        .expect("应当写入");
        assert_eq!(out, vec![0x03, 0x02, 9, 9], "外层长度要含内层前缀");
    }

    #[test]
    fn overflow_is_an_error_and_leaves_nothing_behind() {
        let mut out = Vec::new();
        let error = push_prefixed(&mut out, LengthWidth::U8, "too-long", &[0; 300])
            .expect_err("超长必须报错");
        assert_eq!(error.len, 300);
        assert!(out.is_empty(), "失败时不能留下半截字节");
    }

    #[test]
    fn nested_overflow_rolls_the_whole_block_back() {
        let mut out = Vec::new();
        let error = push_prefixed_with(&mut out, LengthWidth::U8, "outer", |body| {
            body.extend_from_slice(&[0; 100]);
            push_prefixed(body, LengthWidth::U8, "inner", &[0; 300])
        })
        .expect_err("内层超长必须让外层一起失败");
        assert_eq!(error.what, "inner");
        assert!(out.is_empty(), "外层占位也必须回滚");
    }

    #[test]
    fn extension_order_is_preserved_verbatim() {
        let extensions = vec![
            Extension::new(0x000a, vec![0, 2, 0, 0x1d]),
            Extension::new(0x000b, vec![1, 0]),
            Extension::new(0x002b, vec![4, 3, 4, 3]),
        ];
        let mut out = Vec::new();
        push_extensions(&mut out, &extensions).expect("应当写入");
        // 两字节总长 + 依次的 kind/长度/数据，顺序与输入一致。
        assert_eq!(out[0..2], [0x00, 0x16], "三段共 22 字节：6+4+12");
        assert_eq!(out[2..4], [0x00, 0x0a], "第一段是 supported_groups");
        assert_eq!(out[10..12], [0x00, 0x0b], "第二段是 ec_point_formats");
        assert_eq!(out[16..18], [0x00, 0x2b], "第三段是 supported_versions");
    }

    #[test]
    fn handshake_message_prefixes_type_and_three_byte_length() {
        let message = handshake_message(0x01, &[0xaa; 300]).expect("应当写入");
        assert_eq!(message[0], 0x01);
        assert_eq!(message[1..4], [0x00, 0x01, 0x2c], "300 = 0x00012c");
        assert_eq!(message.len(), 304);
    }
}
