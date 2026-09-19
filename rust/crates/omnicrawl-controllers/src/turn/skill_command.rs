//! `controllers/turn/loop.py` 的 `/skill:` 手动技能命令。
//!
//! 每轮开始时先清空上一轮手动注入的技能，再识别 `/skill:<名> [余下任务]`：命中就加载该技能
//! 并把余下文本当作任务，未命中则回一份带可用列表的说明。技能查找与列表读取仍在宿主。

/// 手动技能命令前缀。
pub const SKILL_COMMAND_PREFIX: &str = "/skill:";

/// 手动调用技能时的匹配分数。
pub const MANUAL_SKILL_SCORE: f64 = 1.0;

/// 解析 `/skill:<名> [余下文本]`；不是该命令时返回 `None`。
///
/// 与 Python 的 `str.split(None, 1)` 同义：按第一段连续空白切一次，余下文本保留原样。
pub fn parse_skill_command(text: &str) -> Option<(String, Option<String>)> {
    if !text.starts_with(SKILL_COMMAND_PREFIX) {
        return None;
    }
    let trimmed = text.trim_start();
    let (head, rest) = match trimmed.find(char::is_whitespace) {
        Some(index) => (
            &trimmed[..index],
            Some(trimmed[index..].trim_start().to_string()),
        ),
        None => (trimmed, None),
    };
    let name = head[SKILL_COMMAND_PREFIX.len()..].trim().to_string();
    Some((name, rest))
}

/// 手动调用技能时写进匹配结果的说明。
pub fn manual_skill_reason(name: &str) -> String {
    format!("手动调用：{name}")
}

/// 加载成功后的状态提示。
pub fn skill_loaded_status(name: &str) -> String {
    format!("已加载 Skill：{name}")
}

/// 技能不存在时的状态提示（给用户的短提示）。
pub fn skill_not_found_status(name: &str) -> String {
    format!("未找到 Skill：{name}")
}

/// 技能不存在时给模型的文案；可用列表为空时显示「无」。
pub fn skill_not_found_message(name: &str, available: &[String]) -> String {
    let list = if available.is_empty() {
        "无".to_string()
    } else {
        available.join(", ")
    };
    format!("Skill「{name}」不存在。当前可用的 Skill：{list}")
}

/// 手动加载了技能、但用户没写后续任务时的提示。
pub fn skill_default_prompt(name: &str) -> String {
    format!("请执行 {name} 技能。")
}
