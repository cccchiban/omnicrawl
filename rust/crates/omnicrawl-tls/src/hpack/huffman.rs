//! HPACK 的 Huffman 编解码（RFC 7541 附录 B）。
//!
//! 码表是 257 项的权威值，直接取自 RFC 7541 附录 B 原文，未做任何手抄改动；
//! 自测里用 Kraft 和与前缀性质反向确认它没有被抄错。
//!
//! 解码逐位匹配而不是维护 256 项分派表：HPACK 解码不在热路径上，可读性更重要。
//!
//! EOS（符号 256）只用于校验。它出现在编码流里必须报错；末尾填充必须是 EOS 的前缀
//! （全 1）且不超过 7 位。这两条是 RFC 7541 §5.2 的硬要求，放宽等于允许
//! 「在填充里藏数据」这类畸形输入通过。

/// `(code, bits)`，索引即符号；末项是 EOS（256）。
///
/// 码值按 LSB 对齐，与 RFC 表的「code as hex」列一致。
const TABLE: [(u32, u8); 257] = [
    (0x00001ff8, 13),
    (0x007fffd8, 23),
    (0x0fffffe2, 28),
    (0x0fffffe3, 28),
    (0x0fffffe4, 28),
    (0x0fffffe5, 28),
    (0x0fffffe6, 28),
    (0x0fffffe7, 28),
    (0x0fffffe8, 28),
    (0x00ffffea, 24),
    (0x3ffffffc, 30),
    (0x0fffffe9, 28),
    (0x0fffffea, 28),
    (0x3ffffffd, 30),
    (0x0fffffeb, 28),
    (0x0fffffec, 28),
    (0x0fffffed, 28),
    (0x0fffffee, 28),
    (0x0fffffef, 28),
    (0x0ffffff0, 28),
    (0x0ffffff1, 28),
    (0x0ffffff2, 28),
    (0x3ffffffe, 30),
    (0x0ffffff3, 28),
    (0x0ffffff4, 28),
    (0x0ffffff5, 28),
    (0x0ffffff6, 28),
    (0x0ffffff7, 28),
    (0x0ffffff8, 28),
    (0x0ffffff9, 28),
    (0x0ffffffa, 28),
    (0x0ffffffb, 28),
    (0x00000014, 6),
    (0x000003f8, 10),
    (0x000003f9, 10),
    (0x00000ffa, 12),
    (0x00001ff9, 13),
    (0x00000015, 6),
    (0x000000f8, 8),
    (0x000007fa, 11),
    (0x000003fa, 10),
    (0x000003fb, 10),
    (0x000000f9, 8),
    (0x000007fb, 11),
    (0x000000fa, 8),
    (0x00000016, 6),
    (0x00000017, 6),
    (0x00000018, 6),
    (0x00000000, 5),
    (0x00000001, 5),
    (0x00000002, 5),
    (0x00000019, 6),
    (0x0000001a, 6),
    (0x0000001b, 6),
    (0x0000001c, 6),
    (0x0000001d, 6),
    (0x0000001e, 6),
    (0x0000001f, 6),
    (0x0000005c, 7),
    (0x000000fb, 8),
    (0x00007ffc, 15),
    (0x00000020, 6),
    (0x00000ffb, 12),
    (0x000003fc, 10),
    (0x00001ffa, 13),
    (0x00000021, 6),
    (0x0000005d, 7),
    (0x0000005e, 7),
    (0x0000005f, 7),
    (0x00000060, 7),
    (0x00000061, 7),
    (0x00000062, 7),
    (0x00000063, 7),
    (0x00000064, 7),
    (0x00000065, 7),
    (0x00000066, 7),
    (0x00000067, 7),
    (0x00000068, 7),
    (0x00000069, 7),
    (0x0000006a, 7),
    (0x0000006b, 7),
    (0x0000006c, 7),
    (0x0000006d, 7),
    (0x0000006e, 7),
    (0x0000006f, 7),
    (0x00000070, 7),
    (0x00000071, 7),
    (0x00000072, 7),
    (0x000000fc, 8),
    (0x00000073, 7),
    (0x000000fd, 8),
    (0x00001ffb, 13),
    (0x0007fff0, 19),
    (0x00001ffc, 13),
    (0x00003ffc, 14),
    (0x00000022, 6),
    (0x00007ffd, 15),
    (0x00000003, 5),
    (0x00000023, 6),
    (0x00000004, 5),
    (0x00000024, 6),
    (0x00000005, 5),
    (0x00000025, 6),
    (0x00000026, 6),
    (0x00000027, 6),
    (0x00000006, 5),
    (0x00000074, 7),
    (0x00000075, 7),
    (0x00000028, 6),
    (0x00000029, 6),
    (0x0000002a, 6),
    (0x00000007, 5),
    (0x0000002b, 6),
    (0x00000076, 7),
    (0x0000002c, 6),
    (0x00000008, 5),
    (0x00000009, 5),
    (0x0000002d, 6),
    (0x00000077, 7),
    (0x00000078, 7),
    (0x00000079, 7),
    (0x0000007a, 7),
    (0x0000007b, 7),
    (0x00007ffe, 15),
    (0x000007fc, 11),
    (0x00003ffd, 14),
    (0x00001ffd, 13),
    (0x0ffffffc, 28),
    (0x000fffe6, 20),
    (0x003fffd2, 22),
    (0x000fffe7, 20),
    (0x000fffe8, 20),
    (0x003fffd3, 22),
    (0x003fffd4, 22),
    (0x003fffd5, 22),
    (0x007fffd9, 23),
    (0x003fffd6, 22),
    (0x007fffda, 23),
    (0x007fffdb, 23),
    (0x007fffdc, 23),
    (0x007fffdd, 23),
    (0x007fffde, 23),
    (0x00ffffeb, 24),
    (0x007fffdf, 23),
    (0x00ffffec, 24),
    (0x00ffffed, 24),
    (0x003fffd7, 22),
    (0x007fffe0, 23),
    (0x00ffffee, 24),
    (0x007fffe1, 23),
    (0x007fffe2, 23),
    (0x007fffe3, 23),
    (0x007fffe4, 23),
    (0x001fffdc, 21),
    (0x003fffd8, 22),
    (0x007fffe5, 23),
    (0x003fffd9, 22),
    (0x007fffe6, 23),
    (0x007fffe7, 23),
    (0x00ffffef, 24),
    (0x003fffda, 22),
    (0x001fffdd, 21),
    (0x000fffe9, 20),
    (0x003fffdb, 22),
    (0x003fffdc, 22),
    (0x007fffe8, 23),
    (0x007fffe9, 23),
    (0x001fffde, 21),
    (0x007fffea, 23),
    (0x003fffdd, 22),
    (0x003fffde, 22),
    (0x00fffff0, 24),
    (0x001fffdf, 21),
    (0x003fffdf, 22),
    (0x007fffeb, 23),
    (0x007fffec, 23),
    (0x001fffe0, 21),
    (0x001fffe1, 21),
    (0x003fffe0, 22),
    (0x001fffe2, 21),
    (0x007fffed, 23),
    (0x003fffe1, 22),
    (0x007fffee, 23),
    (0x007fffef, 23),
    (0x000fffea, 20),
    (0x003fffe2, 22),
    (0x003fffe3, 22),
    (0x003fffe4, 22),
    (0x007ffff0, 23),
    (0x003fffe5, 22),
    (0x003fffe6, 22),
    (0x007ffff1, 23),
    (0x03ffffe0, 26),
    (0x03ffffe1, 26),
    (0x000fffeb, 20),
    (0x0007fff1, 19),
    (0x003fffe7, 22),
    (0x007ffff2, 23),
    (0x003fffe8, 22),
    (0x01ffffec, 25),
    (0x03ffffe2, 26),
    (0x03ffffe3, 26),
    (0x03ffffe4, 26),
    (0x07ffffde, 27),
    (0x07ffffdf, 27),
    (0x03ffffe5, 26),
    (0x00fffff1, 24),
    (0x01ffffed, 25),
    (0x0007fff2, 19),
    (0x001fffe3, 21),
    (0x03ffffe6, 26),
    (0x07ffffe0, 27),
    (0x07ffffe1, 27),
    (0x03ffffe7, 26),
    (0x07ffffe2, 27),
    (0x00fffff2, 24),
    (0x001fffe4, 21),
    (0x001fffe5, 21),
    (0x03ffffe8, 26),
    (0x03ffffe9, 26),
    (0x0ffffffd, 28),
    (0x07ffffe3, 27),
    (0x07ffffe4, 27),
    (0x07ffffe5, 27),
    (0x000fffec, 20),
    (0x00fffff3, 24),
    (0x000fffed, 20),
    (0x001fffe6, 21),
    (0x003fffe9, 22),
    (0x001fffe7, 21),
    (0x001fffe8, 21),
    (0x007ffff3, 23),
    (0x003fffea, 22),
    (0x003fffeb, 22),
    (0x01ffffee, 25),
    (0x01ffffef, 25),
    (0x00fffff4, 24),
    (0x00fffff5, 24),
    (0x03ffffea, 26),
    (0x007ffff4, 23),
    (0x03ffffeb, 26),
    (0x07ffffe6, 27),
    (0x03ffffec, 26),
    (0x03ffffed, 26),
    (0x07ffffe7, 27),
    (0x07ffffe8, 27),
    (0x07ffffe9, 27),
    (0x07ffffea, 27),
    (0x07ffffeb, 27),
    (0x0ffffffe, 28),
    (0x07ffffec, 27),
    (0x07ffffed, 27),
    (0x07ffffee, 27),
    (0x07ffffef, 27),
    (0x07fffff0, 27),
    (0x03ffffee, 26),
    (0x3fffffff, 30),
];

