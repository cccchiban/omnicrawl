//! 启动空会话页欢迎 Logo 的「解密扫描」入场动画（对映 Python
//! `ui/fullscreen/rendering/logo_anim.py`）。
//!
//! 动画只在应用首次挂载 Logo 后播放一次：乱码从左到右逐行侵蚀字形、再让清晰
//! 字形从左到右「吐出」，各行的扫描进度按行号错开形成自上而下的波浪。Logo 下方的
//! 会话徽标用同一套乱码→解出，与块字一起入场。
//!
//! Python 侧本模块是纯函数 + 常量，播放驱动在 `app/core.py`（Textual 定时器）；
//! Rust 侧没有 Textual 定时器，由 [`LogoAnimation`] 这枚播放游标承接同样的
//! 「首帧自增、播满总帧数落定静态」语义，帧推进由 TUI 事件循环按经过时间换算。
//!
//! `█`/`▒` 都是单宽字符，乱码按列原位替换不会破坏块字对齐；行首缩进与内部空格
//! 是字形定位，必须保留为空格，不做乱码化。

use std::time::{Duration, Instant};

use crate::ui::fullscreen::random::Rng;
use crate::ui::fullscreen::rendering::welcome_logo::{
    welcome_logo_lines, welcome_logo_text, LOGO_STYLE,
};
use crate::ui::fullscreen::round_half_even;
use crate::ui::fullscreen::terminal::theme::TEXT_MUTED;
use crate::ui::fullscreen::text::{span_char_styles, StyleRuns, StyledText};

/// 乱码字符集：纯 ASCII 单宽字符，按列原位替换不会破坏块字对齐。
pub const GARBLE_CHARS: &str =
    "#@%&*+=<>/\\?^$!~|0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// 乱码字符集按字节抽样；字符集是纯 ASCII，字节索引等价于字符索引。
const GARBLE_BYTES: &[u8] = GARBLE_CHARS.as_bytes();

/// 乱码右侧区间内偶发闪现真实目标字符，形成「解码中」的闪烁观感。
pub const SHIMMER_CHANCE: f64 = 0.16;
/// 总时长与帧间隔：约 24 帧完成一次入场。
pub const LOGO_ANIM_SECONDS: f64 = 1.2;
pub const LOGO_ANIM_FRAME_SECONDS: f64 = 0.05;
/// 每行扫描进度相对全局进度的错位比例：行号越大越滞后，产生波浪推进。
pub const ROW_STAGGER: f64 = 0.05;
/// 行内扫描波前越靠右越滞后（横向 + 纵向双重波浪）。
pub const COL_STAGGER: f64 = 0.012;
/// Logo 下方会话徽标的解密起点：Logo 扫到约三分之一时开始吐字，与块字同时收口。
pub const BADGE_REVEAL_START: f64 = 0.35;

/// 计算第 `row_index` 行相对全局进度的局部扫描进度。
///
/// 全局进度 0~1 被行号错位压缩到各行的 [0, 1] 区间：行错位越大的行越晚开始、
/// 越晚结束，从而形成自上而下的波浪。行末不足部分让最后几行同时收尾，避免动画
/// 拖尾过长。
fn row_progress(global_progress: f64, row_index: usize) -> f64 {
    let span = (1.0 - ROW_STAGGER * 7.0).max(0.1);
    let start = ROW_STAGGER * row_index as f64;
    ((global_progress - start) / span).clamp(0.0, 1.0)
}

/// 把某行的整体进度按列号进一步错位，形成行内从左到右的扫描波。
///
/// `column` 为该字符在行内字形区（去除行首缩进与行尾空白的可见区）的相对索引：
/// 行首缩进是定位空格，不参与进度错位，避免字形区被大段前导空白拖慢。
fn col_progress(row_progress: f64, column: usize, row_len: usize) -> f64 {
    if row_len <= 1 {
        return row_progress;
    }
    let span = (1.0 - COL_STAGGER * (row_len - 1) as f64).max(0.1);
    ((row_progress - COL_STAGGER * column as f64) / span).clamp(0.0, 1.0)
}

/// 生成替换乱码；块字字符本身单宽，按 1:1 替换保持宽度。
fn garble(rand: &mut Rng) -> char {
    *rand.choice(GARBLE_BYTES) as char
}

