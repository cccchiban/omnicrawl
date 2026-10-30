//! 内置命令表的声明与校验。表本身很长且几乎不随处理器演进而变。
//!
//! 从 `slash.rs` 拆出。

use super::*;

// ── 注册表 ────────────────────────────────────────────────────

/// 全仓库唯一的斜杠命令注册表。
///
/// 表本身在 [`build_registry`] 里声明并当场校验（重名、别名冲突、空命令名）；
/// 内置表由本文件固定，非法即视为编码错误，因此在首次使用时直接暴露。
pub fn registry() -> &'static CommandRegistry {
    static REGISTRY: OnceLock<CommandRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(|| build_registry().unwrap_or_else(|error| panic!("内置命令表非法：{error}")))
}

/// 构造内置命令表；注册期失败（重名/别名冲突/空命令名）即返回错误。
///
/// 与 Python 的 `@REGISTRY.command(...)` 装饰器一一对应，声明顺序即注册顺序。
pub fn build_registry() -> Result<CommandRegistry, CommandParseError> {
    let mut registry = CommandRegistry::new();

    registry.register(
        Command::new("tasks", handle_tasks_command)
            .description("查看当前会话可见的后台 SubAgent 任务。")
            .usage("/tasks")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("task", handle_task_command)
            .description("查看或取消一个后台 SubAgent 任务。")
            .usage("/task <task_id> [cancel]")
            // 含取消语义，按状态变更处理：排队执行，避免与后台回合并发改状态。
            .command_type(CommandType::Action)
            .arg_prompt("任务 ID"),
    )?;

    registry.register(
        Command::new("sessions", handle_sessions_command)
            .description("查看当前工作区最近会话。")
            .usage("/sessions")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("archives", handle_archives_command)
            .description("查看已归档会话。")
            .usage("/archives")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("archive", handle_archive_command)
            .description("归档当前会话并开启新会话。")
            .usage("/archive")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("history", handle_history_command)
            .description("查看或筛选提示历史。")
            .usage("/history [关键词]")
            .command_type(CommandType::Query)
            .arg_prompt("关键词"),
    )?;

    registry.register(
        Command::new("undo", handle_undo_command)
            .description(
                "原子回退最近一轮对话与工作区中被 Git 记录的更改；冲突或不可逆操作时拒绝。",
            )
            .usage("/undo")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("compact", handle_compact_command)
            .description("使用结构化摘要模型压缩当前会话上下文。")
            .usage("/compact")
            // 结构化摘要需要模型调用，交给交互端的工作线程执行。
            .command_type(CommandType::Background),
    )?;

    registry.register(
        Command::new("rename", handle_rename_command)
            .description("重命名当前会话。")
            .usage("/rename <会话标题>")
            .command_type(CommandType::Action)
            .arg_prompt("会话标题"),
    )?;

    registry.register(
        Command::new("resume", handle_resume_command)
            .description("恢复指定会话 ID。")
            .usage("/resume <session_id>")
            .command_type(CommandType::Action)
            .arg_prompt("会话 ID"),
    )?;

    registry.register(
        Command::new("approval", handle_approval_query_command)
            .description("查看当前工具审批模式。")
            .usage("/approval")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("approval:manual", handle_approval_manual_command)
            .aliases(["auto-approve:off"])
            .description("工具执行前逐次询问。")
            .usage("/approval:manual")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("approval:auto", handle_approval_auto_command)
            .aliases(["auto-approve:on"])
            .description("自动批准工具执行。")
            .usage("/approval:auto")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("approval:review", handle_approval_review_command)
            .aliases(["auto-review:on"])
            .description("自动审查 bash/powershell 命令。")
            .usage("/approval:review")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("review", handle_review_command)
            .description(
                "派生评审子 Agent（完整 git 权限 + 自动批准）收集 diff 并按结构化 JSON \
                 输出审查结果；可选 git 范围参数（如 /review HEAD~3）。",
            )
            .usage("/review [git 范围]")
            // 含子进程 git 预检 + 模型循环，全部放在延迟部分，避免阻塞交互线程。
            .command_type(CommandType::Background)
            .arg_prompt("git 范围"),
    )?;

    registry.register(
        Command::new("reasoning", handle_reasoning_command)
            .description("查看或切换推理强度。")
            .usage("/reasoning [none|low|medium|high|xhigh|max]")
            // 带参数时写 config.toml，按状态变更处理：排队执行。
            .command_type(CommandType::Action)
            .arg_prompt("推理强度"),
    )?;

    registry.register(
        Command::new("model", handle_model_command)
            .description("查看或切换当前模型（从下一次请求开始生效）。")
            .usage("/model <key|profile/model_id|model_id>")
            .command_type(CommandType::Action)
            .arg_prompt("模型选择"),
    )?;

    registry.register(
        Command::new("advisor", handle_advisor_command)
            .description("查看或设置顾问策略模型（advisor）；/advisor off 关闭。")
            .usage("/advisor [model_key] [effort]")
            .command_type(CommandType::Action)
            .arg_prompt("模型 key"),
    )?;

    registry.register(
        Command::new("plan", handle_mode_command)
            .description("启用主 Agent 计划模式，后续请求追加 templates/plan.md。")
            .usage("/plan")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("programming", handle_mode_command)
            .description("启用主 Agent 编程模式，后续请求追加 templates/programming.md。")
            .usage("/programming")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("speaking-style", handle_speaking_style_command)
            .description("开启或关闭主 Agent 简洁回复模式（默认关闭）。")
            .usage("/speaking-style [on|off]")
            .command_type(CommandType::Action)
            .arg_prompt("on|off"),
    )?;

    registry.register(
        Command::new("skills", handle_skills_command)
            .description("查看当前已加载的 Skill。")
            .usage("/skills")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("memory:clean", handle_memory_clean_command)
            .description("清理过期长期记忆。")
            .usage("/memory:clean")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("mcp", handle_mcp_command)
            .description("查看 MCP 开关、服务和工具状态。")
            .usage("/mcp")
            // 可能连接 MCP Server，交给交互端的工作线程执行。
            .command_type(CommandType::Background),
    )?;

    registry.register(
        Command::new("plugins", handle_plugins_command)
            .description("查看 Hook 插件加载与 Worker 状态（只读）。")
            .usage("/plugins")
            .command_type(CommandType::Query),
    )?;

    registry.register(
        Command::new("new", handle_new_command)
            .description("开启一个空白会话。")
            .usage("/new")
            .command_type(CommandType::Action),
    )?;

    registry.register(
        Command::new("quit", handle_quit_command)
            .aliases(["退出", "结束", "再见"])
            .description("退出当前 TUI，不关闭宿主窗口。")
            .usage("/quit")
            .command_type(CommandType::Ui),
    )?;

    registry.register(
        Command::new("settings", handle_settings_command)
            .description("打开中文设置面板，修改运行时开关并立即保存。")
            .usage("/settings")
            .command_type(CommandType::Ui),
    )?;

    registry.register(
        Command::new("workspace", handle_workspace_command)
            .description("切换当前 Agent 的工作区目录。")
            .usage("/workspace [路径]")
            // 切换会重建 Session/MCP/Monitor 与临时目录，必须交给工作线程。
            .command_type(CommandType::Background)
            .arg_prompt("新工作区路径"),
    )?;

    Ok(registry)
}
