//! 「突发按键」识别粘贴。
//!
//! 终端支持 bracketed paste 时会直接给 [`crossterm::event::Event::Paste`]；但 Windows 的
//! 传统控制台（conhost）等终端根本不发这个序列，粘贴会退化成一连串普通按键——其中的换行
//! 被当作 `Enter`，于是「第一行被提交、后面的行逐条排队」。
//!
//! 这里用相邻按键的间隔做判据：真人连打不可能做到相邻两次按键间隔小于 [`BURST_GAP`]，
//! 所以间隔极短的一串按键可以安全地当成粘贴；其中只要有换行就一定是粘贴（两行之间夹一次
//! 回车还会落在 8ms 内，打字做不到）。判据按「够短 + 够多（或含换行）」取，避免把快速连打
//! 的普通输入误吞成粘贴。

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 判定为同一次粘贴的最大相邻按键间隔。
pub const BURST_GAP: Duration = Duration::from_millis(8);

/// 单行粘贴的门槛：短于这么多个连续按键就按普通输入处理。
///
/// 单行粘贴没有换行可依赖，只能靠长度兜底；门槛取大一点，宁可不折也不要误吞连打。
const SINGLE_LINE_BURST_KEYS: usize = 8;

/// 一次突发按键是否像粘贴。
pub fn looks_like_paste(events: &[KeyEvent]) -> bool {
    if events.len() < 2 {
        return false;
    }
    // 含换行的连续按键一定是粘贴：打字时「回车 + 下一个字符」不可能落在 8ms 内。
    if events.iter().any(is_newline) {
        return true;
    }
    events.len() >= SINGLE_LINE_BURST_KEYS
}

/// 还原一次突发按键的文本；出现无法线性成文的按键（快捷键、退格、方向键…）时返回
/// `None`，交回正常按键路径逐条处理。
pub fn burst_text(events: &[KeyEvent]) -> Option<String> {
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

/// 换行：`Enter` 与裸 LF/CR（某些终端把粘贴里的换行直接当字符送来）。
fn is_newline(event: &KeyEvent) -> bool {
    matches!(
        event.code,
        KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn single_typed_key_is_not_a_paste() {
        assert!(!looks_like_paste(&[key(KeyCode::Char('a'))]));
        assert!(!looks_like_paste(&[
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
        ]));
        assert!(!looks_like_paste(&[key(KeyCode::Enter)]));
    }

    #[test]
    fn a_newline_inside_a_burst_means_paste() {
        let burst = [
            key(KeyCode::Char('第')),
            key(KeyCode::Char('一')),
            key(KeyCode::Enter),
            key(KeyCode::Char('第')),
            key(KeyCode::Char('二')),
        ];
        assert!(looks_like_paste(&burst));
        assert_eq!(burst_text(&burst).as_deref(), Some("第一\n第二"));
    }

    #[test]
    fn long_single_line_burst_is_a_paste() {
        let burst: Vec<KeyEvent> = "abcdefghi".chars().map(|ch| key(KeyCode::Char(ch))).collect();
        assert!(looks_like_paste(&burst));
        assert_eq!(burst_text(&burst).as_deref(), Some("abcdefghi"));
    }

    #[test]
    fn control_keys_are_never_treated_as_paste_text() {
        let burst = [
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            key(KeyCode::Enter),
            key(KeyCode::Char('x')),
        ];
        assert_eq!(burst_text(&burst), None);
    }

    #[test]
    fn editing_keys_fall_back_to_the_normal_path() {
        let burst = [
            key(KeyCode::Char('a')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
        ];
        assert_eq!(burst_text(&burst), None);
    }
}
