"""LaTeX 数学公式 → 终端可读 Unicode 文本的轻量转换器。

终端无法渲染真正的数学排版。本模块把回复中常见的 LaTeX 数学片段
（``$...$`` 行内、``$$...$$`` 块级、``\\(...\\)``、``\\[...\\]``，以及 `````latex``/`````tex``/`````math`` 数学 fenced block）转换为
Unicode 数学符号与上下标，例如：:

    \\frac{a}{b}  →  a/b
    \\sqrt{x}     →  √x
    x^2           →  x²
    \\sum_{i=1}^{n} →  ∑ᵢ₌₁ⁿ
    \\alpha       →  α

设计约束：
- 零外部依赖；单遍扫描，只处理公式片段，非公式文本原样复制；
- 未知命令或解析失败时保留原文（rich markdown 会原样显示 ``\\name``），
  绝不抛异常、绝不丢弃内容；
- 输出避免触发 Markdown 语法（``*`` 转乘号、上下标消费 ``^``/``_``、
  花括号分组不输出），保证公式内嵌在 Markdown 流中显示正确；
- 防御极端输入：公式片段超长或嵌套过深时直接原样返回。
"""

from __future__ import annotations

import re

# ── 常量表 ──────────────────────────────────────────────────

_GREEK = {
    "alpha": "α", "beta": "β", "gamma": "γ", "delta": "δ", "epsilon": "ϵ",
    "varepsilon": "ε", "zeta": "ζ", "eta": "η", "theta": "θ", "vartheta": "ϑ",
    "iota": "ι", "kappa": "κ", "lambda": "λ", "mu": "μ", "nu": "ν", "xi": "ξ",
    "pi": "π", "varpi": "ϖ", "rho": "ρ", "varrho": "ϱ", "sigma": "σ",
    "varsigma": "ς", "tau": "τ", "upsilon": "υ", "phi": "φ", "varphi": "φ",
    "chi": "χ", "psi": "ψ", "omega": "ω", "Gamma": "Γ", "Delta": "Δ",
    "Theta": "Θ", "Lambda": "Λ", "Xi": "Ξ", "Pi": "Π", "Sigma": "Σ",
    "Upsilon": "Υ", "Phi": "Φ", "Psi": "Ψ", "Omega": "Ω", "ell": "ℓ",
    "hbar": "ℏ", "imath": "ı", "jmath": "ȷ",
}

_SYMBOLS = {
    "cdot": "·", "times": "×", "div": "÷", "pm": "±", "mp": "∓",
    "leq": "≤", "leqq": "≤", "geq": "≥", "geqq": "≥", "neq": "≠", "ne": "≠",
    "approx": "≈", "sim": "∼", "simeq": "≃", "equiv": "≡", "propto": "∝",
    "infty": "∞", "in": "∈", "notin": "∉", "ni": "∋", "subset": "⊂",
    "subseteq": "⊆", "supset": "⊃", "supseteq": "⊇", "cup": "∪", "cap": "∩",
    "setminus": "∖", "emptyset": "∅", "varnothing": "∅", "forall": "∀",
    "exists": "∃", "nexists": "∄", "nabla": "∇", "partial": "∂",
    "to": "→", "rightarrow": "→", "leftarrow": "←", "mapsto": "↦",
    "Rightarrow": "⇒", "Leftarrow": "⇐", "Leftrightarrow": "⇔",
    "uparrow": "↑", "downarrow": "↓", "land": "∧", "lor": "∨", "neg": "¬",
    "therefore": "∴", "because": "∵", "dots": "…", "ldots": "…",
    "cdots": "⋯", "vdots": "⋮", "ddots": "⋱", "prime": "′", "degree": "°",
    "circ": "∘", "angle": "∠", "perp": "⊥", "parallel": "∥", "mid": "∣",
    "colon": ":", "backslash": "∖", "star": "∗", "ast": "∗", "oplus": "⊕",
    "ominus": "⊖", "otimes": "⊗", "odot": "⊙", "surd": "√", "aleph": "ℵ",
    "Re": "ℜ", "Im": "ℑ", "mod": "mod", "bmod": "mod",
}

