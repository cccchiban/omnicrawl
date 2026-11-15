//! 二维公式排版：把 `\frac` 与 `\sqrt` 排成字符网格上的上下结构。
//!
//! [`super::latex`] 的一维转换把 `\frac{a}{b}` 压成 `a/b`、`\sqrt{x}` 压成 `√x`——
//! 在行内够用，但块级公式（`$$..$$`）值得更像真排版。这一层在字符网格上堆行：
//!
//! ```text
//!    b² - 4ac
//! √ ─────────
//!      2a
//! ```
//!
//! 布局模型是**盒子**：一组等宽的行 + 一条基线（`baseline`，第几行是「中线」）。
//! 拼接时按基线对齐，因此分子分母、根号内外都能自然对齐。
//!
//! 只处理二维结构；叶子节点仍交给 [`super::latex`] 的一维转换，所以希腊字母、
//! 上下标等既有能力不受影响。遇到任何不认识的结构都退回一维，绝不丢内容。

/// 排版后的盒子：等宽的行、共用的基线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MathBox {
    lines: Vec<String>,
    /// 基线所在行号：拼接时各盒子的这一行对齐。
    baseline: usize,
    /// 显示宽度（按字符计；调用方会再按终端列宽校验）。
    width: usize,
}

impl MathBox {
    /// 单行盒子。
    pub fn text(value: &str) -> Self {
        let value = value.to_string();
        let width = display_width(&value);
        Self {
            lines: vec![value],
            baseline: 0,
            width,
        }
    }

    /// 多行盒子（外部已排好），基线取中间行。
    pub fn block(lines: Vec<String>) -> Self {
        let width = lines.iter().map(|line| display_width(line)).max().unwrap_or(0);
        let baseline = lines.len().saturating_sub(1) / 2;
        Self {
            lines,
            baseline,
            width,
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.lines.len()
    }

    pub fn baseline(&self) -> usize {
        self.baseline
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() || self.width == 0
    }

    /// 右侧接一个盒子，按基线对齐；矮的一侧补空行。
    pub fn beside(&self, right: &MathBox) -> MathBox {
        let above = self.baseline.max(right.baseline);
        let below = (self.height().saturating_sub(self.baseline))
            .max(right.height().saturating_sub(right.baseline));
        let total = above + below;

        // 各自先摊成 `total` 行（按对齐后的基线填空白），再逐行拼接：
        // 逐行偏移版本容易把分数线错位，摊平后简单可靠。
        let left_rows = self.aligned_rows(total, above);
        let right_rows = right.aligned_rows(total, above);
        let lines = left_rows
            .into_iter()
            .zip(right_rows)
            .map(|(left, right)| format!("{left}{right}"))
            .collect();
        MathBox {
            lines,
            baseline: above,
            width: self.width + right.width,
        }
    }

    /// 摊成 `total` 行：内容按 `baseline_row` 落位，其余补满 `width` 宽的空行。
    fn aligned_rows(&self, total: usize, baseline_row: usize) -> Vec<String> {
        let offset = baseline_row as isize - self.baseline as isize;
        (0..total)
            .map(|row| {
                let want = row as isize - offset;
                let text = if want >= 0 && (want as usize) < self.lines.len() {
                    self.lines[want as usize].as_str()
                } else {
                    ""
                };
                let fill = self.width.saturating_sub(display_width(text));
                format!("{text}{}", " ".repeat(fill))
            })
            .collect()
    }

    /// 水平居中补白到 `width`（只补右侧与左侧各一半，保持基线）。
    pub fn pad_to(&self, width: usize, center: bool) -> MathBox {
        if width <= self.width {
            return self.clone();
        }
        let pad = width - self.width;
        let (left, _right) = if center {
            (pad / 2, pad - pad / 2)
        } else {
            (0, pad)
        };
        let lines = self
            .lines
            .iter()
            .map(|line| {
                let fill = width.saturating_sub(display_width(line));
                if left > 0 {
                    format!("{}{}{}", " ".repeat(left), line, " ".repeat(fill.saturating_sub(left)))
                } else {
                    format!("{line}{}", " ".repeat(fill))
                }
            })
            .collect();
        MathBox {
            lines,
            baseline: self.baseline,
            width,
        }
    }

    /// 竖直方向拼接（上下叠放），基线落在下方盒子的基线上。
    pub fn above(&self, bottom: &MathBox) -> MathBox {
        let _total = self.height() + bottom.height();
        let mut lines = self.lines.clone();
        lines.extend(bottom.lines.iter().cloned());
        MathBox {
            lines,
            baseline: self.height() + bottom.baseline,
            width: self.width.max(bottom.width),
        }
    }
}

/// 显示宽度：CJK 与全角按 2 列计，其余按 1。
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| if is_wide(ch) { 2 } else { 1 })
        .sum()
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD)
}

