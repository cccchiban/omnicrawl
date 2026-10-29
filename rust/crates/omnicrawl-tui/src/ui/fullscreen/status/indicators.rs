//! 顶部 HUD 遥测与输入框上方的排队消息预览（对映 `status/indicators.py`）。
//!
//! Python 侧本模块是 Textual `StatusMixin`：读取 App/Agent 状态、装配底部 HUD 的
//! 展示文本、处理排队预览的点击热区（`QueueDelete` / `QueueToggle`）。Rust 侧把
//! 「读状态 + 装配文本」收敛成纯格式化函数与排队预览行构造器，组件
//! 可见性、热区与触发时机由装配层（`ui/fullscreen/app/core`）承接。

use crate::ui::fullscreen::status::hud::{context_summary_text, token_telemetry_text};
use crate::ui::fullscreen::terminal::theme::{ACCENT_AMBER, TEXT_MUTED};
use crate::ui::fullscreen::text::StyledText;

/// 排队预览条折叠时可见的消息条数（对映 `OmniCrawlApp.QUEUE_PREVIEW_MAX_ROWS`）。
pub const QUEUE_PREVIEW_MAX_ROWS: usize = 3;
/// 排队预览条每行摘要的最大字符数（对映 `QUEUE_PREVIEW_SUMMARY_LIMIT`）。
pub const QUEUE_PREVIEW_SUMMARY_LIMIT: usize = 40;

/// 底部单行 HUD 的数据源快照。
///
/// 只保留仍会绘制的内容：上下文用量 / 吞吐 / 速率与工作区。模型与各开关已移到
/// 欢迎 Logo 下方的会话徽标（见 [`crate::ui::fullscreen::rendering::welcome_logo`]），
/// 底部不再轮播留言页。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HudSource {
    pub workspace: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub context_limit: i64,
    pub tokens_per_second: f64,
}

/// 底部单行 HUD 的整行文本：遥测 + 工作区路径。
pub fn hud_line_text(source: &HudSource) -> StyledText {
    let mut rendered = token_telemetry_text(
        source.input_tokens,
        source.output_tokens,
        source.cached_input_tokens,
        source.context_limit,
        source.tokens_per_second,
    );
    rendered.push("⁕ 工作区 ", TEXT_MUTED);
    rendered.append_text(&context_summary_text(&source.workspace));
    rendered
}

/// 排队预览条的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueRow {
    /// 标题行：排队总数。
    Title,
    /// 消息摘要行；`index` 是该消息在队列中的下标（撤回用）。
    Item { index: usize, summary: String },
    /// 展开/收起提示行。
    Toggle { label: String },
}

/// 取消息首行并压缩空白的摘要（对映 `update_items` 里的 summary 计算）。
pub fn queue_summary(text: &str, limit: usize) -> String {
    let first_line = text.lines().next().unwrap_or("");
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(limit).collect()
}

/// 标题行文本：`⏳ N 条消息排队`。
pub fn queue_title(count: usize) -> StyledText {
    StyledText::styled(
        &format!("⏳ {count} 条消息排队"),
        &format!("bold {ACCENT_AMBER}"),
    )
}

/// 消息摘要行文本（对映 `Static(f"  {index + 1}. {summary}")`）。
pub fn queue_item_text(index: usize, summary: &str) -> String {
    format!("  {}. {summary}", index + 1)
}

/// 展开/收起提示行文本。
pub fn queue_toggle_label(count: usize, max_visible: usize, show_all: bool) -> String {
    if show_all {
        "  « 收起".to_string()
    } else {
        format!("  … 还有 {} 条 ›", count.saturating_sub(max_visible))
    }
}

/// 按当前队列内容重建预览行；空队列返回空表（调用方不显示该组件）。
pub fn queue_preview_rows(
    items: &[String],
    expanded: bool,
    max_visible: usize,
    summary_limit: usize,
) -> Vec<QueueRow> {
    let count = items.len();
    let mut rows: Vec<QueueRow> = Vec::new();
    if count == 0 {
        return rows;
    }
    rows.push(QueueRow::Title);
    let show_all = expanded && count > max_visible;
    let visible = if show_all {
        items
    } else {
        &items[..count.min(max_visible)]
    };
    for (index, text) in visible.iter().enumerate() {
        rows.push(QueueRow::Item {
            index,
            summary: queue_summary(text, summary_limit),
        });
    }
    if count > max_visible {
        rows.push(QueueRow::Toggle {
            label: queue_toggle_label(count, max_visible, show_all),
        });
    }
    rows
}

