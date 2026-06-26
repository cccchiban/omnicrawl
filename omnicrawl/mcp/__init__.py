"""MCP 子系统的配置、能力注册和客户端管理入口。"""

from .client import (
    MCPClientManager,
    MCPClientError,
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
