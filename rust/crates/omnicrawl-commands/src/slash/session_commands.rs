//! 会话类命令（`/tasks`、`/sessions`、`/history`、`/undo`、`/compact`、`/rename`、`/resume` 等）。
//!
//! 从 `slash.rs` 拆出。

use super::*;

// ── 命令处理器 ────────────────────────────────────────────────

/// 列出当前会话可见的后台 SubAgent 任务。
pub fn handle_tasks_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_subagent_tasks_list(ctx.agent))
}

/// 处理后台 SubAgent 任务的只读查询和单任务取消。
pub fn handle_task_command(ctx: &CommandContext<'_>) -> CommandResult {
    const USAGE: &str = "用法：/task <task_id>；/task cancel <task_id>。";

    let argv = &ctx.argv;
    if argv.is_empty() {
        return CommandResult::message(USAGE);
    }
    if argv.len() == 1 {
        return CommandResult::message(format_subagent_task(ctx.agent, &argv[0]));
    }
    if argv.len() != 2 || argv[0].to_lowercase() != "cancel" {
        return CommandResult::message(USAGE);
    }

    let task_id = argv[1].clone();
    let result = match ctx.agent.cancel_subagent_task(&task_id) {
        Ok(result) => result,
        Err(error) => return CommandResult::message(format!("子任务取消失败：{error}")),
    };
    if !result
        .get("ok")
        .map(|value| !python_falsy(value))
        .unwrap_or(false)
    {
        return CommandResult::message("未找到当前会话的 SubAgent 任务。");
    }

    let task_status = text_or(result.get("status"), "cancelling");
    match task_status.as_str() {
        "cancelled" => CommandResult::message(format!("已取消后台子任务：{task_id}。")),
        "already_terminal" => {
            CommandResult::message(format!("子任务已结束，无需取消：{task_id}。"))
        }
        _ => CommandResult::message(format!("已请求取消后台子任务：{task_id}。")),
    }
}

/// 列出当前工作区最近会话。
pub fn handle_sessions_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_sessions_list(ctx.agent)).with_refresh_context()
}

/// 列出当前工作区已归档会话。
pub fn handle_archives_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_archived_sessions_list(ctx.agent)).with_refresh_context()
}

/// 归档当前会话，并由 Agent 自动开启新会话。
pub fn handle_archive_command(ctx: &CommandContext<'_>) -> CommandResult {
    let archived = match ctx.agent.archive_current_session() {
        Ok(state) => state,
        Err(error) => {
            return CommandResult::message(format!("会话归档失败：{error}")).with_refresh_context()
        }
    };
    CommandResult::message(format!(
        "已归档会话：{}\n已自动开启新会话。查看归档：/archives；恢复归档：/resume <session_id>。",
        archived.session_id
    ))
    .with_refresh_context()
}

/// 展示当前工作区提示历史，可按关键词筛选（只展示，不注入模型）。
pub fn handle_history_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_prompt_history(ctx.agent, &ctx.args)).with_refresh_context()
}

/// 事务式回退最近一轮；成功后要求 UI 重放会话视图。
pub fn handle_undo_command(ctx: &CommandContext<'_>) -> CommandResult {
    if let Err(error) = ctx.agent.undo_last_turn() {
        return CommandResult::message(format!("会话回退失败：{error}")).with_refresh_context();
    }
    CommandResult::message(
        "已回退最近一轮（事务式）：会话转录、模型上下文与工作区中 Git 记录的更改已同步恢复。",
    )
    .with_refresh_context()
    // 已撤回的消息/工具卡需要从事件流重放中消失，不能只追加提示。
    .with_replay_conversation()
}

/// 压缩当前会话上下文：默认使用结构化摘要模型。
///
/// 摘要模型不可用或校验失败时，`compact_conversation_model` 内部会自动降级为本地确定性
/// 压缩，因此不再保留单独的 `--model` 开关。模型调用与事件写入推迟到 `deferred`，避免阻塞
/// 交互端主线程；摘要正文只进会话事件与投影，不在命令输出里回显。
pub fn handle_compact_command(ctx: &CommandContext<'_>) -> CommandResult {
    if !ctx.args.trim().is_empty() {
        return CommandResult::message("参数错误：/compact。").with_refresh_context();
    }

    let run_compact = |agent: &dyn CommandAgent| -> CommandResult {
        if let Err(error) = agent.compact_conversation_model() {
            return CommandResult::message(format!("模型会话压缩失败：{error}"))
                .with_refresh_context();
        }
        let notice = agent.compaction_notice();
        let lead = if notice.is_empty() {
            String::new()
        } else {
            format!("{notice}\n")
        };
        let tail = "已压缩当前会话，完整转录仍保留，后续恢复将从摘要边界继续。";
        CommandResult::message(format!("{lead}{tail}")).with_refresh_context()
    };

    CommandResult::message("正在压缩当前会话上下文…")
        .with_working_status("正在压缩上下文")
        .with_deferred(Box::new(run_compact))
}

/// 重命名当前会话；标题可为含空格的一整段文本。
pub fn handle_rename_command(ctx: &CommandContext<'_>) -> CommandResult {
    let title = ctx.args.trim();
    if title.is_empty() {
        return CommandResult::message("用法：/rename <会话标题>。").with_refresh_context();
    }
    let state = match ctx.agent.rename_current_session(title) {
        Ok(state) => state,
        Err(error) => {
            return CommandResult::message(format!("会话重命名失败：{error}"))
                .with_refresh_context()
        }
    };
    CommandResult::message(format!("当前会话已重命名为：{}", state.title)).with_refresh_context()
}

/// 恢复指定会话；UI 依据会话 ID 变化重放历史消息。
pub fn handle_resume_command(ctx: &CommandContext<'_>) -> CommandResult {
    let session_id = ctx.args.trim();
    if session_id.is_empty() {
        return CommandResult::message(
            "用法：/resume <session_id>。可先用 /sessions 查看最近会话。",
        )
        .with_refresh_context();
    }
    let state = match ctx.agent.resume_session(session_id) {
        Ok(state) => state,
        Err(error) => {
            return CommandResult::message(format!("会话恢复失败：{error}")).with_refresh_context()
        }
    };
    CommandResult::message(format!(
        "已恢复会话：{}\n标题：{}\n已恢复 {} 条上下文消息。",
        state.session_id,
        title_or_unnamed(&state.title),
        state.message_count
    ))
    .with_refresh_context()
}
