//! 设置面板跨页复用的取值助手。
//!
//! 这些函数原先散在 `state.rs` 中段，被 TTS 页、联网搜索页与渠道表单三处共用；
//! 它们与任何一页的行表无关，只表达「凭据在界面上的投影」与「选项循环」两条约定。

use super::*;

pub(super) fn secret_draft(draft: &str) -> String {
    let trimmed = draft.trim();
    if trimmed == MASKED_SECRET {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// 已配置的凭据以掩码进草稿；未配置的留空。
pub(crate) fn secret_draft_for(configured: bool) -> String {
    if configured {
        MASKED_SECRET.to_string()
    } else {
        String::new()
    }
}

/// 密钥在界面上的展示：只露末 4 位，其余打码；空值直接显示「（未配置）」。
pub(crate) fn mask_api_key(api_key: &str) -> String {
    let trimmed = api_key.trim();
    if trimmed.is_empty() {
        return "（未配置）".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}…{}", "*".repeat(4), tail)
}

/// 在候选表里按方向循环取值；当前值不在候选表里时从首项起步。
pub(super) fn cycle_text(options: &[String], current: &str, direction: isize) -> String {
    if options.is_empty() {
        return current.to_string();
    }
    let index = options
        .iter()
        .position(|option| option == current)
        .map(|index| index as isize)
        .unwrap_or(-direction);
    let next = (index + direction).rem_euclid(options.len() as isize) as usize;
    options[next].clone()
}
