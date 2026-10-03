//! 内置斜杠命令：声明、处理器与展示文案（`omnicrawl/commands/slash.py` 的 Rust 移植）。
//!
//! 全仓库唯一的命令注册表在 [`registry`]；命令在 [`build_registry`] 里逐条声明，
//! 解析/分发/帮助/补全统一由 [`CommandRegistry`] 负责，各入口（TUI、Telegram、飞书、
//! 本地 API）只调用 `registry().dispatch(...)`。
//!
//! 处理器只依赖 [`CommandAgent`] 与 `omnicrawl-config` 的写回函数：宿主副作用（会话生命周期、
//! 子代理、MCP、Skill、工具表重建）走 trait，配置写回直接用已搬好的 Rust 实现——这与 Python
//! `slash.py` 同时调 agent 方法与配置函数的结构一致。远端入口的安全边界（`/approval:auto`、
//! `/quit`、`/settings` 对远程通道的拒绝）逐条保留。

use crate::agent::{CommandAgent, SubAgentRun};
use crate::framework::{
    Command, CommandContext, CommandOption, CommandParseError, CommandRegistry, CommandResult,
    CommandType,
};
use chrono::{DateTime, Local};
use omnicrawl_config::core::runtime::ConfigEnvironment;
use omnicrawl_config::core::workspace::save_workspace_root;
use omnicrawl_config::features::advisor::{
    clear_advisor_config, load_advisor_config, save_advisor_config, AdvisorConfig,
    ADVISOR_EFFORT_OPTIONS, DEFAULT_ADVISOR_EFFORT,
};
use omnicrawl_config::features::approval::{
    approval_mode_label, normalize_approval_mode, save_approval_mode, APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL, APPROVAL_MODE_REVIEW,
};
use omnicrawl_config::models::llm::{save_active_model_ref, save_reasoning_effort, ActiveModelRef};
use omnicrawl_config::models::llm_multi::apply_model_selection;
use omnicrawl_config::models::model_catalog::save_llm_model;
use omnicrawl_config::models::model_store::load_model_store;
use omnicrawl_controllers::json::python_dumps;
use omnicrawl_controllers::tool_args::public_tool_arguments;
use omnicrawl_controllers::AgentError;
use regex::Regex;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::OnceLock;

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
            .usage("/settings [--chat]")
            .command_type(CommandType::Ui)
            .arg_prompt("--chat")
            .parameters([("--chat", "无上下文配置对话")]),
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

// ── 工具确认展示 ──────────────────────────────────────────────

/// 工具的人类可读名（对映 `_TOOL_HUMAN_DESCRIPTIONS`）。
fn tool_human_description(tool_name: &str) -> Option<&'static str> {
    let description = match tool_name {
        "list" => "列出目录内容",
        "find" => "按名称或路径查找文件",
        "read" => "读取文件内容",
        "read_image" => "读取图片",
        "grep" => "在文件中搜索文本",
        "Edit_file" => "替换文件中的文本",
        "write_file" => "写入文件",
        "bash" => "执行 Bash 命令",
        "powershell" => "执行 PowerShell 命令",
        "monitor" => "管理后台命令",
        "windows_window" => "操作 Windows 窗口",
        "windows_control" => "操作 Windows UI 控件",
        "windows_input" => "模拟 Windows 鼠标或键盘输入",
        "windows_clipboard" => "操作 Windows 文本剪贴板",
        "windows_screenshot" => "截取 Windows 桌面画面",
        "memory_search" => "搜索长期记忆",
        "memory_read" => "读取记忆内容",
        "memory_expand_related" => "展开相关记忆",
        "memory_write" => "写入长期记忆",
        "subagent" => "分发只读子任务",
        "advisor" => "咨询顾问模型获取第二意见",
        _ => return None,
    };
    Some(description)
}

/// 截断过长文本，保留可读的关键部分（按字符数，与 Python 的 `len()` 同口径）。
fn truncate_for_display(text: &str, max_len: usize) -> String {
    let count = text.chars().count();
    if count <= max_len {
        return text.to_string();
    }
    let keep = max_len.saturating_sub(3);
    let mut truncated: String = text.chars().take(keep).collect();
    truncated.push_str("...");
    truncated
}

/// Python `str()`：字符串原样，其余走 `repr`（`True`/`None`/数字与 Python 同写法）。
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => omnicrawl_controllers::json::python_repr(other),
    }
}

/// Python 的真值判定（`or` 的短路口径）。
fn python_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// Python `str(value or fallback)`。
fn text_or(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None => fallback.to_string(),
        Some(Value::String(text)) if text.is_empty() => fallback.to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => {
            if python_falsy(other) {
                fallback.to_string()
            } else {
                python_str(other)
            }
        }
    }
}

/// Python `str(value or "")`。
fn text_of(value: Option<&Value>) -> String {
    text_or(value, "")
}

/// 取字符串字段；非字符串回落空串（对映 Python 的 `isinstance(x, str)` 判定）。
fn string_field<'a>(arguments: &'a Map<String, Value>, key: &str) -> &'a str {
    arguments.get(key).and_then(Value::as_str).unwrap_or("")
}

/// 取字符串字段，缺省时回落到给定默认值（对映 `arguments.get(key, default)`）。
fn string_field_or<'a>(arguments: &'a Map<String, Value>, key: &str, fallback: &'a str) -> &'a str {
    match arguments.get(key) {
        Some(Value::String(text)) => text,
        Some(_) => fallback,
        None => fallback,
    }
}

fn has_argument(arguments: &Map<String, Value>, key: &str) -> bool {
    arguments.contains_key(key)
}

fn flag_argument(arguments: &Map<String, Value>, key: &str) -> bool {
    arguments
        .get(key)
        .map(|value| !python_falsy(value))
        .unwrap_or(false)
}