/// 分数：横线上分子居中、线下分母居中。
pub fn fraction(numerator: &MathBox, denominator: &MathBox) -> MathBox {
    let width = numerator.width.max(denominator.width);
    let numerator = numerator.pad_to(width, true);
    let denominator = denominator.pad_to(width, true);
    let mut lines: Vec<String> = Vec::new();
    lines.extend(numerator.lines.iter().cloned());
    lines.push("─".repeat(width.max(1)));
    lines.extend(denominator.lines.iter().cloned());
    MathBox {
        baseline: numerator.height(), // 基线压在分数线所在那一行
        width: width.max(1),
        lines,
    }
}

/// 根号：横线罩住被开方数，左侧 `√` 与内容的中线对齐；`index` 为 n 次方根。
pub fn sqrt(content: &MathBox, index: Option<&MathBox>) -> MathBox {
    let body_width = content.width;
    // 第一行：`√` 上方留空 + 横线；横线宽度与被开方数一致。
    let mut lines: Vec<String> = Vec::with_capacity(content.height() + 1);
    lines.push(format!("  {}", "─".repeat(body_width.max(1))));
    // 内容各行：左侧 `√` 只在基线行出现，其余行补空白。
    for (row, line) in content.lines.iter().enumerate() {
        let prefix = if row == content.baseline { "√ " } else { "  " };
        let fill = body_width.saturating_sub(display_width(line));
        lines.push(format!("{prefix}{line}{}", " ".repeat(fill)));
    }
    let mut boxed = MathBox {
        // 基线下移到内容首行之后（横线之上的根号顶端不占基线）。
        baseline: content.baseline + 1,
        width: body_width + 2,
        lines,
    };
    // n 次方根：把小索引标在根号左上角（横线之上、根号左端）。
    if let Some(index) = index {
        let head = index.lines.first().cloned().unwrap_or_default();
        let head_width = display_width(&head);
        let mut with_index: Vec<String> = Vec::with_capacity(boxed.height() + 1);
        // 索引单独占一行，压在横线左侧上方；横线行再左移补位。
        with_index.push(format!("{head}"));
        for (row, line) in boxed.lines.iter().enumerate() {
            let pad = head_width.saturating_sub(1);
            if row == 0 {
                with_index.push(format!("{}{}", " ".repeat(pad), line));
            } else {
                with_index.push(format!("{}{}", " ".repeat(head_width), line));
            }
        }
        boxed = MathBox {
            lines: with_index,
            // 原基线整体下移一行（多出的索引行在其上方）。
            baseline: boxed.baseline + 1,
            width: boxed.width + head_width,
        };
    }
    boxed
}

/// 把一段 LaTeX 表达式构建成盒子；遇到不认识的结构退回一维文本。
///
/// 只识别 `\frac` / `\dfrac` / `\tfrac` 与 `\sqrt`；其余内容（希腊字母、上下标等）
/// 整段交给一维转换，因此能力只增不减。`width` 是可用列宽：排出来超过它时
/// 返回 `None`，由调用方退回一维，避免把正文挤破。
pub fn layout(expr: &str, width: usize) -> Option<MathBox> {
    let chars: Vec<char> = expr.chars().collect();
    let mut index = 0usize;
    let mut acc: Option<MathBox> = None;

    // 把一段一维文本接到已排好的盒子右边。
    let push_text = |acc: &mut Option<MathBox>, text: &str| {
        if text.is_empty() {
            return;
        }
        let piece = MathBox::text(&super::latex::convert_inline(text));
        *acc = Some(match acc.take() {
            Some(left) => left.beside(&piece),
            None => piece,
        });
    };

    while index < chars.len() {
        // 找到下一个二维结构的位置；中途内容按一维累积。
        let mut start = index;
        let mut found: Option<(usize, String)> = None;
        while index < chars.len() {
            match super::latex::match_cmd(&chars, index) {
                Some((end, _full, name))
                    if name == "frac"
                        || name == "dfrac"
                        || name == "tfrac"
                        || name == "sqrt" =>
                {
                    found = Some((end, name));
                    break;
                }
                Some((end, _full, _)) => index = end,
                None => index += 1,
            }
        }
        // 二维结构之前的一维片段（含非公式命令，交给一维转换）。
        if start < index {
            push_text(&mut acc, &chars[start..index].iter().collect::<String>());
            start = index;
        }
        let Some((end, name)) = found else {
            break;
        };
        let _ = start;
        index = end;
        let built = match name.as_str() {
            "frac" | "dfrac" | "tfrac" => {
                let (numerator, next) = super::latex::read_group(&chars, index).ok()?;
                let (denominator, next2) = super::latex::read_group(&chars, next).ok()?;
                index = next2;
                let num = layout(&numerator, width)?;
                let den = layout(&denominator, width)?;
                fraction(&num, &den)
            }
            "sqrt" => {
                // `\sqrt[3]{x}`：可选方括号根指数。
                let mut cursor = index;
                while cursor < chars.len() && chars[cursor] == ' ' {
                    cursor += 1;
                }
                let mut root_index: Option<MathBox> = None;
                if cursor < chars.len() && chars[cursor] == '[' {
                    let mut close = cursor + 1;
                    while close < chars.len() && chars[close] != ']' {
                        close += 1;
                    }
                    if close < chars.len() {
                        let inner: String = chars[cursor + 1..close].iter().collect();
                        root_index = Some(layout(&inner, width)?);
                        cursor = close + 1;
                    }
                }
                let (content, next) = super::latex::read_group(&chars, cursor).ok()?;
                index = next;
                let body = layout(&content, width)?;
                sqrt(&body, root_index.as_ref())
            }
            _ => return None,
        };
        acc = Some(match acc.take() {
            Some(left) => left.beside(&built),
            None => built,
        });
    }

    let boxed = acc?;
    if boxed.width() > width {
        return None;
    }
    Some(boxed)
}

