//! 代码块逐 token 高亮（对映 Python 侧 Rich `Syntax` 的 monokai 主题）。
//!
//! Python 的 `rich.markdown.Markdown` 把围栏代码块交给 `rich.syntax.Syntax`，由 pygments 按
//! monokai 主题逐 token 着色；Rust 侧不引入 pygments / syntect（新依赖 + 体积），改为表驱动
//! 的手写扫描器，色值直接取 monokai 原始十六进制值，与代码块底色
//! （`#272822`）同族。
//!
//! 取舍：只覆盖项目实际会遇到的常见语言（Rust / Python / JS·TS / JSON / TOML / YAML /
//! Shell / SQL），未知语言保持整块灰底、不做逐 token 着色；也不追求与 pygments 分词等价
//! （例如 Python 的 f-string 内部表达式、Rust 宏体内部），差异记在 crate README。
//!
//! 纯文本片段沿用底色样式（不显式设前景），既保证弱色终端仍可读，也让「无高亮语言」
//! 的输出与本文档落地前完全一致。

/// 底色样式（测试里断言样式串用；生产代码的底色由 `markdown` 模块拼好传入）。
#[cfg(test)]
use crate::ui::fullscreen::terminal::theme::REASONING_BACKGROUND;

/// monokai 关键字与内建名（`fn`、`if`、`True`、`Select`…）。
const KEYWORD: &str = "#66d9ef";
/// monokai 函数名 / 类型名 / 装饰器（`foo(`、`struct Foo`、`@decorator`）。
const NAME: &str = "#a6e22e";
/// monokai 字符串字面量。
const STRING: &str = "#e6db74";
/// monokai 数字字面量。
const NUMBER: &str = "#ae81ff";
/// monokai 注释（额外加斜体）。
const COMMENT: &str = "#75715e";
/// monokai 运算符（`=`、`->`、`::`…）。括号、逗号等标点仍算纯文本。
const OPERATOR: &str = "#f92672";

/// 跨行保留的扫描状态：块注释与三引号字符串。整个代码块共用一份。
#[derive(Debug, Default, Clone)]
pub struct ScanState {
    in_block_comment: bool,
    triple_quote: Option<&'static str>,
}

/// 一门语言的词法表。
struct Spec {
    /// 关键字 + 常量 + 内建名（monokai 里同色）。
    keywords: &'static [&'static str],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// 引号串（长的在前，保证 `"""` 先于 `"` 命中）。
    quotes: &'static [&'static str],
    /// 是否支持三引号跨行字符串（Python）。
    triple_quotes: bool,
    /// 大小写不敏感（SQL）。
    case_insensitive: bool,
    /// `@name` 装饰器（Python）。
    decorator: bool,
    /// 引号前可带前缀字母（Python 的 `r`/`b`/`f`、Rust 的 `r`）。
    string_prefixes: bool,
    /// 单引号只在能就近闭合时才算字面量（Rust 的 `'a` 是生命周期）。
    char_literal_only: bool,
    /// 键名提示：`ident`/字符串后紧跟该字符时按提示色着色（JSON/YAML 的 `:`、TOML 的 `=`）。
    key_hint: Option<(char, &'static str)>,
}

const RUST: Spec = Spec {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
        "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type",
        "unsafe", "use", "where", "while", "yield", "union", "default", "bool", "char", "str",
        "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64",
        "u128", "usize", "String", "Vec", "Option", "Result", "Box", "Rc", "Arc", "HashMap",
        "BTreeMap", "Path", "PathBuf", "Some", "None", "Ok", "Err",
    ],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &["\"", "'"],
    triple_quotes: false,
    case_insensitive: false,
    decorator: false,
    string_prefixes: true,
    char_literal_only: true,
    key_hint: None,
};

const PYTHON: Spec = Spec {
    keywords: &[
        "and",
        "as",
        "assert",
        "async",
        "await",
        "break",
        "class",
        "continue",
        "def",
        "del",
        "elif",
        "else",
        "except",
        "finally",
        "for",
        "from",
        "global",
        "if",
        "import",
        "in",
        "is",
        "lambda",
        "nonlocal",
        "not",
        "or",
        "pass",
        "raise",
        "return",
        "try",
        "while",
        "with",
        "yield",
        "match",
        "case",
        "None",
        "True",
        "False",
        "int",
        "float",
        "str",
        "bool",
        "bytes",
        "list",
        "dict",
        "set",
        "tuple",
        "len",
        "print",
        "range",
        "enumerate",
        "zip",
        "open",
        "isinstance",
        "super",
        "type",
        "Exception",
        "ValueError",
        "RuntimeError",
        "KeyError",
        "TypeError",
        "Path",
    ],
    line_comments: &["#"],
    block_comment: None,
    quotes: &["\"\"\"", "'''", "\"", "'"],
    triple_quotes: true,
    case_insensitive: false,
    decorator: true,
    string_prefixes: true,
    char_literal_only: false,
    key_hint: None,
};