/// 对写入/修改/执行类工具，提取关键内容供用户审查。
///
/// 只读类工具（读取、搜索、列出）不展示参数，保持界面简洁。
fn format_dangerous_tool_detail(tool_name: &str, arguments: &Map<String, Value>) -> String {
    if matches!(tool_name, "bash" | "powershell") {
        let command = string_field(arguments, "command");
        if !command.trim().is_empty() {
            return format!("命令：{}", truncate_for_display(command, 150));
        }
        return String::new();
    }

    if tool_name == "monitor" {
        let action = string_field_or(arguments, "action", "start");
        let monitor_id = string_field(arguments, "monitor_id");
        let command = string_field(arguments, "command");
        if action == "start" && !command.trim().is_empty() {
            return format!("后台命令：{}", truncate_for_display(command, 150));
        }
        if !monitor_id.trim().is_empty() {
            return format!("操作：{action}，任务：{monitor_id}");
        }
        return format!("操作：{action}");
    }

    if tool_name == "write_file" {
        let path = string_field(arguments, "path");
        let content = string_field(arguments, "content");
        let mode = string_field_or(arguments, "mode", "overwrite");
        if !path.trim().is_empty() {
            let mut detail = format!("{mode} 到 {path}");
            if !content.trim().is_empty() {
                detail.push_str(&format!("，内容：{}", truncate_for_display(content, 200)));
            }
            return detail;
        }
        return String::new();
    }

    if tool_name == "Edit_file" {
        let path = string_field(arguments, "path");
        if !path.trim().is_empty() {
            let old = string_field(arguments, "old_text");
            let new = string_field(arguments, "new_text");
            let mut detail = format!("文件：{path}");
            if !old.trim().is_empty() {
                detail.push_str(&format!("，替换 \"{}\"", truncate_for_display(old, 80)));
                detail.push_str(&format!(" → \"{}\"", truncate_for_display(new, 80)));
            } else if matches!(arguments.get("old_text"), Some(Value::String(_))) {
                detail.push_str("，替换（空文本）");
                if !string_field(arguments, "new_text").trim().is_empty() {
                    detail.push_str(&format!(" → \"{}\"", truncate_for_display(new, 80)));
                }
            }
            return detail;
        }
        return String::new();
    }

    if tool_name == "read_image" {
        let path = string_field(arguments, "path");
        let mut detail = if path.trim().is_empty() {
            String::new()
        } else {
            format!("文件：{path}")
        };
        if flag_argument(arguments, "detail") {
            detail.push_str(&format!("，视觉细节：{}", text_of(arguments.get("detail"))));
        }
        let prompt = string_field(arguments, "prompt");
        if !prompt.trim().is_empty() {
            detail.push_str(&format!(
                "，分析提示词：{}",
                truncate_for_display(prompt, 120)
            ));
        }
        return truncate_for_display(&detail, 240);
    }

    if tool_name == "windows_window" {
        let action = string_field(arguments, "action");
        let handle = text_or(arguments.get("window_handle"), "");
        if !handle.is_empty() {
            return format!("操作：{action}，窗口：{handle}");
        }
        return format!("操作：{action}");
    }

    if tool_name == "windows_control" {
        let action = string_field(arguments, "action");
        let handle = text_or(arguments.get("window_handle"), "");
        let mut locator_parts: Vec<String> = Vec::new();
        for key in [
            "automation_id",
            "name",
            "class_name",
            "control_type",
            "index",
        ] {
            if let Some(value) = arguments.get(key) {
                locator_parts.push(format!("{key}={}", python_str(value)));
            }
        }
        let mut detail = format!("操作：{action}");
        if !handle.is_empty() {
            detail.push_str(&format!("，窗口：{handle}"));
        }
        if !locator_parts.is_empty() {
            detail.push_str("，定位：");
            detail.push_str(&locator_parts.join("、"));
        }
        if has_argument(arguments, "value_length") {
            detail.push_str(&format!(
                "，写入文本：{} 字符（内容不展示）",
                python_str(&arguments["value_length"])
            ));
        }
        return truncate_for_display(&detail, 240);
    }

    if tool_name == "windows_input" {
        let action = string_field(arguments, "action");
        let mut detail = format!("操作：{action}");
        if has_argument(arguments, "x") && has_argument(arguments, "y") {
            detail.push_str(&format!(
                "，坐标：({}, {})",
                python_str(&arguments["x"]),
                python_str(&arguments["y"])
            ));
        }
        if has_argument(arguments, "button") {
            detail.push_str(&format!("，按钮：{}", python_str(&arguments["button"])));
        }
        if let Some(Value::Array(keys)) = arguments.get("keys") {
            let joined: Vec<String> = keys.iter().map(python_str).collect();
            detail.push_str(&format!("，按键：{}", joined.join("+")));
        } else if has_argument(arguments, "key") {
            detail.push_str(&format!("，按键：{}", python_str(&arguments["key"])));
        }
        if has_argument(arguments, "text_length") {
            detail.push_str(&format!(
                "，输入文本：{} 字符（内容不展示）",
                python_str(&arguments["text_length"])
            ));
        }
        return truncate_for_display(&detail, 240);
    }

    if tool_name == "windows_clipboard" {
        let action = string_field(arguments, "action");
        let mut detail = format!("操作：{action}");
        if has_argument(arguments, "text_length") {
            detail.push_str(&format!(
                "，文本：{} 字符（内容不展示）",
                python_str(&arguments["text_length"])
            ));
        }
        if has_argument(arguments, "max_chars") {
            detail.push_str(&format!(
                "，最多读取：{} 字符",
                python_str(&arguments["max_chars"])
            ));
        }
        return detail;
    }

    if tool_name == "windows_screenshot" {
        let target = string_field_or(arguments, "target", "desktop");
        let mut detail = format!("目标：{target}");
        if flag_argument(arguments, "window_handle") {
            detail.push_str(&format!(
                "，窗口：{}",
                python_str(&arguments["window_handle"])
            ));
        }
        if ["x", "y", "width", "height"]
            .iter()
            .all(|key| has_argument(arguments, key))
        {
            detail.push_str(&format!(
                "，区域：({}, {}) {}×{}",
                python_str(&arguments["x"]),
                python_str(&arguments["y"]),
                python_str(&arguments["width"]),
                python_str(&arguments["height"])
            ));
        }
        if has_argument(arguments, "max_dimension") {
            detail.push_str(&format!(
                "，模型图片最大边：{}",
                python_str(&arguments["max_dimension"])
            ));
        }
        return truncate_for_display(&detail, 240);
    }

    if tool_name == "memory_write" || tool_name.ends_with("_memory_write") {
        if let Some(Value::Array(memories)) = arguments.get("memories") {
            if !memories.is_empty() {
                return format!("写入 {} 条记忆", memories.len());
            }
        }
        return String::new();
    }

    if tool_name == "subagent" {
        let mut detail = format!(
            "任务数：{}",
            python_str(
                &arguments
                    .get("task_count")
                    .cloned()
                    .unwrap_or(Value::from(0))
            )
        );
        if let Some(Value::Array(descriptions)) = arguments.get("descriptions") {
            if !descriptions.is_empty() {
                let joined: Vec<String> = descriptions.iter().map(python_str).collect();
                detail.push_str("，任务：");
                detail.push_str(&joined.join("；"));
            }
        }
        return truncate_for_display(&detail, 240);
    }

    if tool_name.contains('.') && !arguments.is_empty() {
        let rendered = python_dumps(&Value::Object(arguments.clone()), 0);
        return format!("参数：{}", truncate_for_display(&rendered, 240));
    }

    String::new()
}

/// 把工具调用格式化为行内 UI 的确认提示。
///
/// 只读工具仅显示描述，写入/执行工具额外展示关键内容供审查。
pub fn format_tool_confirmation(tool_name: &str, arguments: &Map<String, Value>) -> String {
    let public_arguments = public_tool_arguments(tool_name, arguments);
    let public_arguments = public_arguments.as_object().cloned().unwrap_or_default();
    let description = match tool_human_description(tool_name) {
        Some(description) => description.to_string(),
        None => {
            if tool_name.contains('.') {
                format!("执行 MCP 工具 {tool_name}")
            } else {
                "执行操作".to_string()
            }
        }
    };
    let detail = format_dangerous_tool_detail(tool_name, &public_arguments);

    let mut lines = vec![format!("Agent 想要{description}。")];
    if !detail.is_empty() {
        lines.push(detail);
    }
    lines.push(String::new());
    lines.push("是否允许执行？".to_string());
    lines.join("\n")
}

// ── Skill / 记忆 / MCP / 插件展示 ─────────────────────────────

/// 格式化 Skill 列表为可展示文本。
pub fn format_skills_list(agent: &dyn CommandAgent) -> String {
    let Some(metas) = agent.skill_metas() else {
        return "Skill 子系统未启用。".to_string();
    };
    if metas.is_empty() {
        return "当前没有已加载的 Skill。在 .omnicrawl/skills/、~/.omnicrawl/skills/ 或 \
                企业级 skills 目录下创建 SKILL.md 来添加。"
            .to_string();
    }
    let mut lines = vec![format!("已加载 {} 个 Skill：", metas.len())];
    for meta in &metas {
        let suffix = if meta.disable_model_invocation {
            " [手动]"
        } else {
            ""
        };
        lines.push(format!("  {}{suffix}  ({})", meta.name, meta.scope));
        lines.push(format!("    {}", meta.description));
    }
    lines.join("\n")
}

/// 行内 UI 打印 Skill 列表。
pub fn print_skills_list(agent: &dyn CommandAgent) {
    println!("{}", format_skills_list(agent));
}

/// 执行过期记忆清理，并返回适合终端展示的结果。
pub fn format_memory_clean_result(agent: &dyn CommandAgent) -> String {
    let deleted_paths = match agent.clean_memory() {
        Ok(paths) => paths,
        Err(error) => return format!("记忆清理失败：{error}"),
    };
    if deleted_paths.is_empty() {
        return "没有需要清理的过期记忆。".to_string();
    }
    let joined: Vec<String> = deleted_paths
        .iter()
        .map(|path| format!("  - {path}"))
        .collect();
    format!(
        "已清理 {} 条过期记忆：\n{}",
        deleted_paths.len(),
        joined.join("\n")
    )
}

/// 行内 UI 打印记忆清理结果。
pub fn print_memory_clean_result(agent: &dyn CommandAgent) {
    println!("{}", format_memory_clean_result(agent));
}

/// 格式化 MCP 状态为可展示文本。
pub fn format_mcp_status(agent: &dyn CommandAgent) -> String {
    agent.mcp_status()
}

/// 行内 UI 打印 MCP 状态。
pub fn print_mcp_status(agent: &dyn CommandAgent) {
    println!("{}", format_mcp_status(agent));
}

/// 格式化插件子系统只读状态。
pub fn format_plugins_status(agent: &dyn CommandAgent) -> String {
    agent
        .plugins_status()
        .unwrap_or_else(|| "插件状态接口不可用。".to_string())
}

/// 行内 UI 打印插件状态。
pub fn print_plugins_status(agent: &dyn CommandAgent) {
    println!("{}", format_plugins_status(agent));
}

// ── 会话 / 历史展示 ───────────────────────────────────────────