# 无参数函数名：输出为名称（可后接 ^ _ 上下标）
_FUNC_NAMES = {
    "sum": "∑", "prod": "∏", "int": "∫", "iint": "∬", "iiint": "∭",
    "oint": "∮", "lim": "lim", "log": "log", "ln": "ln", "lg": "lg",
    "sin": "sin", "cos": "cos", "tan": "tan", "cot": "cot", "sec": "sec",
    "csc": "csc", "arcsin": "arcsin", "arccos": "arccos", "arctan": "arctan",
    "sinh": "sinh", "cosh": "cosh", "tanh": "tanh", "coth": "coth",
    "exp": "exp", "max": "max", "min": "min", "sup": "sup", "inf": "inf",
    "det": "det", "arg": "arg", "dim": "dim", "gcd": "gcd", "deg": "deg",
    "ker": "ker", "rank": "rank", "Pr": "Pr",
}

# 花括号/字号命令：仅影响括号样式，直接丢弃
_LEFT_RIGHT = frozenset(
    {
        "left", "right", "big", "Big", "bigg", "Bigg", "bigl", "bigr",
        "Bigl", "Bigr", "biggl", "biggr", "Biggl", "Biggr",
    }
)

# 文本样式命令：保留参数内容
_TEXT_STYLE = frozenset(
    {
        "text", "mathrm", "mathit", "mathbf", "boldsymbol", "mbox",
        "operatorname", "mathcal", "mathscr", "mathfrak", "mathsf", "mathtt",
        "textrm", "textbf", "textit",
    }
)

# \mathbb{...} 常见字母集
_BLACKBOARD = {"R": "ℝ", "Z": "ℤ", "Q": "ℚ", "N": "ℕ", "C": "ℂ", "P": "ℙ", "H": "ℍ"}

# 转义符号：\x → 字面量
_ESCAPED = {"{": "{", "}": "}", "%": "%", "$": "$", "#": "#", "&": "&", "_": "_", "|": "|"}

# Unicode 上标/下标字符（无变体的字符走 ^x / ^(...) 回退）
_SUPERSCRIPT = {
    "0": "⁰", "1": "¹", "2": "²", "3": "³", "4": "⁴", "5": "⁵",
    "6": "⁶", "7": "⁷", "8": "⁸", "9": "⁹", "+": "⁺", "-": "⁻", "=": "⁼",
    "(": "⁽", ")": "⁾", "i": "ⁱ", "n": "ⁿ", "a": "ᵃ", "b": "ᵇ", "c": "ᶜ",
    "d": "ᵈ", "e": "ᵉ", "f": "ᶠ", "g": "ᵍ", "h": "ʰ", "j": "ʲ", "k": "ᵏ",
    "l": "ˡ", "m": "ᵐ", "o": "ᵒ", "p": "ᵖ", "r": "ʳ", "s": "ˢ", "t": "ᵗ",
    "u": "ᵘ", "v": "ᵛ", "w": "ʷ", "x": "ˣ", "y": "ʸ", "z": "ᶻ",
}
_SUBSCRIPT = {
    "0": "₀", "1": "₁", "2": "₂", "3": "₃", "4": "₄", "5": "₅",
    "6": "₆", "7": "₇", "8": "₈", "9": "₉", "+": "₊", "-": "₋", "=": "₌",
    "(": "₍", ")": "₎", "a": "ₐ", "e": "ₑ", "h": "ₕ", "i": "ᵢ", "j": "ⱼ",
    "k": "ₖ", "l": "ₗ", "m": "ₘ", "n": "ₙ", "o": "ₒ", "p": "ₚ", "r": "ᵣ",
    "s": "ₛ", "t": "ₜ", "u": "ᵤ", "v": "ᵥ", "x": "ₓ",
}