/// 生成欢迎 Logo 解密扫描入场动画的一帧。
///
/// 进度 `0.0` 为纯乱码、`1.0` 为完整白色 Logo；每一行先被乱码波前从左到右侵蚀、
/// 随后由清晰字形波前从左到右吐出。行首缩进与行内空格始终保留（它们决定块字
/// 对齐，不做乱码化）；乱码统一使用弱化灰色，清晰字形使用白色。`rand` 传入固定
/// 种子时输出可复现。
pub fn welcome_logo_frame(progress: f64, rand: &mut Rng) -> StyledText {
    let progress = progress.clamp(0.0, 1.0);
    if progress >= 1.0 {
        // 终态快路径：列错位会残留极少量乱码，这里直接落定完整字形，
        // 保证动画收口与静态 Logo 逐字符一致。
        return welcome_logo_text();
    }
    let mut runs = StyleRuns::new();
    for (row_index, line) in welcome_logo_lines().iter().enumerate() {
        if row_index > 0 {
            runs.push_str("\n", "");
        }
        let row_prog = row_progress(progress, row_index);
        // 行尚未进入扫描窗：字形先以乱码形态占位（行首缩进/内部空格保留），
        // 待行进度进入 (0,1) 后由清晰字形波前从左到右解出。
        if row_prog <= 0.0 {
            for ch in line.chars() {
                if ch == ' ' {
                    runs.push_char(' ', TEXT_MUTED);
                } else {
                    runs.push_char(garble(rand), TEXT_MUTED);
                }
            }
            continue;
        }
        let visible_len = line.trim().chars().count();
        // 字形区相对列号：从第一个非空格字符起算（行首缩进不参与进度错位）。
        let mut glyph_col: usize = 0;
        for ch in line.chars() {
            if ch == ' ' {
                runs.push_char(' ', TEXT_MUTED);
                continue;
            }
            let col_prog = col_progress(row_prog, glyph_col, visible_len);
            glyph_col += 1;
            if col_prog >= 1.0 {
                runs.push_char(ch, LOGO_STYLE);
            } else if col_prog > 0.0 && rand.random() < SHIMMER_CHANCE {
                // 波前：未解密部分偶发闪现真实字符。
                runs.push_char(ch, LOGO_STYLE);
            } else {
                runs.push_char(garble(rand), TEXT_MUTED);
            }
        }
    }
    runs.finish()
}

/// 生成 Logo 下方会话徽标的一帧（与块字同一套「乱码 → 从左到右解出」）。
///
/// 徽标只有一行：`BADGE_REVEAL_START` 之前全是乱码，之后波前从左到右吐出真实字符，
/// 波前右侧偶发闪现（[`SHIMMER_CHANCE`]）。空格保持空格，宽度与对齐不变。
pub fn badge_frame(text: &StyledText, progress: f64, rand: &mut Rng) -> StyledText {
    let progress = progress.clamp(0.0, 1.0);
    let plain: Vec<char> = text.plain().chars().collect();
    let styles = span_char_styles(text);
    let reveal = ((progress - BADGE_REVEAL_START) / (1.0 - BADGE_REVEAL_START)).clamp(0.0, 1.0);
    let front = (plain.len() as f64 * reveal) as usize;
    let mut runs = StyleRuns::new();
    for (index, ch) in plain.iter().enumerate() {
        let style = styles.get(index).copied().unwrap_or(LOGO_STYLE);
        if *ch == ' ' {
            runs.push_char(' ', style);
        } else if index < front || (reveal > 0.0 && rand.random() < SHIMMER_CHANCE) {
            runs.push_char(*ch, style);
        } else {
            runs.push_char(garble(rand), TEXT_MUTED);
        }
    }
    runs.finish()
}

/// 入场动画的播放游标（对映 Python `app/core.py` 的 `_logo_anim_*` 状态位）。
///
/// Python 用 Textual 定时器每 0.05s 推进一帧，且「启动即推一帧」；Rust 事件循环
/// 的节拍跟随按键/内核帧，按计次推进会让动画忽快忽慢，因此改为按经过时间换算
/// 帧号：`floor(已过秒数 / 0.05)`，总帧数与落定时刻仍与 Python 一致。
pub struct LogoAnimation {
    /// 只播一次：已启动后重复 `start` 不再重放（对映 `_logo_anim_started`）。
    started: bool,
    /// 已渲染的帧号；`None` 表示尚未渲染任何帧。
    frame: Option<usize>,
    total_frames: usize,
    started_at: Option<Instant>,
    rand: Rng,
    current: StyledText,
    /// Logo 下方那行会话徽标（模型与关键开关）；`None` 时只画块字。
    subtitle: Option<StyledText>,
}

impl Default for LogoAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl LogoAnimation {
    pub fn new() -> Self {
        Self {
            started: false,
            frame: None,
            // Python 侧同样取 `max(1, round(秒数 / 帧间隔))`，用半数进偶保证与
            // Python 内建 `round` 一致的取整结果。
            total_frames: (round_half_even(LOGO_ANIM_SECONDS / LOGO_ANIM_FRAME_SECONDS).max(1))
                as usize,
            started_at: None,
            rand: Rng::from_entropy(),
            current: welcome_logo_text(),
            subtitle: None,
        }
    }

