"""全屏 TUI 的三级 MCP 设置界面。

P3 重构后本文件保留共享基础（MCPSettingsAction、配置读写辅助函数）
并 re-export 三个拆出的屏幕类，外部导入路径保持不变：
  from .mcp_settings import MCPSettingsScreen / MCPServerListScreen / MCPServerEditorScreen
三个屏幕类分别定义在 mcp_settings_screen.py / mcp_server_list_screen.py /
mcp_server_editor_screen.py。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from textual.screen import ModalScreen

from ....config.core.settings import save_mcp_config
from ....mcp.config import (
    MCPConfig,
    MCP_RISK_EXTERNAL,
    MCP_RISK_RESTRICTED,
    MCP_RISK_TRUSTED,
    MCP_TRANSPORT_STDIO,
    MCP_TRANSPORT_STREAMABLE_HTTP,
    load_mcp_config,
)


@dataclass(frozen=True)
class MCPSettingsAction:
    name: str


_RISKS = (MCP_RISK_TRUSTED, MCP_RISK_RESTRICTED, MCP_RISK_EXTERNAL)
_TRANSPORTS = (MCP_TRANSPORT_STDIO, MCP_TRANSPORT_STREAMABLE_HTTP)


def _current_config(agent: Any) -> MCPConfig:
    manager = getattr(agent, "_mcp_manager", None)
    config = getattr(agent, "config", None)
    candidate = getattr(config, "mcp_config", None) or getattr(manager, "config", None)
    return candidate if isinstance(candidate, MCPConfig) else load_mcp_config()


def _apply_and_save(screen: ModalScreen[Any], config: MCPConfig) -> str:
    """应用配置并写盘；写盘失败时回滚当前运行态。"""

    previous = _current_config(screen._agent)
    apply_config = getattr(screen._agent, "apply_mcp_config", None)
    if callable(apply_config):
        apply_config(config)
    else:
        screen._agent.config.mcp_config = config
    try:
        return str(save_mcp_config(config))
    except Exception:
        try:
            if callable(apply_config):
                apply_config(previous)
            else:
                screen._agent.config.mcp_config = previous
        except Exception:
            pass
        raise


from .mcp_server_editor_screen import MCPServerEditorScreen
from .mcp_server_list_screen import MCPServerListScreen
from .mcp_settings_screen import MCPSettingsScreen


__all__ = ["MCPSettingsAction", "MCPSettingsScreen", "MCPServerEditorScreen", "MCPServerListScreen"]
