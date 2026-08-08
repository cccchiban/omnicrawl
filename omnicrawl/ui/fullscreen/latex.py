"""LaTeX 数学公式 → 终端可读 Unicode 文本的轻量转换器。

终端无法渲染真正的数学排版。本模块把回复中常见的 LaTeX 数学片段
（``$...$`` 行内、``$$...$$`` 块级、``\\(...\\)``、``\\[...\\]``）转换为
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
            if name in ("begin", "end"):
                # 数学环境标记（matrix/align 等）：丢弃标记及其参数，
                # 内容里的 & → 空格、\\ → 换行由下方分支处理。
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

    if not markdown or ("$" not in markdown and "\\" not in markdown):
        return markdown
    try:
        # 先把 \$（字面美元）保护起来，防止被误认作公式边界；
        # \(..\) 是行内数学定界符，等价 $..$，归一化后统一处理。
        guarded = markdown.replace("\\$", _ESCAPED_DOLLAR)
        guarded = guarded.replace("\\(", "$").replace("\\)", "$")
        converted = _LATEX_SPANS_RE.sub(_replace_span, guarded)
        return converted.replace(_ESCAPED_DOLLAR, "$")
    except Exception:
        return markdown
