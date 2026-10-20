//! 正文字符集还原：按 `content-type` 的 `charset` 把响应字节解成文本。
//!
//! 为什么不能一律按 UTF-8 解：`String::from_utf8_lossy` 遇到 GBK 站点不会报错，而是把每个
//! 无法解码的字节换成替换字符——状态码 200、正文看着像乱码，比明确失败更难排查。
//!
//! 判定顺序与浏览器一致（优先级从高到低）：
//! 1. 响应头的 `charset`——服务端最明确的声明；
//! 2. BOM——真正的前缀标记，比任何声明都硬；
//! 3. 文档内 `<meta charset>` / `<meta http-equiv>`——只在头部没声明时才派生；
//! 4. 都没有就按 UTF-8。
//!
//! 第 3 条要读正文前若干字节，因此只在需要时扫一次前缀，不做全文猜测。本模块不做编码
//! 嗅探（chardet 那一类统计猜测）：猜错会把正常页面变成乱码，而声明错的情况用户还能看出
//! 是站点的问题。

use encoding_rs::{Encoding, UTF_8};

/// 探测 `<meta>` 时只看正文开头这么多字节：charset 声明必须在这个范围内才对浏览器有效。
const META_SNIFF_LIMIT: usize = 4096;

/// 还原正文文本：返回解码结果与实际使用的编码名（用于排障与测试）。
pub fn decode_body(body: &[u8], content_type: Option<&str>) -> (String, &'static str) {
    // BOM 优先：它是真实前缀，与声明冲突时以它为准（浏览器的做法）。
    if let Some((text, name)) = decode_bom(body) {
        return (text, name);
    }
    let encoding = content_type
        .and_then(charset_from_content_type)
        .or_else(|| charset_from_meta(body))
        .and_then(|label| lookup(label.as_str()))
        .unwrap_or(UTF_8);
    let (text, _) = encoding.decode_without_bom_handling(body);
    (text.into_owned(), encoding.name())
}

/// 从 `content-type` 里取 `charset` 参数；没有就返回 `None`。
///
/// 参数可能带引号（`charset="gbk"`），也可能大小写混写，因此按参数名匹配后统一去引号。
fn charset_from_content_type(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim().trim_matches(|character| character == '"' || character == '\'');
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// 从正文前缀里的 `<meta>` 声明取 charset；没有就返回 `None`。
fn charset_from_meta(body: &[u8]) -> Option<String> {
    let prefix = &body[..body.len().min(META_SNIFF_LIMIT)];
    // 声明只可能是 ASCII；用有损解码足够，且不会因为多字节字符被截断而丢掉整段前缀。
    let head = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    for tag_start in find_all(&head, "<meta") {
        let rest = &head[tag_start..];
        let end = rest.find('>').map(|offset| offset + 1).unwrap_or(rest.len());
        let attributes = parse_attributes(&rest[..end]);
        // `<meta charset="gbk">` 这种直接写法。
        if let Some(value) = attribute(&attributes, "charset") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        // `<meta http-equiv="content-type" content="text/html; charset=gbk">` 这种老写法。
        if let Some(value) = attribute(&attributes, "content")
            .and_then(|content| charset_from_content_type(content))
        {
            return Some(value);
        }
    }
    None
}

fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut offset = 0;
    while let Some(position) = haystack[offset..].find(needle) {
        let absolute = offset + position;
        found.push(absolute);
        offset = absolute + needle.len();
    }
    found
}

