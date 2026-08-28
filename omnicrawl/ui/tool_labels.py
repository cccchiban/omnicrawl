"""工具调用的用户界面显示标签。

工具卡标题直接显示工具原名（英文，与模型路由/工具配置面板一致），
不再翻译成中文文案；本模块只负责把内部名称映射为图标等装饰信息。
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class ToolDisplay:
    """工具在终端标题中的显示信息。"""

    name: str
    icon: str


_TOOL_DISPLAY_ICONS = {
    "list": "L",
    "find": "F",
    "read": "R",
    "read_image": "▧",
    "grep": "G",
    "web_search": "W",
    "fetcher": "⇣",
    "image_gen": "✦",
    "tts_synthesize": "♪",
    "Edit_file": "✎",
    "write_file": "✚",
    "bash": "B",
    "powershell": "P",
    "monitor": "◌",
    "git": "G",
    "windows_window": "▥",
    "windows_control": "⚙",
    "windows_input": "⌨",
    "windows_clipboard": "▣",
    "windows_screenshot": "▧",
    "subagent": "◇",
    "update_todos": "☑",
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

_MCP_OPERATION_ICONS = {
    "list": "L",
    "read": "R",
    "grep": "G",
    "Edit_file": "✎",
    "write_file": "✚",
    "bash": "B",
    "powershell": "P",
    "git": "G",
}


@dataclass(frozen=True)
class ToolStatus:
    """工具执行状态及其语义图标。"""

    icon: str
    label: str


def tool_display(tool_name: str) -> ToolDisplay:
    """将内部工具名转换为显示名和图标。

    显示名固定为工具原名（英文，与模型路由/工具配置面板一致）：工具卡
    标题直接展示模型实际调用的工具标识；MCP 工具保留完整命名空间原名
    （server.operation），便于与 MCP 配置对应。图标保留既有映射，仅作
    语义装饰。
    """

    name = str(tool_name or "")
    if name in _TOOL_DISPLAY_ICONS:
        return ToolDisplay(name=name, icon=_TOOL_DISPLAY_ICONS[name])

    operation = name.rsplit(".", 1)[-1]
    if "." in name and operation in _MCP_OPERATION_ICONS:
        return ToolDisplay(name=name, icon=_MCP_OPERATION_ICONS[operation])

    if name.startswith("mcp_read_resource__"):
        return ToolDisplay(name=name, icon="▤")
    if name.startswith("mcp_get_prompt__"):
        return ToolDisplay(name=name, icon="◇")
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
