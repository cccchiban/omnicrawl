"""MCP 子系统公共入口。

具体实现按职责位于独立子模块；本入口只保留稳定公共 API。
"""

from .client import (
    MCPClientError,
    MCPClientManager,
    MCPPromptReadResult,
    MCPResourceReadResult,
    MCPToolCallResult,
)
from .config import (
    MCPConfig,
    MCPConfigError,
    MCPPolicyConfig,
    MCPServerConfig,
    load_mcp_config,
)
from .registry import (
    MCPCapabilityRegistry,
    MCPDiagnostic,
    MCPPromptMeta,
    MCPResourceMeta,
    MCPToolMeta,
)

__all__ = [
    "MCPCapabilityRegistry",
    "MCPClientError",
    "MCPClientManager",
    "MCPConfig",
    "MCPConfigError",
    "MCPDiagnostic",
    "MCPPolicyConfig",
    "MCPPromptMeta",
    "MCPPromptReadResult",
    "MCPResourceMeta",
    "MCPResourceReadResult",
    "MCPServerConfig",
    "MCPToolCallResult",
    "MCPToolMeta",
    "load_mcp_config",
]