const JAVASCRIPT: Spec = Spec {
    keywords: &[
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "default",
        "delete",
        "do",
        "else",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "null",
        "of",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "yield",
        "as",
        "enum",
        "implements",
        "interface",
        "private",
        "protected",
        "public",
        "readonly",
        "type",
        "declare",
        "namespace",
        "any",
        "unknown",
        "never",
        "string",
        "number",
        "boolean",
        "symbol",
        "bigint",
        "object",
    ],
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &["\"", "'", "`"],
    triple_quotes: false,
    case_insensitive: false,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: None,
};

const JSON: Spec = Spec {
    keywords: &["true", "false", "null"],
    line_comments: &[],
    block_comment: None,
    quotes: &["\""],
    triple_quotes: false,
    case_insensitive: false,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: Some((':', OPERATOR)),
};

const TOML: Spec = Spec {
    keywords: &["true", "false"],
    line_comments: &["#"],
    block_comment: None,
    quotes: &["\"\"\"", "'''", "\"", "'"],
    triple_quotes: true,
    case_insensitive: false,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: Some(('=', NAME)),
};

const YAML: Spec = Spec {
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    line_comments: &["#"],
    block_comment: None,
    quotes: &["\"", "'"],
    triple_quotes: false,
    case_insensitive: false,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: Some((':', OPERATOR)),
};

const SHELL: Spec = Spec {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
        "in", "function", "return", "local", "export", "readonly", "set", "unset", "shift",
        "source", "alias", "echo", "cd", "exit", "printf", "test", "true", "false",
    ],
    line_comments: &["#"],
    block_comment: None,
    quotes: &["\"", "'"],
    triple_quotes: false,
    case_insensitive: false,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: None,
};

const SQL: Spec = Spec {
    keywords: &[
        "select",
        "from",
        "where",
        "insert",
        "into",
        "values",
        "update",
        "set",
        "delete",
        "create",
        "table",
        "index",
        "view",
        "drop",
        "alter",
        "add",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "on",
        "group",
        "by",
        "order",
        "having",
        "limit",
        "offset",
        "distinct",
        "as",
        "and",
        "or",
        "not",
        "null",
        "is",
        "in",
        "like",
        "between",
        "case",
        "when",
        "then",
        "else",
        "end",
        "union",
        "all",
        "primary",
        "key",
        "foreign",
        "references",
        "default",
        "unique",
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "coalesce",
        "int",
        "integer",
        "text",
        "varchar",
        "boolean",
        "timestamp",
        "true",
        "false",
    ],
    line_comments: &["--"],
    block_comment: Some(("/*", "*/")),
    quotes: &["'"],
    triple_quotes: false,
    case_insensitive: true,
    decorator: false,
    string_prefixes: false,
    char_literal_only: false,
    key_hint: None,
};

/// 围栏信息串 → 词法表；`None` 表示这门语言不做逐 token 着色。
///
/// 别名按 GitHub / pygments 的常见写法收：`py`、`rs`、`js`、`ts`、`sh`、`bash` 等。
fn spec_for(language: &str) -> Option<&'static Spec> {
    let name = language.trim().to_ascii_lowercase();
    match name.as_str() {
        "rust" | "rs" => Some(&RUST),
        "python" | "py" | "python3" => Some(&PYTHON),
        "javascript" | "js" | "jsx" | "mjs" | "cjs" | "typescript" | "ts" | "tsx" => {
            Some(&JAVASCRIPT)
        }
        "json" | "jsonc" => Some(&JSON),
        "toml" => Some(&TOML),
        "yaml" | "yml" => Some(&YAML),
        "bash" | "sh" | "shell" | "zsh" | "console" | "shell-session" => Some(&SHELL),
        "sql" | "postgres" | "postgresql" | "sqlite" => Some(&SQL),
        _ => None,
    }
}

fn with_fg(fg: &str, base: &str) -> String {
    format!("{fg} {base}")
}