# 公式片段定位：$$..$$ 与 \[..\] 跨行；$..$ 与 \(..\) 单行。
# 起始与闭合 $ 都要求不在反斜杠后，避免 \$ 转义被误认作边界。
_ESCAPED_DOLLAR = "\x00"  # \$ 的保护占位符，处理公式后还原
_LATEX_SPANS_RE = re.compile(
    r"\$\$(?P<block_math>[\s\S]+?)\$\$"
    r"|\\\[(?P<block_display>[\s\S]+?)\\\]"
    r"|(?<!\\)\$(?P<inline_math>[^$\n]+?)(?<!\\)\$(?!\$)"
    r"|\\\((?P<inline_display>[^\\\n]+?)\\\)"
)
# 数学环境：矩阵/方程组等，转多行对齐文本。
# 键为环境名，值为 (左括号, 右括号)；无括号环境留空字符串。
_MATRIX_ENVS = {
    "matrix": ("", ""),
    "smallmatrix": ("", ""),
    "pmatrix": ("(", ")"),
    "bmatrix": ("[", "]"),
    "Bmatrix": ("{", "}"),
    "vmatrix": ("|", "|"),
    "Vmatrix": ("‖", "‖"),
    "cases": ("{", ""),
}

# 仅匹配块级公式（跨行 $$..$$、\\[..\\] 与 fenced block），供 split_blocks 分段使用。
_FENCED_BLOCK_PATTERN = (
    r"^[ \t]{0,3}```[ \t]*(?P<fenced_language>[^\r\n`]*)\r?\n"
    r"(?P<fenced_body>.*?)\r?\n[ \t]{0,3}```[ \t]*(?=\r?$)"
)
# 模型有时会把短公式压成单行 fenced block：```latex \\frac{a}{b} ```。
_SINGLE_LINE_FENCED_BLOCK_PATTERN = (
    r"^[ \t]{0,3}```[ \t]*(?P<single_fenced_content>[^`\r\n]*?)"
    r"[ \t]*```[ \t]*(?=\r?$)"
)
_BLOCK_SPANS_RE = re.compile(
    r"\$\$(?P<block_math>[\s\S]+?)\$\$"
    r"|\\\[(?P<block_display>[\s\S]+?)\\\]"
    r"|" + _FENCED_BLOCK_PATTERN
    + r"|" + _SINGLE_LINE_FENCED_BLOCK_PATTERN,
    re.MULTILINE | re.DOTALL,
)
_FENCED_BLOCK_RE = re.compile(
    _FENCED_BLOCK_PATTERN + r"|" + _SINGLE_LINE_FENCED_BLOCK_PATTERN,
    re.MULTILINE | re.DOTALL,
)
_MATH_FENCE_LANGUAGES = frozenset(
    {"latex", "tex", "math", "mathjax", "katex", "equation", "equations"}
)
_MATH_COMMAND_HINTS = frozenset(
    {
        "begin", "end", "frac", "dfrac", "tfrac", "sqrt", "sum", "prod",
        "int", "iint", "iiint", "oint", "lim", "sin", "cos", "tan", "log",
        "ln", "exp", "alpha", "beta", "gamma", "delta", "theta", "lambda",
        "mu", "pi", "sigma", "phi", "omega", "Delta", "Gamma", "Pi", "Sigma",
        "Phi", "Omega", "pm", "mp", "cdot", "times", "leq", "geq", "neq",
        "approx", "infty", "in", "to", "left", "right", "text", "mathrm",
        "mathbf", "mathbb", "quad", "qquad",
    }
)
_MATH_COMMAND_RE = re.compile(r"\\([a-zA-Z]+|[,;!])")
_CMD_RE = re.compile(r"\\([a-zA-Z]+|.)")
_DIGITS_RE = re.compile(r"[0-9]+")

# 防御上限：公式片段长度与嵌套深度
_MAX_FORMULA_LEN = 10_000
_MAX_DEPTH = 16

# 需要 `(..)` 包裹的分子/分母/根号内容：长度 > 1 且含运算符
_OPERATOR_CHARS = frozenset("+-=<>/,")

# 重音/装饰命令：终端无法可靠渲染组合重音，直接丢弃命令保留参数内容
_ACCENTS = frozenset(
    {
        "hat", "bar", "vec", "dot", "ddot", "tilde", "overline",
        "underline", "widehat", "widetilde", "check", "acute", "grave",
    }
)