/// 本地时间戳，与 Python `datetime.astimezone().strftime("%Y-%m-%d %H:%M:%S")` 同形。
fn format_local_timestamp(value: DateTime<chrono::Utc>) -> String {
    value
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// 毫秒时间戳的本地写法；无效时间戳回落 `-`。
fn format_local_millis(value: i64) -> String {
    match DateTime::from_timestamp_millis(value) {
        Some(instant) => format_local_timestamp(instant),
        None => "-".to_string(),
    }
}

/// 格式化当前工作区最近会话列表。
pub fn format_sessions_list(agent: &dyn CommandAgent) -> String {
    let sessions = match agent.list_sessions(10) {
        Ok(sessions) => sessions,
        Err(error) => return format!("会话列表读取失败：{error}"),
    };
    if sessions.is_empty() {
        return "当前工作区还没有可恢复会话。".to_string();
    }

    let current_id = agent.current_session_id();
    let mut lines = vec!["最近会话：".to_string()];
    for entry in &sessions {
        let marker = if entry.session_id == current_id {
            "*"
        } else {
            " "
        };
        let title = title_or_unnamed(&entry.title);
        lines.push(format!(
            "{marker} {}  {}  {} 条消息  {title}",
            entry.session_id,
            format_local_timestamp(entry.updated_at),
            entry.message_count
        ));
    }
    lines.push(String::new());
    lines.push("恢复会话：/resume <session_id>".to_string());
    lines.join("\n")
}

/// 格式化当前工作区已归档会话列表。
pub fn format_archived_sessions_list(agent: &dyn CommandAgent) -> String {
    let sessions = match agent.list_archived_sessions(10) {
        Ok(sessions) => sessions,
        Err(error) => return format!("归档会话列表读取失败：{error}"),
    };
    if sessions.is_empty() {
        return "当前工作区还没有已归档会话。".to_string();
    }

    let mut lines = vec!["归档会话：".to_string()];
    for entry in &sessions {
        let title = title_or_unnamed(&entry.title);
        let archived_at = match entry.archived_at {
            Some(instant) => format_local_timestamp(instant),
            None => "-".to_string(),
        };
        lines.push(format!(
            "  {}  {archived_at}  {} 条消息  {title}",
            entry.session_id, entry.message_count
        ));
    }
    lines.push(String::new());
    lines.push("恢复归档会话：/resume <session_id>（恢复后会重新进入最近会话列表）".to_string());
    lines.join("\n")
}

fn title_or_unnamed(title: &str) -> String {
    if title.is_empty() {
        "未命名会话".to_string()
    } else {
        title.to_string()
    }
}

/// 格式化当前项目用户提示历史；只展示，不注入模型上下文。
pub fn format_prompt_history(agent: &dyn CommandAgent, query: &str) -> String {
    let entries = match agent.search_prompt_history(query, 20) {
        Ok(entries) => entries,
        Err(error) => return format!("提示历史读取失败：{error}"),
    };
    if entries.is_empty() {
        return "当前工作区还没有匹配的提示历史。".to_string();
    }

    let trimmed = query.trim();
    let title = if trimmed.is_empty() {
        "提示历史".to_string()
    } else {
        format!("提示历史（关键词：{trimmed}）")
    };
    let current_id = agent.current_session_id();
    let mut lines = vec![format!("{title}：")];
    for (index, entry) in entries.iter().enumerate() {
        let created_at = format_local_millis(entry.timestamp);
        let display = truncate_for_display(
            &entry
                .display
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            120,
        );
        let marker = if entry.session_id == current_id {
            "*"
        } else {
            " "
        };
        lines.push(format!(
            "{:>2}. {marker} {created_at}  {display}",
            index + 1
        ));
    }
    lines.push(String::new());
    lines.push("筛选历史：/history <关键词>".to_string());
    lines.join("\n")
}

// ── SubAgent 任务展示 ─────────────────────────────────────────

/// 格式化当前会话可见的后台 SubAgent 任务，不展示原始 prompt 或完整结果。
pub fn format_subagent_tasks_list(agent: &dyn CommandAgent) -> String {
    let tasks = match agent.list_subagent_tasks() {
        Ok(tasks) => tasks,
        Err(error) => return format!("子任务查询失败：{error}"),
    };
    if tasks.is_empty() {
        return "当前会话没有后台 SubAgent 任务。".to_string();
    }

    let mut lines = vec!["当前会话后台子任务：".to_string()];
    for task in &tasks {
        let object = task.as_object();
        let task_id = text_or(object.and_then(|map| map.get("task_id")), "-");
        let agent_type = text_or(object.and_then(|map| map.get("agent_type")), "subagent");
        let status = text_or(object.and_then(|map| map.get("status")), "unknown");
        let description = text_or(object.and_then(|map| map.get("description")), "未提供描述");
        lines.push(format!(
            "- {task_id} · {agent_type} · {status} · {description}"
        ));
    }
    lines.push(String::new());
    lines.push("查看详情：/task <task_id>".to_string());
    lines.push("取消任务：/task cancel <task_id>".to_string());
    lines.join("\n")
}

/// 格式化单个安全任务快照，供终端和全屏 TUI 共享使用。
pub fn format_subagent_task(agent: &dyn CommandAgent, task_id: &str) -> String {
    let task = match agent.get_subagent_task(task_id) {
        Ok(task) => task,
        Err(error) => return format!("子任务查询失败：{error}"),
    };
    match task {
        Some(task) => render_subagent_task(&task),
        None => "未找到当前会话的 SubAgent 任务。".to_string(),
    }
}

/// 安全任务快照 → 展示文本（纯函数，便于对照与单测）。
pub fn render_subagent_task(task: &Value) -> String {
    let object = task.as_object();
    let mut lines = vec![
        format!(
            "任务：{}",
            text_or(object.and_then(|map| map.get("task_id")), "-")
        ),
        format!(
            "状态：{}",
            text_or(object.and_then(|map| map.get("status")), "unknown")
        ),
        format!(
            "角色：{}",
            text_or(object.and_then(|map| map.get("agent_type")), "subagent")
        ),
        format!(
            "描述：{}",
            text_or(object.and_then(|map| map.get("description")), "未提供描述")
        ),
    ];
    if let Some(result) = object
        .and_then(|map| map.get("result"))
        .and_then(Value::as_object)
    {
        let summary = text_of(result.get("summary")).trim().to_string();
        if !summary.is_empty() {
            lines.push(String::new());
            lines.push("摘要：".to_string());
            lines.push(summary);
        }
        if let Some(artifacts) = result.get("artifacts").and_then(Value::as_array) {
            if !artifacts.is_empty() {
                lines.push(format!("关联 artifact：{} 项", artifacts.len()));
            }
        }
    }
    if let Some(error) = object
        .and_then(|map| map.get("error"))
        .and_then(Value::as_object)
    {
        let message = text_of(error.get("message")).trim().to_string();
        if !message.is_empty() {
            lines.push(String::new());
            lines.push(format!("错误：{message}"));
        }
    }
    lines.join("\n")
}

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

/// 查看当前工具审批模式；远程连接器额外提示不支持完全自动。
pub fn handle_approval_query_command(ctx: &CommandContext<'_>) -> CommandResult {
    let label = approval_mode_label(&ctx.agent.approval_mode());
    if ctx.is_remote() {
        let lines = [
            format!("当前工具审批模式：{label}。"),
            "可用切换：/approval:manual（手动确认）".to_string(),
            "          /approval:review（自动审查，默认）".to_string(),
            "远程不支持 /approval:auto（完全自动仅限本地 TUI）".to_string(),
        ];
        return CommandResult::message(lines.join("\n"));
    }
    CommandResult::message(format!("当前工具审批模式：{label}。")).with_refresh_context()
}

/// 切换审批模式并持久化；远程入口按安全边界拒绝完全自动。
fn apply_approval_mode(ctx: &CommandContext<'_>, mode: &str) -> CommandResult {
    let normalized = match normalize_approval_mode(mode) {
        Ok(value) => value,
        Err(error) => return CommandResult::message(error.message()),
    };
    let label = approval_mode_label(normalized);
    if ctx.is_remote() && normalized == APPROVAL_MODE_AUTO {
        // 完全自动仅限本地 TUI：远程连接器不得绕过工具审批。
        return CommandResult::message(format!(
            "❌ 远程不支持完全自动批准。完全自动仅限本地 TUI 配置；当前仍为 {}。",
            approval_mode_label(&ctx.agent.approval_mode())
        ));
    }
    if let Err(error) = ctx.agent.set_approval_mode(normalized) {
        return CommandResult::message(error.to_string());
    }
    let environment = ctx.agent.config_environment();
    match save_approval_mode(&environment, normalized, None) {
        Ok(path) => CommandResult::message(format!(
            "审批模式已切换为 {label}，并已同步到 {}。",
            path.display()
        ))
        .with_refresh_context(),
        Err(error) => CommandResult::message(format!(
            "审批模式已临时切换为 {label}，但写入 config.toml 失败：{}",
            error.message()
        ))
        .with_refresh_context(),
    }
}

/// 切换到手动确认模式。
pub fn handle_approval_manual_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_MANUAL)
}

/// 切换到完全自动批准（仅限本地 TUI）。
pub fn handle_approval_auto_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_AUTO)
}

/// 切换到自动审查模式。
pub fn handle_approval_review_command(ctx: &CommandContext<'_>) -> CommandResult {
    apply_approval_mode(ctx, APPROVAL_MODE_REVIEW)
}