fn comment_style(base: &str) -> String {
    format!("{COMMENT} italic {base}")
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    let mut index = at;
    for expected in needle.chars() {
        match chars.get(index) {
            Some(actual) if *actual == expected => index += 1,
            _ => return false,
        }
    }
    true
}

/// 在 `chars[from..]` 里查找 `needle`，返回绝对下标。
fn find_seq(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    if needle.is_empty() || from >= chars.len() {
        return None;
    }
    (from..chars.len()).find(|index| starts_with(chars, *index, needle))
}

fn is_ident_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_ident_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 词是否命中表；`case_insensitive` 时按小写比较（SQL）。
fn in_table(word: &str, table: &[&str], case_insensitive: bool) -> bool {
    if case_insensitive {
        let lowered = word.to_lowercase();
        table.iter().any(|item| *item == lowered)
    } else {
        table.contains(&word)
    }
}

/// 跳过空白后返回下一个字符（用于「是不是函数调用 / 键名」这类前瞻）。
fn next_non_space(chars: &[char], from: usize) -> Option<char> {
    chars[from..].iter().copied().find(|ch| !ch.is_whitespace())
}

/// 一行代码 → 带样式的片段。
///
/// `base` 是代码块底色样式（`on #272822`）；纯文本片段原样沿用，只有被识别的 token 才
/// 追加前景色。`state` 由调用方在同一个代码块内复用，用来承接块注释与三引号字符串。
pub fn highlight_line(
    language: &str,
    line: &str,
    state: &mut ScanState,
    base: &str,
) -> Vec<(String, String)> {
    let plain = |text: &str| vec![(text.to_string(), base.to_string())];
    let Some(spec) = spec_for(language) else {
        return plain(line);
    };

    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<(String, String)> = Vec::new();
    let mut text = String::new();
    let mut index = 0usize;

    /// 把当前累积的纯文本按给定样式落盘。
    macro_rules! flush {
        ($style:expr) => {{
            if !text.is_empty() {
                spans.push((std::mem::take(&mut text), $style));
            }
        }};
    }

    while index < chars.len() {
        // 1）上一行留下的三引号字符串：整段到收尾引号（或行尾）都算字符串。
        if let Some(quote) = state.triple_quote {
            match find_seq(&chars, index, quote) {
                Some(end) => {
                    let piece: String = chars[index..end + quote.chars().count()].iter().collect();
                    flush!(with_fg(STRING, base));
                    spans.push((piece, with_fg(STRING, base)));
                    index = end + quote.chars().count();
                    state.triple_quote = None;
                }
                None => {
                    text.extend(&chars[index..]);
                    flush!(with_fg(STRING, base));
                    index = chars.len();
                }
            }
            continue;
        }

        // 2）上一行留下的块注释。
        if state.in_block_comment {
            let (_, end_marker) = spec.block_comment.expect("块注释状态只可能来自有它的语言");
            match find_seq(&chars, index, end_marker) {
                Some(end) => {
                    let piece: String = chars[index..end + end_marker.chars().count()]
                        .iter()
                        .collect();
                    flush!(comment_style(base));
                    spans.push((piece, comment_style(base)));
                    index = end + end_marker.chars().count();
                    state.in_block_comment = false;
                }
                None => {
                    text.extend(&chars[index..]);
                    flush!(comment_style(base));
                    index = chars.len();
                }
            }
            continue;
        }

        // 3）行注释：该行剩余部分整体是注释。
        if spec
            .line_comments
            .iter()
            .any(|marker| starts_with(&chars, index, marker))
        {
            text.extend(&chars[index..]);
            flush!(comment_style(base));
            break;
        }

        // 4）块注释起点（可能本行不闭合，交给下一行续）。
        if let Some((start, end_marker)) = spec.block_comment {
            if starts_with(&chars, index, start) {
                let search_from = index + start.chars().count();
                match find_seq(&chars, search_from, end_marker) {
                    Some(end) => {
                        let piece: String = chars[index..end + end_marker.chars().count()]
                            .iter()
                            .collect();
                        flush!(comment_style(base));
                        spans.push((piece, comment_style(base)));
                        index = end + end_marker.chars().count();
                    }
                    None => {
                        text.extend(&chars[index..]);
                        flush!(comment_style(base));
                        index = chars.len();
                        state.in_block_comment = true;
                    }
                }
                continue;
            }
        }

        // 5）字符串字面量（含前缀写法与就近闭合的单引号）。
        if let Some(found) = scan_string(&chars, index, spec) {
            let ScanString {
                text: piece,
                consumed,
                opened_triple,
            } = found;
            let style = key_or_string_style(&chars, index + consumed, spec, base);
            flush!(style.clone());
            spans.push((piece, style));
            index += consumed;
            if let Some(quote) = opened_triple {
                state.triple_quote = Some(quote);
            }
            continue;
        }

        // 6）装饰器（Python 的 `@name`）与属性 / 宏（Rust 的 `#[..]`、`name!`）。
        if spec.decorator && chars[index] == '@' {
            let start = index;
            index += 1;
            while index < chars.len() && (is_ident_char(chars[index]) || chars[index] == '.') {
                index += 1;
            }
            if index > start + 1 {
                let piece: String = chars[start..index].iter().collect();
                flush!(with_fg(NAME, base));
                spans.push((piece, with_fg(NAME, base)));
                continue;
            }
            index = start;
        }

        // 7）数字字面量。
        if chars[index].is_ascii_digit() && !is_ident_char(chars[index.saturating_sub(1)]) {
            let start = index;
            index = scan_number(&chars, index);
            let piece: String = chars[start..index].iter().collect();
            flush!(with_fg(NUMBER, base));
            spans.push((piece, with_fg(NUMBER, base)));
            continue;
        }

        // 8）标识符：关键字 / 类型 / 函数名 / 宏名 / 普通名。
        if is_ident_start(chars[index]) {
            let start = index;
            while index < chars.len() && is_ident_char(chars[index]) {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            // Rust 原始字符串 `r"..."` / `r#"..."#` 由字符串分支处理，这里让位。
            let style = classify_word(&word, &chars, index, spec, base);
            let color = style.unwrap_or_else(|| base.to_string());
            flush!(color.clone());
            spans.push((word, color));
            continue;
        }

        // 9）其余：多字符运算符着色，括号与标点保持纯文本。
        if let Some(operator) = match_operator(&chars, index) {
            flush!(base.to_string());
            spans.push((operator.to_string(), with_fg(OPERATOR, base)));
            index += operator.chars().count();
            continue;
        }

        text.push(chars[index]);
        index += 1;
    }

    flush!(base.to_string());
    spans
}

/// 字符串扫描结果。
struct ScanString {
    text: String,
    consumed: usize,
    /// 本行开启但未闭合的三引号（交给下一行继续扫）。
    opened_triple: Option<&'static str>,
}

/// 识别字符串起点；`None` 表示当前位置不是字符串。
fn scan_string(chars: &[char], at: usize, spec: &Spec) -> Option<ScanString> {
    // 前缀写法：`r"`、`rb'`、`f"""`…（前缀字母与引号一起算作字面量）。
    let quote_from = if spec.string_prefixes && is_ident_start(chars[at]) && at + 1 < chars.len() {
        if spec
            .quotes
            .iter()
            .any(|quote| starts_with(chars, at + 1, quote))
        {
            at + 1
        } else {
            at
        }
    } else {
        at
    };

    let quote = spec
        .quotes
        .iter()
        .find(|quote| starts_with(chars, quote_from, quote))?;

    // 三引号：本行不闭合时只吃到行尾，状态交给下一行。
    if spec.triple_quotes && quote.chars().count() == 3 {
        return Some(match find_seq(chars, quote_from + 3, quote) {
            Some(end) => {
                let consumed = end + 3 - at;
                ScanString {
                    text: chars[at..at + consumed].iter().collect(),
                    consumed,
                    opened_triple: None,
                }
            }
            None => ScanString {
                text: chars[at..].iter().collect(),
                consumed: chars.len() - at,
                opened_triple: Some(quote),
            },
        });
    }

    // 单引号在 Rust 里是字符字面量还是生命周期：只有就近能闭合才算字面量。
    if spec.char_literal_only && *quote == "'" {
        let closing = find_seq(chars, quote_from + 1, "'")?;
        let body: String = chars[quote_from + 1..closing].iter().collect();
        if body.contains('\'') || body.chars().count() > 2 {
            return None;
        }
    }

    let mut index = quote_from + quote.chars().count();
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if starts_with(chars, index, quote) {
            let consumed = index + quote.chars().count() - at;
            return Some(ScanString {
                text: chars[at..at + consumed].iter().collect(),
                consumed,
                opened_triple: None,
            });
        }
        index += 1;
    }

    // 未闭合：整行剩余部分当字符串（与 pygments 的容错一致）。
    Some(ScanString {
        text: chars[at..].iter().collect(),
        consumed: chars.len() - at,
        opened_triple: None,
    })
}

