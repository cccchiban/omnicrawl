//! `omnicrawl/agent/controllers/tools/building.py`：模式模板装载与 system prompt 组装。

use crate::error::AgentError;
use crate::shared::SYSTEM_PROMPT_FILE;
use std::path::Path;

/// 顾问策略使用准则；仅当 advisor 真正启用且未命中黑名单时追加。
pub const ADVISOR_GUIDELINES_BLOCK: &str = "顾问策略（advisor）使用准则：\n\
- 你可以在关键时刻调用零参数 `advisor` 工具，把当前整段工作上下文交给已配置的更强顾问模型，获得 plan / correction / stop 三类指导。\n\
- 适合调用：重大实质工作（写代码、下结论）之前；反复失败或方案不收敛（卡住）时；换方向之前；长任务承诺方案前至少一次、声明完成前至少一次（先落盘再调用）。\n\
- 不适合调用：短任务且下一步由刚读到的工具输出直接决定时；只做定向探索时。\n\
- 向用户提问超时或用户未作答（`ask_user` 超时/返回失败）时，可调用一次 `advisor` 代替用户评估并给出合理决策方向，避免任务空等；顾问决策不得代替高危或需审批操作的明确用户授权。\n\
- 收到指导后给其实质权重；若与你自己观察到的证据冲突，用一次 `advisor` 把冲突摆给顾问做 reconcile，不盲从也不盲弃。\n\
- 你必须在调用后下一条对用户可见的回复中转述关键指导（用户看不到折叠的工具卡片）。";

/// 返回 advisor 使用准则；未启用或命中黑名单时返回空串（零成本）。
pub fn advisor_guidelines_block(active: bool, blacklisted: bool) -> &'static str {
    if active && !blacklisted {
        ADVISOR_GUIDELINES_BLOCK
    } else {
        ""
    }
}

/// 模式名闭集：小写字母、数字与单连字符分隔的段。
pub fn is_valid_mode_name(name: &str) -> bool {
    let mut segments = name.split('-');
    match segments.next() {
        Some(first) if is_lower_alnum_run(first) => {}
        _ => return false,
    }
    segments.all(is_lower_alnum_run)
}

fn is_lower_alnum_run(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// 归一化模式名：去空白 + 转小写 + 形状校验。
pub fn normalize_mode_name(mode: &str) -> Result<String, AgentError> {
    let normalized = mode.trim().to_lowercase();
    if !is_valid_mode_name(&normalized) {
        return Err(AgentError::new(
            "模式名称只能使用小写字母、数字和单连字符。",
        ));
    }
    Ok(normalized)
}

/// 读取包内模式模板；空模板与非 UTF-8 分别给出对应文案。
pub fn read_mode_prompt(templates_dir: &Path, mode: &str) -> Result<String, AgentError> {
    let path = templates_dir.join(format!("{mode}.md"));
    let bytes = std::fs::read(&path)
        .map_err(|error| AgentError::new(format!("读取模式模板 {mode}.md 失败：{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| AgentError::new(format!("模式模板 {mode}.md 必须是 UTF-8 文本。")))?
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(AgentError::new(format!("模式模板 {mode}.md 不能为空。")));
    }
    Ok(text)
}

/// 读取独立 system prompt 模板（渲染交给提示上下文子系统）。
pub fn read_system_prompt_template(templates_dir: &Path) -> Result<String, AgentError> {
    let path = templates_dir.join(SYSTEM_PROMPT_FILE);
    let bytes = std::fs::read(&path)
        .map_err(|error| AgentError::new(format!("读取 {SYSTEM_PROMPT_FILE} 失败：{error}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| AgentError::new(format!("{SYSTEM_PROMPT_FILE} 必须是 UTF-8 文本。")))?;
    Ok(text.trim().to_string())
}

/// 在基础 system prompt 末尾追加 advisor 准则与当前活动模式提示词。
pub fn system_prompt_with_mode(
    template: &str,
    advisor_block: &str,
    mode_name: &str,
    mode_prompt: &str,
) -> String {
    let mut prompt = template.to_string();
    if !advisor_block.is_empty() {
        prompt = format!("{prompt}\n\n{advisor_block}");
    }
    let mode_prompt = mode_prompt.trim();
    if !mode_prompt.is_empty() {
        let mode_name = if mode_name.is_empty() {
            "active"
        } else {
            mode_name
        };
        prompt = format!(
            "{prompt}\n\n<active_mode_prompt name=\"{mode_name}\">\n{mode_prompt}\n</active_mode_prompt>"
        );
    }
    prompt
}

/// Agent 临时目录的展示路径；未启用时回落到默认目录。
pub fn agent_temp_dir_display(display_path: Option<&str>) -> String {
    match display_path {
        Some(path) => path.to_string(),
        None => ".omnicrawl/.agent_tmp".to_string(),
    }
}
