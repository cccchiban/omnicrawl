//! 运行期斜杠命令候选（`build_slash_commands` / `build_slash_command_options`）。
//!
//! 从 `slash.rs` 拆出。

use super::*;

// ── 运行期 Skill 拼接 ─────────────────────────────────────────

/// 构建所有可用的斜杠命令列表（注册表声明 + 运行时 Skill）。
///
/// 内置命令直接来自 [`registry`]，不再手写维护；只有运行时发现的 Skill
/// 需在此按 agent 动态拼接。
pub fn build_slash_commands(agent: &dyn CommandAgent) -> Vec<String> {
    let mut commands = registry().display_names(true);
    if let Some(metas) = agent.skill_metas() {
        for meta in metas {
            commands.push(format!("/skill:{}", meta.name));
        }
    }
    commands
}

/// 构建可供交互客户端使用的斜杠命令元数据。
///
/// TUI 使用命令字符串做 Tab 补全；API 客户端可使用说明、显示标题和搜索文本；
/// `parameters` 是命令声明的可选参数 `(参数, 说明)`，供输入框提示参数名。
/// 内置命令的说明与是否接受参数都由注册表声明派生，只有运行时 Skill 在此拼接，
/// 避免不同交互入口的命令不一致。
pub fn build_slash_command_options(agent: &dyn CommandAgent) -> Vec<CommandOption> {
    let mut options = registry().options(false);
    if let Some(metas) = agent.skill_metas() {
        for meta in metas {
            options.push(CommandOption::skill(&meta.name, &meta.description));
        }
    }
    options
}