/// 字符串样式：JSON / YAML / TOML 的键名（后面紧跟 `:` 或 `=`）另有提示色。
fn key_or_string_style(chars: &[char], after: usize, spec: &Spec, base: &str) -> String {
    if let Some((terminator, color)) = spec.key_hint {
        if next_non_space(chars, after) == Some(terminator) {
            return with_fg(color, base);
        }
    }
    with_fg(STRING, base)
}

/// 标识符归类；返回 `None` 表示按纯文本处理。
fn classify_word(
    word: &str,
    chars: &[char],
    next: usize,
    spec: &Spec,
    base: &str,
) -> Option<String> {
    if in_table(word, spec.keywords, spec.case_insensitive) {
        return Some(with_fg(KEYWORD, base));
    }
    if let Some((terminator, color)) = spec.key_hint {
        if next_non_space(chars, next) == Some(terminator) {
            return Some(with_fg(color, base));
        }
    }
    let following = next_non_space(chars, next);
    if following == Some('(') || following == Some('!') {
        // 函数调用与宏名（Rust 的 `println!`）。
        return Some(with_fg(NAME, base));
    }
    let first = word.chars().next()?;
    if first.is_uppercase() {
        // 首字母大写：类型 / 类名；全大写常量与 monokai 的内建名同色。
        return Some(with_fg(NAME, base));
    }
    None
}

