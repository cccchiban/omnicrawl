//! LaTeX 数学公式 → 终端可读 Unicode 文本（对映 Python `rendering/latex.py`）。
//!
//! 终端无法渲染真正的数学排版，这一层把回复中常见的 LaTeX 数学片段（`$...$` 行内、
//! `$$...$$` 块级、`\(...\)`、`\[...\]`，以及 `latex`/`tex`/`math` 数学 fenced block
//! 与整行无定界符的裸公式）转换为 Unicode 数学符号与上下标：
//!
//! ```text
//! \frac{a}{b}  →  a/b
//! \sqrt{x}     →  √x
//! x^2          →  x²
//! \sum_{i=1}^{n} →  ∑ᵢ₌₁ⁿ
//! \alpha       →  α
//! ```
//!
//! 设计约束与 Python 侧逐条一致：
//!
//! - 零外部依赖、单遍扫描，只处理公式片段，非公式文本原样复制；
//! - 未知命令或解析失败时保留原文（Markdown 渲染会原样显示 `\name`），绝不丢内容；
//! - 输出避免触发 Markdown 语法（`*` 转乘号、上下标消费 `^`/`_`、花括号分组不输出）；
//! - 防御极端输入：公式片段超长或嵌套过深时原样返回。
//!
//! Python 侧用正则表达式的四处（公式片段定位、fenced 围栏、多余美元、数学命令探测）
//! 在本层用手写扫描器承接：`regex` crate 不支持环视，而原模式里的 `(?<!\\)` /
//! `(?!\$)` 是语义的一部分。语义等价，不引新增依赖。
//!
//! 已知差异（都不出现在真实语料里）：
//!
//! - 裸公式行判定里的 `str.isdigit()` 用 `char::is_numeric()` 近似（Unicode 数字类别
//!   略有出入），`str.isalpha()` 用 `char::is_alphabetic()`；
//! - `str.strip()` 用 `str::trim()`（Unicode 空白集合略有出入）。

/// 公式片段长度上限：超过即原样保留，避免病态输入拖垮渲染。
const MAX_FORMULA_LEN: usize = 10_000;
/// 递归嵌套深度上限。
const MAX_DEPTH: usize = 16;
/// 裸公式行的长度上限。
const BARE_MATH_LINE_MAX_LEN: usize = 400;
/// `\$` 的保护占位符，处理完公式后还原成 `$`。
const ESCAPED_DOLLAR: char = '\u{0}';

const GREEK: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ϵ"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "φ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("ell", "ℓ"),
    ("hbar", "ℏ"),
    ("imath", "ı"),
    ("jmath", "ȷ"),
];