/// 查看或切换推理强度（切换会同步写入 config.toml）。
pub fn handle_reasoning_command(ctx: &CommandContext<'_>) -> CommandResult {
    let effort = ctx.args.trim();
    if effort.is_empty() {
        let current = ctx
            .agent
            .reasoning_effort()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "默认".to_string());
        return CommandResult::message(format!(
            "当前推理强度：{current}。\n可选：/reasoning none|low|medium|high|xhigh|max"
        ))
        .with_refresh_context();
    }

    let normalized = match ctx.agent.set_reasoning_effort(effort) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::message(format!("推理强度切换失败：{error}"))
                .with_refresh_context()
        }
    };
    let environment = ctx.agent.config_environment();
    let path = match save_reasoning_effort(&environment, &normalized, None) {
        Ok(path) => path,
        Err(error) => {
            return CommandResult::message(format!(
                "推理强度已临时切换为 {normalized}，但写入 config.toml 失败：{}",
                error.message()
            ))
            .with_refresh_context()
        }
    };
    let env_message = reasoning_env_override_message(&environment);
    CommandResult::message(format!(
        "推理强度已切换为 {normalized}，并已同步到 {}{env_message}",
        path.display()
    ))
    .with_refresh_context()
}

fn reasoning_env_override_message(_environment: &ConfigEnvironment) -> String {
    String::new()
}

/// 查看或切换当前模型。
///
/// 支持 `/model` 查看当前模型，`/model <selection>` 切换模型（selection 可为 models.toml
/// key/alias、profile/model_id 或裸 model_id）。切换即时生效：运行中的回合继续用旧模型，
/// 下一次请求自动使用新模型。
pub fn handle_model_command(ctx: &CommandContext<'_>) -> CommandResult {
    let selection = ctx.args.trim().to_string();
    if selection.is_empty() {
        let current = {
            let current = ctx.agent.current_model();
            if current.is_empty() {
                ctx.agent.llm_config().model
            } else {
                current
            }
        };
        return CommandResult::message(format!(
            "当前模型：{current}\n用法：/model <key|profile/model_id|model_id>"
        ))
        .with_refresh_context();
    }

    let environment = ctx.agent.config_environment();
    let mut persist = {
        let selection = selection.clone();
        let environment = environment.clone();
        move || -> Result<(), AgentError> {
            // 与 set_model 的 apply_model_selection 解析保持一致：优先持久化
            // models.toml 引用，否则写回 llm.model。
            let record = match load_model_store(&environment, None) {
                Ok(store) => store
                    .resolve_alias(&selection)
                    .ok()
                    .flatten()
                    .map(|record| (record.key.clone(), record.model_id.clone())),
                Err(_) => None,
            };
            match record {
                Some((key, model_id)) => {
                    let reference = ActiveModelRef {
                        source: "custom".to_string(),
                        key,
                        model_id,
                        ..ActiveModelRef::default()
                    };
                    save_active_model_ref(&environment, &reference, None)
                        .map(|_| ())
                        .map_err(|error| AgentError::new(error.message()))
                }
                None => save_llm_model(&environment, &selection, None)
                    .map(|_| ())
                    .map_err(|error| AgentError::new(error.message())),
            }
        }
    };

    if let Err(error) = ctx.agent.set_model(&selection, &mut persist) {
        return CommandResult::message(format!("模型切换失败：{error}")).with_refresh_context();
    }
    CommandResult::message(format!(
        "模型已切换为：{}（从下一次请求开始生效）",
        ctx.agent.current_model()
    ))
    .with_refresh_context()
}

/// 查看或设置顾问策略模型。
///
/// 支持：
/// - `/advisor`：查看当前顾问模型/effort，并列出可选模型与用法。
/// - `/advisor <model_key> [effort]`：设置顾问模型（models.toml key/alias、profile/model_id
///   或裸 model_id）与可选推理档位（缺省 high）。
/// - `/advisor off`：清除顾问选择（关闭功能，工具即时剥离）。
///
/// 保存后重建工具表，使 advisor 工具即时出现/消失。
pub fn handle_advisor_command(ctx: &CommandContext<'_>) -> CommandResult {
    let environment = ctx.agent.config_environment();
    let current_config = match load_advisor_config(&environment, None) {
        Ok(config) => config,
        Err(error) => return CommandResult::message(error.message()),
    };
    if ctx.argv.is_empty() {
        return CommandResult::message(advisor_status_message(
            ctx.agent,
            &current_config,
            &environment,
        ))
        .with_refresh_context();
    }

    let action = ctx.argv[0].trim().to_string();
    let folded = action.to_lowercase();
    if matches!(folded.as_str(), "off" | "none" | "clear" | "no") {
        return CommandResult::message(advisor_clear(ctx.agent, &environment))
            .with_refresh_context();
    }
    if matches!(folded.as_str(), "help" | "-h" | "--help") {
        return CommandResult::message(advisor_help_message(&current_config))
            .with_refresh_context();
    }

    let model_key = action;
    // effort 取 model_key 之后的整段剩余文本，与旧行为的 split(None, 2) 一致。
    let arguments = ctx.args.trim();
    let remainder = match arguments.find(' ') {
        Some(index) => arguments[index + 1..].trim().to_string(),
        None => String::new(),
    };
    let effort = if remainder.is_empty() {
        DEFAULT_ADVISOR_EFFORT.to_string()
    } else {
        remainder
    };
    let normalized_effort = match normalize_advisor_effort(&effort) {
        Ok(value) => value,
        Err(error) => {
            return CommandResult::message(format!("顾问设置失败：{error}")).with_refresh_context()
        }
    };

    // 校验模型选择可用（apply_model_selection 解析失败即报错），不写无效引用。
    if let Err(error) = apply_model_selection(&environment, &ctx.agent.llm_config(), &model_key) {
        return CommandResult::message(format!("顾问模型无法解析：{}", error.message()))
            .with_refresh_context();
    }

    let next_config = AdvisorConfig {
        enabled: true,
        model_key: model_key.clone(),
        effort: normalized_effort.clone(),
        disabled_for_models: current_config.disabled_for_models.clone(),
    };
    let path = match save_advisor_config(&environment, &next_config, None) {
        Ok(path) => path,
        Err(error) => {
            return CommandResult::message(format!("顾问设置写入失败：{}", error.message()))
                .with_refresh_context()
        }
    };
    // 内存配置同步 + 重建工具表，让 advisor 工具即时出现。
    if let Err(error) = ctx.agent.apply_advisor_config(&next_config) {
        return CommandResult::message(format!(
            "顾问已保存为 {model_key}（effort={normalized_effort}），但工具表刷新失败：{error}"
        ))
        .with_refresh_context();
    }
    CommandResult::message(format!(
        "顾问已启用：{model_key}（effort={normalized_effort}），已写入 {}。",
        path.display()
    ))
    .with_refresh_context()
}

fn advisor_clear(agent: &dyn CommandAgent, environment: &ConfigEnvironment) -> String {
    let path = match clear_advisor_config(environment, None) {
        Ok(path) => path,
        Err(error) => return format!("清除顾问失败：{}", error.message()),
    };
    let next_config = AdvisorConfig::default();
    if let Err(error) = agent.apply_advisor_config(&next_config) {
        return format!(
            "顾问已清除（{}），但工具表刷新失败：{error}",
            path.display()
        );
    }
    format!(
        "顾问已关闭并清除选择（{}）。advisor 工具已从工具表剥离。",
        path.display()
    )
}

fn advisor_status_message(
    agent: &dyn CommandAgent,
    config: &AdvisorConfig,
    environment: &ConfigEnvironment,
) -> String {
    if config.active() {
        return format!(
            "当前顾问：{}（effort={}）\n用法：/advisor <model_key> [effort]，/advisor off 关闭。",
            config.model_key,
            config.display_effort()
        );
    }
    format!(
        "顾问未启用。\n用法：/advisor <model_key> [effort]，effort 可选 {}（缺省 {}）。\n可用模型：\n{}",
        ADVISOR_EFFORT_OPTIONS.join("/"),
        DEFAULT_ADVISOR_EFFORT,
        advisor_model_candidates(agent, environment)
    )
}

fn advisor_help_message(config: &AdvisorConfig) -> String {
    let state = if config.active() {
        format!(
            "当前顾问：{}（effort={}）",
            config.model_key,
            config.display_effort()
        )
    } else {
        "顾问未启用".to_string()
    };
    format!(
        "{state}\n用法：\n  /advisor                   查看状态与可用模型\n  \
         /advisor <model_key> [effort]  设置顾问模型与推理档位\n  \
         /advisor off               关闭并清除顾问\neffort 可选：{}（缺省 {}）",
        ADVISOR_EFFORT_OPTIONS.join("/"),
        DEFAULT_ADVISOR_EFFORT
    )
}

/// 列出可作顾问的候选模型：models.toml enabled 模型 + 当前主模型。
fn advisor_model_candidates(agent: &dyn CommandAgent, environment: &ConfigEnvironment) -> String {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(store) = load_model_store(environment, None) {
        for record in &store.models {
            if !record.enabled {
                continue;
            }
            let mut label = record.key.clone();
            if !record.aliases.is_empty() {
                label.push_str(&format!("（别名：{}）", record.aliases.join("/")));
            }
            candidates.push(format!("  - {label}"));
        }
    }
    let current = {
        let current = agent.current_model();
        if current.is_empty() {
            agent.llm_config().model
        } else {
            current
        }
    };
    if !current.is_empty() {
        candidates.push(format!("  - {current}（当前主模型）"));
    }
    if candidates.is_empty() {
        "  （无可用模型，请先配置 models.toml）".to_string()
    } else {
        candidates.join("\n")
    }
}

