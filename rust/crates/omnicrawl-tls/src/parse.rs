//! ClientHello 的解析：用于往返验证与抓包对照。
//!
//! 解析器的存在理由是**可验证性**：自研的编码层必须能被独立解码回结构，
//! 否则「字节与 Chrome 一致」就只是一句没有依据的话。这里只解析自己会发出的形状，
//! 不做完整 TLS 解析（不需要、也不该在这一层承担）。

use crate::client_hello::{ext, ClientHelloSpec, ExtensionSlot, GREASE_MARK, GREASE_VALUES};
use crate::wire::LengthWidth;

/// 解析错误：带偏移量，便于定位到具体字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub what: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "偏移 {} 处解析失败：{}", self.offset, self.what)
    }
}

impl std::error::Error for ParseError {}

fn error(offset: usize, what: &'static str) -> ParseError {
    ParseError { offset, what }
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    fn take(&mut self, count: usize, what: &'static str) -> Result<&'a [u8], ParseError> {
        if self.remaining() < count {
            return Err(error(self.position, what));
        }
        let slice = &self.data[self.position..self.position + count];
        self.position += count;
        Ok(slice)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, ParseError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, ParseError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u24(&mut self, what: &'static str) -> Result<usize, ParseError> {
        let bytes = self.take(3, what)?;
        Ok(((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | bytes[2] as usize)
    }

    /// 读一个带前缀的块。
    fn block(&mut self, width: LengthWidth, what: &'static str) -> Result<&'a [u8], ParseError> {
        let len = match width {
            LengthWidth::U8 => self.u8(what)? as usize,
            LengthWidth::U16 => self.u16(what)? as usize,
            LengthWidth::U24 => self.u24(what)?,
        };
        self.take(len, what)
    }
}

/// 把 GREASE 具体值归一化回 [`GREASE_MARK`]，使往返比较稳定。
fn normalize_grease(value: u16) -> u16 {
    if GREASE_VALUES.contains(&value) {
        GREASE_MARK
    } else {
        value
    }
}

/// 解析一份完整的 ClientHello 握手消息。
pub fn parse_client_hello(
    message: &[u8],
) -> Result<(ClientHelloSpec, Vec<u8>, [u8; 32]), ParseError> {
    let mut reader = Reader::new(message);
    let kind = reader.u8("握手类型")?;
    if kind != 0x01 {
        return Err(error(0, "不是 ClientHello（握手类型应为 0x01）"));
    }
    let body_len = reader.u24("握手长度")?;
    let body = reader.take(body_len, "握手体")?;

    let mut body = Reader::new(body);
    let legacy_version = body.u16("legacy_version")?;
    let random_slice = body.take(32, "random")?;
    let mut random = [0u8; 32];
    random.copy_from_slice(random_slice);

    let session_id = body.block(LengthWidth::U8, "会话 ID")?.to_vec();

    let suites_block = body.block(LengthWidth::U16, "密码套件")?;
    if suites_block.len() % 2 != 0 {
        return Err(error(body.position, "密码套件长度必须是偶数"));
    }
    let mut cipher_suites = Vec::with_capacity(suites_block.len() / 2);
    // GREASE 统一归一化成标记值：档案之间比较的是「有没有 GREASE、在哪个位置」，
    // 而不是这一次用了哪个具体值。
    for pair in suites_block.chunks_exact(2) {
        let value = u16::from_be_bytes([pair[0], pair[1]]);
        cipher_suites.push(normalize_grease(value));
    }

    let compression_methods = body.block(LengthWidth::U8, "压缩方法")?.to_vec();

    let extensions_block = body.block(LengthWidth::U16, "扩展列表")?;
    let mut extensions = Vec::new();
    let mut extension_reader = Reader::new(extensions_block);
    while extension_reader.remaining() > 0 {
        let kind = extension_reader.u16("扩展类型")?;
        let data = extension_reader
            .block(LengthWidth::U16, "扩展数据")?
            .to_vec();
        // 扩展类型与数据里的 GREASE 值同样归一化。
        let kind = normalize_grease(kind);
        let data = normalize_grease_in(kind, data);
        extensions.push(ExtensionSlot::new(kind, data));
    }

    let spec = ClientHelloSpec {
        legacy_version,
        session_id_len: session_id.len(),
        cipher_suites,
        compression_methods,
        extensions,
    };
    Ok((spec, session_id, random))
}

/// 按扩展类型归一化 GREASE 值。
///
/// 各扩展的内部结构不同，不能统一按两字节扫描：`supported_groups` 与
/// `signature_algorithms` 是「2 字节长度 + 值列表」，`supported_versions` 是
/// 「1 字节长度 + 值列表」，`key_share` 则是「长度 + (组 + 公钥长度 + 公钥)」的重复。
/// 全部按各自形状处理，未列出的扩展原样返回。
///
/// 这份结构知识与 `client_hello` 的写入侧同源：两边都按同一组形状解析，
/// 否则往返验证会在「一个愿意写、一个不会读」的地方悄悄失配。
fn normalize_grease_in(kind: u16, mut data: Vec<u8>) -> Vec<u8> {
    use crate::client_hello::ext;
    match kind {
        ext::SUPPORTED_GROUPS | ext::SIGNATURE_ALGORITHMS => {
            normalized_u16_list(&mut data, 2);
            data
        }
        ext::SUPPORTED_VERSIONS => {
            // 1 字节长度前缀。
            normalized_u16_list(&mut data, 1);
            data
        }
        ext::KEY_SHARE => {
            normalized_key_share(&mut data);
            data
        }
        _ => data,
    }
}

/// 「长度 + 值列表」形状：`prefix` 是长度字段占用的字节数。
fn normalized_u16_list(data: &mut [u8], prefix: usize) {
    if data.len() < prefix {
        return;
    }
    let declared = match prefix {
        1 => data[0] as usize,
        _ => u16::from_be_bytes([data[0], data[1]]) as usize,
    };
    let end = (prefix + declared).min(data.len());
    let mut index = prefix;
    while index + 1 < end {
        let value = u16::from_be_bytes([data[index], data[index + 1]]);
        if GREASE_VALUES.contains(&value) {
            data[index..index + 2].copy_from_slice(&GREASE_MARK.to_be_bytes());
        }
        index += 2;
    }
}

/// `key_share`：长度 + 若干 (组, 公钥长度, 公钥)。
fn normalized_key_share(data: &mut [u8]) {
    if data.len() < 2 {
        return;
    }
    let declared = u16::from_be_bytes([data[0], data[1]]) as usize;
    let end = (2 + declared).min(data.len());
    let mut index = 2;
    while index + 4 <= end {
        let group = u16::from_be_bytes([data[index], data[index + 1]]);
        if GREASE_VALUES.contains(&group) {
            data[index..index + 2].copy_from_slice(&GREASE_MARK.to_be_bytes());
        }
        let key_len = u16::from_be_bytes([data[index + 2], data[index + 3]]) as usize;
        index += 4 + key_len;
    }
}

/// 便捷入口：解析后与原始档案对比（GREASE 已归一化）。
pub fn round_trips(spec: &ClientHelloSpec, message: &[u8]) -> bool {
    match parse_client_hello(message) {
        Ok((parsed, _, _)) => parsed == *spec,
        Err(_) => false,
    }
}

/// 生成一份 `server_name` 数据段（解析侧校验用）。
pub fn rebuild_server_name(name: &str) -> Vec<u8> {
    crate::client_hello::server_name(name)
}

/// 扩展类型是否属于「GREASE 之外的已知扩展」，用于诊断缺漏。
pub fn is_known_extension(kind: u16) -> bool {
    matches!(
        kind,
        ext::SERVER_NAME
            | ext::STATUS_REQUEST
            | ext::SUPPORTED_GROUPS
            | ext::EC_POINT_FORMATS
            | ext::SIGNATURE_ALGORITHMS
            | ext::ALPN
            | ext::SIGNED_CERTIFICATE_TIMESTAMP
            | ext::PADDING
            | ext::EXTENDED_MASTER_SECRET
            | ext::COMPRESS_CERTIFICATE
            | ext::SESSION_TICKET
            | ext::PRE_SHARED_KEY
            | ext::SUPPORTED_VERSIONS
            | ext::PSK_KEY_EXCHANGE_MODES
            | ext::KEY_SHARE
            | ext::APPLICATION_SETTINGS
            | ext::RENEGOTIATION_INFO
            | GREASE_MARK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_extension_is_flagged() {
        assert!(!is_known_extension(0x1234));
        assert!(is_known_extension(ext::KEY_SHARE));
    }

    #[test]
    fn pair_normalization_only_touches_aligned_values() {
        // 0x1a1a 是合法 GREASE 值；X25519（0x001d）不是。
        let data = vec![0x00, 0x06, 0x00, 0x1d, 0x1a, 0x1a, 0x00, 0x17];
        let normalized = normalize_grease_in(crate::client_hello::ext::SUPPORTED_GROUPS, data);
        let grease = GREASE_MARK.to_be_bytes();
        assert_eq!(
            normalized,
            vec![0x00, 0x06, 0x00, 0x1d, grease[0], grease[1], 0x00, 0x17],
            "只归一化值区里的 GREASE，长度前缀与普通组保持不变"
        );
    }

    #[test]
    fn key_share_normalization_walks_entry_boundaries() {
        // 长度 9：(GREASE 组, 公钥长度 1, 公钥) + (X25519, 公钥长度 2, 公钥)。
        let data = vec![
            0x00, 0x09, 0x1a, 0x1a, 0x00, 0x01, 0xff, 0x00, 0x1d, 0x00, 0x02, 0xaa, 0xbb,
        ];
        let normalized = normalize_grease_in(crate::client_hello::ext::KEY_SHARE, data);
        let grease = GREASE_MARK.to_be_bytes();
        assert_eq!(
            normalized,
            vec![
                0x00, 0x09, grease[0], grease[1], 0x00, 0x01, 0xff, 0x00, 0x1d, 0x00, 0x02, 0xaa,
                0xbb
            ],
            "按条目边界推进，不会把公钥内容误判成组号"
        );
    }

    #[test]
    fn truncated_input_is_rejected_with_an_offset() {
        let error = parse_client_hello(&[0x01, 0x00]).expect_err("数据不足必须报错");
        assert_eq!(error.offset, 1);
    }

    #[test]
    fn a_non_client_hello_is_rejected() {
        let error = parse_client_hello(&[0x02, 0x00, 0x00, 0x00]).expect_err("类型不对必须报错");
        assert!(error.what.contains("不是 ClientHello"));
    }

    #[test]
    fn an_empty_body_is_rejected() {
        let error = parse_client_hello(&[0x01, 0x00, 0x00, 0x00]).expect_err("空体必须报错");
        assert_eq!(error.what, "legacy_version");
    }
}
