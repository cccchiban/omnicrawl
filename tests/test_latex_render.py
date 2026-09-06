"""LaTeX → 终端可读 Unicode 数学文本转换器测试。"""

from __future__ import annotations

import unittest

from omnicrawl.ui.fullscreen.rendering.latex import has_block_formula, latex_to_text, split_blocks


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

    def test_escaped_math_dollars_are_normalized(self) -> None:
        # 某些模型/Markdown 转义层会把数学边界输出成 \\$...$ 或 \\$...\\$。
        self.assertEqual(latex_to_text(r"\$E = mc^2$"), "E = mc²")
        self.assertEqual(latex_to_text(r"\$E = mc^2\$"), "E = mc²")

    def test_double_escaped_math_dollars_are_normalized(self) -> None:
        self.assertEqual(latex_to_text(r"\\$E = mc^2\\$"), "E = mc²")

    def test_escaped_dollar_price_is_still_literal(self) -> None:
        self.assertEqual(latex_to_text(r"价格是 \$5，不是数学公式"), "价格是 $5，不是数学公式")

    def test_unclosed_dollar_is_kept_as_is(self) -> None:
        self.assertEqual(latex_to_text("未闭合 $x^2 原样"), "未闭合 $x^2 原样")

    def test_bare_frac_line_is_converted(self) -> None:
        # 模型按“只输出公式”指令给出无定界符的裸 LaTeX 行，也要能转换
        result = latex_to_text(r"\frac{-b \pm \sqrt{b^2 - 4ac}}{2a}")
        self.assertIn("√", result)
        self.assertNotIn("\\frac", result)

    def test_bare_math_line_with_prose_is_unchanged(self) -> None:
        # 含普通英文/中文词的整行不是公式，不能误转换
        self.assertEqual(
            latex_to_text(r"Use \frac{a}{b} for fractions"),
            r"Use \frac{a}{b} for fractions",
        )
        self.assertEqual(
            latex_to_text(r"\frac{a}{b} 表示分数"),
            r"\frac{a}{b} 表示分数",
        )

    def test_bare_math_line_inside_dollar_block_not_double_wrapped(self) -> None:
        # $$ 块内的 \frac 行不能被再次包裹成 $$...$$
        result = latex_to_text("$$\n\\frac{a}{b}\n$$")
        self.assertIn("a/b", result)
        self.assertNotIn("$$", result)

    def test_bare_math_line_inside_code_fence_not_wrapped(self) -> None:
        # 代码围栏中的 \frac 行必须原样保留
        markdown = "```python\n\\frac{a}{b} = 1\n```"
        result = latex_to_text(markdown)
        self.assertIn("\\frac{a}{b} = 1", result)

    def test_bare_math_line_inside_align_env_not_wrapped(self) -> None:
        # 环境体内的行不能被裸公式包裹破坏（否则 aligned 会错乱）
        markdown = "\\begin{aligned}\n\\frac{a}{b} &= c \\\\ \n\\end{aligned}"
        result = latex_to_text(markdown)
        self.assertNotIn("$$\\frac{a}{b}", result)

    def test_text_command_and_accents(self) -> None:
        # \text{...} 内容原样保留（含尾部空格），与 LaTeX 语义一致
        self.assertEqual(latex_to_text(r"$\text{if } x > 0$"), "if  x > 0")
        self.assertEqual(latex_to_text(r"$\hat{x} + \bar{y}$"), "x + y")

    def test_matrix_environment_degrades_to_rows(self) -> None:
        result = latex_to_text(
            r"$$\begin{pmatrix} a & b \\ c & d \end{pmatrix}$$"
        )
        # 矩阵转多行对齐文本，外层带圆括号；\\ → 换行、& → 列分隔
        self.assertIn("(\na b\nc d\n)", result)

    def test_matrix_aligns_columns_and_keeps_nested_commands(self) -> None:
        result = latex_to_text(
            r"$$\begin{matrix} 1 & \frac{1}{2} \\ x^2 & \alpha \end{matrix}$$"
        )
        self.assertIn("1  1/2", result)
        self.assertIn("x² α", result)

    def test_cases_environment_keeps_rows(self) -> None:
        result = latex_to_text(
            r"$$\begin{cases} x + y = 1 \\ x - y = 2 \end{cases}$$"
        )
        self.assertIn("x + y = 1", result)
        self.assertIn("x - y = 2", result)

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