/// EOS 的符号编号。
pub const EOS: usize = 256;

/// 最长码的位数（EOS 的 30 位）。
const MAX_CODE_BITS: u8 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HuffmanError {
    /// 编码流里出现了 EOS，或出现了长度超过 30 位的码。
    EosInPayload,
    /// 末尾填充不是全 1。
    PaddingNotEosPrefix,
    /// 码流在某个码中途截断，且残留比特超过 7 位。
    Truncated,
}

impl std::fmt::Display for HuffmanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::EosInPayload => "Huffman 流里出现了 EOS 符号（或被 1 填充撑到 30 位以上）",
            Self::PaddingNotEosPrefix => "Huffman 末尾填充不是全 1，不是合法的 EOS 前缀",
            Self::Truncated => "Huffman 码流在码中途截断",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for HuffmanError {}

/// 编码所需的字节数。
pub fn encoded_len(input: &[u8]) -> usize {
    let bits: usize = input
        .iter()
        .map(|byte| TABLE[*byte as usize].1 as usize)
        .sum();
    bits.div_ceil(8)
}

/// 用 HPACK Huffman 编码一段字节。
pub fn encode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded_len(input));
    let mut accumulator: u64 = 0;
    let mut pending: u8 = 0;
    for byte in input {
        let (code, bits) = TABLE[*byte as usize];
        accumulator = (accumulator << bits) | code as u64;
        pending += bits;
        while pending >= 8 {
            pending -= 8;
            out.push((accumulator >> pending) as u8);
        }
        // 只保留未输出的低位，避免长串文本把累加器撑溢出。
        accumulator &= (1u64 << pending) - 1;
    }
    if pending > 0 {
        // EOS 全是 1，因此补 1 到字节边界即等于补 EOS 的前缀。
        let shift = 8 - pending;
        out.push(((accumulator << shift) | ((1u64 << shift) - 1)) as u8);
    }
    out
}