def _needs_parens(text: str) -> bool:
    """内容含运算结构时加括号，避免 `a+b/c` 之类的歧义。"""

    return len(text) > 1 and any(c in _OPERATOR_CHARS for c in text)


def _to_superscript(text: str) -> str:
    """上标内容转 Unicode；无法整体映射时回退为 `^x` / `^(..)`。"""

    if len(text) == 1:
        return _SUPERSCRIPT.get(text, "^" + text)
    mapped = "".join(_SUPERSCRIPT.get(c, "") for c in text)
    return mapped if len(mapped) == len(text) else f"^({text})"


def _to_subscript(text: str) -> str:
    """下标内容转 Unicode；无法整体映射时回退为 `_x` / `_(..)`。"""

    if len(text) == 1:
        return _SUBSCRIPT.get(text, "_" + text)
    mapped = "".join(_SUBSCRIPT.get(c, "") for c in text)
    return mapped if len(mapped) == len(text) else f"_({text})"


def _read_group(text: str, index: int) -> tuple[str, int]:
    """读一个 LaTeX 参数：优先 `{...}` 平衡组，否则单个 token。

    返回 ``(参数内容, 结束位置)``；参数内容不含外层花括号。
    """

    n = len(text)
    while index < n and text[index] == " ":
        index += 1
    if index < n and text[index] == "{":
        depth = 1
        index += 1
        start = index
        while index < n and depth:
            ch = text[index]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
            index += 1
        return text[start : index - 1], index
    if index >= n:
        return "", index
    if text[index] == "\\":
        match = _CMD_RE.match(text, index)
        assert match is not None
        return match.group(0), match.end()
    match = _DIGITS_RE.match(text, index)
    if match is not None:
        return match.group(0), match.end()
    return text[index], index + 1


def _convert_matrix_env(
    text: str,
    index: int,
    env: str,
    *,
    block: bool,
    depth: int,
) -> tuple[str, int]:
    """把 begin{env} 之后的矩阵内容转换到匹配的 \\end{env}。

    返回 ``(渲染文本, 结束位置)``；单元格递归转换（支持嵌套命令），
    行与列按内容宽度对齐；环境未闭合时返回空串并停在 begin 之后，
    由外层扫描继续处理（绝不抛异常）。
    """

    if depth > _MAX_DEPTH:
        return "", index
    end_marker = f"\\end{{{env}}}"
    end_pos = text.find(end_marker, index)
    if end_pos == -1:
        # 未闭合矩阵：丢弃 begin 标记继续原扫描，避免卡死。
        return "", index
    content = text[index:end_pos]
    left, right = _MATRIX_ENVS[env]
    # 按行拆分（\\ 换行），每行按 & 拆分单元格。
    rows = [
        [
            _convert_expr(cell.strip(), block=block, depth=depth + 1)
            for cell in row.split("&")
        ]
        for row in content.split(r"\\")
    ]
    next_index = end_pos + len(end_marker)
    if not rows:
        return left + right, next_index
    # 列对齐：按各列最大宽度左对齐，列间以单空格分隔。
    column_count = max(len(row) for row in rows)
    widths = [
        max(
            (len(row[column]) if column < len(row) else 0 for row in rows),
            default=0,
        )
        for column in range(column_count)
    ]
    lines = []
    for row in rows:
        cells = [
            row[column].ljust(widths[column]) if column < len(row) else " " * widths[column]
            for column in range(column_count)
        ]
        lines.append(" ".join(cells).rstrip())
    body = "\n".join(lines)
    if left:
        return f"{left}\n{body}\n{right}", next_index
    return body, next_index


def _looks_like_latex_fence(language: str, body: str) -> bool:
    """判断 fenced 内容是否应按数学公式处理。"""

    normalized_language = language.strip().lower().split(None, 1)[0] if language.strip() else ""
    if normalized_language in _MATH_FENCE_LANGUAGES:
        return True
    if normalized_language:
        return False
    if "$$" in body or "\\[" in body or "\\(" in body:
        return True
    commands = {match.group(1) for match in _MATH_COMMAND_RE.finditer(body)}
    return bool(commands & _MATH_COMMAND_HINTS)


