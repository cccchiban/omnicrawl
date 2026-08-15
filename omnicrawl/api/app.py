"""FastAPI 应用工厂与本地配置装载。"""

from __future__ import annotations

import logging
import os
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, Callable

from fastapi import FastAPI, Request, status
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse

from ..agent import AgentConfig, LocalToolAgent
from ..state.session_artifacts import redact_sensitive_text
from ..config.llm import load_llm_config
from ..config.runtime import get_section, load_config_data
from ..config.settings import load_feature_enabled
from ..config.router import load_router_mode
from ..config.subagents import load_subagent_config
from ..workspace.context import (
    detect_project_context,
    should_disable_broad_workspace_indexes,
)
from ..workspace.temp import load_agent_temp_workspace_config
from .deps import data, error_response
from .models import APIConfig, APIServiceError
from .routes import build_api_router
from .service import AgentAPIService


LOGGER = logging.getLogger(__name__)


def load_api_config() -> APIConfig:
    """按环境变量优先级读取本地 API 配置。"""

    data_section = load_config_data()
    section = get_section(data_section, "api")
    bearer_token = os.getenv("OMNICRAWL_API_TOKEN", "").strip()
    if not bearer_token:
        raw_token = section.get("bearer_token", "")
        bearer_token = raw_token.strip() if isinstance(raw_token, str) else ""
    host = os.getenv("OMNICRAWL_API_HOST", "").strip() or section.get("host", "127.0.0.1")
    raw_port: Any = os.getenv("OMNICRAWL_API_PORT", "").strip() or section.get("port", 8765)
    try:
        port = int(raw_port)
    except (TypeError, ValueError) as exc:
        raise ValueError("api.port 必须是整数。") from exc
    raw_origins = section.get("allowed_origins", [])
    if isinstance(raw_origins, str):
        origins = tuple(item.strip() for item in raw_origins.split(",") if item.strip())
    elif isinstance(raw_origins, list) and all(isinstance(item, str) for item in raw_origins):
        origins = tuple(raw_origins)
    else:
        raise ValueError("api.allowed_origins 必须是字符串列表。")
    raw_timeout = section.get("confirmation_timeout_seconds", 300)
    try:
        timeout_seconds = float(raw_timeout)
    except (TypeError, ValueError) as exc:
        raise ValueError("api.confirmation_timeout_seconds 必须是数字。") from exc
    return APIConfig(
        bearer_token=bearer_token,
        host=str(host),
        port=port,
        allowed_origins=origins,
        confirmation_timeout_seconds=timeout_seconds,
    )


def create_default_agent() -> LocalToolAgent:
    """按 TUI 相同的配置与工作区检测规则创建 Agent，并注入 PluginRuntime。"""

    app_root = Path(__file__).resolve().parents[2]
    project_context = detect_project_context(app_root=app_root)
    plugin_runtime = None
    try:
        from ..extensions.plugin_manager import PluginRuntime

        plugin_runtime = PluginRuntime.from_config_data(
            load_config_data(),
            workspace_root=project_context.workspace_root,
        )
        plugin_runtime.start()
    except Exception as exc:  # noqa: BLE001
        LOGGER.warning("API 插件子系统初始化失败，继续无插件模式：%s", exc)
        plugin_runtime = None

    def _on_workspace_switched(new_root: Path):
        if plugin_runtime is None:
            return None
        try:
            plugin_runtime.switch_workspace(new_root)
        except BaseException:
            plugin_runtime.close_manager_only()
            plugin_runtime.workspace_root = new_root
            raise
        return plugin_runtime.manager

    try:
        agent = LocalToolAgent(
            AgentConfig(
                llm=load_llm_config(),
                workspace_root=project_context.workspace_root,
                workspace_detection_summary=project_context.detection_summary,
                file_name_index_enabled=(
                    load_feature_enabled("file_name_index", default=False)
                    and not should_disable_broad_workspace_indexes(project_context)
                ),
                content_index_enabled=(
                    load_feature_enabled("content_index", default=False)
                    and not should_disable_broad_workspace_indexes(project_context)
                ),
                router_enabled=load_feature_enabled("router", default=False),
                router_mode=load_router_mode(),
                temp_workspace=load_agent_temp_workspace_config(),
                subagents=load_subagent_config(),
            ),
            plugin_manager=None if plugin_runtime is None else plugin_runtime.manager,
            on_workspace_switched=_on_workspace_switched,
        )
    except BaseException:
        if plugin_runtime is not None:
            plugin_runtime.close()
        raise
    if plugin_runtime is not None:
        # PluginRuntime 必须晚于 Agent 及其所有 SubAgent 关闭；Agent 超时进入
        # deferred close 时，该回调也会随最后一个子任务退出后再执行。
        agent.add_close_callback(plugin_runtime.close)
        try:
            plugin_runtime.notify_app_started()
        except Exception as exc:  # noqa: BLE001
            LOGGER.warning("API app.start.after 忽略故障：%s", exc)
    return agent


def create_app(
    *,
    config: APIConfig | None = None,
    agent_factory: Callable[[], Any] | None = None,
) -> FastAPI:
    """创建可测试、可嵌入的 FastAPI 应用。"""

    api_config = config or load_api_config()

    @asynccontextmanager
    async def lifespan(application: FastAPI):
        agent = application.state.agent_factory()
        application.state.service = AgentAPIService(
            agent,
            confirmation_timeout_seconds=api_config.confirmation_timeout_seconds,
        )
        try:
            yield
        finally:
            current = application.state.service
            if current is not None:
                current.close()
                application.state.service = None

    app = FastAPI(
        title="OmniCrawl Local API",
        version="1.0.0",
        description="OmniCrawl 本地 Agent 的 HTTP/SSE 接口。",
        lifespan=lifespan,
    )
    app.state.api_config = api_config
    app.state.agent_factory = agent_factory or create_default_agent
    app.state.service = None

    if api_config.allowed_origins:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=list(api_config.allowed_origins),
            allow_credentials=False,
            allow_methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"],
            allow_headers=["Authorization", "Content-Type", "Last-Event-ID"],
        )

    @app.exception_handler(APIServiceError)
    async def handle_service_error(_request: Request, exc: APIServiceError) -> JSONResponse:
        return error_response(exc.status_code, exc.code, exc.message, exc.details)

    @app.exception_handler(RequestValidationError)
    async def handle_validation_error(_request: Request, exc: RequestValidationError) -> JSONResponse:
        return error_response(
            status.HTTP_422_UNPROCESSABLE_ENTITY,
            "VALIDATION_ERROR",
            "请求参数校验失败。",
            exc.errors(),
        )

    @app.exception_handler(Exception)
    async def handle_unexpected_error(_request: Request, exc: Exception) -> JSONResponse:
        # 不把框架、文件系统或第三方库的异常消息暴露给客户端或普通日志。
        LOGGER.error(
            "Unhandled OmniCrawl API request error: %s: %s",
            type(exc).__name__,
            redact_sensitive_text(str(exc)),
        )
        return error_response(
            status.HTTP_500_INTERNAL_SERVER_ERROR,
            "INTERNAL_ERROR",
            "服务器内部错误。",
        )

    @app.get("/health", tags=["system"])
    def health() -> dict[str, Any]:
        return data({"status": "ok", "service": "omnicrawl"})

    app.include_router(build_api_router())
    return app


__all__ = [
    "create_app",
    "create_default_agent",
    "load_api_config",
]