/// 按 HTML 规范切分标签内的属性：`name`、`name=value`、`name="value"`、`name='value'`。
///
/// 不能在整段标签文本里直接 `find(name)` 再向后找 `=`：`content="text/html; charset=gbk"`
/// 里的无引号值会把紧随的双引号一起吞进值里，`gbk"` 于是查不到编码。
fn parse_attributes(tag: &str) -> Vec<(String, String)> {
    let mut attributes = Vec::new();
    let characters: Vec<char> = tag.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        // 跳过空白与标签名/结尾斜杠。
        while index < characters.len()
            && (characters[index].is_whitespace() || characters[index] == '/')
        {
            index += 1;
        }
        if index >= characters.len() || characters[index] == '>' {
            break;
        }
        let name_start = index;
        while index < characters.len()
            && !characters[index].is_whitespace()
            && !matches!(characters[index], '=' | '/' | '>')
        {
            index += 1;
        }
        let name: String = characters[name_start..index].iter().collect();
        while index < characters.len() && characters[index].is_whitespace() {
            index += 1;
        }
        let mut value = String::new();
        if index < characters.len() && characters[index] == '=' {
            index += 1;
            while index < characters.len() && characters[index].is_whitespace() {
                index += 1;
            }
            if index < characters.len() && matches!(characters[index], '"' | '\'') {
                let quote = characters[index];
                index += 1;
                while index < characters.len() && characters[index] != quote {
                    value.push(characters[index]);
                    index += 1;
                }
                // 吃掉收尾引号（缺失时停在标签末尾）。
                index += 1;
            } else {
                while index < characters.len()
                    && !characters[index].is_whitespace()
                    && characters[index] != '>'
                {
                    value.push(characters[index]);
                    index += 1;
                }
            }
        }
        if !name.is_empty() {
            attributes.push((name, value));
        }
    }
    attributes
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// 按标签取编码；认不出的标签返回 `None`，由调用方回退。
///
/// 不再自建中文标签别名表：`Encoding::for_label` 已按 WHATWG Encoding 规范收录
/// `gbk`/`gb2312`/`gb18030`/`big5`/`shift_jis` 等全部别名，自己映射反而会覆盖错
/// （例如把 `gbk` 归到 `gb18030`，编码名就报错了）。
fn lookup(label: &str) -> Option<&'static Encoding> {
    Encoding::for_label(label.trim().as_bytes())
}

/// BOM 检测：UTF-8 / UTF-16LE / UTF-16BE 三选项标记（UTF-32 站点极少，且不是浏览器的默认路径）。
fn decode_bom(body: &[u8]) -> Option<(String, &'static str)> {
    if body.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let (text, _) = UTF_8.decode_without_bom_handling(&body[3..]);
        return Some((text.into_owned(), "UTF-8"));
    }
    if body.starts_with(&[0xFF, 0xFE]) {
        let (text, _) = encoding_rs::UTF_16LE.decode_without_bom_handling(&body[2..]);
        return Some((text.into_owned(), "UTF-16LE"));
    }
    if body.starts_with(&[0xFE, 0xFF]) {
        let (text, _) = encoding_rs::UTF_16BE.decode_without_bom_handling(&body[2..]);
        return Some((text.into_owned(), "UTF-16BE"));
    }
    None
}

/// 供未声明 charset 的调用方使用的默认解码（UTF-8，与浏览器的最终回退一致）。
pub fn decode_utf8(body: &[u8]) -> String {
    let (text, _) = UTF_8.decode_without_bom_handling(body);
    text.into_owned()
}

/// GBK 用得上但没有被单独列出的编码标签，仅用于测试断言覆盖率。
#[cfg(test)]
const GBK_LABEL: &str = "gbk";

#[cfg(test)]
mod tests {
    use super::*;

    /// GBK 编码的「中文」：0xD6 0xD0 0xCE 0xC4。按 UTF-8 解会变成替换字符。
    const ZHONG_WEN_GBK: &[u8] = &[0xD6, 0xD0, 0xCE, 0xC4];

    #[test]
    fn utf8_body_stays_utf8() {
        let (text, name) = decode_body("中文正文".as_bytes(), Some("text/html; charset=utf-8"));
        assert_eq!(text, "中文正文");
        assert_eq!(name, "UTF-8");
    }

