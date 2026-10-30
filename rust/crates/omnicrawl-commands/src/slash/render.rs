//! 确认提示与各类列表的展示文案：只读 `CommandAgent` 与参数表、产出字符串，不改状态。
//!
//! 从 `slash.rs` 拆出。

use super::*;

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
        "memory_delete" => "删除长期记忆",
        "subagent" => "分发只读子任务",
        "advisor" => "咨询顾问模型获取第二意见",
        _ => return None,
    };
    Some(description)
}

/// 截断过长文本，保留可读的关键部分（按字符数，与 Python 的 `len()` 同口径）。
pub(crate) fn truncate_for_display(text: &str, max_len: usize) -> String {
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
pub(crate) fn python_falsy(value: &Value) -> bool {
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
pub(crate) fn text_or(value: Option<&Value>, fallback: &str) -> String {
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
pub(crate) fn text_of(value: Option<&Value>) -> String {
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
pub fn format_dangerous_tool_detail(tool_name: &str, arguments: &Map<String, Value>) -> String {
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
        if let Some(Value::Array(actions)) = arguments.get("actions") {
            // 顺序动作只报每一步的动作名：文本已经只留长度。
            let names: Vec<&str> = actions
                .iter()
                .map(|item| item.get("action").and_then(Value::as_str).unwrap_or(""))
                .collect();
            return truncate_for_display(
                &format!(
                    "操作：{} 个动作，依次：{}",
                    actions.len(),
                    names.join(" → ")
                ),
                240,
            );
        }
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
        // 桌面目标允许只给部分区域参数（缺的按桌面边补齐），因此逐个展示已有的键。
        let region: Vec<String> = ["x", "y", "width", "height"]
            .iter()
            .filter(|key| has_argument(arguments, key))
            .map(|key| format!("{key}={}", python_str(&arguments[*key])))
            .collect();
        if !region.is_empty() {
            detail.push_str(&format!("，区域：{}", region.join(" ")));
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

// ── Skill / 记忆 / MCP 展示 ───────────────────────────────────

/// 格式化 Skill 列表为可展示文本。
pub fn format_skills_list(agent: &dyn CommandAgent) -> String {
    format_skills_list_with(agent, true)
}

/// 只列 Skill 名称，不带 description：给单条消息有字数上限的入口（QQ 机器人）用。
pub fn format_skills_list_brief(agent: &dyn CommandAgent) -> String {
    format_skills_list_with(agent, false)
}

fn format_skills_list_with(agent: &dyn CommandAgent, with_description: bool) -> String {
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
        if with_description && !meta.description.trim().is_empty() {
            lines.push(format!("    {}", meta.description));
        }
    }
    // 发现阶段的诊断直接跟在列表后：路径写错、名称冲突、名称不合法时用户能当场看到原因。
    if let Some(diagnostics) = agent.skill_diagnostics() {
        if !diagnostics.is_empty() {
            lines.push(format!("发现诊断 {} 条：", diagnostics.len()));
            for diagnostic in &diagnostics {
                lines.push(format!(
                    "  [!] {}（{}）",
                    diagnostic.message, diagnostic.path
                ));
            }
        }
    }
    lines.join("\n")
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

/// 格式化 MCP 状态为可展示文本。
pub fn format_mcp_status(agent: &dyn CommandAgent) -> String {
    agent.mcp_status()
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

pub(crate) fn title_or_unnamed(title: &str) -> String {
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
