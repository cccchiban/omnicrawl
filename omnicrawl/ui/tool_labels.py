"""工具调用的用户界面显示标签。

工具的内部名称用于模型路由和唯一标识，不应直接作为主要 UI 文案。
本模块只负责将内部名称转换为简洁、稳定的显示标签。
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class ToolDisplay:
    """工具在终端标题中的显示信息。"""

    name: str
    icon: str


_TOOL_DISPLAY_NAMES = {
    "list_files": "列出文件",
    "find_files": "查找文件",
    "read_file": "读取文件",
    "read_image": "读取图片",
    "grep": "搜索文本",
    "web_search": "网页搜索",
    "replace_text": "替换文本",
    "write_file": "写入文件",
    "bash": "执行 Bash",
    "powershell": "执行 PowerShell",
    "monitor": "监控任务",
    "windows_window": "管理窗口",
    "windows_control": "操作控件",
    "windows_input": "输入操作",
    "windows_clipboard": "操作剪贴板",
    "windows_screenshot": "截取屏幕",
    "subagent": "运行子代理",
    "memory_search": "搜索记忆",
    "memory_read": "读取记忆",
    "memory_expand_related": "扩展关联记忆",
    "memory_write": "写入记忆",
    "project_memory_search": "搜索项目记忆",
    "project_memory_read": "读取项目记忆",
    "project_memory_expand_related": "扩展项目记忆",
    "project_memory_write": "写入项目记忆",
    "session_memory_search": "搜索会话记忆",
    "session_memory_read": "读取会话记忆",
    "session_memory_expand_related": "扩展会话记忆",
    "session_memory_write": "写入会话记忆",
    "user_memory_search": "搜索用户记忆",
    "user_memory_read": "读取用户记忆",
    "user_memory_expand_related": "扩展用户记忆",
    "user_memory_write": "写入用户记忆",
}

_TOOL_DISPLAY_ICONS = {
    "list_files": "L",
    "find_files": "F",
    "read_file": "R",
    "read_image": "▧",
    "grep": "G",
    "web_search": "W",
    "replace_text": "✎",
    "write_file": "✚",
    "bash": "B",
    "powershell": "P",
    "monitor": "◌",
    "windows_window": "▥",
    "windows_control": "⚙",
    "windows_input": "⌨",
    "windows_clipboard": "▣",
    "windows_screenshot": "▧",
    "subagent": "◇",
    "memory_search": "◎",
    "memory_read": "◎",
    "memory_expand_related": "◎",
    "memory_write": "◎",
    "project_memory_search": "◎",
    "project_memory_read": "◎",
    "project_memory_expand_related": "◎",
    "project_memory_write": "◎",
    "session_memory_search": "◎",
    "session_memory_read": "◎",
    "session_memory_expand_related": "◎",
    "session_memory_write": "◎",
    "user_memory_search": "◎",
    "user_memory_read": "◎",
    "user_memory_expand_related": "◎",
    "user_memory_write": "◎",
}

_MCP_OPERATION_NAMES = {
    "list_files": "列出文件",
    "read_file": "读取文件",
    "grep": "搜索文本",
    "replace_text": "替换文本",
    "write_file": "写入文件",
    "bash": "执行 Bash",
    "powershell": "执行 PowerShell",
}

_MCP_OPERATION_ICONS = {
    "list_files": "L",
    "read_file": "R",
    "grep": "G",
    "replace_text": "✎",
    "write_file": "✚",
    "bash": "B",
    "powershell": "P",
}


@dataclass(frozen=True)
class ToolStatus:
    """工具执行状态及其语义图标。"""

    icon: str
    label: str


def tool_display(tool_name: str) -> ToolDisplay:
    """将内部工具名转换为简洁的显示名和图标。

    MCP 工具通常以 ``server.namespace.operation`` 命名，因此只对已知
    operation 做友好化；无法识别的工具保留完整原名，便于诊断。
    """

    name = str(tool_name or "")
    if name in _TOOL_DISPLAY_NAMES:
        return ToolDisplay(
            name=_TOOL_DISPLAY_NAMES[name],
            icon=_TOOL_DISPLAY_ICONS[name],
        )

    operation = name.rsplit(".", 1)[-1]
    if operation in _MCP_OPERATION_NAMES and "." in name:
        return ToolDisplay(
            name=_MCP_OPERATION_NAMES[operation],
            icon=_MCP_OPERATION_ICONS[operation],
        )

    if name.startswith("mcp_read_resource__"):
        return ToolDisplay(name="读取资源", icon="▤")
    if name.startswith("mcp_get_prompt__"):
        return ToolDisplay(name="获取提示词", icon="◇")
    return ToolDisplay(name=name or "未知工具", icon="⌁")


def format_tool_status(status: str) -> ToolStatus:
    """为状态添加文字图标，文字仍保留以支持无障碍和无色终端。"""

    labels = {
        "成功": "✓",
        "失败": "✗",
        "调用中": "…",
        "等待确认": "!",
        "已取消": "↷",
    }
    return ToolStatus(icon=labels.get(status, "·"), label=status)


def format_duration(duration_seconds: float) -> str:
    """按时长选择更易读的单位。"""

    duration = max(0.0, float(duration_seconds))
    if duration < 1:
        return f"{duration * 1000:.0f}ms"
    if duration < 60:
        return f"{duration:.1f}s"
    minutes, seconds = divmod(int(duration), 60)
    return f"{minutes}m {seconds:02d}s"


__all__ = [
    "ToolDisplay",
    "ToolStatus",
    "format_duration",
    "format_tool_status",
    "tool_display",
]