def _unwrap_fenced_formula(body: str) -> str:
    """去掉数学 fenced 外层及其内部重复的显示/行内定界符。"""

    formula = _normalize_escaped_math_dollars(body.strip())
    if formula.startswith("$$") and formula.endswith("$$"):
        return formula[2:-2].strip()
    if formula.startswith("\\[") and formula.endswith("\\]"):
        return formula[2:-2].strip()
    if formula.startswith("\\(") and formula.endswith("\\)"):
        return formula[2:-2].strip()
    if formula.startswith("$") and formula.endswith("$") and "\n" not in formula:
        return formula[1:-1].strip()
    return formula


def _split_single_line_fenced_content(content: str) -> tuple[str, str]:
    """拆分单行 fenced 内容中的语言标签和公式正文。"""

    content = content.strip()
    if not content:
        return "", ""
    first, separator, rest = content.partition(" ")
    if not separator:
        return "", content
    if first.strip().lower() in _MATH_FENCE_LANGUAGES:
        return first, rest.strip()
    return "", content


def _fenced_formula_from_match(match: re.Match[str]) -> str | None:
    """从 fenced 匹配中提取数学公式；普通代码围栏返回 None。"""

    groups = match.groupdict()
    language = groups.get("fenced_language") or ""
    body = groups.get("fenced_body")
    if body is None:
        language, body = _split_single_line_fenced_content(
            groups.get("single_fenced_content") or ""
        )
    if not _looks_like_latex_fence(language, body or ""):
        return None
    formula = _unwrap_fenced_formula(body or "")
    return formula if formula and len(formula) <= _MAX_FORMULA_LEN else None


def _normalize_fenced_blocks(markdown: str) -> tuple[str, dict[str, str]]:
    """把数学 fenced 归一化，并保护普通代码围栏免受公式替换影响。"""

    protected: dict[str, str] = {}

    def replace(match: re.Match[str]) -> str:
        formula = _fenced_formula_from_match(match)
        if formula is None:
            token = f"\x01omni-fence-{len(protected)}\x02"
            protected[token] = match.group(0)
            return token
        return f"$${formula}$$"

    return _FENCED_BLOCK_RE.sub(replace, markdown), protected


# 裸公式行自动识别的限制：仅处理整行、以反斜杠命令开头、长度有限的文本
_BARE_MATH_LINE_MAX_LEN = 400


def _math_like_token(token: str) -> bool:
    """空格分隔的 token 是否属于数学片段。

    含数字、反斜杠命令、上/下标标记的 token 视为数学片段；连续
    2+ 个纯字母（无任何数学特征）视为普通英文词，说明整行是正文
    而非公式。
    """

    if len(token) <= 1:
        return True
    if any(ch.isdigit() for ch in token):
        return True
    if "\\" in token or "^" in token or "_" in token:
        return True
    return False


def _looks_like_bare_math_line(line: str) -> bool:
    """判断一行是否为需要自动识别的裸 LaTeX 公式。

    模型有时按“只输出公式”的指令给出无定界符（无 ``$$``、无 fenced）
    的整行公式，如 ``\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}``。识别条件：

    - 以反斜杠命令开头（排除普通英文/代码行），长度受限；
    - 含至少一个已知数学命令（frac/sqrt/sum/pm 等）；
    - 不含中日韩文字（正文通常是中文）；
    - 空格分隔的 token 均为数学片段，排除 ``Use \\frac{a}{b} for
      fractions`` 这类带解释的句子。
    """

    s = line.strip()
    if not s.startswith("\\") or len(s) < 3 or len(s) > _BARE_MATH_LINE_MAX_LEN:
        return False
    # 以 \(、\[ 或 $ 开头的行本身带定界符，由现有公式路径处理，
    # 不得再当作裸公式包裹（否则 \(..\) 会变成块级导致换行）。
    if s.startswith(("\\(", "\\[", "$")):
        return False
    if any("\u4e00" <= ch <= "\u9fff" for ch in s):
        return False
    commands = {match.group(1) for match in _MATH_COMMAND_RE.finditer(s)}
    if not (commands & _MATH_COMMAND_HINTS):
        return False
    return all(_math_like_token(token) for token in s.split())


