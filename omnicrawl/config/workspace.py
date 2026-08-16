"""工作区运行配置：持久化 ``[workspace] root`` 供多入口共享。

设计动机：TUI 内 ``/workspace`` 切换只更新进程内 Agent 状态，其他进程
（如远程 Telegram Bot）无法感知。把当前工作区根目录写回 config.toml 的
``[workspace] root``，其他入口在合适时机重读即可实现跨进程工作区同步。

优先级说明：``AI_WORKSPACE_ROOT`` 环境变量在 ``detect_project_context``
中优先级最高；此处持久化的值用于运行中同步，不改变启动时的检测规则。
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data

WORKSPACE_SECTION = "workspace"
WORKSPACE_ROOT_KEY = "root"


def load_workspace_root(config_path: str | Path | None = None) -> str | None:
    """读取 config.toml 的 [workspace] root；未配置或值为空时返回 None。"""

    try:
        data = load_config_data(config_path)
        section = get_section(data, WORKSPACE_SECTION)
    except RuntimeConfigError:
        raise
    value = section.get(WORKSPACE_ROOT_KEY)
    if value is None:
        return None
    if not isinstance(value, str) or not value.strip():
        return None
    return value.strip()


def save_workspace_root(path: str | Path, config_path: str | Path | None = None) -> Path:
    """把工作区根目录写回 config.toml 的 [workspace] root，保留已有配置项。

    路径会 expanduser 并 resolve 为绝对路径；目录不存在也允许写入，
    由调用方（switch_workspace）先行校验可切换性。
    """

    root = os.path.abspath(os.path.expanduser(str(path)))
    data = load_config_data(config_path)
    section = get_section(data, WORKSPACE_SECTION)
    section[WORKSPACE_ROOT_KEY] = root
    data[WORKSPACE_SECTION] = section
    return save_config_data(data, config_path)
