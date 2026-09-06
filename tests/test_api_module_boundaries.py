from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from fastapi.testclient import TestClient

from omnicrawl import api as api_package
from omnicrawl.api import APIConfig, create_app
from tests.test_api import FakeAgent, TOKEN


class APIModuleBoundaryTests(unittest.TestCase):
    """锁定 API 真实模块边界与公共导出兼容。"""

    MODULE_NAMES = (
        "app",
        "deps",
        "models",
        "service",
    )

    ROUTE_MODULES = (
        "configuration",
        "monitors",
        "projects",
        "runs",
        "sessions",
        "subagents",
        "support",
        "system",
    )

    def test_api_submodules_are_real_files(self) -> None:
        package_path = Path(api_package.__file__).resolve().parent

        for module_name in self.MODULE_NAMES:
            with self.subTest(module=module_name):
                module = importlib.import_module(f"omnicrawl.api.{module_name}")
                self.assertEqual(module.__name__, f"omnicrawl.api.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    package_path / f"{module_name}.py",
                )

        routes_path = package_path / "routes"
        for module_name in self.ROUTE_MODULES:
            with self.subTest(route=module_name):
                module = importlib.import_module(f"omnicrawl.api.routes.{module_name}")
                self.assertEqual(module.__name__, f"omnicrawl.api.routes.{module_name}")
                self.assertEqual(
                    Path(module.__file__).resolve(),
                    routes_path / f"{module_name}.py",
                )

    def test_package_keeps_public_exports(self) -> None:
        self.assertEqual(
            api_package.__all__,
            [
                "APIConfig",
                "APIServiceError",
                "AgentAPIService",
                "create_app",
                "create_default_agent",
                "load_api_config",
            ],
        )
        for export_name in api_package.__all__:
            with self.subTest(export=export_name):
                self.assertTrue(hasattr(api_package, export_name))

    def test_package_entry_does_not_embed_route_handlers(self) -> None:
        source = Path(api_package.__file__).read_text(encoding="utf-8")
        self.assertNotIn("@router.", source)
        self.assertNotIn("APIRouter(", source)
        self.assertNotIn("class AgentAPIService", source)
        self.assertLess(len(source.splitlines()), 40)

    def test_openapi_contract_keeps_expected_operations(self) -> None:
        app = create_app(
            config=APIConfig(bearer_token=TOKEN, allowed_origins=("http://localhost:5173",)),
            agent_factory=FakeAgent,
        )
        with TestClient(app) as client:
            openapi = client.get("/openapi.json").json()

        operations = {
            f"{method.upper()} {path}"
            for path, methods in openapi["paths"].items()
            for method in methods
            if not method.startswith("x-")
        }
        expected = {
            "GET /health",
            "GET /api/v1/runtime",
            "POST /api/v1/runs",
            "GET /api/v1/runs/{run_id}",
            "GET /api/v1/runs/{run_id}/events",
            "POST /api/v1/runs/{run_id}/cancel",
            "POST /api/v1/runs/{run_id}/confirmations/{confirmation_id}",
            "GET /api/v1/monitors",
            "GET /api/v1/monitors/{monitor_id}",
            "GET /api/v1/monitors/{monitor_id}/events",
            "GET /api/v1/subagents",
            "GET /api/v1/subagents/events",
            "GET /api/v1/subagents/{task_id}",
            "POST /api/v1/subagents/{task_id}/cancel",
            "GET /api/v1/sessions",
            "POST /api/v1/sessions",
            "GET /api/v1/sessions/diagnostics",
            "GET /api/v1/sessions/{session_id}/diagnostics",
            "GET /api/v1/sessions/{session_id}/events",
            "POST /api/v1/sessions/{session_id}/resume",
            "PATCH /api/v1/sessions/current",
            "POST /api/v1/sessions/current/compact",
            "POST /api/v1/sessions/current/archive",
            "DELETE /api/v1/sessions/{session_id}",
            "POST /api/v1/sessions/current/export",
            "GET /api/v1/sessions/{session_id}/artifacts/{artifact_path}",
            "GET /api/v1/projects",
            "POST /api/v1/projects",
            "POST /api/v1/projects/import",
            "PATCH /api/v1/projects",
            "POST /api/v1/projects/pin",
            "DELETE /api/v1/projects",
            "POST /api/v1/projects/switch",
            "GET /api/v1/models",
            "GET /api/v1/models/catalog",
            "POST /api/v1/models/refresh",
            "PUT /api/v1/models/current",
            "PUT /api/v1/reasoning",
            "PUT /api/v1/approval",
            "GET /api/v1/history",
            "GET /api/v1/skills",
            "GET /api/v1/mcp",
            "POST /api/v1/memory/clean",
        }
        self.assertEqual(operations, expected)

        # OpenAPI 对 StreamingResponse 的 content-type 声明可能仍是默认 JSON，
        # 因此额外用真实请求锁定运行时 SSE 媒体类型。
        with TestClient(app) as client:
            headers = {"Authorization": f"Bearer {TOKEN}"}
            run_id = client.post(
                "/api/v1/runs",
                headers=headers,
                json={"message": "契约检查"},
            ).json()["data"]["run_id"]
            for _ in range(100):
                status_payload = client.get(
                    f"/api/v1/runs/{run_id}",
                    headers=headers,
                ).json()["data"]
                if status_payload["status"] in {"completed", "cancelled", "failed"}:
                    break
            run_stream = client.get(f"/api/v1/runs/{run_id}/events", headers=headers)

        self.assertIn("text/event-stream", run_stream.headers.get("content-type", ""))
        self.assertIn("event: run.started", run_stream.text)