/// 排队预览条当前应占用的行数（无排队时为 0）。
///
/// 行数 = 1 行标题 + 可见消息行数 + （超出可见上限时）1 行展开/收起提示行。
pub fn pending_queue_rows(count: usize, expanded: bool, max_visible: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let visible = if expanded && count > max_visible {
        count
    } else {
        count.min(max_visible)
    };
    let mut rows = 1 + visible;
    if count > max_visible {
        rows += 1;
    }
    rows
}

/// 对映 `_withdraw_pending_input` 的前置守卫：生成期间、索引有效且未在提问。
pub fn can_withdraw_pending(
    is_generating: bool,
    index: usize,
    count: usize,
    ask_user_active: bool,
) -> bool {
    is_generating && index < count && !ask_user_active
}

/// 对映 `_toggle_pending_queue_expanded`：队列不超过可见上限时不动作。
pub fn can_toggle_queue_expanded(count: usize, max_visible: usize) -> bool {
    count > max_visible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> HudSource {
        HudSource {
            workspace: "D:/work".to_string(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_input_tokens: 600,
            context_limit: 128_000,
            tokens_per_second: 12.3,
        }
    }

    #[test]
    fn hud_line_joins_telemetry_and_workspace() {
        let text = hud_line_text(&source());
        assert!(text.plain().contains("t/s"), "{}", text.plain());
        assert!(
            text.plain().contains("⁕ 工作区 D:/work"),
            "{}",
            text.plain()
        );
        assert!(
            !text.plain().contains("MAN"),
            "模型与开关已移到欢迎 Logo 的会话徽标：{}",
            text.plain()
        );
    }

    #[test]
    fn queue_summary_collapses_first_line() {
        assert_eq!(
            queue_summary("  第一行   有   空白\n第二行", 40),
            "第一行 有 空白"
        );
        assert_eq!(queue_summary("", 40), "");
        assert_eq!(queue_summary("abcdef", 3), "abc");
    }

    #[test]
    fn queue_preview_rows_fold_and_expand() {
        let items: Vec<String> = (1..=5).map(|index| format!("消息{index}")).collect();
        let folded = queue_preview_rows(&items, false, QUEUE_PREVIEW_MAX_ROWS, 40);
        assert_eq!(folded.len(), 5);
        assert_eq!(folded[0], QueueRow::Title);
        assert_eq!(
            folded[1],
            QueueRow::Item {
                index: 0,
                summary: "消息1".to_string()
            }
        );
        assert_eq!(
            folded[4],
            QueueRow::Toggle {
                label: "  … 还有 2 条 ›".to_string()
            }
        );
        let expanded = queue_preview_rows(&items, true, QUEUE_PREVIEW_MAX_ROWS, 40);
        assert_eq!(expanded.len(), 7);
        assert_eq!(
            expanded[6],
            QueueRow::Toggle {
                label: "  « 收起".to_string()
            }
        );
        assert!(queue_preview_rows(&[], false, QUEUE_PREVIEW_MAX_ROWS, 40).is_empty());
    }

    #[test]
    fn queue_row_texts_match_python_layout() {
        assert_eq!(queue_item_text(0, "消息1"), "  1. 消息1");
        assert_eq!(queue_toggle_label(5, 3, false), "  … 还有 2 条 ›");
        assert_eq!(queue_toggle_label(5, 3, true), "  « 收起");
        assert_eq!(queue_title(3).plain(), "⏳ 3 条消息排队");
        assert_eq!(queue_title(3).spans()[0].style, "bold yellow");
    }

    #[test]
    fn pending_queue_rows_counts_title_and_toggle() {
        assert_eq!(pending_queue_rows(0, false, QUEUE_PREVIEW_MAX_ROWS), 0);
        assert_eq!(pending_queue_rows(1, false, QUEUE_PREVIEW_MAX_ROWS), 2);
        assert_eq!(pending_queue_rows(3, false, QUEUE_PREVIEW_MAX_ROWS), 4);
        assert_eq!(pending_queue_rows(5, false, QUEUE_PREVIEW_MAX_ROWS), 5);
        assert_eq!(pending_queue_rows(5, true, QUEUE_PREVIEW_MAX_ROWS), 7);
    }

    #[test]
    fn withdraw_and_toggle_guards_match_app_rules() {
        assert!(can_withdraw_pending(true, 0, 2, false));
        assert!(!can_withdraw_pending(false, 0, 2, false));
        assert!(!can_withdraw_pending(true, 2, 2, false));
        assert!(!can_withdraw_pending(true, 0, 2, true));
        assert!(!can_toggle_queue_expanded(3, QUEUE_PREVIEW_MAX_ROWS));
        assert!(can_toggle_queue_expanded(4, QUEUE_PREVIEW_MAX_ROWS));
    }
}
