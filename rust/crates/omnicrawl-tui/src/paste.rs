//! 粘贴识别：把「一帧里积攒的一串按键」判断成粘贴。
//!
//! 终端支持 bracketed paste 时事件本身就是 [`crossterm::event::Event::Paste`]；但 Windows 的
//! 传统控制台（conhost）根本不发这个序列，粘贴会退化成一连串普通按键——其中的换行被当作
//! `Enter`，于是「第一行被提交、后面的行逐条排队」。
//!
//! 判据以 Python 侧为准（`omnicrawl/ui/fullscreen/terminal/handling.py`）：**把按键流规范化后
//! 与系统剪贴板逐字比对**，相等才是一次粘贴。这比「看起来像多行」的形状推断可靠得多——
//! 后者会把「手速快的连续输入」误判成粘贴，也会漏掉那些只含一个换行的粘贴。
//!
//! 大文本粘贴会被控制台的输入批次切开（Python 侧每批最多 1024 条输入记录），因此还有一个
//! **前缀挂起**态：当前按键流是剪贴板的严格前缀时先不发，等下一批；超时后按普通输入冲刷，
//! 避免用户输入被无限暂存。
//!
//! 剪贴板读不到（非 Windows、被其它进程占用、内容不是文本）时退回形状判定，保证在那些
//! 场景下仍不会「逐行提交」。

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 单行粘贴的门槛：无换行、与剪贴板对不上时，连续这么多按键才按粘贴处理。
const SINGLE_LINE_BURST_KEYS: usize = 8;

/// 已见到第二行后的等待窗口：大文本粘贴还在分批到达（对映 Python `PASTE_PREFIX_TIMEOUT`）。
pub const PASTE_PREFIX_TIMEOUT: Duration = Duration::from_millis(500);

/// 只有「首行 + 一个换行」时的快速回退窗口：它也可能只是用户按了 Enter，
/// 卡太久会让人觉得提交键失灵（对映 Python `PASTE_SINGLE_LINE_PREFIX_TIMEOUT`）。
pub const PASTE_SINGLE_LINE_PREFIX_TIMEOUT: Duration = Duration::from_millis(50);

/// 按键流与剪贴板｛Desensitized:683｝一为 LF 换行后比较（对映 Python `_normalize_stream_text`）。
fn normalize_stream_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 剥离流开头的控制字符（Ctrl+V 等快捷键被 conhost 透传的残留）。
///
/// 与 Python `_STREAM_CONTROL_PREFIX_CHARS` 同口径：ASCII < 0x20 且不是 `\t` / `\r` / `\n`。
/// 不剥离的话，`\x16` 开头的按键流永远匹配不上剪贴板，粘贴会退化成逐行提交。
fn strip_stream_control_prefix(text: &str) -> &str {
    text.trim_start_matches(|ch: char| ch < ' ' && ch != '\t' && ch != '\r' && ch != '\n')
}

