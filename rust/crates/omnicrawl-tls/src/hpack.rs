//! HPACK 头部压缩（RFC 7541）。
//!
//! 只实现 HTTP/2 需要的部分：请求/响应头部的有状态压缩。HTTP/3 的 QPACK 是另一套，
//! 不复用这里的动态表。
//!
//! 四条容易踩的边界，都在这里显式处理：
//!
//! 1. **动态表大小上限由对端 SETTINGS 决定**，收缩时必须从尾部淘汰。编码端在收到更小的
//!    上限后，必须在下一个头块开头先发一个动态表大小更新（`001` 前缀），否则对端会以
//!    `COMPRESSION_ERROR` 断开。
//! 2. **索引 0 不是合法索引**，静态表从 1 开始。把 0 当「无索引」会让畸形输入变成
//!    静默误解析。
//! 3. **Huffman 标志位在字符串长度的最高位**，长度本身是 7 位前缀整数。把两者混在
//!    一起是这类实现最常见的错。
//! 4. **「已索引」与「已存在名字」是两件事**。前者可以只发一个索引字节，后者仍要发名字
//!    和值；混用会让编码结果对、但压缩率极差。
//!
//! 表项大小按 RFC 7541 §4.1 计算：`name.len() + value.len() + 32`。那 32 字节是实现
//! 开销的固定估值，不是任意数字——它直接决定动态表能装多少项。

use std::collections::VecDeque;

pub mod huffman;

/// 静态表的 61 项（RFC 7541 附录 A），索引从 1 开始。
const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),                   // 1
    (":method", "GET"),                   // 2
    (":method", "POST"),                  // 3
    (":path", "/"),                       // 4
    (":path", "/index.html"),             // 5
    (":scheme", "http"),                  // 6
    (":scheme", "https"),                 // 7
    (":status", "200"),                   // 8
    (":status", "204"),                   // 9
    (":status", "206"),                   // 10
    (":status", "304"),                   // 11
    (":status", "400"),                   // 12
    (":status", "404"),                   // 13
    (":status", "500"),                   // 14
    ("accept-charset", ""),               // 15
    ("accept-encoding", "gzip, deflate"), // 16
    ("accept-language", ""),              // 17
    ("accept-ranges", ""),                // 18
    ("accept", ""),                       // 19
    ("access-control-allow-origin", ""),  // 20
    ("age", ""),                          // 21
    ("allow", ""),                        // 22
    ("authorization", ""),                // 23
    ("cache-control", ""),                // 24
    ("content-disposition", ""),          // 25
    ("content-encoding", ""),             // 26
    ("content-language", ""),             // 27
    ("content-length", ""),               // 28
    ("content-location", ""),             // 29
    ("content-range", ""),                // 30
    ("content-type", ""),                 // 31
    ("cookie", ""),                       // 32
    ("date", ""),                         // 33
    ("etag", ""),                         // 34
    ("expect", ""),                       // 35
    ("expires", ""),                      // 36
    ("from", ""),                         // 37
    ("host", ""),                         // 38
    ("if-match", ""),                     // 39
    ("if-modified-since", ""),            // 40
    ("if-none-match", ""),                // 41
    ("if-range", ""),                     // 42
    ("if-unmodified-since", ""),          // 43
    ("last-modified", ""),                // 44
    ("link", ""),                         // 45
    ("location", ""),                     // 46
    ("max-forwards", ""),                 // 47
    ("proxy-authenticate", ""),           // 48
    ("proxy-authorization", ""),          // 49
    ("range", ""),                        // 50
    ("referer", ""),                      // 51
    ("refresh", ""),                      // 52
    ("retry-after", ""),                  // 53
    ("server", ""),                       // 54
    ("set-cookie", ""),                   // 55
    ("strict-transport-security", ""),    // 56
    ("transfer-encoding", ""),            // 57
    ("user-agent", ""),                   // 58
    ("vary", ""),                         // 59
    ("via", ""),                          // 60
    ("www-authenticate", ""),             // 61
];