def _normalize_bare_math_lines(markdown: str) -> str:
    """把整行裸 LaTeX 公式自动包裹成块级 ``$$...$$``。

    逐行扫描并跟踪状态，跳过以下区域内的裸公式识别，避免破坏已有
    结构：``$$..$$`` 块、``\\[..\\]`` 块、``\\begin{..}..\\end{..}`` 环境
    以及 ``` 代码围栏。被包裹的行保持独立成块（``$$行$$``），供
    ``split_blocks`` 提取或 ``latex_to_text`` 转换。
    """

    lines = markdown.split("\n")
    out: list[str] = []
    in_dollar = False  # $$..$$ 块内（不含定界行本身）
    in_bracket = False  # \[..\] 块内
    in_env = False  # \begin{..}..\end{..} 环境内
    in_fence = False  # ``` 代码围栏内
    for line in lines:
        s = line.strip()
        if s.startswith("```"):
            in_fence = not in_fence
            out.append(line)
            continue
        if in_fence:
            out.append(line)
            continue
        if line.count("$$") % 2 == 1:
            in_dollar = not in_dollar
            out.append(line)
            continue
        if "\\[" in line and "\\]" not in line:
            in_bracket = True
            out.append(line)
            continue
        if "\\]" in line:
            in_bracket = False
            out.append(line)
            continue
        if in_dollar or in_bracket:
            out.append(line)
            continue
        if "\\begin{" in line and "\\end{" not in line:
            in_env = True
            out.append(line)
            continue
        if "\\end{" in line:
            in_env = False
            out.append(line)
            continue
        if in_env:
            out.append(line)
            continue
        if _looks_like_bare_math_line(line):
            out.append(f"$${s}$$")
            continue
        out.append(line)
    return "\n".join(out)


def has_block_formula(markdown: str) -> bool:
    """快速判断文本是否含可拆分的块级公式。"""

    normalized = _normalize_bare_math_lines(markdown)
    if "$$" in normalized or "\\[" in normalized:
        return True
    return any(
        _fenced_formula_from_match(match) is not None
        for match in _FENCED_BLOCK_RE.finditer(normalized)
    )


def split_blocks(markdown: str) -> list[tuple[str, str | None]]:
    """把 Markdown 按块级公式分段，供图像渲染管线使用。

    返回 ``[(text, None), (text, formula), ...]`` 交替列表：块级公式
    （$$..$$ 与 \\[..\\]）单独成段，其余文本段仍可能含行内公式
    （由 ``latex_to_text`` 处理）。未闭合的块级公式留在文本段，
    保证流式输出过程中不触发不完整公式的渲染。
    """

    parts: list[tuple[str, str | None]] = []
    position = 0
    markdown = _normalize_bare_math_lines(markdown)
    for match in _BLOCK_SPANS_RE.finditer(markdown):
        formula = match.group("block_math") or match.group("block_display")
        if (
            match.groupdict().get("fenced_body") is not None
            or match.groupdict().get("single_fenced_content") is not None
        ):
            formula = _fenced_formula_from_match(match)
            if formula is None:
                continue
        if not formula or len(formula) > _MAX_FORMULA_LEN:
            continue
        if match.start() > position:
            parts.append((markdown[position : match.start()], None))
        parts.append(("", formula))
        position = match.end()
    if position < len(markdown):
        parts.append((markdown[position:], None))
    return parts or [(markdown, None)]


