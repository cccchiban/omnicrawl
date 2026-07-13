"""OmniCrawl 本地 HTTP/SSE 接口公共入口。

具体实现按职责位于独立子模块；本入口只保留稳定公共 API。
"""

from __future__ import annotations

from .app import create_app, create_default_agent, load_api_config
from .models import APIConfig, APIServiceError
from .service import AgentAPIService

__all__ = [
    "APIConfig",
    "APIServiceError",
    "AgentAPIService",
    "create_app",
    "create_default_agent",
    "load_api_config",
]
