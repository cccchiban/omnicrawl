//! 工具调用的用户界面显示标签（对映 `omnicrawl/ui/tool_labels.py`）。
//!
//! 工具卡标题直接显示工具原名（英文，与模型路由/工具配置面板一致），本模块只
//! 把内部名称映射为图标等装饰信息。

/// 工具在终端标题中的显示信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDisplay {
    pub name: String,
    pub icon: String,
}

/// 工具执行状态及其语义图标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatus {
    pub icon: String,
    pub label: String,
}

const TOOL_DISPLAY_ICONS: &[(&str, &str)] = &[
    ("list", "L"),
    ("find", "F"),
    ("read", "R"),
    ("read_image", "▧"),
    ("grep", "G"),
    ("web_search", "W"),
    ("fetcher", "⇣"),
    ("image_gen", "✦"),
    ("tts_synthesize", "♪"),
    ("Edit_file", "✎"),
    ("write_file", "✚"),
    ("bash", "B"),
    ("powershell", "P"),
    ("monitor", "◌"),
    ("git", "G"),
    ("windows_window", "▥"),
    ("windows_control", "⚙"),
    ("windows_input", "⌨"),
    ("windows_clipboard", "▣"),
    ("windows_screenshot", "▧"),
    ("subagent", "◇"),
    ("update_todos", "☑"),
    ("memory_search", "◎"),
    ("memory_read", "◎"),
    ("memory_expand_related", "◎"),
    ("memory_write", "◎"),
];

const MCP_OPERATION_ICONS: &[(&str, &str)] = &[
    ("list", "L"),
    ("read", "R"),
    ("grep", "G"),
    ("Edit_file", "✎"),
    ("write_file", "✚"),
    ("bash", "B"),
    ("powershell", "P"),
    ("git", "G"),
];

fn lookup(table: &'static [(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, icon)| *icon)
}

/// 将内部工具名转换为显示名和图标。
///
/// 显示名固定为工具原名；MCP 工具保留完整命名空间原名（`server.operation`）。
pub fn tool_display(tool_name: &str) -> ToolDisplay {
    let name = tool_name;
    if let Some(icon) = lookup(TOOL_DISPLAY_ICONS, name) {
        return ToolDisplay {
            name: name.to_string(),
            icon: icon.to_string(),
        };
    }

    if let Some((_, operation)) = name.rsplit_once('.') {
        if let Some(icon) = lookup(MCP_OPERATION_ICONS, operation) {
            return ToolDisplay {
                name: name.to_string(),
                icon: icon.to_string(),
            };
        }
    }

    if name.starts_with("mcp_read_resource__") {
        return ToolDisplay {
            name: name.to_string(),
            icon: "▤".to_string(),
        };
    }
    if name.starts_with("mcp_get_prompt__") {
        return ToolDisplay {
            name: name.to_string(),
            icon: "◇".to_string(),
        };
    }
    ToolDisplay {
        name: if name.is_empty() {
            "未知工具".to_string()
        } else {
            name.to_string()
        },
        icon: "⌁".to_string(),
    }
}

/// 为状态添加文字图标，文字仍保留以支持无障碍和无色终端。
pub fn format_tool_status(status: &str) -> ToolStatus {
    let icon = match status {
        "成功" => "✓",
        "失败" => "✗",
        "调用中" => "…",
        "等待确认" => "!",
        "已取消" => "↷",
        _ => "·",
    };
    ToolStatus {
        icon: icon.to_string(),
        label: status.to_string(),
    }
}

/// 按时长选择更易读的单位。
pub fn format_duration(duration_seconds: f64) -> String {
    let duration = duration_seconds.max(0.0);
    if duration < 1.0 {
        return format!("{:.0}ms", duration * 1000.0);
    }
    if duration < 60.0 {
        return format!("{duration:.1}s");
    }
    let total_seconds = duration as i64;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes}m {seconds:02}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_keep_original_name_and_icon() {
        let display = tool_display("Edit_file");
        assert_eq!(display.name, "Edit_file");
        assert_eq!(display.icon, "✎");
        assert_eq!(tool_display("read_image").icon, "▧");
    }

    #[test]
    fn mcp_namespaced_tools_reuse_operation_icons() {
        let display = tool_display("github.grep");
        assert_eq!(display.name, "github.grep");
        assert_eq!(display.icon, "G");
        assert_eq!(tool_display("filesystem.read").icon, "R");
        // 命名空间内没有登记的操作名回退到 ⌁，而裸操作名不算 MCP。
        assert_eq!(tool_display("custom.operation").icon, "⌁");
        assert_eq!(tool_display("grep").icon, "G");
    }

    #[test]
    fn mcp_resource_and_prompt_prefixes_have_own_icons() {
        assert_eq!(tool_display("mcp_read_resource__x").icon, "▤");
        assert_eq!(tool_display("mcp_get_prompt__y").icon, "◇");
    }

    #[test]
    fn unknown_and_empty_names_fall_back() {
        assert_eq!(tool_display("whatever").icon, "⌁");
        assert_eq!(tool_display("whatever").name, "whatever");
        assert_eq!(tool_display("").name, "未知工具");
    }

    #[test]
    fn status_icons_match_python_labels() {
        assert_eq!(format_tool_status("成功").icon, "✓");
        assert_eq!(format_tool_status("失败").icon, "✗");
        assert_eq!(format_tool_status("调用中").icon, "…");
        assert_eq!(format_tool_status("等待确认").icon, "!");
        assert_eq!(format_tool_status("已取消").icon, "↷");
        assert_eq!(format_tool_status("等待回复").icon, "·");
        assert_eq!(format_tool_status("成功").label, "成功");
    }

    #[test]
    fn duration_switches_units() {
        assert_eq!(format_duration(0.0), "0ms");
        assert_eq!(format_duration(0.4567), "457ms");
        assert_eq!(format_duration(-1.0), "0ms");
        assert_eq!(format_duration(1.0), "1.0s");
        assert_eq!(format_duration(59.94), "59.9s");
        assert_eq!(format_duration(60.0), "1m 00s");
        assert_eq!(format_duration(125.0), "2m 05s");
    }
}