def _convert_expr(text: str, *, block: bool, depth: int = 0) -> str:
    """把单个公式片段转换为 Unicode 可读文本（单遍扫描）。"""

    if depth > _MAX_DEPTH or len(text) > _MAX_FORMULA_LEN:
        return text
    out: list[str] = []
    index = 0
    n = len(text)
    while index < n:
        ch = text[index]
        if ch == "\\":
            match = _CMD_RE.match(text, index)
            if match is None:  # 孤立的反斜杠
                out.append("\\")
                index += 1
                continue
            name = match.group(1)
            index = match.end()
            if name == "begin":
                env, index = _read_group(text, index)
                if env in _MATRIX_ENVS:
                    # 矩阵/方程组环境：转多行对齐文本（单元格可含嵌套命令）。
                    rendered, index = _convert_matrix_env(
                        text, index, env, block=block, depth=depth
                    )
                    out.append(rendered)
                    continue
                # 其他环境（align/gather 等）：丢弃 begin 标记，内容里的
                # & → 空格、\\ → 换行由下方分支处理。
                continue
            if name == "end":
                # 丢弃 end 标记及其环境名；矩阵内容的 \\ 已被矩阵函数消费。
                _, index = _read_group(text, index)
                continue
            if name in _LEFT_RIGHT:
                continue
            if name in _GREEK:
                out.append(_GREEK[name])
                continue
            if name in _SYMBOLS:
                out.append(_SYMBOLS[name])
                continue
            if name in _FUNC_NAMES:
                out.append(_FUNC_NAMES[name])
                continue
            if name == "pmod":
                arg, index = _read_group(text, index)
                out.append(f"(mod {_convert_expr(arg, block=block, depth=depth + 1)})")
                continue
            if name == "mathbb":
                arg, index = _read_group(text, index)
                out.append(_BLACKBOARD.get(arg, arg))
                continue
            if name in _ACCENTS:
                arg, index = _read_group(text, index)
                out.append(_convert_expr(arg, block=block, depth=depth + 1))
                continue
            if name in _TEXT_STYLE:
                arg, index = _read_group(text, index)
                out.append(_convert_expr(arg, block=block, depth=depth + 1))
                continue
            if name in ("frac", "dfrac", "tfrac"):
                numerator, index = _read_group(text, index)
                denominator, index = _read_group(text, index)
                num = _convert_expr(numerator, block=block, depth=depth + 1)
                den = _convert_expr(denominator, block=block, depth=depth + 1)
                num = f"({num})" if _needs_parens(num) else num
                den = f"({den})" if _needs_parens(den) else den
                out.append(f"{num}/{den}")
                continue
            if name == "sqrt":
                root: str | None = None
                if index < n and text[index] == "[":
                    close = text.find("]", index)
                    if close != -1:
                        root = text[index + 1 : close]
                        index = close + 1
                arg, index = _read_group(text, index)
                body = _convert_expr(arg, block=block, depth=depth + 1)
                body = f"({body})" if _needs_parens(body) else body
                prefix = f"{_to_superscript(root)}√" if root else "√"
                out.append(f"{prefix}{body}")
                continue
            if name in _ESCAPED:
                out.append(_ESCAPED[name])
                continue
            if name == "\\":
                # 换行命令：块级公式里是显式换行，行内降级为空格
                out.append("\n" if block else " ")
                continue
            if name in (" ", ",", ";", "!", "quad", "qquad", "enspace", "thinspace"):
                out.append(" ")
                continue
            # 未知命令：保留原文（rich markdown 会原样显示），
            # 并补一个空格避免与后续字符粘连（如 \foox 被误读）。
            out.append(match.group(0) + " ")
            continue
        if ch == "{":
            # 花括号分组：递归转换内容，不输出括号本身
            depth_count = 1
            index += 1
            start = index
            while index < n and depth_count:
                if text[index] == "{":
                    depth_count += 1
                elif text[index] == "}":
                    depth_count -= 1
                index += 1
            inner = text[start : index - 1]
            out.append(_convert_expr(inner, block=block, depth=depth + 1))
            continue
        if ch == "}":
            out.append("}")  # 防御：孤立的右花括号原样输出
            index += 1
            continue
        if ch == "^":
            arg, index = _read_group(text, index + 1)
            out.append(_to_superscript(_convert_expr(arg, block=block, depth=depth + 1)))
            continue
        if ch == "_":
            arg, index = _read_group(text, index + 1)
            out.append(_to_subscript(_convert_expr(arg, block=block, depth=depth + 1)))
            continue
        if ch in ("&", "~"):
            out.append(" ")
            index += 1
            continue
        if ch == "*":
            # 避免触发 Markdown 强调，数学中的 * 即乘号
            out.append("×")
            index += 1
            continue
        out.append(ch)
        index += 1
    return "".join(out)