/// 数字字面量扫描：十进制 / 十六进制 / 二进制 / 浮点与类型后缀。
fn scan_number(chars: &[char], at: usize) -> usize {
    let mut index = at;
    let radix_prefixed = chars.get(index) == Some(&'0')
        && matches!(
            chars.get(index + 1).map(|ch| ch.to_ascii_lowercase()),
            Some('x') | Some('o') | Some('b')
        );
    if radix_prefixed {
        index += 2;
        while index < chars.len() {
            let ch = chars[index];
            if ch.is_ascii_hexdigit() || ch == '_' {
                index += 1;
            } else {
                break;
            }
        }
    } else {
        while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '_') {
            index += 1;
        }
        if chars.get(index) == Some(&'.')
            && chars
                .get(index + 1)
                .map(|ch| ch.is_ascii_digit())
                .unwrap_or(false)
        {
            index += 1;
            while index < chars.len() && (chars[index].is_ascii_digit() || chars[index] == '_') {
                index += 1;
            }
        }
        if matches!(chars.get(index), Some('e') | Some('E')) {
            let mut probe = index + 1;
            if matches!(chars.get(probe), Some('+') | Some('-')) {
                probe += 1;
            }
            if chars
                .get(probe)
                .map(|ch| ch.is_ascii_digit())
                .unwrap_or(false)
            {
                index = probe;
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                }
            }
        }
    }
    // 类型后缀（`1u32`、`1.0f64`、`10i64`）跟着数字走。
    while index < chars.len() && is_ident_char(chars[index]) {
        index += 1;
    }
    index
}

/// 运算符表（长的在前，保证 `->` 先于 `-`）。
const OPERATORS: &[&str] = &[
    "...", "..=", "->", "=>", "::", "==", "!=", "<=", ">=", "&&", "||", "+=", "-=", "*=", "/=",
    "%=", "**", "++", "--", "..", "<<", ">>", "|>", ":=", "&=", "|=", "^=", "?!",
];