/// 一串按键还原成的文本。出现无法线性成文的按键（快捷键、退格、方向键…）时返回 `None`。
///
/// 返回 `None` 表示这一串不可能是粘贴：交回正常按键路径逐条处理。
pub fn burst_text(events: &[KeyEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    let mut text = String::new();
    for event in events {
        if event
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match event.code {
            // 某些终端把粘贴里的换行当普通字符送来（先判，否则会被下一支吃掉）。
            KeyCode::Char('\n') | KeyCode::Char('\r') => text.push('\n'),
            KeyCode::Char(ch) => text.push(ch),
            KeyCode::Enter => text.push('\n'),
            KeyCode::Tab => text.push('\t'),
            _ => return None,
        }
    }
    Some(text)
}

/// 按键流与剪贴板是否**完整匹配**：是则返回该粘贴内容（用剪贴板原文，含用户复制的原始换行）。
///
/// 只处理含换行的流：单行粘贴走普通按键路径就能正确插入，不必为每次击键读剪贴板。
pub fn clipboard_paste(text: &str, clipboard: &str) -> Option<String> {
    if !text.contains('\n') {
        return None;
    }
    if clipboard.is_empty() {
        return None;
    }
    let stream = normalize_stream_text(strip_stream_control_prefix(text));
    if stream == normalize_stream_text(clipboard) {
        return Some(clipboard.to_string());
    }
    None
}

/// 按键流是否为多行剪贴板的**严格前缀**（等后续批次，不能立刻当普通输入）。
///
/// 判定要点与 Python `_is_pending_clipboard_prefix` 一致：
/// * 单独的换行不算（那是用户按 Enter，不能被挂起）；
/// * 剥离控制字符后为空也不算——空串是任意文本的前缀，会把编辑键无限挂起；
/// * 剪贴板必须本身是多行（单行粘贴不需要挂起）。
pub fn is_pending_clipboard_prefix(text: &str, clipboard: &str) -> bool {
    if matches!(text, "\r" | "\n" | "\r\n") {
        return false;
    }
    if clipboard.is_empty() || !clipboard.contains('\n') {
        return false;
    }
    let stream = normalize_stream_text(strip_stream_control_prefix(text));
    if stream.is_empty() {
        return false;
    }
    let target = normalize_stream_text(clipboard);
    target.starts_with(&stream) && stream != target
}

/// 已见到第二行（或更多换行）时，粘贴大概率还在路上，给更长的等待窗口。
///
/// 对映 Python `flush_keys` 里的 `has_multiline_evidence`：只有「首行 + 一个换行」时仍可能
/// 是普通 Enter，必须快速回退，否则用户按回车会卡住不提交。
pub fn has_multiline_evidence(text: &str) -> bool {
    let normalized = normalize_stream_text(strip_stream_control_prefix(text));
    match normalized.find('\n') {
        Some(index) => !normalized[index + 1..].is_empty(),
        None => false,
    }
}

/// 剪贴板不可用时的形状判定：一串按键是否像粘贴。
///
/// 这是**兜底**路径（读不到剪贴板时才会用到），判据与早先版本一致：
/// 多行基本形状，或「无换行但连续按键足够多」。
pub fn looks_like_paste_shape(events: &[KeyEvent]) -> bool {
    if events.len() < 2 {
        return false;
    }
    let Some(text) = burst_text(events) else {
        return false;
    };
    paste_shaped(&text) || (events.len() >= SINGLE_LINE_BURST_KEYS && !text.contains('\n'))
}

/// 文本本身是否具备「多行粘贴」的形状。
fn paste_shaped(text: &str) -> bool {
    let newlines = text.matches('\n').count();
    if newlines >= 2 {
        return true;
    }
    if newlines == 0 {
        return false;
    }
    // 只夹一个换行：换行两侧都有内容才算粘贴（打字按回车时后面不会再有字符）。
    let (head, tail) = text.split_once('\n').unwrap_or((text, ""));
    !head.trim().is_empty() && !tail.trim().is_empty()
}

/// 粘贴识别状态机：承接跨输入批次的多行粘贴。
///
/// 为什么要有状态：Windows 控制台的输入记录是一批一批读上来的（Python 侧每批最多
/// 1024 条），大文本粘贴会被切成多批。若每批各自判定，中途的 `\n` 就会被当成 `Enter`
/// 提交出去——这正是「多行粘贴被拆成多次提交」的现象。
///
/// 生命周期（与 Python `OmniCrawlWindowsEventMonitor` 一致）：
/// * 按键流与剪贴板完整匹配 → 一次 `Paste`；
/// * 是剪贴板的严格前缀 → 先挂起，等下一批；
/// * 既不是前缀也不是完整匹配 → 立即按普通按键处理；
/// * 挂起超时（见 [`Self::poll_timeout`]）→ 强制按普通按键冲刷，不无限期暂存用户输入。
#[derive(Debug, Default)]
pub struct PasteTracker {
    /// 已收下、尚未判定的按键（挂起期间跨批次累积）。
    pending: Vec<KeyEvent>,
    /// 进入挂起态的时刻与当时的前缀长度（只在「前缀变长」时重置计时）。
    pending_since: Option<Instant>,
    pending_length: usize,
}

/// 一次投喂的处置结果。
#[derive(Debug, PartialEq, Eq)]
pub enum PasteOutcome {
    /// 已判定为一次粘贴：把这段文本当作 `Event::Paste` 处理。
    Paste(String),
    /// 暂时挂起（可能是跨批次粘贴的前缀）：不处理任何按键，等后续批次或超时。
    Pending,
    /// 不是粘贴：把这一串按键按普通输入逐条处理，并清空挂起态。
    Bypass(Vec<KeyEvent>),
}

impl PasteTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前是否有挂起的候选前缀（事件循环据此收紧等待时长）。
    pub fn is_pending(&self) -> bool {
        self.pending_since.is_some()
    }

    /// 挂起是否已超时；到点就该按普通输入冲刷。
    pub fn poll_timeout(&self, now: Instant) -> bool {
        let Some(since) = self.pending_since else {
            return false;
        };
        let window = if has_multiline_evidence(&self.pending_text()) {
            PASTE_PREFIX_TIMEOUT
        } else {
            PASTE_SINGLE_LINE_PREFIX_TIMEOUT
        };
        now.saturating_duration_since(since) >= window
    }

    /// 超时冲刷：把挂起的按键交回普通输入路径。
    pub fn flush(&mut self) -> Vec<KeyEvent> {
        self.pending_since = None;
        self.pending_length = 0;
        std::mem::take(&mut self.pending)
    }

    /// 投喂一批新按键（已按帧归并），返回处置结果。
    ///
    /// `clipboard` 是调用方读到的系统剪贴板文本；读不到时传 `None`，退回形状判定。
    pub fn feed(&mut self, keys: &[KeyEvent], clipboard: Option<&str>, now: Instant) -> PasteOutcome {
        // 空批次没有任何可判定的内容：保持现状（已有挂起就继续挂着）。
        if keys.is_empty() {
            return if self.is_pending() {
                PasteOutcome::Pending
            } else {
                PasteOutcome::Bypass(Vec::new())
            };
        }
        // 挂起期间累积；否则从空开始（上一批已被判定或冲刷）。
        self.pending.extend_from_slice(keys);
        let text = self.pending_text();
        if let Some(clipboard) = clipboard {
            if let Some(pasted) = clipboard_paste(&text, clipboard) {
                self.pending.clear();
                self.pending_since = None;
                self.pending_length = 0;
                return PasteOutcome::Paste(pasted);
            }
            if is_pending_clipboard_prefix(&text, clipboard) {
                // 只有前缀变长才算「还在来」，避免同一批反复刷新计时把超时推远。
                let length = normalize_stream_text(strip_stream_control_prefix(&text)).chars().count();
                if self.pending_since.is_none() || length > self.pending_length {
                    self.pending_since = Some(now);
                    self.pending_length = length;
                }
                return PasteOutcome::Pending;
            }
        }
        // 剪贴板对不上（或读不到）：留下形状兜底。
        let outcome = if looks_like_paste_shape(&self.pending) {
            match burst_text(&self.pending) {
                Some(pasted) => PasteOutcome::Paste(pasted),
                None => PasteOutcome::Bypass(std::mem::take(&mut self.pending)),
            }
        } else {
            PasteOutcome::Bypass(std::mem::take(&mut self.pending))
        };
        self.pending_since = None;
        self.pending_length = 0;
        outcome
    }

    /// 挂起缓冲的纯文本（按键无法线性成文时为空串，形状判定自然不成立）。
    fn pending_text(&self) -> String {
        burst_text(&self.pending).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(value: &str) -> Vec<KeyEvent> {
        value
            .chars()
            .map(|ch| {
                if ch == '\n' {
                    key(KeyCode::Enter)
                } else {
                    key(KeyCode::Char(ch))
                }
            })
            .collect()
    }

    #[test]
    fn clipboard_match_wins_over_shape() {
        // 剪贴板里就是这串多行文本：完整匹配即粘贴。
        let events = text("第一行\n第二行");
        let stream = burst_text(&events).expect("可线性成文");
        assert_eq!(
            clipboard_paste(&stream, "第一行\n第二行").as_deref(),
            Some("第一行\n第二行")
        );
        // 用剪贴板原文回传：用户复制的是 CRLF，就按 CRLF 交出去（归一化在折叠入口做）。
        assert_eq!(
            clipboard_paste(&stream, "第一行\r\n第二行").as_deref(),
            Some("第一行\r\n第二行")
        );
    }

    #[test]
    fn single_newline_paste_is_recognized_by_clipboard() {
        // 只含一个换行的粘贴：形状判定会漏（看着像「打完字按回车」），剪贴板比对不会。
        let stream = "第一行\n第二行";
        assert!(paste_shaped(stream), "两行文本在形状上也算粘贴");
        assert_eq!(
            clipboard_paste(stream, "第一行\n第二行").as_deref(),
            Some("第一行\n第二行")
        );
    }

    #[test]
    fn typed_enter_is_not_a_clipboard_paste() {
        // 用户打了字再按回车：流是剪贴板的前缀/无关，都不该判成粘贴。
        assert!(clipboard_paste("你好\n", "第一行\n第二行").is_none());
        assert!(clipboard_paste("第一行\n第一行\n", "第一行\n第二行").is_none());
    }

    #[test]
    fn single_line_stream_never_matches_clipboard() {
        // 单行粘贴不读剪贴板：直接走普通按键路径插入。
        assert!(clipboard_paste("hello", "hello").is_none());
    }

    #[test]
    fn control_prefix_is_stripped_before_matching() {
        // conhost 把 Ctrl+V 透传成 \x16：剥离后才能与剪贴板匹配。
        let stream = "\x16第一行\n第二行";
        assert_eq!(
            clipboard_paste(stream, "第一行\n第二行").as_deref(),
            Some("第一行\n第二行")
        );
    }

    #[test]
    fn strict_prefix_is_pending_for_later_batches() {
        // 首批只到「第一行」：剪贴板还有后文，等下一批。
        assert!(is_pending_clipboard_prefix("第一行", "第一行\n第二行"));
        assert!(is_pending_clipboard_prefix("第一行\n", "第一行\n第二行"));
        assert!(is_pending_clipboard_prefix("第一行\n第", "第一行\n第二行"));
        // 已经完整（或多出内容）：不再挂起。
        assert!(!is_pending_clipboard_prefix("第一行\n第二行", "第一行\n第二行"));
        assert!(!is_pending_clipboard_prefix("第一行\n第二行\n第三", "第一行\n第二行"));
    }

    #[test]
    fn lone_newline_is_never_pending() {
        // 单独的回车必须立刻提交，不能被当成候选前缀挂住。
        assert!(!is_pending_clipboard_prefix("\n", "第一行\n第二行"));
        assert!(!is_pending_clipboard_prefix("\r\n", "第一行\n第二行"));
    }

    #[test]
    fn control_only_stream_is_never_pending() {
        // 长按退格积累的 \x08 流：剥离后为空，不能当「任意文本的前缀」而无限挂起。
        assert!(!is_pending_clipboard_prefix("\x08\x08", "第一行\n第二行"));
    }

    #[test]
    fn single_line_clipboard_is_never_pending() {
        // 剪贴板只有一行时不需要挂起等待（单行走普通按键路径）。
        assert!(!is_pending_clipboard_prefix("第一", "第一行"));
    }

    #[test]
    fn multiline_evidence_needs_content_after_the_first_newline() {
        assert!(!has_multiline_evidence("第一行\n"));
        assert!(!has_multiline_evidence("第一行"));
        assert!(has_multiline_evidence("第一行\n第"));
        assert!(has_multiline_evidence("第一行\n\n"));
    }

    #[test]
    fn shape_fallback_still_handles_multiline_without_clipboard() {
        // 读不到剪贴板时的兜底：多行形状仍按粘贴处理，别退回「逐行提交」。
        assert!(looks_like_paste_shape(&text("一\n二\n三")));
        assert!(looks_like_paste_shape(&text("第一行\n第二行")));
        assert!(!looks_like_paste_shape(&text("你好")));
        assert!(!looks_like_paste_shape(&text("你好\n")));
        assert!(looks_like_paste_shape(&text("0123456789")));
    }

    #[test]
    fn control_keys_are_never_treated_as_paste_text() {
        let events = vec![
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            key(KeyCode::Char('a')),
        ];
        assert!(burst_text(&events).is_none());
        assert!(!looks_like_paste_shape(&events));
    }

    #[test]
    fn editing_keys_fall_back_to_the_normal_path() {
        let events = vec![key(KeyCode::Char('a')), key(KeyCode::Up), key(KeyCode::Char('b'))];
        assert!(burst_text(&events).is_none());
        assert!(!looks_like_paste_shape(&events));
    }

    // ---- 状态机：跨批次粘贴 -------------------------------------------------------

    const MULTILINE: &str = "第一行\n第二行";

    #[test]
    fn tracker_emits_one_paste_when_the_stream_matches_the_clipboard() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        let outcome = tracker.feed(&text(MULTILINE), Some(MULTILINE), now);
        assert_eq!(outcome, PasteOutcome::Paste(MULTILINE.to_string()));
        assert!(!tracker.is_pending(), "判定完成后不该还挂着");
    }

    #[test]
    fn tracker_holds_a_strict_prefix_until_the_next_batch_arrives() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        // 首批只到「第一行」：挂起，不把其中的换行当 Enter 提交出去。
        assert_eq!(
            tracker.feed(&text("第一行"), Some(MULTILINE), now),
            PasteOutcome::Pending
        );
        assert!(tracker.is_pending());
        // 后续批次到齐：跨批次合起来完整匹配，仍只发一次粘贴。
        let outcome = tracker.feed(&text("\n第二行"), Some(MULTILINE), now);
        assert_eq!(outcome, PasteOutcome::Paste(MULTILINE.to_string()));
        assert!(!tracker.is_pending());
    }

    #[test]
    fn tracker_bypasses_keys_that_are_neither_paste_nor_prefix() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        // 用户敲的字与剪贴板无关：立刻交回普通输入路径。
        let outcome = tracker.feed(&text("你好"), Some(MULTILINE), now);
        assert_eq!(outcome, PasteOutcome::Bypass(text("你好")));
        assert!(!tracker.is_pending());
    }

    #[test]
    fn tracker_never_holds_a_lone_enter() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        // 单独的回车必须立刻提交，不能被当成候选前缀挂住。
        let outcome = tracker.feed(&[key(KeyCode::Enter)], Some(MULTILINE), now);
        assert_eq!(outcome, PasteOutcome::Bypass(vec![key(KeyCode::Enter)]));
        assert!(!tracker.is_pending());
    }

    #[test]
    fn tracker_releases_a_held_stream_when_it_turns_out_not_to_be_a_prefix() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        // 先挂起（看着像剪贴板前缀）。
        tracker.feed(&text("第一行"), Some("第一行\n第二行"), now);
        assert!(tracker.is_pending());
        // 用户继续打字，偏离了剪贴板：把**整段**（含先前挂起的部分）交回普通输入，
        // 不能丢掉已挂起的前缀。
        let outcome = tracker.feed(&text("错"), Some("第一行\n第二行"), now);
        assert_eq!(outcome, PasteOutcome::Bypass(text("第一行错")));
        assert!(!tracker.is_pending());
    }

    #[test]
    fn tracker_bypasses_control_keys_while_a_prefix_is_held() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        tracker.feed(&text("第一行"), Some("第一行\n第二行"), now);
        assert!(tracker.is_pending());
        // 挂起期间按下 Ctrl+C 之类的控制键：立刻放行给正常按键路径，
        // 不能被扣在挂起缓冲里（否则取消/退出会「按了没反应」）。
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let outcome = tracker.feed(&[ctrl_c], Some("第一行\n第二行"), now);
        assert_eq!(
            outcome,
            PasteOutcome::Bypass(text("第一行").into_iter().chain([ctrl_c]).collect()),
            "先前挂起的按键要一并吐出"
        );
        assert!(!tracker.is_pending());
    }

    #[test]
    fn tracker_falls_back_to_shape_when_clipboard_is_unavailable() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        // 读不到剪贴板（None）时退回形状判定：多行仍按一次粘贴处理，不逐行提交。
        let outcome = tracker.feed(&text(MULTILINE), None, now);
        assert_eq!(outcome, PasteOutcome::Paste(MULTILINE.to_string()));
    }

    #[test]
    fn tracker_timeout_window_depends_on_multiline_evidence() {
        let start = Instant::now();
        let mut tracker = PasteTracker::new();

        // 只收到「某 + 换行」：可能是候选粘贴，也可能是用户按了 Enter —— 短窗口。
        tracker.feed(&text("第一行\n"), Some("第一行\n第二行"), start);
        assert!(tracker.is_pending());
        assert!(
            !tracker.poll_timeout(start + PASTE_SINGLE_LINE_PREFIX_TIMEOUT / 2),
            "短窗口内不该超时"
        );
        assert!(
            tracker.poll_timeout(start + PASTE_SINGLE_LINE_PREFIX_TIMEOUT),
            "短窗口到点要允许回退（否则回车提交像卡住）"
        );

        // 已见到第二行内容：大粘贴还在分批，给长窗口。
        let long = Instant::now();
        let mut tracker = PasteTracker::new();
        tracker.feed(&text("第一行\n第"), Some("第一行\n第二行"), long);
        assert!(!tracker.poll_timeout(long + PASTE_SINGLE_LINE_PREFIX_TIMEOUT));
        assert!(tracker.poll_timeout(long + PASTE_PREFIX_TIMEOUT));
    }

    #[test]
    fn tracker_flush_returns_the_held_keys() {
        let now = Instant::now();
        let mut tracker = PasteTracker::new();
        tracker.feed(&text("第一行"), Some(MULTILINE), now);
        assert!(tracker.is_pending());
        assert_eq!(tracker.flush(), text("第一行"));
        assert!(!tracker.is_pending(), "冲刷后不再挂起");
        assert!(tracker.flush().is_empty(), "重复冲刷不该吐出内容");
    }

    #[test]
    fn tracker_keeps_waiting_while_the_prefix_only_grows() {
        let start = Instant::now();
        let mut tracker = PasteTracker::new();
        let clipboard = "第一行\n第二行\n第三行";
        tracker.feed(&text("第"), Some(clipboard), start);
        // 前缀继续变长：一直挂起，直到完整匹配或超时。
        for (index, chunk) in ["一行", "\n第二行", "\n第三行"].iter().enumerate() {
            let at = start + PASTE_SINGLE_LINE_PREFIX_TIMEOUT * (index as u32 + 2);
            let outcome = tracker.feed(&text(chunk), Some(clipboard), at);
            if index < 2 {
                assert_eq!(outcome, PasteOutcome::Pending, "第 {index} 批仍该挂起");
            } else {
                assert_eq!(outcome, PasteOutcome::Paste(clipboard.to_string()));
            }
        }
    }
}