def _looks_like_escaped_math(body: str) -> bool:
    """判断被转义美元包围的内容是否明显是数学，而不是金额文本。"""

    text = body.strip()
    if not text:
        return False
    has_command = bool(_MATH_COMMAND_RE.search(text))
    has_structure = any(mark in text for mark in ("^", "_", "="))
    return has_command or (has_structure and any(character.isalpha() for character in text))


def _normalize_escaped_math_dollars(markdown: str) -> str:
    """恢复模型误转义的数学美元边界，同时保留普通美元文本。"""

    output: list[str] = []
    slash = chr(92)
    index = 0
    length = len(markdown)
    while index < length:
        if markdown[index] != slash:
            output.append(markdown[index])
            index += 1
            continue

        run_start = index
        while index < length and markdown[index] == slash:
            index += 1
        if index >= length or markdown[index] != "$":
            output.append(markdown[run_start:index])
            continue

        opening = index
        closing = opening + 1
        converted = False
        while closing < length and ord(markdown[closing]) not in (10, 13):
            if markdown[closing] == "$":
                body_end = closing
                while body_end > opening + 1 and markdown[body_end - 1] == slash:
                    body_end -= 1
                body = markdown[opening + 1 : body_end]
                if _looks_like_escaped_math(body):
                    output.append("$" + body + "$")
                    index = closing + 1
                    converted = True
                    break
            closing += 1
        if converted:
            continue
        output.append(markdown[run_start : opening + 1])
        index = opening + 1
    return "".join(output)


def _normalize_redundant_block_dollars(markdown: str) -> str:
    """把多余的美元定界符收敛为一对块级 ``$$``。"""

    pattern = re.compile(r"\${3,}(?P<body>[\s\S]+?)\${3,}")
    return pattern.sub(lambda match: f"$${match.group('body')}$$", markdown)

def _replace_span(match: re.Match[str]) -> str:
    block = match.group("block_math") or match.group("block_display")
    if block is not None:
        if len(block) > _MAX_FORMULA_LEN:
            return match.group(0)  # 超长公式原样保留，不截断边界符
        body = _convert_expr(block, block=True).strip()
        return f"\n{body}\n" if body else ""
    inline = match.group("inline_math") or match.group("inline_display")
    if len(inline) > _MAX_FORMULA_LEN:
        return match.group(0)
    return _convert_expr(inline, block=False).strip()


def latex_to_text(markdown: str) -> str:
    """把 Markdown 文本中的 LaTeX 公式片段转换为 Unicode 可读文本。

    无公式标记时原样返回（快速路径）；任何解析异常都回退原文，
    保证该函数绝不影响回复内容的完整性。
    """

    if not markdown:
        return markdown
    if (
        "$" not in markdown
        and "\\" not in markdown
        and not has_block_formula(markdown)
    ):
        return markdown
    try:
        # 数学 fenced 归一化为 $$...$$；普通代码 fenced 先替换为占位符，
        # 防止其中的 $x$ 被误当作公式，最终再完整恢复原文。
        guarded, protected_fences = _normalize_fenced_blocks(markdown)
        # 整行裸 LaTeX 公式（无定界符）自动包裹为 $$...$$；此时代码围栏
        # 已是占位符，其内容不会被误识别。
        guarded = _normalize_bare_math_lines(guarded)
        guarded = _normalize_escaped_math_dollars(guarded)
        guarded = guarded.replace("\\(", "$").replace("\\)", "$")
        guarded = guarded.replace(chr(92) + "$", _ESCAPED_DOLLAR)
        guarded = _normalize_redundant_block_dollars(guarded)
        converted = _LATEX_SPANS_RE.sub(_replace_span, guarded)
        converted = converted.replace(_ESCAPED_DOLLAR, "$")
        for token, original in protected_fences.items():
            converted = converted.replace(token, original)
        return converted
    except Exception:
        return markdown