fn match_operator(chars: &[char], at: usize) -> Option<&'static str> {
    if let Some(found) = OPERATORS
        .iter()
        .find(|operator| starts_with(chars, at, operator))
    {
        return Some(found);
    }
    match chars[at] {
        '+' => Some("+"),
        '-' => Some("-"),
        '*' => Some("*"),
        '/' => Some("/"),
        '%' => Some("%"),
        '=' => Some("="),
        '<' => Some("<"),
        '>' => Some(">"),
        '!' => Some("!"),
        '&' => Some("&"),
        '|' => Some("|"),
        '^' => Some("^"),
        '~' => Some("~"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> String {
        format!("on {REASONING_BACKGROUND}")
    }

    /// 把一行拆成 `(片段, 样式)`，方便断言某段文字用了哪个颜色。
    fn styles_of(line: &str, language: &str) -> Vec<(String, String)> {
        let mut state = ScanState::default();
        highlight_line(language, line, &mut state, &base())
    }

    fn piece_style<'a>(spans: &'a [(String, String)], piece: &str) -> Option<&'a str> {
        spans
            .iter()
            .find(|(text, _)| text.contains(piece))
            .map(|(_, style)| style.as_str())
    }

    #[test]
    fn unknown_language_stays_plain() {
        let spans = styles_of("let x = 1;", "brainfuck");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, "let x = 1;");
        assert_eq!(spans[0].1, base());
    }

    #[test]
    fn rust_line_is_tokenized() {
        let spans = styles_of("let x: u32 = 1; // 注释", "rust");
        assert!(piece_style(&spans, "let").unwrap().contains(KEYWORD));
        assert!(piece_style(&spans, "u32").unwrap().contains(KEYWORD));
        assert!(piece_style(&spans, "=").unwrap().contains(OPERATOR));
        assert!(piece_style(&spans, "1").unwrap().contains(NUMBER));
        assert!(piece_style(&spans, "注释").unwrap().contains(COMMENT));
        // 拼接回来必须与原文逐字相同（渲染不允许丢字）。
        let joined: String = spans.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(joined, "let x: u32 = 1; // 注释");
    }

    #[test]
    fn rust_char_literal_is_a_string_but_lifetime_is_not() {
        let char_literal = styles_of("let c = 'x';", "rust");
        assert!(piece_style(&char_literal, "'x'").unwrap().contains(STRING));

        let lifetime = styles_of("fn f<'a>(s: &'a str) {}", "rust");
        assert!(
            !lifetime
                .iter()
                .any(|(text, style)| text.contains("'a") && style.contains(STRING)),
            "生命周期不该被当成字符字面量"
        );
    }

    #[test]
    fn json_keys_use_the_key_color_and_values_stay_strings() {
        let spans = styles_of("{\"name\": \"oc\", \"n\": 1}", "json");
        assert!(piece_style(&spans, "\"name\"").unwrap().contains(OPERATOR));
        assert!(piece_style(&spans, "\"oc\"").unwrap().contains(STRING));
        assert!(piece_style(&spans, "1").unwrap().contains(NUMBER));
    }

    #[test]
    fn toml_keys_are_names() {
        let spans = styles_of("name = \"omnicrawl\"", "toml");
        assert!(piece_style(&spans, "name").unwrap().contains(NAME));
        assert!(piece_style(&spans, "omnicrawl").unwrap().contains(STRING));
    }

    #[test]
    fn block_comment_state_carries_across_lines() {
        let mut state = ScanState::default();
        let first = highlight_line("rust", "/* 开始", &mut state, &base());
        assert!(first.iter().any(|(_, style)| style.contains(COMMENT)));
        assert!(state.in_block_comment);

        let second = highlight_line("rust", "仍然注释 */ let x = 1;", &mut state, &base());
        assert!(second
            .iter()
            .any(|(text, style)| text.contains("仍然注释") && style.contains(COMMENT)));
        assert!(!state.in_block_comment);
        assert!(second
            .iter()
            .any(|(text, style)| text.contains("let") && style.contains(KEYWORD)));
    }

    #[test]
    fn python_triple_quote_carries_across_lines() {
        let mut state = ScanState::default();
        let first = highlight_line("python", "text = \"\"\"第一行", &mut state, &base());
        assert!(first.iter().any(|(_, style)| style.contains(STRING)));
        assert_eq!(state.triple_quote, Some("\"\"\""));

        let second = highlight_line("python", "第二行\"\"\"", &mut state, &base());
        assert!(second.iter().all(|(_, style)| style.contains(STRING)));
        assert_eq!(state.triple_quote, None);
    }

    #[test]
    fn sql_keywords_are_case_insensitive() {
        for line in ["select 1 from t", "SELECT 1 FROM t"] {
            let spans = styles_of(line, "sql");
            assert!(
                spans
                    .iter()
                    .any(|(text, style)| text.eq_ignore_ascii_case("select")
                        && style.contains(KEYWORD)),
                "{line} 的首个词应当是高亮关键字"
            );
        }
    }

    #[test]
    fn python_decorator_and_shell_builtin_are_colored() {
        let decorator = styles_of("@staticmethod", "python");
        assert!(piece_style(&decorator, "@staticmethod")
            .unwrap()
            .contains(NAME));

        let shell = styles_of("echo \"$HOME\" # 提示", "bash");
        assert!(piece_style(&shell, "echo").unwrap().contains(KEYWORD));
        assert!(piece_style(&shell, "$HOME").unwrap().contains(STRING));
        assert!(piece_style(&shell, "提示").unwrap().contains(COMMENT));
    }
}