/// 解码 HPACK Huffman 编码的字节。
pub fn decode(input: &[u8]) -> Result<Vec<u8>, HuffmanError> {
    let mut out = Vec::new();
    let mut code: u32 = 0;
    let mut bits: u8 = 0;
    for byte in input {
        for position in (0..8).rev() {
            code = (code << 1) | ((byte >> position) & 1) as u32;
            bits += 1;
            if let Some(symbol) = lookup(code, bits) {
                if symbol == EOS {
                    return Err(HuffmanError::EosInPayload);
                }
                out.push(symbol as u8);
                code = 0;
                bits = 0;
            } else if bits > MAX_CODE_BITS {
                // 30 位都匹配不上，说明码流已经偏离这棵树。
                return Err(HuffmanError::EosInPayload);
            }
        }
    }
    if bits == 0 {
        return Ok(out);
    }
    // 残留比特只可能是填充。填充是 EOS 的前缀，所以必然是若干位 1 且不超过 7 位。
    if bits > 7 {
        return Err(if code == (1u32 << bits.min(31)) - 1 {
            HuffmanError::EosInPayload
        } else {
            HuffmanError::Truncated
        });
    }
    if code != (1u32 << bits) - 1 {
        return Err(HuffmanError::PaddingNotEosPrefix);
    }
    Ok(out)
}

