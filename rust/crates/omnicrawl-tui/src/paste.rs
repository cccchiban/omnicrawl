//! 粘贴识别：把「一帧里积攒的一串按键」判断成粘贴。
//!
//! 终端支持 bracketed paste 时事件本身就是 [`crossterm::event::Event::Paste`]；但 Windows 的
//! 传统控制台（conhost）根本不发这个序列，粘贴会退化成一连串普通按键——其中的换行被当作
//! `Enter`，于是「第一行被提交、后面的行逐条排队」。
//!
//! 早先的判据是「相邻按键间隔 < 8ms」，真机上漏判（粘贴里的 keyup 记录、事件读取粒度都会
//! 把这一串切碎）。现在改成**按帧归并 + 看内容**：事件循环把一帧内读到的按键全收进来，满足
//! 下面任一条就当粘贴：
//!
//! * 出现两个及以上换行——多行粘贴的基本形状；
//! * 出现换行且**换行两侧都有文字**——人打完字按回车时后面不可能还有字符，这正是「粘贴的
//!   多行」与「打字提交」的分水岭；
//! * 没有换行但连续按键足够多（单行长粘贴）。
//!
//! 判据完全不依赖时间，因此不受终端投递节奏与 keyup 记录影响。误判风险只剩「一帧（约
//! 50–100ms）内打出 4 个以上按键」这种非人类速度，可以忽略。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 单行粘贴的门槛：短于这么多个连续按键就按普通输入处理。
const SINGLE_LINE_BURST_KEYS: usize = 8;

/// 一串按键是否像粘贴。
pub fn looks_like_paste(events: &[KeyEvent]) -> bool {
    if events.len() < 2 {
        return false;
    }
    // 只有能原样还原成文本的一串才有可能是一次粘贴；夹杂方向键/退格/快捷键的按正常按键走。
    let Some(text) = burst_text(events) else {
        return false;
    };
    if paste_shaped(&text) {
        return true;
    }
    events.len() >= SINGLE_LINE_BURST_KEYS && !text.contains('\n')
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

/// 还原一串按键的文本；出现无法线性成文的按键（快捷键、退格、方向键…）时返回 `None`，
/// 交回正常按键路径逐条处理。
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
    fn single_typed_key_is_not_a_paste() {
        assert!(!looks_like_paste(&[key(KeyCode::Char('a'))]));
        assert!(!looks_like_paste(&[key(KeyCode::Char('a')), key(KeyCode::Backspace)]));
    }

    #[test]
    fn a_newline_with_text_on_both_sides_is_a_paste() {
        let events = text("第一行\n第二行");
        assert!(looks_like_paste(&events));
        assert_eq!(burst_text(&events).as_deref(), Some("第一行\n第二行"));
    }

    #[test]
    fn typed_enter_at_the_end_is_not_a_paste() {
        // 打完字按回车：换行后面没有内容，不能当成粘贴（否则消息永远发不出去）。
        assert!(!looks_like_paste(&text("你好")));
        assert!(!looks_like_paste(&text("你好\n")));
        // 三行以上一定是粘贴，不需要看两侧。
        assert!(looks_like_paste(&text("一\n二\n三")));
    }

    #[test]
    fn single_line_burst_is_a_paste() {
        let events = text("0123456789");
        assert!(looks_like_paste(&events));
        assert!(!looks_like_paste(&text("01")));
    }

    #[test]
    fn control_keys_are_never_treated_as_paste_text() {
        let events = vec![
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            key(KeyCode::Char('a')),
        ];
        assert!(burst_text(&events).is_none());
        assert!(!looks_like_paste(&events));
    }

    #[test]
    fn editing_keys_fall_back_to_the_normal_path() {
        let events = vec![key(KeyCode::Char('a')), key(KeyCode::Up), key(KeyCode::Char('b'))];
        assert!(burst_text(&events).is_none());
        assert!(!looks_like_paste(&events));
    }
}