    #[test]
    fn gbk_body_is_restored_from_content_type() {
        let (text, name) = decode_body(ZHONG_WEN_GBK, Some("text/html; charset=gbk"));
        assert_eq!(text, "中文", "GBK 正文不该出现替换字符");
        assert_eq!(name, "GBK");
    }

    /// 上一版的行为缺陷：按 UTF-8 强解 GBK 正文会得到替换字符，却不报错。
    #[test]
    fn naive_utf8_lossy_would_corrupt_gbk() {
        let corrupted = String::from_utf8_lossy(ZHONG_WEN_GBK);
        assert!(corrupted.contains('\u{FFFD}'), "前提：UTF-8 强解确实会破坏 GBK");
        let (text, _) = decode_body(ZHONG_WEN_GBK, Some("text/html; charset=gbk"));
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn quoted_and_mixed_case_charset_parameters_are_accepted() {
        for header in [
            "text/html; charset=\"GBK\"",
            "text/html;charset=Gb2312",
            "text/html; charset= 'gb18030' ",
        ] {
            let (text, _) = decode_body(ZHONG_WEN_GBK, Some(header));
            assert_eq!(text, "中文", "头：{header}");
        }
    }

    #[test]
    fn meta_declaration_is_used_when_header_is_silent() {
        let mut body = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        body.extend_from_slice(ZHONG_WEN_GBK);
        body.extend_from_slice(b"</body></html>");
        let (text, name) = decode_body(&body, Some("text/html"));
        assert_eq!(name, "GBK");
        assert!(text.contains("中文"), "{text}");
    }

    #[test]
    fn http_equiv_meta_declaration_is_used() {
        let mut body =
            b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gbk\">".to_vec();
        body.extend_from_slice(ZHONG_WEN_GBK);
        let (text, _) = decode_body(&body, None);
        assert!(text.contains("中文"), "{text}");
    }

    /// 头部声明优先于文档内声明：服务端明确说 UTF-8 时不该被页面里的 meta 改写。
    #[test]
    fn header_declaration_wins_over_meta() {
        let mut body = b"<meta charset=\"gbk\">".to_vec();
        body.extend_from_slice("中文".as_bytes());
        let (text, name) = decode_body(&body, Some("text/html; charset=utf-8"));
        assert_eq!(name, "UTF-8");
        assert!(text.contains("中文"));
    }

    #[test]
    fn bom_decides_even_when_header_disagrees() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice("中文".as_bytes());
        let (text, name) = decode_body(&body, Some("text/html; charset=gbk"));
        assert_eq!(name, "UTF-8");
        assert_eq!(text, "中文");
    }

    #[test]
    fn utf16le_bom_is_recognized() {
        let mut body = vec![0xFF, 0xFE];
        for unit in "中文".encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        let (text, name) = decode_body(&body, None);
        assert_eq!(name, "UTF-16LE");
        assert_eq!(text, "中文");
    }

    #[test]
    fn unknown_charset_falls_back_to_utf8() {
        let (text, name) = decode_body("中文".as_bytes(), Some("text/html; charset=x-unknown-xyz"));
        assert_eq!(name, "UTF-8");
        assert_eq!(text, "中文");
    }

    /// `data-charset` 不是 charset 声明：属性名必须独立匹配。
    #[test]
    fn attribute_name_must_match_whole_word() {
        let body = b"<meta data-charset=\"gbk\">".as_slice();
        assert!(charset_from_meta(body).is_none());
    }

    /// 标签解析交给 WHATWG 表：`gbk` 报 `GBK`、`gb2312` 归并到 `GBK`，不该被自建映射改写。
    #[test]
    fn labels_resolve_through_the_whatwg_table() {
        assert_eq!(lookup(GBK_LABEL).map(Encoding::name), Some("GBK"));
        assert_eq!(lookup("gb2312").map(Encoding::name), Some("GBK"));
        assert_eq!(lookup("big5").map(Encoding::name), Some("Big5"));
        assert_eq!(lookup("shift_jis").map(Encoding::name), Some("Shift_JIS"));
    }
}