fn lookup(code: u32, bits: u8) -> Option<usize> {
    TABLE
        .iter()
        .position(|(value, length)| *length == bits && *value == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_a_complete_prefix_code() {
        // Kraft 和必须恰为 1：任何一项抄错都会让它偏离。
        let mut kraft = 0f64;
        for (_, bits) in TABLE.iter() {
            kraft += 1.0 / (1u64 << bits) as f64;
        }
        assert!((kraft - 1.0).abs() < 1e-12, "Kraft 和 = {kraft}");
    }

    #[test]
    fn no_code_is_a_prefix_of_another() {
        for (index, (code, bits)) in TABLE.iter().enumerate() {
            for (other, (other_code, other_bits)) in TABLE.iter().enumerate() {
                if index == other || *other_bits <= *bits {
                    continue;
                }
                let shifted = other_code >> (*other_bits - *bits);
                assert_ne!(
                    shifted, *code,
                    "符号 {index} 的码是符号 {other} 的前缀，码表有误"
                );
            }
        }
    }

    #[test]
    fn known_codes_match_the_rfc() {
        // RFC 7541 §B 与正文举例。
        assert_eq!(TABLE[b'/' as usize], (0x18, 6), "'/' 应为 6 位 011000");
        assert_eq!(TABLE[b'0' as usize], (0x0, 5), "'0' 应为 5 位 00000");
        assert_eq!(TABLE[b'e' as usize], (0x5, 5), "'e' 应为 5 位 00101");
        assert_eq!(TABLE[EOS], (0x3fff_ffff, 30));
    }

    #[test]
    fn encoding_matches_the_rfc_vector() {
        // RFC 7541 C.4.1：www.example.com 的 Huffman 编码。
        let encoded = encode(b"www.example.com");
        assert_eq!(
            encoded,
            vec![0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff]
        );
        assert_eq!(decode(&encoded).expect("应当解码"), b"www.example.com");
    }

    #[test]
    fn rfc_response_vectors_decode() {
        // RFC 7541 C.6.1：cache-control: private 的 Huffman 值。
        assert_eq!(
            decode(&[0xae, 0xc3, 0x77, 0x1a, 0x4b]).expect("应当解码"),
            b"private"
        );
        // RFC 7541 C.6.2：location: https://www.example.com 的 Huffman 值。
        assert_eq!(
            decode(&[
                0x9d, 0x29, 0xad, 0x17, 0x18, 0x63, 0xc7, 0x8f, 0x0b, 0x97, 0xc8, 0xe9, 0xae, 0x82,
                0xae, 0x43, 0xd3
            ])
            .expect("应当解码"),
            b"https://www.example.com"
        );
    }

    #[test]
    fn short_vectors_roundtrip() {
        for text in [
            &b""[..],
            b"a",
            b"ab",
            b"http",
            b"no-cache",
            b"custom-key",
            b"custom-value",
            b"/sample/path",
            b"Mon, 21 Oct 2013 20:13:21 GMT",
            b"https://www.example.com",
        ] {
            let encoded = encode(text);
            assert_eq!(
                decode(&encoded).expect("应当解码"),
                text,
                "往返失败：{text:?}"
            );
        }
    }

    #[test]
    fn every_byte_value_roundtrips() {
        let all: Vec<u8> = (0..=255).collect();
        let encoded = encode(&all);
        assert_eq!(decode(&encoded).expect("应当解码"), all);
    }

    #[test]
    fn long_inputs_do_not_overflow_the_accumulator() {
        // 累加器只保留未输出的低位；这段若没做掩码，会在数十字节后出错。
        let text = b"x".repeat(4096);
        let encoded = encode(&text);
        assert_eq!(decode(&encoded).expect("应当解码"), text);
    }

    #[test]
    fn an_eos_symbol_in_the_payload_is_rejected() {
        // 30 个 1 就是 EOS 本身。
        assert_eq!(
            decode(&[0xff, 0xff, 0xff, 0xff]),
            Err(HuffmanError::EosInPayload)
        );
    }

    #[test]
    fn padding_must_be_all_ones() {
        // '0' 占 5 位，后面补 000 而不是 111。
        assert_eq!(
            decode(&[0b0000_0000]),
            Err(HuffmanError::PaddingNotEosPrefix)
        );
    }

    #[test]
    fn encoded_length_matches_actual_output() {
        for text in [&b""[..], b"a", b"www.example.com", b"X-Custom-Header"] {
            assert_eq!(encode(text).len(), encoded_len(text), "{text:?}");
        }
    }
}