fn normalize_advisor_effort(effort: &str) -> Result<String, AgentError> {
    let mut normalized = effort.trim().to_lowercase();
    if matches!(normalized.as_str(), "disabled" | "off" | "none") {
        normalized = "none".to_string();
    }
    if !ADVISOR_EFFORT_OPTIONS.contains(&normalized.as_str()) {
        let allowed = ADVISOR_EFFORT_OPTIONS.join("/");
        return Err(AgentError::new(format!(
            "effort 仅支持 {allowed}，当前值：{effort}。"
        )));
    }
    Ok(normalized)
}

/// 启用主 Agent 模式（当前仅 plan）。
pub fn handle_mode_command(ctx: &CommandContext<'_>) -> CommandResult {
    // 主 Agent 模式切换命令：命令名（不含前导斜杠）→ `activate_mode` 的模式标识。
    let name = match ctx.command.as_ref() {
        Some(command) => command.name.clone(),
        None => "plan".to_string(),
    };
    let mode = match name.as_str() {
        "plan" => "plan",
        _ => {
            return CommandResult::message(format!("未知模式：{name}"));
        }
    };
    let activated = match ctx.agent.activate_mode(mode) {
        Ok(activated) => activated,
        Err(error) => {
            return CommandResult::message(format!("模式启用失败：{error}")).with_refresh_context()
        }
    };
    let label = if activated == "plan" {
        "计划模式".to_string()
    } else {
        activated
    };
    CommandResult::message(format!("已启用{label}。后续任务将遵循该模式提示词。"))
        .with_refresh_context()
}

/// 列出当前已加载的 Skill。
pub fn handle_skills_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_skills_list(ctx.agent))
}

/// 清理过期长期记忆。
pub fn handle_memory_clean_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_memory_clean_result(ctx.agent))
}

/// 读取 MCP 状态；真正的连接推迟到 `deferred`。
pub fn handle_mcp_command(_ctx: &CommandContext<'_>) -> CommandResult {
    let read_status = |agent: &dyn CommandAgent| -> CommandResult {
        CommandResult::message(format_mcp_status(agent))
    };
    CommandResult::message("正在读取 MCP 状态").with_deferred(Box::new(read_status))
}

/// 读取插件子系统只读状态。
pub fn handle_plugins_command(ctx: &CommandContext<'_>) -> CommandResult {
    CommandResult::message(format_plugins_status(ctx.agent))
}

/// 清空当前对话并开启新会话。
pub fn handle_new_command(ctx: &CommandContext<'_>) -> CommandResult {
    let old_session_id = ctx.agent.current_session_id();
    if let Err(error) = ctx.agent.reset_conversation() {
        return CommandResult::message(error.to_string());
    }
    let message = if old_session_id.is_empty() {
        "已新开会话。".to_string()
    } else {
        format!("已新开会话，旧会话：{old_session_id}")
    };
    CommandResult::message(message)
        .with_refresh_context()
        .with_clear_conversation()
}

/// 请求交互端退出当前 TUI；远程入口无退出语义，只给出提示。
pub fn handle_quit_command(ctx: &CommandContext<'_>) -> CommandResult {
    if ctx.is_remote() {
        return CommandResult::message("远程连接不支持 /quit；如需停止请关闭对应 Bot 会话。");
    }
    CommandResult::default().with_exit_requested()
}

/// 请求交互端打开设置面板或无上下文配置对话。
pub fn handle_settings_command(ctx: &CommandContext<'_>) -> CommandResult {
    if ctx.is_remote() {
        return CommandResult::message("远程连接不支持 /settings；请在本地 TUI 中打开设置面板。");
    }
    if ctx.args.trim().to_lowercase() == "--chat" {
        return CommandResult::message("进入配置对话")
            .with_open_config_chat()
            .with_clear_conversation()
            .with_refresh_context();
    }
    if !ctx.args.trim().is_empty() {
        return CommandResult::error("/settings 只支持可选参数 --chat。");
    }
    CommandResult::message("打开设置面板")
        .with_open_settings()
        .with_refresh_context()
}

/// 按 Python `str(Path)` 的口径渲染路径：分隔符规范成当前平台的分隔符。
///
/// Python 的 `Path.__str__` 在 Windows 上输出 `\`，因此同一段路径在两侧的文案不同；
/// 界面文案要与 Python 逐字一致就得跟这条规则走（不是简单的 `display()`）。
fn path_text(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text
    }
}

/// `/workspace` 带参数时的即时反馈。
///
/// 文案只有这一处来源：命令层的内联延迟执行体与宿主的慢命令 worker（TUI 把重建放
/// 到工作线程）都读它，避免两份"正在切换"。
pub const WORKSPACE_SWITCH_PENDING_MESSAGE: &str = "正在切换工作区";

/// `/workspace` 切换成功后的回执文案（`root` 是解析后的根，不是用户输入的原文）。
pub fn workspace_switch_success_message(root: &Path) -> String {
    format!("已切换工作区：{}", path_text(root))
}

/// 把新工作区写回 `config.toml`（跨进程同步：远程入口在任务开始前重读并跟随）。
///
/// 失败只提示不阻断：切换本身已经生效，持久化只是让下一个进程看到同一个根。
pub fn persist_workspace_root(agent: &dyn CommandAgent, workspace: &str) {
    let environment = agent.config_environment();
    if let Err(error) = save_workspace_root(&environment, Path::new(workspace), None) {
        eprintln!(
            "[commands] 工作区持久化到 config.toml 失败：{}",
            error.message()
        );
    }
}

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

// ── 代码评审（/review）─────────────────────────────────────────

/// 评审子任务的任务描述（写进任务快照，供 `/tasks` 展示）。
pub const REVIEW_TASK_DESCRIPTION: &str = "评审当前代码变更";

/// `/review` 不带参数时的评审范围说明。
pub const REVIEW_DEFAULT_SCOPE: &str = "工作区未提交改动（git diff）";

/// 评审前置检查：返回错误消息（应阻止评审），可评审时返回 `None`。
///
/// 覆盖两类「无意义评审」：工作区不是 git 仓库、工作区没有可评审的改动（无提交 +
/// 无未提交改动）。不做精确范围校验——非法范围由子 Agent 的 git 工具在收集 diff 时
/// 给出明确错误。
pub fn check_review_preconditions(
    agent: &dyn CommandAgent,
    workspace_root: &Path,
    scope: &str,
) -> Option<String> {
    let git_missing = || Some("未找到 git 可执行文件，请确认 Git 已安装。".to_string());

    let is_worktree = match agent.run_git(workspace_root, &["rev-parse", "--is-inside-work-tree"]) {
        Some(output) => output,
        None => return git_missing(),
    };
    if is_worktree.code != 0 || is_worktree.stdout.trim() != "true" {
        return Some("当前工作区不是 git 仓库，无法评审。".to_string());
    }

    if !scope.is_empty() {
        // 指定范围评审：至少需要仓库存在提交。
        let head = match agent.run_git(workspace_root, &["rev-parse", "--verify", "HEAD"]) {
            Some(output) => output,
            None => return git_missing(),
        };
        if head.code != 0 {
            return Some("当前仓库还没有任何提交，无法按指定范围评审。".to_string());
        }
        return None;
    }

    // 默认范围（工作区未提交改动）：存在未提交改动或未跟踪文件才值得评审。
    let status = match agent.run_git(workspace_root, &["status", "--porcelain"]) {
        Some(output) => output,
        None => return git_missing(),
    };
    if status.code != 0 {
        return Some("无法读取 git 工作区状态，无法评审。".to_string());
    }
    if status.stdout.trim().is_empty() {
        return Some("工作区没有未提交改动，无需评审。".to_string());
    }
    None
}

/// 构造评审子 Agent 的任务 prompt：只描述评审范围，评审标准由定义注入。
pub fn build_review_task_prompt(scope: &str) -> String {
    let scope_text = if scope.is_empty() {
        format!(
            "评审范围：{REVIEW_DEFAULT_SCOPE}。\
             请先用 git status 查看变更文件，再用 `git diff`（含 `git diff --cached`）收集改动。"
        )
    } else {
        format!(
            "评审范围由 /review 参数指定：`{scope}`。\
             请先使用 git 工具确认当前分支与提交历史，再用 `git diff <范围>` 收集改动。"
        )
    };
    format!(
        "请评审当前工作区的代码变更。\n{scope_text}\n\n\
         执行步骤：\n\
         1. 用 git 工具收集 diff（status/diff/log/show 等只读子命令）；\n\
         2. 必要时用 read / grep 阅读受影响的文件与上下文；\n\
         3. 严格按系统提示中的评审标准独立评审。\n\n\
         输出要求：只输出系统提示中 OUTPUT FORMAT 规定的 JSON 审查结果本身，\
         不要输出 Markdown 代码块、XML 或任何额外解释。"
    )
}