const SYMBOLS: &[(&str, &str)] = &[
    ("cdot", "·"),
    ("times", "×"),
    ("div", "÷"),
    ("pm", "±"),
    ("mp", "∓"),
    ("leq", "≤"),
    ("leqq", "≤"),
    ("geq", "≥"),
    ("geqq", "≥"),
    ("neq", "≠"),
    ("ne", "≠"),
    ("approx", "≈"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("equiv", "≡"),
    ("propto", "∝"),
    ("infty", "∞"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("subseteq", "⊆"),
    ("supset", "⊃"),
    ("supseteq", "⊇"),
    ("cup", "∪"),
    ("cap", "∩"),
    ("setminus", "∖"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("nabla", "∇"),
    ("partial", "∂"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("leftarrow", "←"),
    ("mapsto", "↦"),
    ("Rightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("land", "∧"),
    ("lor", "∨"),
    ("neg", "¬"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("dots", "…"),
    ("ldots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("prime", "′"),
    ("degree", "°"),
    ("circ", "∘"),
    ("angle", "∠"),
    ("perp", "⊥"),
    ("parallel", "∥"),
    ("mid", "∣"),
    ("colon", ":"),
    ("backslash", "∖"),
    ("star", "∗"),
    ("ast", "∗"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("odot", "⊙"),
    ("surd", "√"),
    ("aleph", "ℵ"),
    ("Re", "ℜ"),
    ("Im", "ℑ"),
    ("mod", "mod"),
    ("bmod", "mod"),
];

/// 无参数函数名：输出为名称（可后接 `^`/`_` 上下标）。
const FUNC_NAMES: &[(&str, &str)] = &[
    ("sum", "∑"),
    ("prod", "∏"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("lim", "lim"),
    ("log", "log"),
    ("ln", "ln"),
    ("lg", "lg"),
    ("sin", "sin"),
    ("cos", "cos"),
    ("tan", "tan"),
    ("cot", "cot"),
    ("sec", "sec"),
    ("csc", "csc"),
    ("arcsin", "arcsin"),
    ("arccos", "arccos"),
    ("arctan", "arctan"),
    ("sinh", "sinh"),
    ("cosh", "cosh"),
    ("tanh", "tanh"),
    ("coth", "coth"),
    ("exp", "exp"),
    ("max", "max"),
    ("min", "min"),
    ("sup", "sup"),
    ("inf", "inf"),
    ("det", "det"),
    ("arg", "arg"),
    ("dim", "dim"),
    ("gcd", "gcd"),
    ("deg", "deg"),
    ("ker", "ker"),
    ("rank", "rank"),
    ("Pr", "Pr"),
];

/// 花括号/字号命令：仅影响括号样式，直接丢弃。
const LEFT_RIGHT: &[&str] = &[
    "left", "right", "big", "Big", "bigg", "Bigg", "bigl", "bigr", "Bigl", "Bigr", "biggl",
    "biggr", "Biggl", "Biggr",
];

/// 文本样式命令：保留参数内容。
const TEXT_STYLE: &[&str] = &[
    "text",
    "mathrm",
    "mathit",
    "mathbf",
    "boldsymbol",
    "mbox",
    "operatorname",
    "mathcal",
    "mathscr",
    "mathfrak",
    "mathsf",
    "mathtt",
    "textrm",
    "textbf",
    "textit",
];

/// `\mathbb{...}` 常见字母集。
const BLACKBOARD: &[(&str, &str)] = &[
    ("R", "ℝ"),
    ("Z", "ℤ"),
    ("Q", "ℚ"),
    ("N", "ℕ"),
    ("C", "ℂ"),
    ("P", "ℙ"),
    ("H", "ℍ"),
];

/// 转义符号：`\x` → 字面量。
const ESCAPED: &[(&str, &str)] = &[
    ("{", "{"),
    ("}", "}"),
    ("%", "%"),
    ("$", "$"),
    ("#", "#"),
    ("&", "&"),
    ("_", "_"),
    ("|", "|"),
];

/// 重音/装饰命令：终端无法可靠渲染组合重音，丢弃命令保留参数内容。
const ACCENTS: &[&str] = &[
    "hat",
    "bar",
    "vec",
    "dot",
    "ddot",
    "tilde",
    "overline",
    "underline",
    "widehat",
    "widetilde",
    "check",
    "acute",
    "grave",
];

/// Unicode 上标字符（无变体的字符走 `^x` / `^(..)` 回退）。
const SUPERSCRIPT: &[(char, char)] = &[
    ('0', '⁰'),
    ('1', '¹'),
    ('2', '²'),
    ('3', '³'),
    ('4', '⁴'),
    ('5', '⁵'),
    ('6', '⁶'),
    ('7', '⁷'),
    ('8', '⁸'),
    ('9', '⁹'),
    ('+', '⁺'),
    ('-', '⁻'),
    ('=', '⁼'),
    ('(', '⁽'),
    (')', '⁾'),
    ('i', 'ⁱ'),
    ('n', 'ⁿ'),
    ('a', 'ᵃ'),
    ('b', 'ᵇ'),
    ('c', 'ᶜ'),
    ('d', 'ᵈ'),
    ('e', 'ᵉ'),
    ('f', 'ᶠ'),
    ('g', 'ᵍ'),
    ('h', 'ʰ'),
    ('j', 'ʲ'),
    ('k', 'ᵏ'),
    ('l', 'ˡ'),
    ('m', 'ᵐ'),
    ('o', 'ᵒ'),
    ('p', 'ᵖ'),
    ('r', 'ʳ'),
    ('s', 'ˢ'),
    ('t', 'ᵗ'),
    ('u', 'ᵘ'),
    ('v', 'ᵛ'),
    ('w', 'ʷ'),
    ('x', 'ˣ'),
    ('y', 'ʸ'),
    ('z', 'ᶻ'),
];

const SUBSCRIPT: &[(char, char)] = &[
    ('0', '₀'),
    ('1', '₁'),
    ('2', '₂'),
    ('3', '₃'),
    ('4', '₄'),
    ('5', '₅'),
    ('6', '₆'),
    ('7', '₇'),
    ('8', '₈'),
    ('9', '₉'),
    ('+', '₊'),
    ('-', '₋'),
    ('=', '₌'),
    ('(', '₍'),
    (')', '₎'),
    ('a', 'ₐ'),
    ('e', 'ₑ'),
    ('h', 'ₕ'),
    ('i', 'ᵢ'),
    ('j', 'ⱼ'),
    ('k', 'ₖ'),
    ('l', 'ₗ'),
    ('m', 'ₘ'),
    ('n', 'ₙ'),
    ('o', 'ₒ'),
    ('p', 'ₚ'),
    ('r', 'ᵣ'),
    ('s', 'ₛ'),
    ('t', 'ₜ'),
    ('u', 'ᵤ'),
    ('v', 'ᵥ'),
    ('x', 'ₓ'),
];

/// 数学环境：矩阵/方程组等，转多行对齐文本（键为环境名，值为左右括号）。
const MATRIX_ENVS: &[(&str, &str, &str)] = &[
    ("matrix", "", ""),
    ("smallmatrix", "", ""),
    ("pmatrix", "(", ")"),
    ("bmatrix", "[", "]"),
    ("Bmatrix", "{", "}"),
    ("vmatrix", "|", "|"),
    ("Vmatrix", "‖", "‖"),
    ("cases", "{", ""),
];

const MATH_FENCE_LANGUAGES: &[&str] = &[
    "latex",
    "tex",
    "math",
    "mathjax",
    "katex",
    "equation",
    "equations",
];

const MATH_COMMAND_HINTS: &[&str] = &[
    "begin", "end", "frac", "dfrac", "tfrac", "sqrt", "sum", "prod", "int", "iint", "iiint",
    "oint", "lim", "sin", "cos", "tan", "log", "ln", "exp", "alpha", "beta", "gamma", "delta",
    "theta", "lambda", "mu", "pi", "sigma", "phi", "omega", "Delta", "Gamma", "Pi", "Sigma", "Phi",
    "Omega", "pm", "mp", "cdot", "times", "leq", "geq", "neq", "approx", "infty", "in", "to",
    "left", "right", "text", "mathrm", "mathbf", "mathbb", "quad", "qquad",
];

/// 需要 `(..)` 包裹的分子/分母/根号内容：长度 > 1 且含运算符。
const OPERATOR_CHARS: &[char] = &['+', '-', '=', '<', '>', '/', ','];

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

fn lookup_char(table: &[(char, char)], key: char) -> Option<char> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

fn matrix_env(name: &str) -> Option<(&'static str, &'static str)> {
    MATRIX_ENVS
        .iter()
        .find(|(env, _, _)| *env == name)
        .map(|(_, left, right)| (*left, *right))
}

// ── char 切片小工具 ─────────────────────────────────────────

fn starts_with(chars: &[char], index: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    index + needle.len() <= chars.len() && chars[index..index + needle.len()] == needle[..]
}

fn find_from(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return None;
    }
    let mut index = from;
    while index + needle.len() <= chars.len() {
        if chars[index..index + needle.len()] == needle[..] {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn ws_run(chars: &[char], index: usize) -> usize {
    let mut end = index;
    while end < chars.len() && (chars[end] == ' ' || chars[end] == '\t') {
        end += 1;
    }
    end - index
}

fn dollar_run(chars: &[char], index: usize) -> usize {
    let mut end = index;
    while end < chars.len() && chars[end] == '$' {
        end += 1;
    }
    end - index
}

fn is_line_start(chars: &[char], index: usize) -> bool {
    index == 0 || chars[index - 1] == '\n'
}

/// 对映 `(?=\r?$)`
fn eol_ok(chars: &[char], index: usize) -> bool {
    let n = chars.len();
    if index >= n {
        return true;
    }
    if chars[index] == '\n' {
        return true;
    }
    chars[index] == '\r' && (index + 1 >= n || chars[index + 1] == '\n')
}

/// Python `str.split(sep)` 的 char 切片等价物（丢弃分隔符、保留空字段）。
fn split_seq(chars: &[char], separator: &[char]) -> Vec<Vec<char>> {
    let mut parts: Vec<Vec<char>> = Vec::new();
    let mut current: Vec<char> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if index + separator.len() <= chars.len()
            && chars[index..index + separator.len()] == separator[..]
        {
            parts.push(std::mem::take(&mut current));
            index += separator.len();
            continue;
        }
        current.push(chars[index]);
        index += 1;
    }
    parts.push(current);
    parts
}

// ── 命令匹配（对映 `\\([a-zA-Z]+|.)`） ─────────────────────

/// 匹配一个 LaTeX 命令，返回 `(结束下标, 原文, 参数名)`。
///
/// 与 Python 的 `\\([a-zA-Z]+|.)` 同语义：优先吃掉整段 ASCII 字母，否则吃单个
/// 字符；`.` 不匹配换行，因此反斜杠后紧跟换行时无匹配。
fn match_cmd(chars: &[char], index: usize) -> Option<(usize, String, String)> {
    if index >= chars.len() || chars[index] != '\\' {
        return None;
    }
    let next = index + 1;
    if next >= chars.len() {
        return None;
    }
    let ch = chars[next];
    if ch.is_ascii_alphabetic() {
        let mut end = next;
        while end < chars.len() && chars[end].is_ascii_alphabetic() {
            end += 1;
        }
        let name: String = chars[next..end].iter().collect();
        Some((end, format!("\\{name}"), name))
    } else if ch == '\n' {
        None
    } else {
        Some((next + 1, format!("\\{ch}"), ch.to_string()))
    }
}

/// 对映 `\\([a-zA-Z]+|[,;!])` 的 `finditer` 取词：返回所有命令名，不重叠。
fn math_command_names(chars: &[char]) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() {
            let ch = chars[index + 1];
            if ch.is_ascii_alphabetic() {
                let mut end = index + 1;
                while end < chars.len() && chars[end].is_ascii_alphabetic() {
                    end += 1;
                }
                names.push(chars[index + 1..end].iter().collect());
                index = end;
                continue;
            }
            if matches!(ch, ',' | ';' | '!') {
                names.push(ch.to_string());
                index += 2;
                continue;
            }
        }
        index += 1;
    }
    names
}

fn has_math_command_hint(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    math_command_names(&chars)
        .iter()
        .any(|name| MATH_COMMAND_HINTS.contains(&name.as_str()))
}

// ── 公式版式转换 ────────────────────────────────────────────

fn needs_parens(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars.len() > 1 && chars.iter().any(|ch| OPERATOR_CHARS.contains(ch))
}

/// 上标内容转 Unicode；无法整体映射时回退为 `^x` / `^(..)`。
fn to_superscript(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() == 1 {
        return match lookup_char(SUPERSCRIPT, chars[0]) {
            Some(mapped) => mapped.to_string(),
            None => format!("^{text}"),
        };
    }
    let mapped: Option<String> = chars
        .iter()
        .map(|ch| lookup_char(SUPERSCRIPT, *ch))
        .collect();
    match mapped {
        Some(mapped) => mapped,
        None => format!("^({text})"),
    }
}

/// 下标内容转 Unicode；无法整体映射时回退为 `_x` / `_(..)`。
fn to_subscript(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() == 1 {
        return match lookup_char(SUBSCRIPT, chars[0]) {
            Some(mapped) => mapped.to_string(),
            None => format!("_{text}"),
        };
    }
    let mapped: Option<String> = chars.iter().map(|ch| lookup_char(SUBSCRIPT, *ch)).collect();
    match mapped {
        Some(mapped) => mapped,
        None => format!("_({text})"),
    }
}

/// 读一个 LaTeX 参数：优先 `{...}` 平衡组，否则单个 token。
///
/// 返回 `(参数内容, 结束位置)`；参数内容不含外层花括号。反斜杠后无字符时
/// 对应 Python 里的 `assert match is not None` 失败，用 `Err` 让整次转换回退原文。
fn read_group(text: &[char], mut index: usize) -> Result<(String, usize), ()> {
    let n = text.len();
    while index < n && text[index] == ' ' {
        index += 1;
    }
    if index < n && text[index] == '{' {
        let mut depth = 1usize;
        index += 1;
        let start = index;
        while index < n && depth > 0 {
            if text[index] == '{' {
                depth += 1;
            } else if text[index] == '}' {
                depth -= 1;
            }
            index += 1;
        }
        let end = index.saturating_sub(1).min(n);
        let content = if start <= end {
            text[start..end].iter().collect()
        } else {
            String::new()
        };
        return Ok((content, index));
    }
    if index >= n {
        return Ok((String::new(), index));
    }
    if text[index] == '\\' {
        return match match_cmd(text, index) {
            Some((end, full, _name)) => Ok((full, end)),
            None => Err(()),
        };
    }
    if text[index].is_ascii_digit() {
        let mut end = index;
        while end < n && text[end].is_ascii_digit() {
            end += 1;
        }
        return Ok((text[index..end].iter().collect(), end));
    }
    Ok((text[index].to_string(), index + 1))
}

/// 把 `\begin{env}` 之后的矩阵内容转换到匹配的 `\end{env}`。
///
/// 单元格递归转换（支持嵌套命令），行与列按内容宽度对齐；环境未闭合时返回空串
/// 并停在 `begin` 之后，由外层扫描继续处理（绝不抛异常）。
fn convert_matrix_env(
    text: &[char],
    index: usize,
    env: &str,
    block: bool,
    depth: usize,
) -> Result<(String, usize), ()> {
    if depth > MAX_DEPTH {
        return Ok((String::new(), index));
    }
    let end_marker = format!("\\end{{{env}}}");
    let Some(end_pos) = find_from(text, index, &end_marker) else {
        return Ok((String::new(), index));
    };
    let content: Vec<char> = text[index..end_pos].to_vec();
    let (left, right) = matrix_env(env).unwrap_or(("", ""));
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row in split_seq(&content, &['\\', '\\']) {
        let mut cells = Vec::new();
        for cell in split_seq(&row, &['&']) {
            let cell: String = cell.iter().collect();
            let cell = cell.trim();
            let cell_chars: Vec<char> = cell.chars().collect();
            cells.push(convert_expr(&cell_chars, block, depth + 1)?);
        }
        rows.push(cells);
    }
    let next_index = end_pos + end_marker.chars().count();
    if rows.is_empty() {
        return Ok((format!("{left}{right}"), next_index));
    }
    let column_count = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    let mut widths = vec![0usize; column_count];
    for column in 0..column_count {
        let mut width = 0usize;
        for row in &rows {
            let length = if column < row.len() {
                row[column].chars().count()
            } else {
                0
            };
            width = width.max(length);
        }
        widths[column] = width;
    }
    let mut lines = Vec::new();
    for row in &rows {
        let mut cells = Vec::new();
        for column in 0..column_count {
            let cell = if column < row.len() {
                let content = &row[column];
                let pad = widths[column].saturating_sub(content.chars().count());
                format!("{content}{}", " ".repeat(pad))
            } else {
                " ".repeat(widths[column])
            };
            cells.push(cell);
        }
        lines.push(cells.join(" ").trim_end().to_string());
    }
    let body = lines.join("\n");
    if left.is_empty() {
        Ok((body, next_index))
    } else {
        Ok((format!("{left}\n{body}\n{right}"), next_index))
    }
}

/// 把单个公式片段转换为 Unicode 可读文本（单遍扫描）。
fn convert_expr(text: &[char], block: bool, depth: usize) -> Result<String, ()> {
    if depth > MAX_DEPTH || text.len() > MAX_FORMULA_LEN {
        return Ok(text.iter().collect());
    }
    let n = text.len();
    let mut out = String::new();
    let mut index = 0usize;
    while index < n {
        let ch = text[index];
        if ch == '\\' {
            let Some((end, full, name)) = match_cmd(text, index) else {
                out.push('\\');
                index += 1;
                continue;
            };
            index = end;
            if name == "begin" {
                let (env, next) = read_group(text, index)?;
                index = next;
                if matrix_env(&env).is_some() {
                    let (rendered, next) = convert_matrix_env(text, index, &env, block, depth)?;
                    index = next;
                    out.push_str(&rendered);
                }
                continue;
            }
            if name == "end" {
                let (_env, next) = read_group(text, index)?;
                index = next;
                continue;
            }
            if LEFT_RIGHT.contains(&name.as_str()) {
                continue;
            }
            if let Some(value) = lookup(GREEK, &name) {
                out.push_str(value);
                continue;
            }
            if let Some(value) = lookup(SYMBOLS, &name) {
                out.push_str(value);
                continue;
            }
            if let Some(value) = lookup(FUNC_NAMES, &name) {
                out.push_str(value);
                continue;
            }
            if name == "pmod" {
                let (arg, next) = read_group(text, index)?;
                index = next;
                let converted = convert_expr(&arg.chars().collect::<Vec<_>>(), block, depth + 1)?;
                out.push_str(&format!("(mod {converted})"));
                continue;
            }
            if name == "mathbb" {
                let (arg, next) = read_group(text, index)?;
                index = next;
                out.push_str(lookup(BLACKBOARD, &arg).unwrap_or(&arg));
                continue;
            }
            if ACCENTS.contains(&name.as_str()) || TEXT_STYLE.contains(&name.as_str()) {
                let (arg, next) = read_group(text, index)?;
                index = next;
                out.push_str(&convert_expr(
                    &arg.chars().collect::<Vec<_>>(),
                    block,
                    depth + 1,
                )?);
                continue;
            }
            if name == "frac" || name == "dfrac" || name == "tfrac" {
                let (numerator, next) = read_group(text, index)?;
                let (denominator, next2) = read_group(text, next)?;
                index = next2;
                let mut num =
                    convert_expr(&numerator.chars().collect::<Vec<_>>(), block, depth + 1)?;
                let mut den =
                    convert_expr(&denominator.chars().collect::<Vec<_>>(), block, depth + 1)?;
                if needs_parens(&num) {
                    num = format!("({num})");
                }
                if needs_parens(&den) {
                    den = format!("({den})");
                }
                out.push_str(&format!("{num}/{den}"));
                continue;
            }
            if name == "sqrt" {
                let mut root: Option<String> = None;
                if index < n && text[index] == '[' {
                    if let Some(close) = find_from(text, index, "]") {
                        root = Some(text[index + 1..close].iter().collect());
                        index = close + 1;
                    }
                }
                let (arg, next) = read_group(text, index)?;
                index = next;
                let mut body = convert_expr(&arg.chars().collect::<Vec<_>>(), block, depth + 1)?;
                if needs_parens(&body) {
                    body = format!("({body})");
                }
                let prefix = match &root {
                    Some(root) => format!("{}√", to_superscript(root)),
                    None => "√".to_string(),
                };
                out.push_str(&format!("{prefix}{body}"));
                continue;
            }
            if let Some(value) = lookup(ESCAPED, &name) {
                out.push_str(value);
                continue;
            }
            if name == "\\" {
                // 换行命令：块级公式里是显式换行，行内降级为空格
                out.push_str(if block { "\n" } else { " " });
                continue;
            }
            if name == " "
                || name == ","
                || name == ";"
                || name == "!"
                || name == "quad"
                || name == "qquad"
                || name == "enspace"
                || name == "thinspace"
            {
                out.push(' ');
                continue;
            }
            // 未知命令：保留原文，并补一个空格避免与后续字符粘连。
            out.push_str(&full);
            out.push(' ');
            continue;
        }
        if ch == '{' {
            let mut depth_count = 1usize;
            index += 1;
            let start = index;
            while index < n && depth_count > 0 {
                if text[index] == '{' {
                    depth_count += 1;
                } else if text[index] == '}' {
                    depth_count -= 1;
                }
                index += 1;
            }
            let end = index.saturating_sub(1).min(n);
            let inner = if start <= end {
                text[start..end].iter().collect::<String>()
            } else {
                String::new()
            };
            out.push_str(&convert_expr(
                &inner.chars().collect::<Vec<_>>(),
                block,
                depth + 1,
            )?);
            continue;
        }
        if ch == '}' {
            out.push('}');
            index += 1;
            continue;
        }
        if ch == '^' {
            let (arg, next) = read_group(text, index + 1)?;
            index = next;
            let converted = convert_expr(&arg.chars().collect::<Vec<_>>(), block, depth + 1)?;
            out.push_str(&to_superscript(&converted));
            continue;
        }
        if ch == '_' {
            let (arg, next) = read_group(text, index + 1)?;
            index = next;
            let converted = convert_expr(&arg.chars().collect::<Vec<_>>(), block, depth + 1)?;
            out.push_str(&to_subscript(&converted));
            continue;
        }
        if ch == '&' || ch == '~' {
            out.push(' ');
            index += 1;
            continue;
        }
        if ch == '*' {
            // 避免触发 Markdown 强调，数学中的 * 即乘号
            out.push('×');
            index += 1;
            continue;
        }
        out.push(ch);
        index += 1;
    }
    Ok(out)
}

// ── fenced 围栏 ─────────────────────────────────────────────

/// 一个 fenced 围栏的匹配结果。
#[derive(Debug, Clone)]
enum FenceKind {
    /// 多行围栏：```lang\n body \n```
    Multi { language: String, body: String },
    /// 单行围栏：```lang body```
    Single { content: String },
}

/// 对映 `_FENCED_BLOCK_PATTERN`（多行）。返回 `(结束位置, 结果)`。
fn try_multi_line(chars: &[char], start: usize) -> Option<(usize, FenceKind)> {
    let n = chars.len();
    let indent = ws_run(chars, start);
    if indent > 3 {
        return None;
    }
    let mut p = start + indent;
    if !starts_with(chars, p, "```") {
        return None;
    }
    p += 3;
    p += ws_run(chars, p);
    let language_start = p;
    while p < n && chars[p] != '`' && chars[p] != '\r' && chars[p] != '\n' {
        p += 1;
    }
    let language: String = chars[language_start..p].iter().collect();
    if p < n && chars[p] == '\r' && p + 1 < n && chars[p + 1] == '\n' {
        p += 2;
    } else if p < n && chars[p] == '\n' {
        p += 1;
    } else {
        return None;
    }
    let body_start = p;
    let mut x = body_start;
    while x <= n {
        let newline = if x < n && chars[x] == '\n' {
            Some(1usize)
        } else if x + 1 < n && chars[x] == '\r' && chars[x + 1] == '\n' {
            Some(2usize)
        } else {
            None
        };
        if let Some(newline) = newline {
            let close_line = x + newline;
            let closing_indent = ws_run(chars, close_line);
            if closing_indent <= 3 {
                let fence = close_line + closing_indent;
                if starts_with(chars, fence, "```") {
                    let end = fence + 3 + ws_run(chars, fence + 3);
                    if eol_ok(chars, end) {
                        let body: String = chars[body_start..x].iter().collect();
                        return Some((end, FenceKind::Multi { language, body }));
                    }
                }
            }
        }
        x += 1;
    }
    None
}

/// 对映 `_SINGLE_LINE_FENCED_BLOCK_PATTERN`。返回 `(结束位置, 结果)`。
fn try_single_line(chars: &[char], start: usize) -> Option<(usize, FenceKind)> {
    let n = chars.len();
    let indent = ws_run(chars, start);
    if indent > 3 {
        return None;
    }
    let mut p = start + indent;
    if !starts_with(chars, p, "```") {
        return None;
    }
    p += 3;
    p += ws_run(chars, p);
    let content_start = p;
    let mut q = content_start;
    loop {
        let gap = ws_run(chars, q);
        if starts_with(chars, q + gap, "```") {
            let end = q + gap + 3 + ws_run(chars, q + gap + 3);
            if eol_ok(chars, end) {
                let content: String = chars[content_start..q].iter().collect();
                return Some((end, FenceKind::Single { content }));
            }
        }
        if q < n && chars[q] != '`' && chars[q] != '\r' && chars[q] != '\n' {
            q += 1;
        } else {
            return None;
        }
    }
}

/// 在一个行首位置尝试两种 fenced 模式（多行优先，与 Python 的交替顺序一致）。
fn match_fence(chars: &[char], start: usize) -> Option<(usize, FenceKind)> {
    try_multi_line(chars, start).or_else(|| try_single_line(chars, start))
}

/// 扫描全部 fenced 围栏（只在行首匹配，与 `re.MULTILINE` 的 `^` 一致）。
fn fence_spans(chars: &[char]) -> Vec<(usize, usize, FenceKind)> {
    let n = chars.len();
    let mut spans = Vec::new();
    let mut index = 0usize;
    while index < n {
        if is_line_start(chars, index) {
            if let Some((end, kind)) = match_fence(chars, index) {
                spans.push((index, end, kind));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    spans
}

/// 对映 `_looks_like_latex_fence`。
fn looks_like_latex_fence(language: &str, body: &str) -> bool {
    let trimmed = language.trim();
    let normalized = if trimmed.is_empty() {
        String::new()
    } else {
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase()
    };
    if MATH_FENCE_LANGUAGES.contains(&normalized.as_str()) {
        return true;
    }
    if !normalized.is_empty() {
        return false;
    }
    if body.contains("$$") || body.contains("\\[") || body.contains("\\(") {
        return true;
    }
    has_math_command_hint(body)
}

/// 对映 `_split_single_line_fenced_content`。
fn split_single_line_fenced_content(content: &str) -> (String, String) {
    let content = content.trim();
    if content.is_empty() {
        return (String::new(), String::new());
    }
    let Some((first, rest)) = content.split_once(' ') else {
        return (String::new(), content.to_string());
    };
    if MATH_FENCE_LANGUAGES.contains(&first.trim().to_lowercase().as_str()) {
        (first.to_string(), rest.trim().to_string())
    } else {
        (String::new(), content.to_string())
    }
}

/// Python 切片 `chars[from_start:len-from_end].strip()`。
fn slice_strip(chars: &[char], from_start: usize, from_end: usize) -> String {
    let len = chars.len();
    let start = from_start.min(len);
    let end = len.saturating_sub(from_end);
    if start >= end {
        return String::new();
    }
    chars[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// 对映 `_unwrap_fenced_formula`：去掉数学 fenced 外层及其内部重复的定界符。
fn unwrap_fenced_formula(body: &str) -> String {
    let formula = normalize_escaped_math_dollars(body.trim());
    let chars: Vec<char> = formula.chars().collect();
    if formula.starts_with("$$") && formula.ends_with("$$") {
        return slice_strip(&chars, 2, 2);
    }
    if formula.starts_with("\\[") && formula.ends_with("\\]") {
        return slice_strip(&chars, 2, 2);
    }
    if formula.starts_with("\\(") && formula.ends_with("\\)") {
        return slice_strip(&chars, 2, 2);
    }
    if formula.starts_with('$') && formula.ends_with('$') && !formula.contains('\n') {
        return slice_strip(&chars, 1, 1);
    }
    formula
}

/// 对映 `_fenced_formula_from_match`：普通代码围栏返回 `None`。
fn fenced_formula(kind: &FenceKind) -> Option<String> {
    let (language, body) = match kind {
        FenceKind::Multi { language, body } => (language.clone(), body.clone()),
        FenceKind::Single { content } => split_single_line_fenced_content(content),
    };
    if !looks_like_latex_fence(&language, &body) {
        return None;
    }
    let formula = unwrap_fenced_formula(&body);
    if formula.is_empty() || formula.chars().count() > MAX_FORMULA_LEN {
        return None;
    }
    Some(formula)
}

// ── 预处理 ──────────────────────────────────────────────────

/// 对映 `_normalize_fenced_blocks`：数学 fenced 归一化为 `$$...$$`；
/// 普通代码围栏替换为占位符，防止其中的 `$x$` 被误当作公式。
fn normalize_fenced_blocks(markdown: &str) -> (String, Vec<(String, String)>) {
    let chars: Vec<char> = markdown.chars().collect();
    let spans = fence_spans(&chars);
    if spans.is_empty() {
        return (markdown.to_string(), Vec::new());
    }
    let mut out = String::new();
    let mut protected: Vec<(String, String)> = Vec::new();
    let mut position = 0usize;
    for (start, end, kind) in spans {
        out.push_str(&chars[position..start].iter().collect::<String>());
        match fenced_formula(&kind) {
            Some(formula) => {
                out.push_str("$$");
                out.push_str(&formula);
                out.push_str("$$");
            }
            None => {
                let token = format!("\u{1}omni-fence-{}\u{2}", protected.len());
                out.push_str(&token);
                let original: String = chars[start..end].iter().collect();
                protected.push((token, original));
            }
        }
        position = end;
    }
    out.push_str(&chars[position..].iter().collect::<String>());
    (out, protected)
}

/// 裸公式行的 token 判定（对映 `_math_like_token`）。
fn math_like_token(token: &str) -> bool {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 1 {
        return true;
    }
    if chars.iter().any(|ch| ch.is_numeric()) {
        return true;
    }
    token.contains('\\') || token.contains('^') || token.contains('_')
}

/// 对映 `_looks_like_bare_math_line`。
fn looks_like_bare_math_line(line: &str) -> bool {
    let text = line.trim();
    let length = text.chars().count();
    if !text.starts_with('\\') || !(3..=BARE_MATH_LINE_MAX_LEN).contains(&length) {
        return false;
    }
    // 以 `\(`、`\[` 或 `$` 开头的行本身带定界符，由现有公式路径处理。
    if text.starts_with("\\(") || text.starts_with("\\[") || text.starts_with('$') {
        return false;
    }
    if text
        .chars()
        .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
    {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let names = math_command_names(&chars);
    if !names
        .iter()
        .any(|name| MATH_COMMAND_HINTS.contains(&name.as_str()))
    {
        return false;
    }
    text.split_whitespace().all(math_like_token)
}

/// 对映 `_normalize_bare_math_lines`：整行裸 LaTeX 公式自动包裹为块级 `$$...$$`。
fn normalize_bare_math_lines(markdown: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_dollar = false;
    let mut in_bracket = false;
    let mut in_env = false;
    let mut in_fence = false;
    for line in markdown.split('\n') {
        let text = line.trim();
        if text.starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }
        if in_fence {
            out.push(line.to_string());
            continue;
        }
        if line.matches("$$").count() % 2 == 1 {
            in_dollar = !in_dollar;
            out.push(line.to_string());
            continue;
        }
        if line.contains("\\[") && !line.contains("\\]") {
            in_bracket = true;
            out.push(line.to_string());
            continue;
        }
        if line.contains("\\]") {
            in_bracket = false;
            out.push(line.to_string());
            continue;
        }
        if in_dollar || in_bracket {
            out.push(line.to_string());
            continue;
        }
        if line.contains("\\begin{") && !line.contains("\\end{") {
            in_env = true;
            out.push(line.to_string());
            continue;
        }
        if line.contains("\\end{") {
            in_env = false;
            out.push(line.to_string());
            continue;
        }
        if in_env {
            out.push(line.to_string());
            continue;
        }
        if looks_like_bare_math_line(line) {
            out.push(format!("$${text}$$"));
            continue;
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// 对映 `_looks_like_escaped_math`。
fn looks_like_escaped_math(body: &str) -> bool {
    let text = body.trim();
    if text.is_empty() {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let has_command = !math_command_names(&chars).is_empty();
    let has_structure = text.contains('^') || text.contains('_') || text.contains('=');
    has_command || (has_structure && text.chars().any(|ch| ch.is_alphabetic()))
}

/// 对映 `_normalize_escaped_math_dollars`：恢复模型误转义的数学美元边界。
fn normalize_escaped_math_dollars(markdown: &str) -> String {
    let chars: Vec<char> = markdown.chars().collect();
    let n = chars.len();
    let slash = '\\';
    let mut output = String::new();
    let mut index = 0usize;
    while index < n {
        if chars[index] != slash {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        let run_start = index;
        while index < n && chars[index] == slash {
            index += 1;
        }
        if index >= n || chars[index] != '$' {
            output.push_str(&chars[run_start..index].iter().collect::<String>());
            continue;
        }
        let opening = index;
        let mut closing = opening + 1;
        let mut converted = false;
        while closing < n && chars[closing] != '\n' && chars[closing] != '\r' {
            if chars[closing] == '$' {
                let mut body_end = closing;
                while body_end > opening + 1 && chars[body_end - 1] == slash {
                    body_end -= 1;
                }
                let body: String = chars[opening + 1..body_end].iter().collect();
                if looks_like_escaped_math(&body) {
                    output.push('$');
                    output.push_str(&body);
                    output.push('$');
                    index = closing + 1;
                    converted = true;
                    break;
                }
            }
            closing += 1;
        }
        if converted {
            continue;
        }
        output.push_str(&chars[run_start..opening + 1].iter().collect::<String>());
        index = opening + 1;
    }
    output
}

/// 对映 `_normalize_redundant_block_dollars`：把多余的美元定界符收敛为一对 `$$`。
fn normalize_redundant_block_dollars(markdown: &str) -> String {
    let chars: Vec<char> = markdown.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut position = 0usize;
    let mut index = 0usize;
    while index < n {
        if chars[index] == '$' {
            let run = dollar_run(&chars, index);
            if run >= 3 {
                let mut cursor = index + run;
                let mut closing: Option<(usize, usize)> = None;
                while cursor < n {
                    if chars[cursor] == '$' {
                        let close_run = dollar_run(&chars, cursor);
                        if close_run >= 3 {
                            closing = Some((cursor, close_run));
                            break;
                        }
                        cursor += close_run;
                    } else {
                        cursor += 1;
                    }
                }
                if let Some((close_start, close_run)) = closing {
                    out.push_str(&chars[position..index].iter().collect::<String>());
                    out.push_str("$$");
                    out.push_str(&chars[index + run..close_start].iter().collect::<String>());
                    out.push_str("$$");
                    position = close_start + close_run;
                    index = position;
                    continue;
                }
            }
        }
        index += 1;
    }
    out.push_str(&chars[position..n].iter().collect::<String>());
    out
}

// ── 公式片段定位（对映 `_LATEX_SPANS_RE`） ─────────────────

/// 命中一个公式片段：`(结束位置, 片段正文, 是否块级)`。
fn match_formula_span(chars: &[char], index: usize) -> Option<(usize, String, bool)> {
    // 块级 $$..$$（正文至少一个字符，取最近的闭合符）
    if starts_with(chars, index, "$$") {
        if let Some(close) = find_from(chars, index + 3, "$$") {
            return Some((close + 2, chars[index + 2..close].iter().collect(), true));
        }
    }
    // 块级 \[..\]
    if starts_with(chars, index, "\\[") {
        if let Some(close) = find_from(chars, index + 3, "\\]") {
            return Some((close + 2, chars[index + 2..close].iter().collect(), true));
        }
    }
    // 行内 $..$：起始与闭合 $ 都不得在反斜杠后，闭合后不得紧跟 $
    if chars[index] == '$' && (index == 0 || chars[index - 1] != '\\') {
        let mut close = index + 1;
        while close < chars.len() && chars[close] != '$' && chars[close] != '\n' {
            close += 1;
        }
        if close < chars.len()
            && chars[close] == '$'
            && close > index + 1
            && chars[close - 1] != '\\'
            && (close + 1 >= chars.len() || chars[close + 1] != '$')
        {
            return Some((close + 1, chars[index + 1..close].iter().collect(), false));
        }
    }
    // 行内 \(..\)
    if starts_with(chars, index, "\\(") {
        let mut close = index + 2;
        while close < chars.len() && chars[close] != '\\' && chars[close] != '\n' {
            close += 1;
        }
        if close + 1 < chars.len()
            && chars[close] == '\\'
            && chars[close + 1] == ')'
            && close >= index + 3
        {
            return Some((close + 2, chars[index + 2..close].iter().collect(), false));
        }
    }
    None
}

/// 对映 `_replace_span`。
fn render_formula_span(body: &str, is_block: bool, raw: &str) -> Result<String, ()> {
    if body.chars().count() > MAX_FORMULA_LEN {
        return Ok(raw.to_string());
    }
    let body_chars: Vec<char> = body.chars().collect();
    let converted = convert_expr(&body_chars, is_block, 0)?;
    let stripped = converted.trim();
    if is_block {
        if stripped.is_empty() {
            Ok(String::new())
        } else {
            Ok(format!("\n{stripped}\n"))
        }
    } else {
        Ok(stripped.to_string())
    }
}

/// 对映 `_LATEX_SPANS_RE.sub(_replace_span, ...)`。
fn replace_formula_spans(text: &str) -> Result<String, ()> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut position = 0usize;
    let mut index = 0usize;
    while index < n {
        if let Some((end, body, is_block)) = match_formula_span(&chars, index) {
            out.push_str(&chars[position..index].iter().collect::<String>());
            let raw: String = chars[index..end].iter().collect();
            out.push_str(&render_formula_span(&body, is_block, &raw)?);
            position = end;
            index = end;
            continue;
        }
        index += 1;
    }
    out.push_str(&chars[position..n].iter().collect::<String>());
    Ok(out)
}

// ── 对外入口 ────────────────────────────────────────────────

/// 快速判断文本是否含可拆分的块级公式。
pub fn has_block_formula(markdown: &str) -> bool {
    let normalized = normalize_bare_math_lines(markdown);
    if normalized.contains("$$") || normalized.contains("\\[") {
        return true;
    }
    let chars: Vec<char> = normalized.chars().collect();
    fence_spans(&chars)
        .iter()
        .any(|(_, _, kind)| fenced_formula(kind).is_some())
}

/// 把 Markdown 按块级公式分段，供图像渲染管线使用。
///
/// 返回 `[(text, None), (text, Some(formula)), ...]` 交替列表：块级公式
/// （`$$..$$` 与 `\[..\]`）单独成段，其余文本段仍可能含行内公式。
/// 未闭合的块级公式留在文本段，保证流式输出过程中不触发不完整公式的渲染。
pub fn split_blocks(markdown: &str) -> Vec<(String, Option<String>)> {
    let normalized = normalize_bare_math_lines(markdown);
    let chars: Vec<char> = normalized.chars().collect();
    let n = chars.len();
    let mut parts: Vec<(String, Option<String>)> = Vec::new();
    let mut position = 0usize;
    let mut index = 0usize;
    while index < n {
        let mut candidate: Option<(usize, usize, Option<String>)> = None;
        if starts_with(&chars, index, "$$") {
            if let Some(close) = find_from(&chars, index + 3, "$$") {
                candidate = Some((
                    index,
                    close + 2,
                    Some(chars[index + 2..close].iter().collect()),
                ));
            }
        }
        if candidate.is_none() && starts_with(&chars, index, "\\[") {
            if let Some(close) = find_from(&chars, index + 3, "\\]") {
                candidate = Some((
                    index,
                    close + 2,
                    Some(chars[index + 2..close].iter().collect()),
                ));
            }
        }
        if candidate.is_none() && is_line_start(&chars, index) {
            if let Some((end, kind)) = match_fence(&chars, index) {
                candidate = Some((index, end, fenced_formula(&kind)));
            }
        }
        match candidate {
            Some((start, end, formula)) => {
                index = end;
                let Some(formula) = formula else { continue };
                if formula.is_empty() || formula.chars().count() > MAX_FORMULA_LEN {
                    continue;
                }
                if start > position {
                    parts.push((chars[position..start].iter().collect(), None));
                }
                parts.push((String::new(), Some(formula)));
                position = end;
            }
            None => index += 1,
        }
    }
    if position < n {
        parts.push((chars[position..].iter().collect(), None));
    }
    if parts.is_empty() {
        vec![(normalized, None)]
    } else {
        parts
    }
}

/// 把 Markdown 文本中的 LaTeX 公式片段转换为 Unicode 可读文本。
///
/// 无公式标记时原样返回（快速路径）；任何解析失败都回退原文，
/// 保证该函数绝不影响回复内容的完整性。
pub fn latex_to_text(markdown: &str) -> String {
    if markdown.is_empty() {
        return markdown.to_string();
    }
    if !markdown.contains('$') && !markdown.contains('\\') && !has_block_formula(markdown) {
        return markdown.to_string();
    }
    match latex_to_text_inner(markdown) {
        Ok(text) => text,
        Err(()) => markdown.to_string(),
    }
}

fn latex_to_text_inner(markdown: &str) -> Result<String, ()> {
    // 数学 fenced 归一化为 $$...$$；普通代码 fenced 先替换为占位符，
    // 防止其中的 $x$ 被误当作公式，最终再完整恢复原文。
    let (mut guarded, protected_fences) = normalize_fenced_blocks(markdown);
    // 整行裸 LaTeX 公式（无定界符）自动包裹为 $$...$$；此时代码围栏已是占位符。
    guarded = normalize_bare_math_lines(&guarded);
    guarded = normalize_escaped_math_dollars(&guarded);
    guarded = guarded.replace("\\(", "$").replace("\\)", "$");
    guarded = guarded.replace("\\$", &ESCAPED_DOLLAR.to_string());
    guarded = normalize_redundant_block_dollars(&guarded);
    let mut converted = replace_formula_spans(&guarded)?;
    converted = converted.replace(ESCAPED_DOLLAR, "$");
    for (token, original) in protected_fences {
        converted = converted.replace(&token, &original);
    }
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fraction_and_sqrt_convert_to_readable_text() {
        assert_eq!(
            latex_to_text("$\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}$"),
            "(-b ± √(b² - 4ac))/2a"
        );
        assert_eq!(latex_to_text("$\\sqrt[3]{x}$"), "³√x");
    }

    #[test]
    fn super_and_subscripts_map_to_unicode() {
        assert_eq!(latex_to_text("$\\sum_{i=1}^{n} i$"), "∑ᵢ₌₁ⁿ i");
    }

    #[test]
    fn greek_and_symbols_lookup() {
        assert_eq!(latex_to_text("$\\alpha\\beta\\Gamma\\infty$"), "αβΓ∞");
    }

    #[test]
    fn unknown_command_keeps_original_text() {
        assert_eq!(latex_to_text("$\\foobar{x}$"), "\\foobar x");
    }

    #[test]
    fn matrix_environment_aligns_columns() {
        assert_eq!(
            latex_to_text("$$\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}$$"),
            "\n(\na b\nc d\n)\n"
        );
    }

    #[test]
    fn plain_text_without_math_is_untouched() {
        assert_eq!(
            latex_to_text("普通中文文本，没有公式。"),
            "普通中文文本，没有公式。"
        );
        assert_eq!(
            latex_to_text("路径 C:\\Users\\test 与 a\\b"),
            "路径 C:\\Users\\test 与 a\\b"
        );
    }

    #[test]
    fn code_fence_dollars_are_protected() {
        let text = "```python\nprice = \"$5\"\n```";
        assert_eq!(latex_to_text(text), text);
    }

    #[test]
    fn math_fence_becomes_block_formula() {
        assert_eq!(latex_to_text("```math\n\\frac{1}{2}\n```"), "\n1/2\n");
    }

    #[test]
    fn bare_math_line_is_wrapped_and_converted() {
        assert_eq!(latex_to_text("\\frac{a}{b}"), "\na/b\n");
    }

    #[test]
    fn explanatory_sentence_is_not_treated_as_math() {
        let text = "Use \\frac{a}{b} for fractions";
        assert_eq!(latex_to_text(text), text);
    }

    #[test]
    fn split_blocks_separates_block_formulas() {
        assert_eq!(
            split_blocks("前文 $$a+b$$ 后文"),
            vec![
                ("前文 ".to_string(), None),
                (String::new(), Some("a+b".to_string())),
                (" 后文".to_string(), None),
            ]
        );
    }

    #[test]
    fn has_block_formula_detects_markers() {
        assert!(has_block_formula("前文 $$a+b$$ 后文"));
        assert!(!has_block_formula("没有块级公式 $x$ 只有行内"));
    }
}
