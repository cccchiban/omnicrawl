"""LaTeX → 终端可读 Unicode 数学文本转换器测试。"""

from __future__ import annotations

import unittest

from omnicrawl.ui.fullscreen.latex import latex_to_text


class LatexToTextTest(unittest.TestCase):
    """转换器核心行为：行内/块级公式、符号映射、防御与降级。"""

    def test_plain_text_without_latex_is_unchanged(self) -> None:
        self.assertEqual(latex_to_text("普通文本"), "普通文本")
        self.assertEqual(latex_to_text(""), "")
        self.assertEqual(latex_to_text("仅含 $ 符号"), "仅含 $ 符号")

    def test_inline_math_converts_superscripts(self) -> None:
        self.assertEqual(
            latex_to_text("勾股定理 $a^2 + b^2 = c^2$"),
            "勾股定理 a² + b² = c²",
        )

    def test_inline_math_greek_and_blackboard(self) -> None:
        self.assertEqual(
            latex_to_text(r"$\alpha + \beta$ 与 $\mathbb{R}$"),
            "α + β 与 ℝ",
        )

    def test_block_math_gets_own_paragraph(self) -> None:
        self.assertEqual(latex_to_text(r"$$\frac{1}{2}$$"), "\n1/2\n")

    def test_display_brackets(self) -> None:
        self.assertEqual(latex_to_text(r"\[x^2\]"), "\nx²\n")
        self.assertEqual(latex_to_text(r"\(\alpha\)"), "α")

    def test_fraction_with_parens_when_needed(self) -> None:
        self.assertEqual(latex_to_text(r"$x = \frac{a+b}{c}$"), "x = (a+b)/c")
        self.assertEqual(latex_to_text(r"$\frac{1}{2}$"), "1/2")
        self.assertEqual(
            latex_to_text(r"$\frac{\frac{a}{b}}{c}$"),
            "(a/b)/c",
        )

    def test_sqrt_and_nth_root(self) -> None:
        self.assertEqual(latex_to_text(r"$\sqrt{x}$"), "√x")
        self.assertEqual(latex_to_text(r"$\sqrt{x^2 + 1}$"), "√(x² + 1)")
        self.assertEqual(latex_to_text(r"$\sqrt[3]{8}$"), "³√8")

    def test_sum_with_limits(self) -> None:
        self.assertEqual(
            latex_to_text(r"$\sum_{i=1}^{n} i$"),
            "∑ᵢ₌₁ⁿ i",
        )
        self.assertEqual(
            latex_to_text(r"$\int_a^b f(x)\,dx$"),
            "∫ₐᵇ f(x) dx",
        )
        self.assertEqual(
            latex_to_text(r"$\lim_{x \to 0} \frac{\sin x}{x}$"),
            "lim_(x → 0) sin x/x",
        )

    def test_operator_symbols(self) -> None:
        self.assertEqual(
            latex_to_text(r"$a \leq b \geq c \neq d \approx e$"),
            "a ≤ b ≥ c ≠ d ≈ e",
        )
        self.assertEqual(latex_to_text(r"$x \in \mathbb{Z}$"), "x ∈ ℤ")
        self.assertEqual(latex_to_text(r"$\pm 3$"), "± 3")

    def test_asterisk_does_not_trigger_markdown_emphasis(self) -> None:
        self.assertEqual(latex_to_text(r"$a * b$"), "a × b")

    def test_unknown_command_is_kept(self) -> None:
        self.assertEqual(latex_to_text(r"未知 $\foo{x}$"), "未知 \\foo x")

    def test_escaped_dollar_is_not_a_formula_boundary(self) -> None:
        self.assertEqual(latex_to_text(r"价格 $\$5$"), "价格 $5")
        self.assertEqual(latex_to_text(r"价格是 \$5"), "价格是 $5")

    def test_unclosed_dollar_is_kept_as_is(self) -> None:
        self.assertEqual(latex_to_text("未闭合 $x^2 原样"), "未闭合 $x^2 原样")

    def test_text_command_and_accents(self) -> None:
        # \text{...} 内容原样保留（含尾部空格），与 LaTeX 语义一致
        self.assertEqual(latex_to_text(r"$\text{if } x > 0$"), "if  x > 0")
        self.assertEqual(latex_to_text(r"$\hat{x} + \bar{y}$"), "x + y")

    def test_matrix_environment_degrades_to_rows(self) -> None:
        result = latex_to_text(
            r"$$\begin{pmatrix} a & b \\ c & d \end{pmatrix}$$"
        )
        # & → 空格（含两侧空格共 3 个），\\ → 换行
        self.assertIn("a   b", result)
        self.assertIn("c   d", result)

    def test_aligned_environment(self) -> None:
        result = latex_to_text(
            r"$$\begin{aligned} a &= b \\ c &= d \end{aligned}$$"
        )
        self.assertIn("a", result)
        self.assertIn("= b", result)

    def test_escaped_braces(self) -> None:
        # \{x\} 是集合字面量，输出 {x}（rich markdown 中无特殊含义）
        self.assertEqual(latex_to_text(r"$\{x\}$"), "{x}")

    def test_exponential(self) -> None:
        self.assertEqual(latex_to_text(r"$e^{i\pi} + 1 = 0$"), "e^(iπ) + 1 = 0")
        self.assertEqual(latex_to_text(r"$\Delta E = mc^2$"), "Δ E = mc²")

    def test_oversized_formula_is_returned_unchanged(self) -> None:
        huge = "$" + "a" * 20_000 + "$"
        self.assertEqual(latex_to_text(huge), huge)

    def test_mixed_inline_and_block(self) -> None:
        result = latex_to_text(r"$x^2$ 与 $$a = \sqrt{b^2 + c^2}$$")
        self.assertEqual(result, "x² 与 \na = √(b² + c²)\n")


if __name__ == "__main__":
    unittest.main()