    /// 设置块字下方那行会话徽标（启动动画前调用；未播放时立刻落到当前文本上）。
    ///
    /// 空文本表示不画徽标。返回内容是否真的变了，调用方据此决定要不要置脏重画。
    pub fn set_subtitle(&mut self, subtitle: StyledText) -> bool {
        let subtitle = if subtitle.is_empty() {
            None
        } else {
            Some(subtitle)
        };
        if self.subtitle == subtitle {
            return false;
        }
        self.subtitle = subtitle;
        if !self.is_playing() {
            self.current = self.compose_static();
        }
        true
    }

    /// 静态终态文本：块字 + 空行 + 徽标。
    fn compose_static(&self) -> StyledText {
        let mut text = welcome_logo_text();
        if let Some(subtitle) = self.subtitle.as_ref() {
            text.push("\n\n", "");
            text.append_text(subtitle);
        }
        text
    }

    /// 徽标原文（未乱码的静态版本）；没有徽标时为 `None`。
    ///
    /// 动画帧只替换字形、不改字符布局，界面据此取可点段的字符区间。
    pub fn subtitle(&self) -> Option<&StyledText> {
        self.subtitle.as_ref()
    }

    /// 首屏挂载时启动动画；幂等，只在首次调用生效。
    pub fn start(&mut self, now: Instant) {
        if self.started {
            return;
        }
        self.started = true;
        self.started_at = Some(now);
        self.tick(now);
    }