impl std::fmt::Display for MathBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text: Vec<&str> = self
            .lines
            .iter()
            .map(|line| line.trim_end())
            .collect();
        write!(f, "{}", text.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_box_keeps_its_text_and_width() {
        let boxed = MathBox::text("abc");
        assert_eq!(boxed.lines(), &["abc"]);
        assert_eq!(boxed.width(), 3);
        assert_eq!(boxed.height(), 1);
        assert_eq!(boxed.baseline(), 0);
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("a中b"), 4);
        assert_eq!(display_width("abc"), 3);
    }

    #[test]
    fn fraction_stacks_numerator_over_denominator() {
        let boxed = fraction(&MathBox::text("a"), &MathBox::text("b"));
        assert_eq!(boxed.lines(), &["a", "─", "b"]);
        assert_eq!(boxed.baseline(), 1, "基线在分数线上");
    }

    #[test]
    fn fraction_centres_wider_parts() {
        let boxed = fraction(&MathBox::text("abc"), &MathBox::text("d"));
        // 分母只有 1 个字符，要在 3 宽里居中。
        assert_eq!(boxed.lines()[2], " d ");
        assert_eq!(boxed.width(), 3);
    }

    #[test]
    fn nested_fractions_stack_without_losing_alignment() {
        // (a/b) / c：分子本身是两行的分数。
        let inner = fraction(&MathBox::text("a"), &MathBox::text("b"));
        let boxed = fraction(&inner, &MathBox::text("c"));
        assert!(boxed.height() >= 3);
        // 每一行都不超过盒子宽度。
        for line in boxed.lines() {
            assert!(
                display_width(line) <= boxed.width(),
                "行 {line:?} 超出宽度 {}",
                boxed.width()
            );
        }
    }

    #[test]
    fn sqrt_puts_the_bar_over_the_content() {
        let boxed = sqrt(&MathBox::text("x"), None);
        assert_eq!(boxed.lines()[0], "  ─");
        assert_eq!(boxed.lines()[1], "√ x");
        assert_eq!(boxed.width(), 3);
    }

    #[test]
    fn sqrt_index_sits_at_top_left() {
        let boxed = sqrt(&MathBox::text("x"), Some(&MathBox::text("3")));
        assert!(boxed.lines()[0].starts_with('3'), "{:?}", boxed.lines());
    }

    #[test]
    fn side_by_side_aligns_on_the_baseline() {
        // 左边单行、右边三行的分数：单行的基线与分数盒的基线（分数线）对齐，
        // 所以 "x=" 应当落在分数线那一行，上下各是分子与分母。
        let left = MathBox::text("x=");
        let right = fraction(&MathBox::text("a"), &MathBox::text("b"));
        let joined = left.beside(&right);
        assert_eq!(joined.height(), 3);
        assert_eq!(joined.baseline(), 1);
        assert_eq!(
            joined.lines(),
            &["  a".to_string(), "x=─".to_string(), "  b".to_string()]
        );
    }

    #[test]
    fn display_trims_trailing_spaces() {
        let boxed = MathBox::block(vec!["ab".to_string(), "a ".to_string()]);
        assert_eq!(boxed.to_string(), "ab\na");
    }
}
