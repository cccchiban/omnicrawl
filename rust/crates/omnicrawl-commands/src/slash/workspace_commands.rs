//! 工作区切换命令与它需要的路径/提示文案。
//!
//! 从 `slash.rs` 拆出。

use super::*;

/// 查询或切换工作区。
///
/// 无参数时是只读查询，直接返回；带参数时把重建与持久化推迟到 `deferred`，
/// 避免文件/进程操作阻塞交互线程。
pub fn handle_workspace_command(ctx: &CommandContext<'_>) -> CommandResult {
    let workspace = ctx.args.trim().to_string();
    if workspace.is_empty() {
        return CommandResult::message(format!(
            "当前工作区：{}\n用法：/workspace <新工作区路径>",
            path_text(&ctx.agent.workspace_root())
        ));
    }

    let switch_workspace = move |agent: &dyn CommandAgent| -> CommandResult {
        // 用 `switch_workspace` 返回的**解析后**根：Python 侧切换后展示的就是
        // agent 上那个重新解析过的 `workspace_root`，不是用户输入的原文。
        let root = match agent.switch_workspace(&workspace) {
            Ok(root) => root,
            Err(error) => return CommandResult::message(error.to_string()),
        };
        persist_workspace_root(agent, &workspace);
        CommandResult::message(workspace_switch_success_message(&root))
    };

    CommandResult::message(WORKSPACE_SWITCH_PENDING_MESSAGE)
        .with_refresh_context()
        .with_workspace_switch_requested()
        .with_deferred(Box::new(switch_workspace))
}
