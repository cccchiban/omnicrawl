//! 顶部 HUD 遥测与输入框上方的排队消息预览（对映 `status/indicators.py`）。
//!
//! Python 侧本模块是 Textual `StatusMixin`：读取 App/Agent 状态、装配轮播展示
//! 文本、处理排队预览的点击热区（`QueueDelete` / `QueueToggle`）。Rust 侧把
//! 「读状态 + 装配文本」收敛成纯状态机 [`Carousel`] 与排队预览行构造器，组件
//! 可见性、热区与触发时机由装配层（`ui/fullscreen/app/core`）承接。

use crate::ui::fullscreen::random::Rng;
use crate::ui::fullscreen::round_half_even;
use crate::ui::fullscreen::status::hud::{
    context_summary_text, decrypt_frame, status_summary_text, token_telemetry_text,
};
use crate::ui::fullscreen::terminal::theme::{ACCENT_AMBER, TEXT_MUTED};
use crate::ui::fullscreen::text::StyledText;

/// 排队预览条折叠时可见的消息条数（对映 `OmniCrawlApp.QUEUE_PREVIEW_MAX_ROWS`）。
pub const QUEUE_PREVIEW_MAX_ROWS: usize = 3;
/// 排队预览条每行摘要的最大字符数（对映 `QUEUE_PREVIEW_SUMMARY_LIMIT`）。
pub const QUEUE_PREVIEW_SUMMARY_LIMIT: usize = 40;

// 底部单行轮播 HUD：遥测（含工作区）20s → 留言页 10s 循环（用户指定的停留时长）。
// 刻意差异：Python 把工作区路径单独占一页（`_context_summary_text`），这里按用户要求并入
// 遥测行尾（`⁕ 工作区 <path>`），轮播只剩两页，工作区不再单独占屏。
pub const CAROUSEL_TELEMETRY_SECONDS: f64 = 20.0;
pub const CAROUSEL_MESSAGE_SECONDS: f64 = 10.0;
/// 留言页无内容时的兜底占位文本。
pub const CAROUSEL_MESSAGE_FALLBACK: &str = "🎲 留言本空空如也，去写一条吧～";
/// 切换时的解密扫描特效时长与帧间隔。
pub const CAROUSEL_ANIMATION_SECONDS: f64 = 1.5;
pub const CAROUSEL_ANIMATION_FRAME_SECONDS: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarouselPage {
    Telemetry,
    Message,
}

/// 轮播页面装配所需的 App/Agent 状态快照。
///
/// Python 侧 `_token_telemetry_text` / `_status_summary_text` / `_context_summary_text`
/// 直接从 `self` 与 `self.agent` 读取这些字段；Rust 侧显式传入快照，保持本模块
/// 不依赖装配层类型。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CarouselSource {
    pub workspace: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub context_limit: i64,
    pub tokens_per_second: f64,
    pub model: String,
    pub reasoning_effort: String,
    pub approval_mode: String,
    pub mcp_enabled_count: i64,
    pub pending_count: i64,
}

/// `switch_to` 的结果：直接落定，或已进入动画需要装配层按帧间隔驱动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarouselSwitch {
    Settled,
    Animating,
}

/// `animation_tick` 的结果：动画中的一帧，或收口后的落定文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarouselTick {
    Frame(StyledText),
    Settled(StyledText),
}

/// 底部单行轮播的状态机（对映 `_carousel_*` 字段）。
///
/// Python 侧把停留定时器与帧定时器交给 Textual；Rust 侧只暴露「当前页停留时长」
/// 与「是否需要驱动动画帧」，定时器由装配层持有。
#[derive(Debug, Clone)]
pub struct Carousel {
    page: CarouselPage,
    settled_text: Option<StyledText>,
    animating: bool,
    anim_target: Option<StyledText>,
    anim_target_old: Option<StyledText>,
    anim_frame: usize,
    anim_total_frames: usize,
    message_line: Option<String>,
}

impl Default for Carousel {
    fn default() -> Self {
        Self::new()
    }
}

impl Carousel {
    pub fn new() -> Self {
        Self {
            page: CarouselPage::Telemetry,
            settled_text: None,
            animating: false,
            anim_target: None,
            anim_target_old: None,
            anim_frame: 0,
            anim_total_frames: 0,
            message_line: None,
        }
    }