/// RFC 7541 §4.1 的每项固定开销。
const ENTRY_OVERHEAD: usize = 32;

/// 一个头部字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// 表项大小：名字、值，加上固定开销。
    fn size(&self) -> usize {
        self.name.len() + self.value.len() + ENTRY_OVERHEAD
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HpackError {
    /// 整数编码溢出（续字节链贡献的位数超过 `u64`）。
    IntegerOverflow,
    /// 整数编码的续字节链在数据末尾截断。
    TruncatedInteger,
    /// 字符串长度超出剩余数据，或内容不是合法 UTF-8。
    TruncatedString,
    /// 索引 0，或索引超出静态表 + 动态表的范围。
    InvalidIndex(usize),
    /// 动态表大小更新不是出现在头部块开头。
    LateTableSizeUpdate,
    /// 动态表大小更新超过本端 SETTINGS 给的上限。
    TableSizeExceedsLimit { requested: usize, limit: usize },
    /// Huffman 解码失败。
    Huffman(huffman::HuffmanError),
}

impl std::fmt::Display for HpackError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IntegerOverflow => formatter.write_str("HPACK 整数编码溢出"),
            Self::TruncatedInteger => formatter.write_str("HPACK 整数编码的续字节截断"),
            Self::TruncatedString => formatter.write_str("HPACK 字符串越界或不是合法 UTF-8"),
            Self::InvalidIndex(index) => write!(formatter, "HPACK 索引 {index} 非法"),
            Self::LateTableSizeUpdate => formatter.write_str("动态表大小更新必须位于头部块开头"),
            Self::TableSizeExceedsLimit { requested, limit } => {
                write!(formatter, "动态表大小 {requested} 超过本端上限 {limit}")
            }
            Self::Huffman(error) => write!(formatter, "Huffman 解码失败：{error}"),
        }
    }
}

impl std::error::Error for HpackError {}

impl From<huffman::HuffmanError> for HpackError {
    fn from(error: huffman::HuffmanError) -> Self {
        Self::Huffman(error)
    }
}