class SplitBlocksTest(unittest.TestCase):
    """块级公式分段：供图像渲染管线使用的边界行为。"""

    def test_separates_closed_block_formulas(self) -> None:
        parts = split_blocks("开头 $x^2$ 行内，然后 $$\\frac{a}{b}$$ 块级")
        self.assertEqual(
            parts,
            [
                ("开头 $x^2$ 行内，然后 ", None),
                ("", "\\frac{a}{b}"),
                (" 块级", None),
            ],
        )

    def test_bare_frac_line_detected_as_block(self) -> None:
        formula = r"\frac{-b \pm \sqrt{b^2 - 4ac}}{2a}"
        self.assertTrue(has_block_formula(formula))
        self.assertEqual(split_blocks(formula), [("", formula)])

    def test_bare_math_prose_not_detected_as_block(self) -> None:
        self.assertFalse(has_block_formula(r"Use \frac{a}{b} for fractions"))

    def test_keeps_unclosed_formula_in_text(self) -> None:
        # 流式过程中公式未闭合：留在文本段，不触发渲染
        self.assertEqual(
            split_blocks("流式中 $$\\frac{a"),
            [("流式中 $$\\frac{a", None)],
        )

    def test_multiple_and_trailing_text(self) -> None:
        self.assertEqual(
            split_blocks("A $$x$$ B $$y$$ C"),
            [("A ", None), ("", "x"), (" B ", None), ("", "y"), (" C", None)],
        )

    def test_display_brackets_are_blocks(self) -> None:
        self.assertEqual(
            split_blocks(r"前 \[x^2\] 后"),
            [("前 ", None), ("", "x^2"), (" 后", None)],
        )

    def test_latex_fenced_block_is_extracted_as_formula(self) -> None:
        markdown = r"""章节：
```latex
e^{i\pi} + 1 = 0
```
"""
        parts = split_blocks(markdown)
        self.assertEqual(
            parts,
            [("章节：\n", None), ("", r"e^{i\pi} + 1 = 0"), ("\n", None)],
        )

    def test_math_fenced_block_without_language_is_extracted_when_latex_is_obvious(self) -> None:
        markdown = r"""```
\int_{-\infty}^{+\infty} e^{-x^2} \, dx = \sqrt{\pi}
```"""
        parts = split_blocks(markdown)
        self.assertEqual(
            parts,
            [("", r"\int_{-\infty}^{+\infty} e^{-x^2} \, dx = \sqrt{\pi}")],
        )

    def test_regular_fenced_code_block_is_preserved(self) -> None:
        markdown = "```python\nvalue = x ** 2\n```"
        self.assertEqual(split_blocks(markdown), [(markdown, None)])
        self.assertEqual(latex_to_text(markdown), markdown)

    def test_latex_to_text_removes_latex_fence_and_converts_formula(self) -> None:
        markdown = r"""```tex
e^{i\pi} + 1 = 0
```"""
        result = latex_to_text(markdown)
        self.assertNotIn("```", result)
        self.assertIn("e^(iπ) + 1 = 0", result)

    def test_fenced_formula_strips_nested_inline_math_delimiters(self) -> None:
        markdown = r"""```latex
$E = mc^2$
```"""
        self.assertEqual(split_blocks(markdown), [("", "E = mc^2")])
        self.assertEqual(latex_to_text(markdown), "\nE = mc²\n")

    def test_redundant_block_dollar_wrappers_are_removed(self) -> None:
        self.assertEqual(latex_to_text(r"$$$E = mc^2$$$"), "\nE = mc²\n")

    def test_plain_latex_fence_without_delimiters_is_converted(self) -> None:
        markdown = "```latex\nE = mc^2\n```"
        self.assertEqual(latex_to_text(markdown), "\nE = mc²\n")

    def test_regular_code_fence_preserves_formula_like_text(self) -> None:
        markdown = "```python\nvalue = '$x^2$'\n```"
        self.assertEqual(latex_to_text(markdown), markdown)

    def test_single_line_latex_fence_is_converted(self) -> None:
        markdown = r"```latex \frac{-b \pm \sqrt{b^2 - 4ac}}{2a} ```"
        self.assertEqual(
            split_blocks(markdown),
            [("", r"\frac{-b \pm \sqrt{b^2 - 4ac}}{2a}")],
        )
        self.assertEqual(
            latex_to_text(markdown),
            "\n(-b ± √(b² - 4ac))/2a\n",
        )

    def test_plain_fence_with_math_delimiters_is_extracted(self) -> None:
        markdown = r"""```
$$\frac{1}{2}$$
```"""
        self.assertEqual(
            split_blocks(markdown),
            [("", r"\frac{1}{2}")],
        )

    def test_plain_text_single_part(self) -> None:
        self.assertEqual(split_blocks("无公式"), [("无公式", None)])
        self.assertEqual(split_blocks(""), [("", None)])


if __name__ == "__main__":
    unittest.main()