    /// 推进到 `now` 对应的一帧；返回动画是否仍在播放（落定静态 Logo 后为 `false`）。
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(started_at) = self.started_at else {
            return false;
        };
        let elapsed = now.saturating_duration_since(started_at).as_secs_f64();
        let frame = (elapsed / LOGO_ANIM_FRAME_SECONDS) as usize;
        if frame >= self.total_frames {
            // 播满总帧数：落定静态白色 Logo（含徽标），避免收口停在乱码中间帧。
            self.frame = None;
            self.current = self.compose_static();
            return false;
        }
        if self.frame != Some(frame) {
            self.frame = Some(frame);
            let progress = frame as f64 / self.total_frames as f64;
            let mut text = welcome_logo_frame(progress, &mut self.rand);
            if let Some(subtitle) = self.subtitle.as_ref() {
                // 徽标与块字同时入场：同一进度、同一套乱码→解出，空行留出呼吸感。
                text.push("\n\n", "");
                text.append_text(&badge_frame(subtitle, progress, &mut self.rand));
            }
            self.current = text;
        }
        true
    }

    /// 当前应展示的文本：播放中为乱码扫描帧，未播放或播完为静态白色 Logo。
    pub fn text(&self) -> &StyledText {
        &self.current
    }

    /// 是否正在播放（供事件循环跳过无谓的帧推进）。
    pub fn is_playing(&self) -> bool {
        self.frame.is_some()
    }

    /// 距下一帧还有多久；未播放或已播完时返回 `None`（事件循环据此不再为动画设唤醒点）。
    ///
    /// `started_at` 在播完后仍然留着（`frame` 才是"还在播"的判据），因此不能只看它。
    pub fn next_frame_delay(&self, now: Instant) -> Option<Duration> {
        if !self.is_playing() {
            return None;
        }
        let started_at = self.started_at?;
        let elapsed = now.saturating_duration_since(started_at).as_secs_f64();
        let frame = (elapsed / LOGO_ANIM_FRAME_SECONDS).floor() * LOGO_ANIM_FRAME_SECONDS;
        let remaining = (frame + LOGO_ANIM_FRAME_SECONDS - elapsed).max(0.001);
        Some(Duration::from_secs_f64(remaining))
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn with_seed(seed: u64) -> Rng {
        Rng::new(seed)
    }

    #[test]
    fn full_progress_matches_static_logo() {
        let garbled = welcome_logo_frame(1.0, &mut with_seed(3));
        let static_logo = welcome_logo_text();
        assert_eq!(garbled.plain(), static_logo.plain());
        assert_eq!(garbled.spans().len(), 1);
        assert_eq!(garbled.spans()[0].style, LOGO_STYLE);
        assert_eq!(
            welcome_logo_frame(1.5, &mut with_seed(3)).plain(),
            static_logo.plain(),
            "越界进度按 1.0 处理"
        );
    }

    #[test]
    fn zero_progress_garbles_glyphs_but_keeps_layout() {
        let mut rand = with_seed(5);
        let frame = welcome_logo_frame(0.0, &mut rand);
        let lines = welcome_logo_lines();
        let static_logo = welcome_logo_text();
        assert_eq!(
            frame.plain().chars().count(),
            static_logo.plain().chars().count()
        );
        for (out, style) in frame.plain().chars().zip(frame.char_styles()) {
            if out == '\n' {
                continue;
            }
            assert_eq!(style, TEXT_MUTED, "乱码统一使用弱化灰色");
        }
        for (rendered, original) in frame.plain().lines().zip(lines.iter()) {
            assert_eq!(
                rendered.len(),
                original.chars().count(),
                "乱码按列 1:1 替换，不改变块字对齐"
            );
            for (out, source) in rendered.chars().zip(original.chars()) {
                if source == ' ' {
                    assert_eq!(out, ' ', "行首缩进/内部空格不参与乱码化");
                } else {
                    assert!(GARBLE_CHARS.contains(out), "乱码字符取自共享字符集：{out}");
                }
            }
        }
    }

    #[test]
    fn frames_are_deterministic_for_a_fixed_seed() {
        let left = welcome_logo_frame(0.6, &mut with_seed(17)).plain();
        let right = welcome_logo_frame(0.6, &mut with_seed(17)).plain();
        assert_eq!(left, right);
        let other = welcome_logo_frame(0.6, &mut with_seed(18)).plain();
        assert_ne!(left, other, "不同种子应产生不同乱码序列");
    }

    #[test]
    fn row_progress_staggers_then_settles() {
        assert_eq!(row_progress(0.0, 0), 0.0);
        for row in 0..8 {
            assert_eq!(row_progress(1.0, row), 1.0, "全局进度 1 必须让每行收口");
        }
        let first = row_progress(0.4, 0);
        let later = row_progress(0.4, 5);
        assert!(first > later, "行号越大越滞后：{first} vs {later}");
        assert!(row_progress(0.3, 2) <= row_progress(0.5, 2));
    }

    #[test]
    fn col_progress_is_monotonic_and_handles_short_rows() {
        assert_eq!(col_progress(0.5, 0, 1), 0.5, "单列行不做列错位");
        assert_eq!(col_progress(1.0, 0, 40), 1.0);
        assert!(
            col_progress(0.6, 0, 20) > col_progress(0.6, 15, 20),
            "越靠右越滞后"
        );
        assert_eq!(col_progress(0.0, 3, 20), 0.0);
    }

    #[test]
    fn animation_settles_after_total_frames_and_plays_once() {
        let mut animation = LogoAnimation::new();
        assert_eq!(animation.total_frames(), 24);
        let start = Instant::now();
        animation.start(start);
        assert!(animation.is_playing());
        assert_ne!(
            animation.text().plain(),
            welcome_logo_text().plain(),
            "启动即进入乱码扫描帧"
        );
        assert!(animation.tick(start + Duration::from_millis(600)));
        assert!(animation.is_playing());
        assert!(!animation.tick(start + Duration::from_millis(1250)));
        assert!(!animation.is_playing());
        assert_eq!(animation.text().plain(), welcome_logo_text().plain());
        // 落定后重复推进不再回到乱码帧。
        assert!(!animation.tick(start + Duration::from_millis(1300)));
        assert_eq!(animation.text().plain(), welcome_logo_text().plain());
        // 幂等启动：已播过不再重放。
        animation.start(start + Duration::from_secs(5));
        assert!(!animation.is_playing());
    }

    #[test]
    fn animation_frames_change_between_ticks() {
        let mut animation = LogoAnimation::new();
        let start = Instant::now();
        animation.start(start);
        let first = animation.text().plain();
        animation.tick(start + Duration::from_millis(900));
        assert_ne!(first, animation.text().plain(), "扫描过程中每帧内容应推进");
    }

    #[test]
    fn subtitle_enters_with_the_logo_and_settles_under_it() {
        let badge = crate::ui::fullscreen::rendering::welcome_logo::session_badge_text(
            "deepseek-4.1-flash",
            "high",
            "review",
            1,
        );
        let settled = format!("\n\n{}", badge.plain());
        let mut animation = LogoAnimation::new();
        animation.set_subtitle(badge);

        // 未播放：静态文本已带徽标（空行分隔，落在块字下方）。
        assert!(animation.text().plain().ends_with(&settled));

        let start = Instant::now();
        animation.start(start);
        let first_frame = animation.text().plain();
        assert_eq!(
            first_frame.lines().count(),
            welcome_logo_text().plain().lines().count() + 2,
            "块字下方空一行再画徽标"
        );
        assert!(
            first_frame.chars().count()
                == welcome_logo_text().plain().chars().count() + settled.chars().count(),
            "徽标逐字替换成乱码，行宽不变"
        );
        assert!(!first_frame.ends_with(&settled), "徽标不应立刻清晰");

        assert!(!animation.tick(start + Duration::from_millis(1250)));
        assert!(
            animation.text().plain().ends_with(&settled),
            "播完后徽标落定为静态文本"
        );
    }
}