/// 从子 Agent 输出中提取评审 JSON 对象；带围栏/前后缀时宽容解析。
pub fn extract_review_json(text: &str) -> Option<Map<String, Value>> {
    let stripped = text.trim();
    if stripped.is_empty() {
        return None;
    }
    let mut candidates: Vec<String> = Vec::new();
    // 1) 整体直接解析
    candidates.push(stripped.to_string());
    // 2) 去掉 ```json ... ``` 围栏
    if let Some(captures) = review_fence_pattern().captures(stripped) {
        if let Some(group) = captures.get(1) {
            candidates.push(group.as_str().to_string());
        }
    }
    // 3) 取第一个 { 到最后一个 } 的子串
    if let (Some(first), Some(last)) = (stripped.find('{'), stripped.rfind('}')) {
        if last > first {
            candidates.push(stripped[first..=last].to_string());
        }
    }
    for candidate in candidates {
        if let Ok(parsed) = serde_json::from_str::<Value>(&candidate) {
            if let Some(object) = parsed.as_object() {
                return Some(object.clone());
            }
        }
    }
    None
}

/// 评审围栏：`r"```(?:json)?\s*(\{.*\})\s*```"`（DOTALL）。
fn review_fence_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?s)```(?:json)?\s*(\{.*\})\s*```").expect("评审围栏正则必须合法")
    })
}

/// 把 JSON 中的 priority 数值映射为 `[P0]`-`[P3]` 标签。
pub fn format_priority_tag(priority: Option<&Value>) -> String {
    let Some(priority) = priority else {
        return String::new();
    };
    let value = match priority {
        Value::Number(number) => match number.as_i64() {
            Some(value) => value,
            None => match number.as_f64() {
                Some(float) => float as i64,
                None => return String::new(),
            },
        },
        Value::String(text) => match text.trim().parse::<i64>() {
            Ok(value) => value,
            Err(_) => return String::new(),
        },
        _ => return String::new(),
    };
    if !(0..=3).contains(&value) {
        return String::new();
    }
    format!("[P{value}] ")
}

/// Python `format(value, "g")`：6 位有效数字、去掉尾随零、超出范围走科学计数法。
pub fn python_general_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if value == 0.0 {
        return "0".to_string();
    }
    const PRECISION: i32 = 6;
    let exponent = value.abs().log10().floor() as i32;
    if (-4..PRECISION).contains(&exponent) {
        let decimals = (PRECISION - 1 - exponent).max(0) as usize;
        return strip_trailing_zeros(&format!("{value:.decimals$}"));
    }
    let mantissa = value / 10f64.powi(exponent);
    let rendered = strip_trailing_zeros(&format!("{mantissa:.5}"));
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{rendered}e{sign}{:02}", exponent.abs())
}

fn strip_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    let trimmed = text.trim_end_matches('0');
    let trimmed = trimmed.trim_end_matches('.');
    trimmed.to_string()
}

