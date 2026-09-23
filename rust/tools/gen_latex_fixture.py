#!/usr/bin/env python3
"""生成 `omnicrawl-tui` LaTeX 转换层的对照数据集。

期望值来自 Python 真实现 `omnicrawl/ui/fullscreen/rendering/latex.py`：
`latex_to_text`（行内/块级/数学 fenced/裸公式 → Unicode 近似文本）、
`split_blocks`（按块级公式分段）与 `has_block_formula`（快速判定）。

`latex.py` 是零外部依赖模块，但 `omnicrawl.ui.fullscreen` 的父包 `__init__`
会拉 Textual 等运行时依赖；为避免在无 GUI 环境里导入失败，这里按文件路径直接
加载该模块，不走包导入。

用法（仓库根目录）：

    python rust/tools/gen_latex_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test latex_parity
"""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "omnicrawl/ui/fullscreen/rendering/latex.py"
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/latex_parity.json"


def load_latex_module():
    spec = importlib.util.spec_from_file_location("oc_latex_parity", MODULE_PATH)
    if spec is None or spec.loader is None:
        raise SystemExit(f"无法加载 {MODULE_PATH}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


# 非公式文本与边界输入
PLAIN_TEXTS = [
    "",
    "普通中文文本，没有公式。",
    "路径 C:\\Users\\test 与 a\\b",
    "价格 $5 到 $10 之间",
    "普通代码 `x = 1` 与 **加粗**",
    "只有单独的 $ 符号",
    "反斜杠结尾 x^",
]

# 行内公式
INLINE_TEXTS = [
    "行内公式 $x^2 + y^2 = z^2$ 结束",
    "分数 $\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}$",
    "根号 $\\sqrt{x+1}$ 与 $\\sqrt[3]{x}$",
    "求和 $\\sum_{i=1}^{n} i$ 与 $\\prod_{k=0}^{m} a_k$",
    "希腊字母 $\\alpha\\beta\\Gamma\\Omega$",
    "符号 $\\cdot \\times \\div \\leq \\geq \\neq \\approx \\infty \\in \\notin$",
    "上下标 $x^{2n}_{i,j}$ 与 $a_1 + a_2$",
    "文本样式 $\\text{速度}=\\mathrm{d}x/\\mathrm{d}t$ 与 $\\mathbf{v}$",
    "黑板体 $\\mathbb{R}^n$ 与 $\\mathbb{Z}$ 与 $\\mathbb{Q}$",
    "转义符号 $a\\%b\\&c\\#d\\_e$",
    "未知命令 $\\foobar{x}$ 结束",
    "重音 $\\hat{x} + \\bar{y} + \\vec{z}$",
    "pmod $a \\pmod{n}$",
    "三角函数 $\\sin^2\\theta + \\cos^2\\theta = 1$",
    "乘法 $a * b$ 与 $2 * 3$",
    "花括号分组 $x_{n+1} = {a+b}$",
    "孤立右花括号 $a } b$",
    "角度 $90^\\circ$ 与箭头 $A \\to B \\Rightarrow C$",
    "mod $a \\mod b$ 与 surd $\\surd$",
    "空格命令 $a \\quad b \\qquad c$",
    "换行命令 $a \\\\ b$",
    "转义美元混用 \\$ 与公式 $x$ 结束",
    "误转义公式 \\$x^2\\$ 结束",
    "多余美元 $$$x^2$$$ 结束",
    "行内括号 \\( a_i \\) 结束",
    "行内括号跨行 \\(a\nb\\) 结束",
    "大写命令集 $\\Delta\\Theta\\Lambda\\Xi\\Pi\\Sigma\\Upsilon\\Phi\\Psi$",
    "省略号与点 $\\dots \\ldots \\cdots \\vdots \\ddots$",
    "逻辑与集合 $\\land \\lor \\neg \\subset \\subseteq \\cup \\cap \\setminus$",
    "存在量词 $\\forall x \\exists y$ 与 $\\therefore \\because$",
    "特殊符号 $\\partial \\nabla \\aleph \\Re \\Im \\hbar \\ell$",
]

# 块级公式
BLOCK_TEXTS = [
    "块级公式：\n$$\n\\frac{a}{b}\n$$\n后续文字",
    "显示公式 \\[ E = mc^2 \\] 结束",
    "无穷级数 \\[ \\sum_{n=1}^{\\infty} \\frac{1}{n^2} = \\frac{\\pi^2}{6} \\]",
    "单行块级 $$\\text{面积} = \\pi r^2$$ 结束",
    "跨行块级 $$\na+b\n$$ 结束",
    "未闭合块级 $$\\frac{a}{b} 结束",
    "矩阵 $$\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}$$",
    "分段函数 $$\\begin{cases} 1 & x > 0 \\\\ 0 & x \\le 0 \\end{cases}$$",
    "对齐环境 $$\\begin{align} a &= b \\\\ c &= d \\end{align}$$",
    "小矩阵 $$\\begin{smallmatrix} 1 & 0 \\\\ 0 & 1 \\end{smallmatrix}$$",
    "方括号矩阵 $$\\begin{bmatrix} 1 \\\\ 2 \\end{bmatrix}$$",
    "行列式 $$\\begin{vmatrix} a & b \\\\ c & d \\end{vmatrix}$$",
    "双竖矩阵 $$\\begin{Vmatrix} x \\end{Vmatrix}$$",
    "花括号矩阵 $$\\begin{Bmatrix} 1 & 2 \\end{Bmatrix}$$",
    "未知环境 $$\\begin{gather} a \\\\ b \\end{gather}$$",
    "未闭合矩阵 $$\\begin{pmatrix} a & b",
    "矩阵无括号 $$\\begin{matrix} x & y \\end{matrix}$$",
]

# 数学 fenced block 与普通代码围栏
FENCED_TEXTS = [
    "数学 fenced:\n```latex\n\\frac{1}{2}\n```\n结束",
    "tex fenced:\n```tex\nE = mc^2\n```",
    "math fenced:\n```math\n\\int_0^1 x \\, dx\n```",
    "mathjax fenced:\n```mathjax\n\\alpha + \\beta\n```",
    "无语言 fenced 含公式:\n```\n\\frac{a}{b}\n```",
    "无语言 fenced 普通文本:\n```\nhello world\n```",
    "普通代码围栏含美元:\n```python\nprice = \"$5\"\nprint(price)\n```\n结束",
    "单行 fenced: ```latex \\frac{a}{b} ```",
    "单行 fenced 无语言: ```\\sqrt{2}```",
    "单行 fenced 普通: ```python pass```",
    "缩进 fenced:\n  ```latex\n  x^2\n  ```",
    "四个空格缩进 fenced:\n    ```latex\n    x^2\n    ```",
    "数学 fenced 含定界符:\n```latex\n$$a+b$$\n```",
    "空 fenced:\n```latex\n\n```",
    "未闭合 fenced:\n```latex\n\\frac{a}{b}",
]

# 裸公式行
BARE_TEXTS = [
    "裸公式行:\n\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}",
    "带解释的句子 Use \\frac{a}{b} for fractions",
    "中文裸公式行:\n\\frac{a}{b} 表示比例",
    "短裸公式:\n\\alpha",
    "代码行:\n\\begin{code}",
    "普通行以反斜杠开头但非公式:\n\\documentclass{article}",
]

# 边界与病态输入：解析失败必须回退原文，绝不丢内容
EDGE_TEXTS = [
    "$$",
    "$$$",
    "$$$$",
    "$ $",
    "[]",
    "\\[",
    "\\]",
    "\\[x\\]",
    "$a^$",
    "$a_$",
    "$^$",
    "$a^\\$",
    "$\\\\$",
    "${" + "x" * 20 + "}$",
    "${{{{x}}}}$",
    "$\\frac{1}{2}^3$",
    "$\\pmod$",
    "$\\mathbb{}$",
    "$\\sqrt[]$",
    "$\\sqrt$",
    "$\\begin{cases}x\\end{cases}$",
    "行内 $a$ 与块级 $$b$$ 混排",
    "嵌套 \\(a $b$ c\\)",
    "```latex\na^\\\n```",
    "```latex\n\\sqrt\n```",
    "```latex\n\\frac{1}{2}\n```\n```python\nx=1\n```",
    "```\n$$a$$\n```",
    "$\u4e2d\u6587$",
    "$\\text{\u4e2d\u6587}$",
    "超长: $" + "x" * 10001 + "$",
    "$$" + "y" * 10001 + "$$",
]

# split_blocks 用例
SPLIT_TEXTS = [
    "前文 $$a+b$$ 后文",
    "前文\n\\[\nc+d\n\\]\n后文",
    "前文\n```math\ne+f\n```\n后文",
    "前文\n```python\nx = 1\n```\n后文",
    "没有块级公式 $x$ 只有行内",
    "未闭合 $$\na+b",
    "多个块 $$a$$ 中间 $$b$$ 结尾",
    "裸公式块:\n\\frac{a}{b}",
    "",
]


def main() -> int:
    latex = load_latex_module()
    converter = latex.latex_to_text
    split_blocks = latex.split_blocks
    has_block = latex.has_block_formula

    texts = PLAIN_TEXTS + INLINE_TEXTS + BLOCK_TEXTS + FENCED_TEXTS + BARE_TEXTS + EDGE_TEXTS
    conversion_cases = [{"text": text, "expected": converter(text)} for text in texts]

    split_cases = [
        {
            "text": text,
            "blocks": [[part, formula] for part, formula in split_blocks(text)],
        }
        for text in SPLIT_TEXTS
    ]

    has_block_cases = [
        {"text": text, "expected": has_block(text)} for text in SPLIT_TEXTS + BLOCK_TEXTS
    ]

    data = {
        "source": "omnicrawl/ui/fullscreen/rendering/latex.py",
        "conversion": conversion_cases,
        "split_blocks": split_cases,
        "has_block_formula": has_block_cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH}（conversion {len(conversion_cases)} 例、"
        f"split_blocks {len(split_cases)}、has_block_formula {len(has_block_cases)}）"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