/// 前缀整数解码（RFC 7541 §5.1）。返回 `(值, 消耗字节数)`。
pub fn decode_integer(data: &[u8], prefix_bits: u8) -> Result<(usize, usize), HpackError> {
    let first = *data.first().ok_or(HpackError::TruncatedInteger)?;
    let mask = prefix_mask(prefix_bits);
    let prefix = first & mask as u8;
    if prefix < mask as u8 {
        return Ok((prefix as usize, 1));
    }
    let mut value = mask as u64;
    let mut shift: u32 = 0;
    let mut consumed = 1;
    loop {
        let byte = *data.get(consumed).ok_or(HpackError::TruncatedInteger)?;
        consumed += 1;
        let chunk = (byte & 0x7f) as u64;
        if shift >= 64 {
            return Err(HpackError::IntegerOverflow);
        }
        let shifted = chunk
            .checked_shl(shift)
            .filter(|shifted| shifted >> shift == chunk)
            .ok_or(HpackError::IntegerOverflow)?;
        value = value
            .checked_add(shifted)
            .ok_or(HpackError::IntegerOverflow)?;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    let value = usize::try_from(value).map_err(|_| HpackError::IntegerOverflow)?;
    Ok((value, consumed))
}

/// 前缀整数编码。`flags` 是首字节里除前缀位以外要保留的高位。
pub fn encode_integer(value: usize, prefix_bits: u8, flags: u8, out: &mut Vec<u8>) {
    let mask = prefix_mask(prefix_bits);
    let flags = flags & !mask as u8;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    let mut remainder = value - mask;
    while remainder >= 0x80 {
        out.push(((remainder & 0x7f) as u8) | 0x80);
        remainder >>= 7;
    }
    out.push(remainder as u8);
}

fn prefix_mask(prefix_bits: u8) -> usize {
    if prefix_bits >= 8 {
        0xff
    } else {
        (1usize << prefix_bits) - 1
    }
}

fn decode_string(data: &[u8]) -> Result<(String, usize), HpackError> {
    let first = *data.first().ok_or(HpackError::TruncatedString)?;
    let use_huffman = first & 0x80 != 0;
    let (length, prefix_len) = decode_integer(data, 7)?;
    let end = prefix_len
        .checked_add(length)
        .ok_or(HpackError::TruncatedString)?;
    let raw = data
        .get(prefix_len..end)
        .ok_or(HpackError::TruncatedString)?;
    let bytes = if use_huffman {
        huffman::decode(raw)?
    } else {
        raw.to_vec()
    };
    let text = String::from_utf8(bytes).map_err(|_| HpackError::TruncatedString)?;
    Ok((text, end))
}

/// 编码字符串。Huffman 只在确实更短时使用——否则是纯粹的体积损失。
fn encode_string(value: &str, use_huffman: bool, out: &mut Vec<u8>) {
    if use_huffman {
        let encoded = huffman::encode(value.as_bytes());
        if encoded.len() < value.len() {
            encode_integer(encoded.len(), 7, 0x80, out);
            out.extend_from_slice(&encoded);
            return;
        }
    }
    encode_integer(value.len(), 7, 0x00, out);
    out.extend_from_slice(value.as_bytes());
}

/// 动态表：双向使用，编码端与解码端必须保持一致。
#[derive(Debug, Clone)]
pub struct DynamicTable {
    /// 最新插入的项在最前。
    entries: VecDeque<Header>,
    /// 当前表项总大小（含固定开销）。
    size: usize,
    /// 当前允许的最大大小。
    max_size: usize,
    /// 对端 SETTINGS 给的上限；`max_size` 不得超过它。
    limit: usize,
    /// 尚未发送的大小更新，编码端在下一个头块开头补发。
    pending_update: Option<usize>,
}

impl DynamicTable {
    /// `limit` 来自对端 SETTINGS 的 `HEADER_TABLE_SIZE`（默认 4096）。
    pub fn new(limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            max_size: limit,
            limit,
            pending_update: None,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按动态表索引取项（索引 1 是最新的一项）。
    pub fn get(&self, index: usize) -> Option<&Header> {
        self.entries.get(index.checked_sub(1)?)
    }

    /// 对端把上限调低：立即淘汰，并记下待发送的更新。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        if self.max_size > limit {
            self.max_size = limit;
            self.evict_to_fit();
        }
        self.pending_update = Some(self.max_size);
    }

    /// 自行把当前最大大小调到 `size`（不得超过对端上限）。
    pub fn set_max_size(&mut self, size: usize) -> Result<(), HpackError> {
        if size > self.limit {
            return Err(HpackError::TableSizeExceedsLimit {
                requested: size,
                limit: self.limit,
            });
        }
        self.max_size = size;
        self.evict_to_fit();
        self.pending_update = Some(size);
        Ok(())
    }

    /// 取出待发送的大小更新。
    pub fn take_pending_update(&mut self) -> Option<usize> {
        self.pending_update.take()
    }

    /// 插入一项（最新项在前）。单项超过整个表容量时按 §4.4 清空且不插入。
    pub fn insert(&mut self, header: Header) {
        let size = header.size();
        if size > self.max_size {
            self.entries.clear();
            self.size = 0;
            return;
        }
        self.size += size;
        self.entries.push_front(header);
        self.evict_to_fit();
    }

    fn evict_to_fit(&mut self) {
        while self.size > self.max_size {
            match self.entries.pop_back() {
                Some(evicted) => self.size -= evicted.size(),
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    /// 名字与值都命中：返回可用的索引。
    fn lookup(&self, name: &str, value: &str) -> Option<usize> {
        if let Some(index) = static_find(name, value) {
            return Some(index);
        }
        let position = self
            .entries
            .iter()
            .position(|entry| entry.name == name && entry.value == value)?;
        static_table_len().checked_add(position + 1)
    }

    /// 只命中名字：返回名字可引用的索引。
    fn lookup_name(&self, name: &str) -> Option<usize> {
        if let Some(index) = static_find_name(name) {
            return Some(index);
        }
        let position = self.entries.iter().position(|entry| entry.name == name)?;
        static_table_len().checked_add(position + 1)
    }

    /// 按绝对索引解析，静态表优先。
    fn resolve(&self, index: usize) -> Result<Header, HpackError> {
        if index == 0 {
            return Err(HpackError::InvalidIndex(index));
        }
        if index <= STATIC_TABLE.len() {
            let (name, value) = STATIC_TABLE[index - 1];
            return Ok(Header::new(name, value));
        }
        self.get(index - STATIC_TABLE.len())
            .cloned()
            .ok_or(HpackError::InvalidIndex(index))
    }
}

fn static_table_len() -> usize {
    STATIC_TABLE.len()
}

fn static_find(name: &str, value: &str) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .position(|(n, v)| *n == name && *v == value)
        .map(|index| index + 1)
}

fn static_find_name(name: &str) -> Option<usize> {
    STATIC_TABLE
        .iter()
        .position(|(n, _)| *n == name)
        .map(|index| index + 1)
}

/// 解码器：维护动态表，把一个完整的头部块解成头部列表。
#[derive(Debug, Clone)]
pub struct Decoder {
    table: DynamicTable,
}

impl Decoder {
    /// `limit` 是本端 SETTINGS 里告知对端的 `HEADER_TABLE_SIZE`。
    pub fn new(limit: usize) -> Self {
        Self {
            table: DynamicTable::new(limit),
        }
    }

    pub fn table(&self) -> &DynamicTable {
        &self.table
    }

    /// 解码一个完整头部块（HEADERS + 全部 CONTINUATION 的拼接）。
    pub fn decode(&mut self, mut data: &[u8]) -> Result<Vec<Header>, HpackError> {
        let mut headers = Vec::new();
        let mut seen_field = false;
        while !data.is_empty() {
            let first = data[0];
            if first & 0x80 != 0 {
                // 1xxxxxxx：索引头部字段
                let (index, used) = decode_integer(data, 7)?;
                headers.push(self.table.resolve(index)?);
                data = &data[used..];
                seen_field = true;
            } else if first & 0x40 != 0 {
                // 01xxxxxx：字面量，带增量索引
                let (name, value, used) = self.decode_literal(data, 6)?;
                data = &data[used..];
                self.table.insert(Header::new(name.clone(), value.clone()));
                headers.push(Header::new(name, value));
                seen_field = true;
            } else if first & 0x20 != 0 {
                // 001xxxxx：动态表大小更新，只允许出现在块开头
                if seen_field {
                    return Err(HpackError::LateTableSizeUpdate);
                }
                let (size, used) = decode_integer(data, 5)?;
                self.table.set_max_size(size)?;
                data = &data[used..];
            } else {
                // 0000xxxx 不索引 / 0001xxxx 永不索引：两者都不动动态表
                let (name, value, used) = self.decode_literal(data, 4)?;
                data = &data[used..];
                headers.push(Header::new(name, value));
                seen_field = true;
            }
        }
        Ok(headers)
    }

    /// 解一个「字面量」表示。`prefix_bits` 是索引部分占用的位数。
    fn decode_literal(
        &self,
        data: &[u8],
        prefix_bits: u8,
    ) -> Result<(String, String, usize), HpackError> {
        let (index, used) = decode_integer(data, prefix_bits)?;
        let name = if index == 0 {
            let (name, name_used) = decode_string(&data[used..])?;
            let consumed = used + name_used;
            let (value, value_used) = decode_string(&data[consumed..])?;
            return Ok((name, value, consumed + value_used));
        } else {
            self.table.resolve(index)?.name
        };
        let (value, value_used) = decode_string(&data[used..])?;
        Ok((name, value, used + value_used))
    }
}

/// 编码器：维护动态表，把头部列表压进一个头部块。
///
/// 默认策略是「能索引就索引」：HTTP/2 的压缩收益几乎全部来自动态表复用，
/// 不索引等于把 HPACK 降级成静态表查表。
#[derive(Debug, Clone)]
pub struct Encoder {
    table: DynamicTable,
    use_huffman: bool,
}

impl Encoder {
    pub fn new(limit: usize) -> Self {
        Self {
            table: DynamicTable::new(limit),
            use_huffman: true,
        }
    }

    /// 关闭 Huffman（对照测试用）。
    pub fn without_huffman(mut self) -> Self {
        self.use_huffman = false;
        self
    }

    pub fn table(&self) -> &DynamicTable {
        &self.table
    }

    /// 对端 SETTINGS 更新了 `HEADER_TABLE_SIZE`。
    pub fn set_peer_limit(&mut self, limit: usize) {
        self.table.set_limit(limit);
    }

    /// 编码一个头部列表。返回的字节要放进 HEADERS 帧的头部块。
    pub fn encode(&mut self, headers: &[Header]) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(size) = self.table.take_pending_update() {
            encode_integer(size, 5, 0x20, &mut out);
        }
        for header in headers {
            self.encode_one(header, &mut out);
        }
        out
    }

    fn encode_one(&mut self, header: &Header, out: &mut Vec<u8>) {
        if let Some(index) = self.table.lookup(&header.name, &header.value) {
            encode_integer(index, 7, 0x80, out);
            return;
        }
        let name_index = self.table.lookup_name(&header.name);
        // 单项就超过表容量时不能索引：插进去会把整张表清空，反而更差。
        if header.size() <= self.table.max_size() {
            match name_index {
                Some(index) => encode_integer(index, 6, 0x40, out),
                None => {
                    out.push(0x40);
                    encode_string(&header.name, self.use_huffman, out);
                }
            }
            encode_string(&header.value, self.use_huffman, out);
            self.table.insert(header.clone());
        } else {
            match name_index {
                Some(index) => encode_integer(index, 4, 0x00, out),
                None => {
                    out.push(0x00);
                    encode_string(&header.name, self.use_huffman, out);
                }
            }
            encode_string(&header.value, self.use_huffman, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 的十六进制转储按四位一组排版，因此必须按字符对解析而不是按空白切分。
    fn hex(text: &str) -> Vec<u8> {
        let digits: Vec<u8> = text
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .map(|ch| ch.to_digit(16).expect("十六进制字符") as u8)
            .collect();
        assert!(digits.len().is_multiple_of(2), "十六进制位数必须是偶数");
        digits
            .chunks(2)
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect()
    }

    #[test]
    fn integers_roundtrip_at_every_prefix_width() {
        for prefix in 1..=8u8 {
            let flags = !(prefix_mask(prefix) as u8);
            for value in [0usize, 1, 5, 30, 31, 127, 128, 255, 1337, 100_000] {
                let mut out = Vec::new();
                encode_integer(value, prefix, flags, &mut out);
                let (decoded, used) = decode_integer(&out, prefix).expect("应当解码");
                assert_eq!(decoded, value, "prefix={prefix} value={value}");
                assert_eq!(used, out.len());
            }
        }
    }

    #[test]
    fn the_rfc_integer_examples_decode() {
        // RFC 7541 C.1
        assert_eq!(decode_integer(&[0x0a], 5).expect("解码"), (10, 1));
        assert_eq!(
            decode_integer(&[0x1f, 0x9a, 0x0a], 5).expect("解码"),
            (1337, 3)
        );
        assert_eq!(decode_integer(&[0x2a], 8).expect("解码"), (42, 1));
    }

    #[test]
    fn static_table_entries_match_the_rfc() {
        assert_eq!(STATIC_TABLE.len(), 61);
        assert_eq!(STATIC_TABLE[0], (":authority", ""));
        assert_eq!(STATIC_TABLE[1], (":method", "GET"));
        assert_eq!(STATIC_TABLE[15], ("accept-encoding", "gzip, deflate"));
        assert_eq!(STATIC_TABLE[60], ("www-authenticate", ""));
    }

    #[test]
    fn the_first_request_vector_decodes() {
        // RFC 7541 C.3.1
        let data = hex("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d");
        let mut decoder = Decoder::new(4096);
        let headers = decoder.decode(&data).expect("应当解码");
        assert_eq!(
            headers,
            vec![
                Header::new(":method", "GET"),
                Header::new(":scheme", "http"),
                Header::new(":path", "/"),
                Header::new(":authority", "www.example.com"),
            ]
        );
        assert_eq!(decoder.table().size(), 57, "动态表应含一项 :authority");
    }

    #[test]
    fn the_second_request_vector_uses_the_dynamic_table() {
        // RFC 7541 C.3.2：:authority 变成索引 62。
        let first = hex("8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d");
        let second = hex("8286 84be 5808 6e6f 2d63 6163 6865");
        let mut decoder = Decoder::new(4096);
        decoder.decode(&first).expect("第一块");
        let headers = decoder.decode(&second).expect("第二块");
        assert_eq!(
            headers,
            vec![
                Header::new(":method", "GET"),
                Header::new(":scheme", "http"),
                Header::new(":path", "/"),
                Header::new(":authority", "www.example.com"),
                Header::new("cache-control", "no-cache"),
            ]
        );
    }

    #[test]
    fn huffman_response_vectors_decode() {
        // RFC 7541 C.6.1
        let data = hex(
            "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6
             2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
        );
        let mut decoder = Decoder::new(4096);
        let headers = decoder.decode(&data).expect("应当解码");
        assert_eq!(
            headers,
            vec![
                Header::new(":status", "302"),
                Header::new("cache-control", "private"),
                Header::new("date", "Mon, 21 Oct 2013 20:13:21 GMT"),
                Header::new("location", "https://www.example.com"),
            ]
        );
        assert_eq!(decoder.table().size(), 222, "C.6.1 的动态表大小");
    }

    #[test]
    fn shrinking_the_limit_evicts_from_the_tail() {
        let mut decoder = Decoder::new(4096);
        let first = hex(
            "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6
             2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
        );
        decoder.decode(&first).expect("第一块");
        assert_eq!(decoder.table().size(), 222);
        decoder.table.set_limit(100);
        assert!(decoder.table().size() <= 100, "应淘汰到上限以下");
        assert_eq!(decoder.table().max_size(), 100);
    }

    #[test]
    fn an_oversized_entry_clears_the_table() {
        let mut table = DynamicTable::new(100);
        table.insert(Header::new("a", "b"));
        assert_eq!(table.len(), 1);
        table.insert(Header::new("x".repeat(200), "y"));
        assert!(table.is_empty(), "超容量的项必须清空表且不插入");
        assert_eq!(table.size(), 0);
    }

    #[test]
    fn index_zero_is_rejected() {
        let mut decoder = Decoder::new(4096);
        assert_eq!(decoder.decode(&[0x80]), Err(HpackError::InvalidIndex(0)));
    }

    #[test]
    fn an_out_of_range_index_is_rejected() {
        let mut decoder = Decoder::new(4096);
        // 索引 62 = 动态表第 1 项，但表是空的。
        assert_eq!(decoder.decode(&[0xbe]), Err(HpackError::InvalidIndex(62)));
    }

    #[test]
    fn a_late_table_size_update_is_rejected() {
        let mut decoder = Decoder::new(4096);
        assert_eq!(
            decoder.decode(&[0x82, 0x20]),
            Err(HpackError::LateTableSizeUpdate)
        );
    }

    #[test]
    fn an_update_above_the_limit_is_rejected() {
        let mut decoder = Decoder::new(100);
        let mut data = Vec::new();
        encode_integer(4096, 5, 0x20, &mut data);
        assert!(matches!(
            decoder.decode(&data),
            Err(HpackError::TableSizeExceedsLimit { .. })
        ));
    }

    #[test]
    fn roundtrip_via_encoder_and_decoder_stays_in_sync() {
        let mut encoder = Encoder::new(4096);
        let mut decoder = Decoder::new(4096);
        let lists: [Vec<Header>; 2] = [
            vec![
                Header::new(":method", "GET"),
                Header::new(":scheme", "https"),
                Header::new(":path", "/"),
                Header::new(":authority", "example.com"),
                Header::new("user-agent", "omnicrawl/1.0"),
            ],
            vec![
                Header::new(":method", "GET"),
                Header::new(":scheme", "https"),
                Header::new(":path", "/next"),
                Header::new(":authority", "example.com"),
                Header::new("user-agent", "omnicrawl/1.0"),
                Header::new("accept", "*/*"),
            ],
        ];
        for (round, headers) in lists.iter().enumerate() {
            let encoded = encoder.encode(headers);
            let decoded = decoder.decode(&encoded).expect("应当解码");
            assert_eq!(&decoded, headers, "第 {round} 轮");
        }
        assert_eq!(
            encoder.table().size(),
            decoder.table().size(),
            "两端动态表必须保持一致"
        );
    }

    #[test]
    fn repeated_headers_compress_down_to_index_references() {
        let mut encoder = Encoder::new(4096);
        let headers = vec![
            Header::new(":method", "GET"),
            Header::new(":authority", "example.com"),
            Header::new("user-agent", "omnicrawl/1.0"),
        ];
        let first = encoder.encode(&headers);
        let second = encoder.encode(&headers);
        assert!(
            second.len() < first.len(),
            "第二轮应更短：{} vs {}",
            second.len(),
            first.len()
        );
        assert_eq!(second.len(), headers.len(), "全命中时每项 1 字节");
        assert!(second.iter().all(|byte| byte & 0x80 != 0), "应全是索引形式");
    }

    #[test]
    fn utf8_values_survive_a_roundtrip() {
        let mut encoder = Encoder::new(4096);
        let mut decoder = Decoder::new(4096);
        let headers = vec![
            Header::new("x-note", "中文值"),
            Header::new("x-empty", ""),
            Header::new("x-long", "y".repeat(1000)),
        ];
        let encoded = encoder.encode(&headers);
        assert_eq!(decoder.decode(&encoded).expect("应当解码"), headers);
    }

    #[test]
    fn huffman_can_be_turned_off() {
        let headers = vec![Header::new("x-plain", "abcdef")];
        let mut with = Encoder::new(4096);
        let mut without = Encoder::new(4096).without_huffman();
        let a = with.encode(&headers);
        let b = without.encode(&headers);
        assert!(b.windows(6).any(|window| window == b"abcdef"));
        assert!(!a.windows(6).any(|window| window == b"abcdef"));
    }

    #[test]
    fn a_truncated_string_is_rejected() {
        let mut decoder = Decoder::new(4096);
        // 新名字，长度 10 但只给 2 字节。
        assert_eq!(
            decoder.decode(&[0x00, 0x0a, b'a', b'b']),
            Err(HpackError::TruncatedString)
        );
    }

    #[test]
    fn a_truncated_integer_is_rejected() {
        assert_eq!(
            decode_integer(&[0x1f, 0x80], 5),
            Err(HpackError::TruncatedInteger)
        );
        assert_eq!(decode_integer(&[], 5), Err(HpackError::TruncatedInteger));
    }

    #[test]
    fn shrinking_the_encoder_limit_emits_an_update_first() {
        let mut encoder = Encoder::new(4096);
        encoder.encode(&[Header::new("x-a", "1")]);
        encoder.set_peer_limit(0);
        let encoded = encoder.encode(&[Header::new("x-b", "2")]);
        assert_eq!(encoded[0] & 0xe0, 0x20, "应先在块头发大小更新");
        let mut decoder = Decoder::new(0);
        decoder.decode(&encoded).expect("上限 0 时应当仍可解码");
    }
}