/// 把评审子 Agent 返回的 JSON 渲染为可读报告；解析失败时原样展示。
pub fn format_review_report(review_text: &str) -> String {
    let Some(data) = extract_review_json(review_text) else {
        let stripped = review_text.trim();
        return if stripped.is_empty() {
            "（评审子 Agent 未返回内容。）".to_string()
        } else {
            stripped.to_string()
        };
    };

    let correctness = text_of(data.get("overall_correctness"));
    let explanation = text_of(data.get("overall_explanation")).trim().to_string();
    let confidence = data.get("overall_confidence_score");

    let correctness_norm = correctness.to_lowercase();
    let verdict = if correctness_norm.contains("incorrect") {
        "patch is incorrect ❌"
    } else if correctness_norm.contains("correct") {
        "patch is correct ✅"
    } else {
        "无法判定 ⚠️（overall_correctness 缺失或值无效）"
    };
    let confidence_text = match confidence.and_then(Value::as_f64) {
        Some(score) => format!("（置信度 {}）", python_general_float(score)),
        None => String::new(),
    };
    let mut lines = vec![
        "## 代码评审结果".to_string(),
        String::new(),
        format!("**总体结论**：{verdict}{confidence_text}"),
    ];
    if !explanation.is_empty() {
        lines.push(String::new());
        lines.push(explanation);
    }

    let findings = match data.get("findings") {
        Some(Value::Array(findings)) => findings.clone(),
        _ => {
            lines.push(String::new());
            lines.push("⚠️ 评审结果缺少有效的 findings 列表，无法展示问题明细。".to_string());
            return lines.join("\n");
        }
    };
    if findings.is_empty() {
        lines.push(String::new());
        lines.push("未发现问题。".to_string());
        return lines.join("\n");
    }

    lines.push(String::new());
    lines.push(format!("**发现问题 {} 项**：", findings.len()));
    lines.push(String::new());
    for (index, finding) in findings.iter().enumerate() {
        let number = index + 1;
        let Some(finding) = finding.as_object() else {
            continue;
        };
        let title = text_or(finding.get("title"), &format!("问题 {number}"));
        let tag = format_priority_tag(finding.get("priority"));
        let confidence_suffix = match finding.get("confidence_score").and_then(Value::as_f64) {
            Some(score) => format!("（置信度 {}）", python_general_float(score)),
            None => String::new(),
        };
        lines.push(format!("{number}. **{tag}{title}**{confidence_suffix}"));
        if let Some(location) = finding.get("code_location").and_then(Value::as_object) {
            let path = text_of(location.get("absolute_file_path"));
            let range = location.get("line_range").and_then(Value::as_object);
            let start = range
                .and_then(|map| map.get("start"))
                .and_then(Value::as_i64);
            let end = range.and_then(|map| map.get("end")).and_then(Value::as_i64);
            if !path.is_empty() {
                match (start, end) {
                    (Some(start), Some(end)) => {
                        lines.push(format!("   - 位置：`{path}` 行 {start}-{end}"))
                    }
                    _ => lines.push(format!("   - 位置：`{path}`")),
                }
            }
        }
        let body = text_of(finding.get("body")).trim().to_string();
        if !body.is_empty() {
            let indented: Vec<String> = body.lines().map(|line| format!("  {line}")).collect();
            lines.push(String::new());
            lines.push(indented.join("\n"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

/// 把评审范围转发给 review 子 Agent，并把 JSON 结果渲染为可读报告。
///
/// 主线程只解析参数并立即返回：git 预检、子 Agent 模型循环与报告注入都推迟到 `deferred` 中，
/// 由交互端的工作线程执行（`ctx.on_subagent_event` 透传给子任务，供 UI 显示审查进度）。
pub fn handle_review_command(ctx: &CommandContext<'_>) -> CommandResult {
    let scope = ctx.args.trim().to_string();
    let workspace_root = ctx.agent.workspace_root();
    let on_event = ctx.on_subagent_event.clone();

    let run_review = move |agent: &dyn CommandAgent| -> CommandResult {
        if let Some(error) = check_review_preconditions(agent, &workspace_root, &scope) {
            return CommandResult::message(error);
        }
        let report = match agent.run_subagent_task(SubAgentRun {
            agent_type: "review".to_string(),
            description: REVIEW_TASK_DESCRIPTION.to_string(),
            prompt: build_review_task_prompt(&scope),
            on_event: on_event.clone(),
        }) {
            Ok(report) => report,
            Err(error) => return CommandResult::message(format!("评审失败：{error}")),
        };
        let rendered = format_review_report(&report);
        // 报告进父模型上下文：下一轮模型请求能看到报告并继续处理（如修复、提交）。
        // 注入失败不影响报告展示。
        let _ = agent.remember_review_report(&rendered);
        CommandResult::message(rendered)
    };

    CommandResult::default()
        .with_working_status("正在评审")
        .with_stream_subagent_conversation()
        .with_deferred(Box::new(run_review))
}

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
/// `parameters` 是命令声明的可选参数 `(参数, 说明)`，供输入框提示 `--chat`。
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

#[cfg(test)]
mod tests {
    // 测试里逐字段填充假 Agent 比一次性结构体字面量更贴近用例语义，保留这种写法。
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use crate::agent::testing::FakeAgent;
    use crate::agent::GitOutput;
    use crate::framework::Channel;
    use chrono::{TimeZone, Utc};
    use omnicrawl_extensions::skill::SkillMeta;
    use omnicrawl_session::index::SessionIndexEntry;
    use omnicrawl_session::PromptHistoryEntry;
    use serde_json::json;
    use std::path::PathBuf;

    fn session_entry(session_id: &str, title: &str, day: u32) -> SessionIndexEntry {
        let created = Utc.with_ymd_and_hms(2026, 9, day, 3, 4, 5).unwrap();
        SessionIndexEntry {
            session_id: session_id.to_string(),
            title: title.to_string(),
            workspace_root: "D:/work".to_string(),
            path: format!("sessions/{session_id}.jsonl"),
            created_at: created,
            updated_at: created,
            event_count: 4,
            message_count: 2,
            last_event_type: "assistant_message".to_string(),
            archived_at: None,
        }
    }

    fn skill(name: &str, description: &str) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: description.to_string(),
            source_path: PathBuf::from("D:/work/.omnicrawl/skills/")
                .join(name)
                .join("SKILL.md"),
            base_dir: PathBuf::from("D:/work/.omnicrawl/skills/").join(name),
            scope: "project".to_string(),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn registry_declares_all_builtin_commands_in_order() {
        let registry = build_registry().expect("内置命令表必须合法");
        let names: Vec<String> = registry
            .commands(true)
            .iter()
            .map(|command| command.name.clone())
            .collect();
        assert_eq!(names.len(), 27);
        assert_eq!(names[0], "tasks");
        assert!(names.contains(&"memory:clean".to_string()));
        assert!(names.contains(&"approval:auto".to_string()));
        assert_eq!(names.last().map(String::as_str), Some("workspace"));
    }

    #[test]
    fn registry_is_built_once() {
        assert!(std::ptr::eq(registry(), registry()));
    }

    #[test]
    fn quit_is_immediate_and_workspace_defers() {
        assert!(registry()
            .parse("/quit")
            .expect("quit 可解析")
            .command
            .command_type
            .immediate());
        assert!(!registry()
            .parse("/workspace")
            .expect("workspace 可解析")
            .command
            .command_type
            .immediate());
    }

    #[test]
    fn shell_tool_confirmation_shows_command() {
        let text = format_tool_confirmation(
            "bash",
            &json!({"command": "rm -rf build"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(text.starts_with("Agent 想要执行 Bash 命令。"));
        assert!(text.contains("命令：rm -rf build"));
        assert!(text.ends_with("是否允许执行？"));
    }

    #[test]
    fn read_only_tool_confirmation_hides_arguments() {
        let text = format_tool_confirmation(
            "read",
            &json!({"path": "a.py", "offset": 10})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert_eq!(text, "Agent 想要读取文件内容。\n\n是否允许执行？");
    }

    #[test]
    fn mcp_tool_confirmation_uses_dotted_name() {
        let text = format_tool_confirmation(
            "ocr.scan",
            &json!({"path": "a.png"}).as_object().unwrap().clone(),
        );
        assert!(text.contains("执行 MCP 工具 ocr.scan"));
        assert!(text.contains("参数："));
    }

    #[test]
    fn edit_tool_confirmation_shows_replacement() {
        let text = format_tool_confirmation(
            "Edit_file",
            &json!({"path": "a.py", "old_text": "foo", "new_text": "bar"})
                .as_object()
                .unwrap()
                .clone(),
        );
        assert!(text.contains("文件：a.py"));
        assert!(text.contains("替换 \"foo\""));
        assert!(text.contains("→ \"bar\""));
    }

    #[test]
    fn python_general_float_matches_python_format_g() {
        assert_eq!(python_general_float(0.0), "0");
        assert_eq!(python_general_float(1.0), "1");
        assert_eq!(python_general_float(0.9), "0.9");
        assert_eq!(python_general_float(0.85), "0.85");
        assert_eq!(python_general_float(0.123456789), "0.123457");
        assert_eq!(python_general_float(0.00001), "1e-05");
        assert_eq!(python_general_float(1234567.0), "1.23457e+06");
    }

    #[test]
    fn truncate_keeps_character_budget() {
        assert_eq!(truncate_for_display("abcdef", 6), "abcdef");
        assert_eq!(truncate_for_display("abcdefg", 6), "abc...");
        assert_eq!(truncate_for_display("中文中文中文", 4), "中...");
    }

    #[test]
    fn extract_review_json_accepts_fenced_payload() {
        let fenced = "前缀\n```json\n{\"overall_correctness\": \"correct\"}\n```\n后缀";
        let parsed = extract_review_json(fenced).expect("应解析出 JSON 对象");
        assert_eq!(
            parsed.get("overall_correctness").and_then(Value::as_str),
            Some("correct")
        );
        assert!(extract_review_json("没有 JSON").is_none());
    }

    #[test]
    fn review_report_renders_findings() {
        let report = json!({
            "overall_correctness": "patch is incorrect",
            "overall_explanation": "边界条件漏了",
            "overall_confidence_score": 0.9,
            "findings": [
                {
                    "title": "越界写入",
                    "priority": 1,
                    "confidence_score": 0.85,
                    "code_location": {
                        "absolute_file_path": "D:/a.py",
                        "line_range": {"start": 10, "end": 12}
                    },
                    "body": "第一行\n第二行"
                }
            ]
        })
        .to_string();
        let rendered = format_review_report(&report);
        assert!(rendered.starts_with("## 代码评审结果\n"));
        assert!(rendered.contains("**总体结论**：patch is incorrect ❌（置信度 0.9）"));
        assert!(rendered.contains("边界条件漏了"));
        assert!(rendered.contains("**发现问题 1 项**："));
        assert!(rendered.contains("1. **[P1] 越界写入**（置信度 0.85）"));
        assert!(rendered.contains("   - 位置：`D:/a.py` 行 10-12"));
        assert!(rendered.contains("  第一行\n  第二行"));
    }

    #[test]
    fn review_report_falls_back_to_raw_text() {
        assert_eq!(format_review_report("  自由文本  "), "自由文本");
        assert_eq!(format_review_report("   "), "（评审子 Agent 未返回内容。）");
    }

    #[test]
    fn priority_tag_rejects_out_of_range() {
        assert_eq!(format_priority_tag(Some(&json!(0))), "[P0] ");
        assert_eq!(format_priority_tag(Some(&json!(3))), "[P3] ");
        assert_eq!(format_priority_tag(Some(&json!(4))), "");
        assert_eq!(format_priority_tag(Some(&json!(-1))), "");
        assert_eq!(format_priority_tag(Some(&json!("2"))), "[P2] ");
        assert_eq!(format_priority_tag(None), "");
    }

    #[test]
    fn review_preconditions_report_missing_git() {
        let agent = FakeAgent::default();
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("未找到 git 可执行文件，请确认 Git 已安装。".to_string())
        );
    }

    #[test]
    fn review_preconditions_reject_non_repository() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 128,
                stdout: String::new(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("当前工作区不是 git 仓库，无法评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_require_commit_for_explicit_scope() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "rev-parse --verify HEAD".to_string(),
            Some(GitOutput {
                code: 128,
                stdout: String::new(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), "HEAD~3"),
            Some("当前仓库还没有任何提交，无法按指定范围评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_skip_clean_worktree() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "\n".to_string(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            Some("工作区没有未提交改动，无需评审。".to_string())
        );
    }

    #[test]
    fn review_preconditions_pass_with_changes() {
        let mut agent = FakeAgent::default();
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: " M a.py\n".to_string(),
            }),
        );
        assert_eq!(
            check_review_preconditions(&agent, Path::new("D:/work"), ""),
            None
        );
    }

    #[test]
    fn sessions_list_marks_current_session() {
        let mut agent = FakeAgent::default();
        agent.session_id = "20260920-030405-abcdef".to_string();
        agent.sessions = vec![
            session_entry("20260920-030405-abcdef", "", 20),
            session_entry("20260919-030405-bbbbbb", "归档前", 19),
        ];
        let text = format_sessions_list(&agent);
        assert!(text.starts_with("最近会话："));
        assert!(text.contains("* 20260920-030405-abcdef"));
        assert!(text.contains("  20260919-030405-bbbbbb"));
        assert!(text.contains("未命名会话"));
        assert!(text.contains("恢复会话：/resume <session_id>"));
    }

    #[test]
    fn sessions_list_reports_empty_workspace() {
        let agent = FakeAgent::default();
        assert_eq!(format_sessions_list(&agent), "当前工作区还没有可恢复会话。");
        assert_eq!(
            format_archived_sessions_list(&agent),
            "当前工作区还没有已归档会话。"
        );
    }

    #[test]
    fn prompt_history_formats_rows_and_query_title() {
        let mut agent = FakeAgent::default();
        agent.session_id = "s1".to_string();
        agent.history = vec![PromptHistoryEntry {
            display: "  第一行\n  第二行  ".to_string(),
            timestamp: 1_700_000_000_000,
            project: "D:/work".to_string(),
            session_id: "s1".to_string(),
            pasted_contents: Map::new(),
        }];
        let text = format_prompt_history(&agent, "行");
        assert!(text.starts_with("提示历史（关键词：行）："));
        assert!(text.contains("1. * "));
        assert!(text.contains("第一行 第二行"));
        assert!(text.contains("筛选历史：/history <关键词>"));
        assert_eq!(
            format_prompt_history(&agent, "查不到"),
            "当前工作区还没有匹配的提示历史。"
        );
    }

    #[test]
    fn skills_list_reports_disabled_subsystem() {
        let agent = FakeAgent::default();
        assert_eq!(format_skills_list(&agent), "Skill 子系统未启用。");
    }

    #[test]
    fn skills_list_marks_manual_skills() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        let text = format_skills_list(&agent);
        assert!(text.starts_with("已加载 1 个 Skill："));
        assert!(text.contains("  bili-note  (project)"));
        assert!(text.contains("    提取 B 站内容"));
    }

    #[test]
    fn plugins_status_falls_back_to_interface_hint() {
        let agent = FakeAgent::default();
        assert_eq!(format_plugins_status(&agent), "插件状态接口不可用。");
    }

    #[test]
    fn memory_clean_reports_paths() {
        let mut agent = FakeAgent::default();
        agent.memory_paths = vec!["~/.omnicrawl/memory/a.md".to_string()];
        assert!(format_memory_clean_result(&agent).starts_with("已清理 1 条过期记忆："));
        let empty = FakeAgent::default();
        assert_eq!(
            format_memory_clean_result(&empty),
            "没有需要清理的过期记忆。"
        );
    }

    #[test]
    fn subagent_task_list_and_detail() {
        let mut agent = FakeAgent::default();
        agent.subagent_tasks = vec![json!({
            "task_id": "task-1",
            "agent_type": "review",
            "status": "running",
            "description": "评审当前代码变更"
        })];
        let text = format_subagent_tasks_list(&agent);
        assert!(text.contains("- task-1 · review · running · 评审当前代码变更"));
        assert!(text.contains("查看详情：/task <task_id>"));

        agent.task_snapshot = Some(json!({
            "task_id": "task-1",
            "status": "succeeded",
            "agent_type": "review",
            "description": "评审当前代码变更",
            "result": {"summary": " 没问题 ", "artifacts": ["a.md"]}
        }));
        let detail = format_subagent_task(&agent, "task-1");
        assert!(detail.contains("任务：task-1"));
        assert!(detail.contains("摘要：\n没问题"));
        assert!(detail.contains("关联 artifact：1 项"));
    }

    #[test]
    fn remote_channel_rejects_auto_approval() {
        let agent = FakeAgent::default();
        let result = registry().dispatch("/approval:auto", &agent, Channel::Telegram, None);
        let message = result.message.unwrap_or_default();
        assert!(message.starts_with("❌ 远程不支持完全自动批准。"));
        assert!(agent.calls().iter().all(|call| call != "set_approval_mode"));
    }

    #[test]
    fn quit_and_settings_follow_channel_capabilities() {
        let agent = FakeAgent::default();
        let remote_quit = registry().dispatch("/quit", &agent, Channel::Telegram, None);
        assert!(remote_quit
            .message
            .unwrap_or_default()
            .contains("远程连接不支持 /quit"));
        assert!(!remote_quit.exit_requested);

        let local_quit = registry().dispatch("/quit", &agent, Channel::Tui, None);
        assert!(local_quit.exit_requested);

        let remote_settings = registry().dispatch("/settings", &agent, Channel::Telegram, None);
        assert!(remote_settings
            .message
            .unwrap_or_default()
            .contains("远程连接不支持 /settings"));
        assert!(!remote_settings.open_settings);
    }

    #[test]
    fn settings_supports_chat_switch_and_rejects_extra_arguments() {
        let agent = FakeAgent::default();
        let chat = registry().dispatch("/settings --chat", &agent, Channel::Tui, None);
        assert!(chat.open_config_chat);
        assert!(chat.clear_conversation);

        let panel = registry().dispatch("/settings", &agent, Channel::Tui, None);
        assert!(panel.open_settings);

        let invalid = registry().dispatch("/settings --other", &agent, Channel::Tui, None);
        assert_eq!(
            invalid.error.as_deref(),
            Some("/settings 只支持可选参数 --chat。")
        );
    }

    #[test]
    fn compact_rejects_arguments_and_defers_work() {
        let agent = FakeAgent::default();
        let invalid = registry().dispatch("/compact now", &agent, Channel::Tui, None);
        assert_eq!(invalid.message.as_deref(), Some("参数错误：/compact。"));

        let slow = registry().dispatch("/compact", &agent, Channel::Tui, None);
        assert_eq!(slow.message.as_deref(), Some("正在压缩当前会话上下文…"));
        assert_eq!(slow.working_status.as_deref(), Some("正在压缩上下文"));
        assert!(slow.deferred.is_some());
        let resolved = slow.resolve(&agent);
        assert!(resolved
            .message
            .unwrap_or_default()
            .contains("已压缩当前会话，完整转录仍保留"));
        assert!(agent
            .calls()
            .iter()
            .any(|call| call == "compact_conversation_model"));
    }

    #[test]
    fn mcp_status_is_read_inside_deferred() {
        let mut agent = FakeAgent::default();
        agent.mcp_status = "MCP：已启用".to_string();
        let result = registry().dispatch("/mcp", &agent, Channel::Tui, None);
        assert_eq!(result.message.as_deref(), Some("正在读取 MCP 状态"));
        let resolved = result.resolve(&agent);
        assert_eq!(resolved.message.as_deref(), Some("MCP：已启用"));
    }

    #[test]
    fn workspace_without_arguments_is_read_only() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        let result = registry().dispatch("/workspace", &agent, Channel::Tui, None);
        let message = result.message.unwrap_or_default();
        // 文案按 Python `str(Path)` 渲染：Windows 上是反斜杠。
        assert!(
            message.contains(&format!("当前工作区：{}", path_text(&agent.workspace))),
            "{message}"
        );
        assert!(result.deferred.is_none());
    }

    #[test]
    fn workspace_switch_defers_and_reports_new_root() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        let result = registry().dispatch("/workspace D:/other", &agent, Channel::Tui, None);
        assert!(result.workspace_switch_requested);
        assert!(result.deferred.is_some());
        let resolved = result.resolve(&agent);
        // 切换后的文案用 agent 返回的解析结果，而不是用户输入的原文。
        let expected = format!("已切换工作区：{}", path_text(Path::new("D:/other")));
        assert!(
            resolved.message.unwrap_or_default().contains(&expected),
            "{expected}"
        );
        assert!(agent.calls().iter().any(|call| call == "switch_workspace"));
    }

    #[test]
    fn review_runs_subagent_through_deferred() {
        let mut agent = FakeAgent::default();
        agent.workspace = PathBuf::from("D:/work");
        agent.git.insert(
            "rev-parse --is-inside-work-tree".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: "true\n".to_string(),
            }),
        );
        agent.git.insert(
            "status --porcelain".to_string(),
            Some(GitOutput {
                code: 0,
                stdout: " M a.py\n".to_string(),
            }),
        );
        agent.subagent_report = "自由文本".to_string();
        let result = registry().dispatch("/review", &agent, Channel::Tui, None);
        assert!(result.message.is_none());
        assert_eq!(result.working_status.as_deref(), Some("正在评审"));
        assert!(result.stream_subagent_conversation);
        let resolved = result.resolve(&agent);
        assert_eq!(resolved.message.as_deref(), Some("自由文本"));
        let calls = agent.calls();
        assert!(calls.iter().any(|call| call == "subagent_type:review"));
        assert!(calls
            .iter()
            .any(|call| call.starts_with("subagent_prompt:请评审当前工作区的代码变更。")));
        assert_eq!(agent.review_report(), "自由文本");
    }

    #[test]
    fn slash_commands_and_options_append_skills() {
        let mut agent = FakeAgent::default();
        agent.skills = Some(vec![skill("bili-note", "提取 B 站内容")]);
        let commands = build_slash_commands(&agent);
        assert!(commands.contains(&"/skill:bili-note".to_string()));
        assert!(commands.contains(&"/undo".to_string()));
        assert!(!commands.contains(&"/secret".to_string()));

        let options = build_slash_command_options(&agent);
        let skill_option = options
            .iter()
            .find(|option| option.command == "/skill:bili-note")
            .expect("Skill 候选");
        assert_eq!(skill_option.category, crate::framework::SKILL_CATEGORY);
        assert!(skill_option.parameters.is_none());
        let builtin = options
            .iter()
            .find(|option| option.command == "/undo")
            .expect("内置候选");
        assert_eq!(builtin.insert, "/undo");
        assert_eq!(builtin.parameters, Some(Vec::new()));
    }

    #[test]
    fn task_command_reports_cancel_outcomes() {
        let mut agent = FakeAgent::default();
        agent.cancel_result = json!({"ok": true, "status": "cancelled"});
        let cancelled = registry().dispatch("/task cancel task-1", &agent, Channel::Tui, None);
        assert_eq!(
            cancelled.message.as_deref(),
            Some("已取消后台子任务：task-1。")
        );

        agent.cancel_result = json!({"ok": false});
        let missing = registry().dispatch("/task cancel task-9", &agent, Channel::Tui, None);
        assert_eq!(
            missing.message.as_deref(),
            Some("未找到当前会话的 SubAgent 任务。")
        );

        let usage = registry().dispatch("/task", &agent, Channel::Tui, None);
        assert_eq!(
            usage.message.as_deref(),
            Some("用法：/task <task_id>；/task cancel <task_id>。")
        );
    }
}
