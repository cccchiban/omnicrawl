//! TOML 文档的解析与写回。
//!
//! 解析用 `toml` crate；**写回不用它的序列化器**——Python 侧写回走 `tomli_w`，两侧会
//! 交替读写同一份 `config.toml`，因此文本必须逐字节对齐 `tomli_w.dumps` 的形状：
//! 顶层标量在前、子表随后并以空行分隔，非空数组一律多行，浮点用 Python `repr` 写法。
//! 这些规则都在 `tests/fixtures/config_toml_parity.json` 里按真实现对照过。

pub use toml::Value;

/// 配置文档：与 Python 侧的 `dict[str, Any]` 对应。
pub type Table = toml::Table;

/// 解析 TOML 文本；错误文案由调用方包装（Python 侧同样只透传解析器消息）。
pub fn parse_document(text: &str) -> Result<Table, String> {
    match text.parse::<Table>() {
        Ok(table) => Ok(table),
        Err(error) => Err(error.to_string()),
    }
}

/// 把文档渲染成 TOML 文本，形状与 `tomli_w.dumps` 一致。
pub fn dump_document(table: &Table) -> String {
    let mut sections: Vec<String> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    emit_table(table, &mut path, &mut sections);
    if sections.is_empty() {
        return String::new();
    }
    let mut text = sections.join("\n\n");
    text.push('\n');
    text
}

fn emit_table(table: &Table, path: &mut Vec<String>, sections: &mut Vec<String>) {
    let mut lines: Vec<String> = Vec::new();
    let has_scalars = table
        .values()
        .any(|value| !matches!(value, Value::Table(_)));
    // `tomli_w` 只在表自己带标量、或它是叶子空表时才写表头；中间层空壳不占段落。
    if !path.is_empty() && (has_scalars || table.is_empty()) {
        let keys: Vec<String> = path.iter().map(|key| format_key(key)).collect();
        lines.push(format!("[{}]", keys.join(".")));
    }
    for (key, value) in table {
        if matches!(value, Value::Table(_)) {
            continue;
        }
        lines.push(format!("{} = {}", format_key(key), format_value(value, 0)));
    }
    if !lines.is_empty() {
        sections.push(lines.join("\n"));
    }
    for (key, value) in table {
        if let Value::Table(inner) = value {
            path.push(key.clone());
            emit_table(inner, path, sections);
            path.pop();
        }
    }
}

fn format_value(value: &Value, indent: usize) -> String {
    match value {
        Value::String(text) => format_string(text),
        Value::Integer(number) => number.to_string(),
        Value::Float(number) => float_repr(*number),
        Value::Boolean(flag) => {
            if *flag {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Datetime(datetime) => datetime.to_string(),
        Value::Array(items) => format_array(items, indent),
        Value::Table(inner) => format_inline_table(inner),
    }
}

fn format_array(items: &[Value], indent: usize) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let inner_indent = indent + 4;
    let padding = " ".repeat(inner_indent);
    let mut text = String::from("[\n");
    for item in items {
        text.push_str(&padding);
        text.push_str(&format_value(item, inner_indent));
        text.push_str(",\n");
    }
    text.push_str(&" ".repeat(indent));
    text.push(']');
    text
}

fn format_inline_table(table: &Table) -> String {
    if table.is_empty() {
        return "{}".to_string();
    }
    let parts: Vec<String> = table
        .iter()
        .map(|(key, value)| format!("{} = {}", format_key(key), format_value(value, 0)))
        .collect();
    format!("{{ {} }}", parts.join(", "))
}

/// 键名裸写条件与 `tomli_w` 一致：只允许 ASCII 字母数字、下划线与连字符。
fn format_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if bare {
        key.to_string()
    } else {
        format_string(key)
    }
}

fn format_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push('\t'),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// 浮点的 Python `repr` 写法。
///
/// Rust 的 `{:?}` 与 Python 一样给出最短往返表示，并采用同一套「何时用科学计数法」阈值
/// （指数 < -4 或 >= 16），差别只在指数部分：Rust 写 `1e-7` / `1e16`，Python 写
/// `1e-07` / `1e+16`（符号固定、至少两位数字）。
pub fn float_repr(value: f64) -> String {
    let text = format!("{:?}", value);
    let Some(index) = text.find('e') else {
        return text;
    };
    let (mantissa, exponent) = text.split_at(index);
    let exponent = &exponent[1..];
    let (sign, digits) = match exponent.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("+", exponent.strip_prefix('+').unwrap_or(exponent)),
    };
    let digits = if digits.len() < 2 {
        format!("0{digits}")
    } else {
        digits.to_string()
    };
    format!("{mantissa}e{sign}{digits}")
}