    pub fn page(&self) -> CarouselPage {
        self.page
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    pub fn anim_frame(&self) -> usize {
        self.anim_frame
    }

    pub fn anim_total_frames(&self) -> usize {
        self.anim_total_frames
    }

    pub fn settled_text(&self) -> Option<&StyledText> {
        self.settled_text.as_ref()
    }

    /// 当前页的停留时长：遥测（含工作区）与留言各 10s。
    pub fn page_duration(&self) -> f64 {
        match self.page {
            CarouselPage::Telemetry => CAROUSEL_TELEMETRY_SECONDS,
            CarouselPage::Message => CAROUSEL_MESSAGE_SECONDS,
        }
    }

    /// 下一页类型：telemetry → message 循环。
    pub fn next_page(&self) -> CarouselPage {
        match self.page {
            CarouselPage::Telemetry => CarouselPage::Message,
            CarouselPage::Message => CarouselPage::Telemetry,
        }
    }

    /// 在载入页面时固定当前留言：首次切入（或已有留言失效）时随机抽取。
    pub fn ensure_message_line(&mut self, lines: &[String], rand: &mut Rng) -> Option<String> {
        if lines.is_empty() {
            return None;
        }
        let current = self.message_line.clone();
        let line = match current {
            Some(line) if lines.contains(&line) => line,
            _ => {
                let picked = rand.choice(lines).clone();
                self.message_line = Some(picked.clone());
                picked
            }
        };
        Some(line)
    }

    /// 按已固定留言渲染留言页；无候选时显示占位文本。
    pub fn message_text(&mut self, lines: &[String], rand: &mut Rng) -> StyledText {
        match self.ensure_message_line(lines, rand) {
            None => StyledText::styled(CAROUSEL_MESSAGE_FALLBACK, TEXT_MUTED),
            Some(line) => StyledText::styled(&line, TEXT_MUTED),
        }
    }

    /// 按页类型装配完整内容：遥测+模型状态+工作区 / 留言页。
    pub fn build_page_text(
        &mut self,
        page: CarouselPage,
        source: &CarouselSource,
        lines: &[String],
        rand: &mut Rng,
    ) -> StyledText {
        match page {
            CarouselPage::Message => self.message_text(lines, rand),
            CarouselPage::Telemetry => {
                let mut rendered = token_telemetry_text(
                    source.input_tokens,
                    source.output_tokens,
                    source.cached_input_tokens,
                    source.context_limit,
                    source.tokens_per_second,
                );
                rendered.append_text(&status_summary_text(
                    &source.approval_mode,
                    source.mcp_enabled_count,
                    source.pending_count,
                    &source.model,
                    &source.reasoning_effort,
                ));
                // 工作区并进遥测行尾（不再单独占一页）。
                rendered.push("⁕ 工作区 ", TEXT_MUTED);
                rendered.append_text(&context_summary_text(&source.workspace));
                rendered
            }
        }
    }

    /// 当前页的稳态展示文本（compose 初始渲染用）。
    pub fn display_text(
        &mut self,
        source: &CarouselSource,
        lines: &[String],
        rand: &mut Rng,
    ) -> StyledText {
        if self.page == CarouselPage::Message {
            // 预抽取一次，保证首帧与其他路径渲染的留言一致。
            self.ensure_message_line(lines, rand);
        }
        self.build_page_text(self.page, source, lines, rand)
    }

    /// 切换到指定页；`animate=false` 时直接落定（测试/即时路径）。
    ///
    /// 切入留言页时清空上一条已固定留言，使该轮重新随机抽取。
    pub fn switch_to(
        &mut self,
        page: CarouselPage,
        animate: bool,
        source: &CarouselSource,
        lines: &[String],
        rand: &mut Rng,
    ) -> CarouselSwitch {
        let current = self.page;
        let old = self
            .settled_text
            .clone()
            .unwrap_or_else(|| self.build_page_text(current, source, lines, rand));
        if page == CarouselPage::Message {
            self.message_line = None;
        }
        let target = self.build_page_text(page, source, lines, rand);
        self.page = page;
        self.anim_target = Some(target);
        self.anim_target_old = Some(old);
        self.animating = animate;
        if !animate {
            self.animation_finish(source, lines, rand);
            return CarouselSwitch::Settled;
        }
        self.anim_frame = 0;
        self.anim_total_frames =
            (round_half_even(CAROUSEL_ANIMATION_SECONDS / CAROUSEL_ANIMATION_FRAME_SECONDS)).max(1)
                as usize;
        CarouselSwitch::Animating
    }

    /// 推进一帧解密扫描动画；结束后落定并给出目标页文本。
    pub fn animation_tick(
        &mut self,
        source: &CarouselSource,
        lines: &[String],
        rand: &mut Rng,
    ) -> CarouselTick {
        self.anim_frame += 1;
        if self.anim_frame >= self.anim_total_frames {
            return CarouselTick::Settled(self.animation_finish(source, lines, rand));
        }
        let progress = self.anim_frame as f64 / self.anim_total_frames as f64;
        let old = self.anim_target_old.clone().unwrap_or_default();
        let target = self.anim_target.clone().unwrap_or_default();
        CarouselTick::Frame(decrypt_frame(&old, &target, progress, rand))
    }

    /// 动画收口：落定目标页文本并交出（装配层在此之后安排下一次停留）。
    pub fn animation_finish(
        &mut self,
        source: &CarouselSource,
        lines: &[String],
        rand: &mut Rng,
    ) -> StyledText {
        self.animating = false;
        let page = self.page;
        let settled = self.build_page_text(page, source, lines, rand);
        self.settled_text = Some(settled.clone());
        settled
    }
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

    fn lines() -> Vec<String> {
        vec!["第一条留言".to_string(), "第二条留言".to_string()]
    }

    fn source() -> CarouselSource {
        CarouselSource {
            workspace: "D:/work".to_string(),
            input_tokens: 1_200,
            output_tokens: 300,
            cached_input_tokens: 600,
            context_limit: 128_000,
            tokens_per_second: 12.3,
            model: "deepseek-v4".to_string(),
            reasoning_effort: "high".to_string(),
            approval_mode: "manual".to_string(),
            mcp_enabled_count: 1,
            pending_count: 0,
        }
    }

    #[test]
    fn carousel_holds_telemetry_twenty_seconds_and_message_ten() {
        let mut carousel = Carousel::new();
        assert_eq!(carousel.page(), CarouselPage::Telemetry);
        // 用户指定：遥测 20 秒、句子（留言）页 10 秒。
        assert_eq!(carousel.page_duration(), 20.0);
        assert_eq!(carousel.next_page(), CarouselPage::Message);
        carousel.page = CarouselPage::Message;
        assert_eq!(carousel.next_page(), CarouselPage::Telemetry);
        assert_eq!(carousel.page_duration(), 10.0);
    }

    #[test]
    fn telemetry_page_joins_both_segments_and_the_workspace() {
        let mut carousel = Carousel::new();
        let mut rand = Rng::new(1);
        let mut lines_holder = lines();
        let text =
            carousel.build_page_text(CarouselPage::Telemetry, &source(), &lines_holder, &mut rand);
        assert!(text.plain().contains("t/s"));
        assert!(text.plain().contains("MAN"));
        // 工作区并进遥测行尾，不再单独占一页。
        assert!(text.plain().contains("⁕ 工作区 D:/work"), "{}", text.plain());
        let _ = &mut lines_holder;
    }

    #[test]
    fn message_page_falls_back_when_no_candidates() {
        let mut carousel = Carousel::new();
        let mut rand = Rng::new(1);
        let empty: Vec<String> = Vec::new();
        let text = carousel.message_text(&empty, &mut rand);
        assert_eq!(text.plain(), CAROUSEL_MESSAGE_FALLBACK);
    }

    #[test]
    fn ensure_message_line_pins_choice_until_switching_back() {
        let mut carousel = Carousel::new();
        let mut rand = Rng::new(9);
        let candidates = lines();
        let first = carousel
            .ensure_message_line(&candidates, &mut rand)
            .unwrap();
        let second = carousel
            .ensure_message_line(&candidates, &mut rand)
            .unwrap();
        assert_eq!(first, second);
        // 候选集合变化后旧留言失效，重新抽取。
        let other = vec!["另一条".to_string()];
        let third = carousel.ensure_message_line(&other, &mut rand).unwrap();
        assert_eq!(third, "另一条");
    }

    #[test]
    fn switch_without_animation_settles_immediately() {
        let mut carousel = Carousel::new();
        let mut rand = Rng::new(2);
        let candidates = lines();
        let outcome = carousel.switch_to(
            CarouselPage::Message,
            false,
            &source(),
            &candidates,
            &mut rand,
        );
        assert_eq!(outcome, CarouselSwitch::Settled);
        assert!(!carousel.is_animating());
        assert_eq!(carousel.page(), CarouselPage::Message);
        assert!(
            carousel.settled_text().is_some(),
            "切页后应有固定下来的正文"
        );
    }

    #[test]
    fn switch_with_animation_runs_thirty_frames() {
        let mut carousel = Carousel::new();
        let mut rand = Rng::new(4);
        let candidates = lines();
        let source = source();
        let outcome =
            carousel.switch_to(CarouselPage::Message, true, &source, &candidates, &mut rand);
        assert_eq!(outcome, CarouselSwitch::Animating);
        assert_eq!(carousel.anim_total_frames(), 30);
        assert!(carousel.is_animating());
        for _ in 0..29 {
            assert!(matches!(
                carousel.animation_tick(&source, &candidates, &mut rand),
                CarouselTick::Frame(_)
            ));
        }
        match carousel.animation_tick(&source, &candidates, &mut rand) {
            CarouselTick::Settled(text) => {
                assert_eq!(text.plain(), carousel.settled_text().unwrap().plain())
            }
            CarouselTick::Frame(_) => panic!("第 30 帧应收口"),
        }
        assert!(!carousel.is_animating());
        assert_eq!(carousel.page(), CarouselPage::Message);
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
