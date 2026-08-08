"""内置工具开关配置：逐工具控制 Agent 可见的工具集合。

默认除 ``powershell`` 关闭外，其余内置工具全部启用；用户可在
config.yaml 的 ``tools`` 段覆盖任意工具开关。开关只影响 Agent
工具表的注册（模型不可见即不可调用），不影响审批等其他配置。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data


class ToolSwitchConfigError(RuntimeConfigError):
    """工具开关配置读取、校验或写回失败。"""


# 可开关的内置工具（含条件注册工具；未注册条件下配置开关不会报错）。
TOOL_SWITCH_KEYS: tuple[str, ...] = (
    "list_files",
    "find_files",
    "read_file",
    "read_image",
    "search_text",
    "replace_text",
    "write_file",
    "bash",
    "powershell",
    "monitor",
    "recall_session_evidence",
    "windows_window",
    "windows_control",
    "windows_input",
    "windows_clipboard",
    "windows_screenshot",
    "subagent",
    "project_memory_search",
    "project_memory_read",
    "project_memory_expand_related",
    "project_memory_write",
    "session_memory_search",
    "session_memory_read",
    "session_memory_expand_related",
    "session_memory_write",
    "user_memory_search",
    "user_memory_read",
    "user_memory_expand_related",
    "user_memory_write",
)

# 默认开关：除 powershell 外全部启用。
TOOL_SWITCH_DEFAULTS: dict[str, bool] = {
    name: True for name in TOOL_SWITCH_KEYS
}
TOOL_SWITCH_DEFAULTS["powershell"] = False

TOOL_SWITCH_LABELS: dict[str, str] = {
    "list_files": "列出目录内容",
    "find_files": "按名称或路径查找文件",
    "read_file": "读取文件内容",
    "read_image": "读取图片",
    "search_text": "在文件中搜索文本",
    "replace_text": "替换文件中的文本",
    "write_file": "写入文件",
    "bash": "执行 Bash 命令",
    "powershell": "执行 PowerShell 命令",
    "monitor": "管理后台命令",
    "recall_session_evidence": "会话证据恢复",
    "windows_window": "操作 Windows 窗口",
    "windows_control": "操作 Windows UI 控件",
    "windows_input": "模拟 Windows 鼠标键盘",
    "windows_clipboard": "操作 Windows 剪贴板",
    "windows_screenshot": "截取 Windows 桌面",
    "subagent": "分发受限子任务",
    "project_memory_search": "搜索项目级记忆",
    "project_memory_read": "读取项目级记忆",
    "project_memory_expand_related": "展开项目级相关记忆",
    "project_memory_write": "写入项目级记忆",
    "session_memory_search": "搜索会话记忆",
    "session_memory_read": "读取会话记忆",
    "session_memory_expand_related": "展开会话相关记忆",
    "session_memory_write": "写入会话记忆",
    "user_memory_search": "搜索用户级记忆",
    "user_memory_read": "读取用户级记忆",
    "user_memory_expand_related": "展开用户级相关记忆",
    "user_memory_write": "写入用户级记忆",
}


def validate_tool_switch_name(name: str) -> str:
    """校验工具名是否为可开关的内置工具。"""

    normalized = str(name).strip()
    if normalized not in TOOL_SWITCH_DEFAULTS:
        raise ToolSwitchConfigError(
            f"tools 配置不支持工具：{normalized}。可用：{', '.join(TOOL_SWITCH_KEYS)}。"
        )
    return normalized


def load_tool_switches(config_path: str | Path | None = None) -> dict[str, bool]:
    """读取 ``tools`` 段并与默认开关合并，返回完整工具开关表。"""

    try:
        section = get_section(load_config_data(config_path), "tools")
    except RuntimeConfigError as exc:
        raise ToolSwitchConfigError(str(exc)) from exc

    switches = dict(TOOL_SWITCH_DEFAULTS)
    for raw_name, value in section.items():
        name = validate_tool_switch_name(raw_name)
        if not isinstance(value, bool):
            raise ToolSwitchConfigError(f"配置项 tools.{name} 必须是布尔值。")
        switches[name] = value
    return switches


def load_disabled_tools(config_path: str | Path | None = None) -> frozenset[str]:
    """读取配置并返回当前禁用的内置工具名集合。"""

    return frozenset(
        name for name, enabled in load_tool_switches(config_path).items() if not enabled
    )


def save_tool_switch(
    name: str,
    enabled: bool,
    config_path: str | Path | None = None,
) -> Path:
    """保留其他配置段，只更新单个工具开关并原子写回。"""

    normalized_name = validate_tool_switch_name(name)
    if not isinstance(enabled, bool):
        raise ToolSwitchConfigError(f"配置项 tools.{normalized_name} 必须是布尔值。")
    try:
        data: dict[str, Any] = load_config_data(config_path)
        section = get_section(data, "tools")
        section[normalized_name] = enabled
        data["tools"] = section
        return save_config_data(data, config_path)
    except RuntimeConfigError as exc:
        raise ToolSwitchConfigError(str(exc)) from exc


__all__ = [
    "TOOL_SWITCH_KEYS",
    "TOOL_SWITCH_DEFAULTS",
    "TOOL_SWITCH_LABELS",
    "ToolSwitchConfigError",
    "load_disabled_tools",
    "load_tool_switches",
    "save_tool_switch",
    "validate_tool_switch_name",
]
