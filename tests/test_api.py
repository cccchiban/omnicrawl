from __future__ import annotations

import json
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace

from fastapi import Request
from fastapi.testclient import TestClient

from omnicrawl.agent import ToolCall, ToolResult
from omnicrawl.api import APIConfig, AgentAPIService, create_app
from omnicrawl.state.project import ProjectEntry
from omnicrawl.state.session import PromptHistoryEntry, SessionEvent, SessionIndexEntry, SessionState


TOKEN = "test-api-token"
AUTH_HEADERS = {"Authorization": f"Bearer {TOKEN}"}
SESSION_ID = "20260710-120000-a1b2c3"


class FakeSkillManager:
    count = 1

    def list_all(self):
        return [
            SimpleNamespace(
                name="demo-skill",
                description="用于 API 测试的 Skill",
                scope="project",
                disable_model_invocation=False,
            )
        ]


class FakeAgent:
    def __init__(self, *, block: bool = False, require_confirmation: bool = False) -> None:
        self.workspace_root = Path.cwd().resolve()
        self.current_session_id = SESSION_ID
        self.current_model = "demo-model"
        self.reasoning_effort = "medium"
        self.approval_mode = "manual"
        self.skill_manager = FakeSkillManager()
        self.config = SimpleNamespace(
            llm=SimpleNamespace(
                base_url="https://example.test/v1",
                api_key="secret-key-that-must-not-leak",
            )
        )
        self.block = block
        self.require_confirmation = require_confirmation
        self.release = threading.Event()
        self.closed = False
        self._confirm = lambda _tool_name, _arguments: True

    def set_confirm_handler(self, confirm) -> None:
        self._confirm = confirm

    def run_stream(self, text: str, on_delta, **callbacks) -> str:
        callbacks["on_status"]("正在思考")
        if self.require_confirmation:
            approved = self._confirm("write_file", {"path": "demo.txt", "content": "demo"})
            result = ToolResult(ok=approved, output="approved" if approved else "denied")
            tool_call = ToolCall(name="write_file", arguments={"path": "demo.txt"}, id="tool-1")
            callbacks["on_tool_start"](1, tool_call)
            callbacks["on_tool_result"](tool_call, result)
        if self.block:
            self.release.wait(timeout=2)
        on_delta(f"回复：{text}")
        callbacks["on_token_usage"](10, 4, 2)
        return f"回复：{text}"

    def close(self) -> None:
        self.closed = True

    def reset_conversation(self) -> None:
        self.current_session_id = "session-new"

    def list_sessions(self, limit: int = 10, *, project_path=None):
        del limit, project_path
        return [self._session_entry()]

    def list_archived_sessions(self, limit: int = 10):
        del limit
        return []

    def load_session_events(self, session_id: str):
        return [
            SessionEvent.create(
                session_id=session_id,
                event_type="assistant_message",
                payload={"content": "历史回复"},
            )
        ]

    def resume_session(self, session_id: str):
        self.current_session_id = session_id
        return self._session_state(session_id)

    def rename_current_session(self, title: str):
        state = self._session_state(self.current_session_id)
        return SessionState(**{**state.__dict__, "title": title})

    def compact_conversation(self) -> str:
        return "压缩摘要"

    def archive_current_session(self):
        return self._session_state(self.current_session_id)

    def delete_session(self, session_id: str) -> None:
        del session_id

    def export_current_session_markdown(self, markdown_text: str) -> Path:
        del markdown_text
        return Path(f".agent_sessions/exports/{SESSION_ID}.md")

    def read_session_artifact_text(self, session_id: str, artifact_path: str) -> str:
        if ".." in artifact_path or session_id not in artifact_path:
            raise RuntimeError("artifact 路径越界")
        return "<h1>demo</h1>"

    def list_projects(self):
        return [self._project_entry()]

    def create_project(self, name: str, path: str = ""):
        return self._project_entry(name=name, path=path or str(Path.cwd() / name))

    def import_project(self, name: str, path: str):
        return self._project_entry(name=name, path=path)

    def rename_project(self, project_path: str, name: str):
        return self._project_entry(name=name, path=project_path)

    def pin_project(self, project_path: str, *, pinned: bool = True):
        return self._project_entry(path=project_path, pinned=pinned)

    def remove_project(self, project_path: str) -> None:
        del project_path

    def switch_workspace(self, new_path: str):
        self.workspace_root = Path(new_path).resolve()
        return self.workspace_root

    def set_model(self, model: str) -> None:
        self.current_model = model

    def set_reasoning_effort(self, effort: str) -> str:
        self.reasoning_effort = effort
        return effort

    def set_approval_mode(self, mode: str) -> None:
        self.approval_mode = mode

    def search_prompt_history(self, **_kwargs):
        return [
            PromptHistoryEntry.create(
                display="历史问题",
                project=self.workspace_root,
                session_id=self.current_session_id,
            )
        ]

    def format_mcp_status(self) -> str:
        return "MCP 已关闭"

    def clean_memory(self):
        return ["memory/expired.md"]

    def _session_entry(self) -> SessionIndexEntry:
        state = self._session_state(self.current_session_id)
        return SessionIndexEntry(
            session_id=state.session_id,
            title=state.title,
            workspace_root=state.workspace_root,
            path=f"sessions/{state.session_id}.jsonl",
            created_at=state.created_at,
            updated_at=state.updated_at,
            event_count=2,
            message_count=2,
            last_event_type="assistant_message",
        )

    def _session_state(self, session_id: str) -> SessionState:
        event = SessionEvent.create(session_id=session_id, event_type="session_started")
        return SessionState(
            session_id=session_id,
            title="测试会话",
            workspace_root=str(self.workspace_root),
            path=Path(f".agent_sessions/sessions/{session_id}.jsonl"),
            created_at=event.created_at,
            updated_at=event.created_at,
            messages=[],
            last_event_type="assistant_message",
            event_count=2,
        )

    def _project_entry(
        self,
        *,
        name: str = "测试项目",
        path: str | None = None,
        pinned: bool = False,
    ) -> ProjectEntry:
        event = SessionEvent.create(session_id=SESSION_ID, event_type="session_started")
        return ProjectEntry(
            name=name,
            path=path or str(self.workspace_root),
            created_at=event.created_at,
            updated_at=event.created_at,
            pinned=pinned,
        )


def make_client(agent: FakeAgent, *, origins: tuple[str, ...] = ("http://localhost:5173",)):
    app = create_app(
        config=APIConfig(bearer_token=TOKEN, allowed_origins=origins),
        agent_factory=lambda: agent,
    )
    return TestClient(app)


def wait_for_run(client: TestClient, run_id: str, expected: set[str]) -> dict:
    for _ in range(100):
        payload = client.get(f"/api/v1/runs/{run_id}", headers=AUTH_HEADERS).json()["data"]
        if payload["status"] in expected:
            return payload
        time.sleep(0.01)
    raise AssertionError(f"run {run_id} 未进入状态 {expected}")


class APITest(unittest.TestCase):
    def test_health_and_openapi_are_public_but_api_requires_bearer_token(self) -> None:
        with make_client(FakeAgent()) as client:
            self.assertEqual(client.get("/health").status_code, 200)
            self.assertEqual(client.get("/openapi.json").status_code, 200)
            response = client.get("/api/v1/runtime")

        self.assertEqual(response.status_code, 401)
        self.assertEqual(response.json()["error"]["code"], "UNAUTHORIZED")

    def test_cors_allows_exact_origin_and_rejects_unknown_origin(self) -> None:
        with make_client(FakeAgent()) as client:
            allowed = client.options(
                "/api/v1/runtime",
                headers={
                    "Origin": "http://localhost:5173",
                    "Access-Control-Request-Method": "GET",
                },
            )
            rejected = client.options(
                "/api/v1/runtime",
                headers={
                    "Origin": "http://evil.example",
                    "Access-Control-Request-Method": "GET",
                },
            )

        self.assertEqual(allowed.headers["access-control-allow-origin"], "http://localhost:5173")
        self.assertNotIn("access-control-allow-origin", rejected.headers)

    def test_run_stream_emits_ordered_sse_events_and_replays_from_last_event_id(self) -> None:
        with make_client(FakeAgent()) as client:
            response = client.post(
                "/api/v1/runs",
                headers=AUTH_HEADERS,
                json={"message": "你好"},
            )
            run_id = response.json()["data"]["run_id"]
            wait_for_run(client, run_id, {"completed"})

            full_stream = client.get(f"/api/v1/runs/{run_id}/events", headers=AUTH_HEADERS)
            replay = client.get(
                f"/api/v1/runs/{run_id}/events",
                headers={**AUTH_HEADERS, "Last-Event-ID": "1"},
            )

        self.assertEqual(full_stream.status_code, 200)
        self.assertIn("event: run.started", full_stream.text)
        self.assertIn("event: assistant.delta", full_stream.text)
        self.assertIn("event: usage.updated", full_stream.text)
        self.assertIn("event: run.completed", full_stream.text)
        self.assertNotIn("secret-key-that-must-not-leak", full_stream.text)
        self.assertNotIn("id: 1", replay.text)
        self.assertIn("id: 2", replay.text)

    def test_only_one_run_can_be_active_and_cancel_releases_it(self) -> None:
        agent = FakeAgent(block=True)
        with make_client(agent) as client:
            first = client.post(
                "/api/v1/runs",
                headers=AUTH_HEADERS,
                json={"message": "第一个"},
            )
            run_id = first.json()["data"]["run_id"]
            wait_for_run(client, run_id, {"running"})

            conflict = client.post(
                "/api/v1/runs",
                headers=AUTH_HEADERS,
                json={"message": "第二个"},
            )
            cancelled = client.post(f"/api/v1/runs/{run_id}/cancel", headers=AUTH_HEADERS)
            agent.release.set()
            final = wait_for_run(client, run_id, {"cancelled"})

        self.assertEqual(conflict.status_code, 409)
        self.assertEqual(conflict.json()["error"]["code"], "RUN_ACTIVE")
        self.assertEqual(cancelled.status_code, 200)
        self.assertEqual(final["status"], "cancelled")

    def test_confirmation_can_be_approved_through_http(self) -> None:
        agent = FakeAgent(require_confirmation=True)
        with make_client(agent) as client:
            run_id = client.post(
                "/api/v1/runs",
                headers=AUTH_HEADERS,
                json={"message": "写文件"},
            ).json()["data"]["run_id"]
            confirmation_id = self._wait_for_confirmation(client, run_id)

            decision = client.post(
                f"/api/v1/runs/{run_id}/confirmations/{confirmation_id}",
                headers=AUTH_HEADERS,
                json={"approved": True},
            )
            final = wait_for_run(client, run_id, {"completed"})

        self.assertEqual(decision.status_code, 200)
        self.assertEqual(final["status"], "completed")

    def test_cancelled_confirmation_cannot_be_approved_or_execute_tool(self) -> None:
        agent = FakeAgent(block=True)
        service = AgentAPIService(agent)
        run = service.start_run("等待确认")
        confirmation_finished = threading.Event()
        result: list[bool] = []

        wait_for_run = time.monotonic() + 1
        while run.status == "pending" and time.monotonic() < wait_for_run:
            time.sleep(0.01)
        self.assertEqual(run.status, "running")

        def wait_for_confirmation() -> None:
            result.append(service._confirm_tool_call("write_file", {"path": "demo.txt"}))
            confirmation_finished.set()

        thread = threading.Thread(target=wait_for_confirmation)
        thread.start()
        for _ in range(100):
            if run.confirmations:
                break
            time.sleep(0.01)
        self.assertTrue(run.confirmations)
        confirmation_id = next(iter(run.confirmations))

        service.cancel_run(run.run_id)
        with self.assertRaisesRegex(Exception, "确认请求已经处理"):
            service.decide_confirmation(run.run_id, confirmation_id, True)
        agent.release.set()
        thread.join(timeout=1)

        self.assertTrue(confirmation_finished.is_set())
        self.assertEqual(result, [False])

    def test_expired_confirmation_cannot_be_approved_after_run_completes(self) -> None:
        agent = FakeAgent(require_confirmation=True)
        app = create_app(
            config=APIConfig(bearer_token=TOKEN, confirmation_timeout_seconds=0.02),
            agent_factory=lambda: agent,
        )
        with TestClient(app) as client:
            run_id = client.post(
                "/api/v1/runs", headers=AUTH_HEADERS, json={"message": "写文件"}
            ).json()["data"]["run_id"]
            confirmation_id = self._wait_for_confirmation(client, run_id)
            wait_for_run(client, run_id, {"completed"})
            response = client.post(
                f"/api/v1/runs/{run_id}/confirmations/{confirmation_id}",
                headers=AUTH_HEADERS,
                json={"approved": True},
            )

        self.assertEqual(response.status_code, 409)
        self.assertEqual(response.json()["error"]["code"], "CONFIRMATION_RESOLVED")

    def test_concurrent_confirmation_decisions_allow_only_one_winner(self) -> None:
        agent = FakeAgent(block=True)
        service = AgentAPIService(agent)
        run = service.start_run("等待确认")
        confirmation_result: list[bool] = []
        confirmation_thread = threading.Thread(
            target=lambda: confirmation_result.append(
                service._confirm_tool_call("write_file", {"path": "demo.txt"})
            )
        )
        confirmation_thread.start()
        for _ in range(100):
            if run.confirmations:
                break
            time.sleep(0.01)
        self.assertTrue(run.confirmations)
        confirmation_id = next(iter(run.confirmations))

        start = threading.Barrier(2)
        decisions: list[str] = []

        def decide(approved: bool) -> None:
            start.wait()
            try:
                service.decide_confirmation(run.run_id, confirmation_id, approved)
                decisions.append("accepted")
            except Exception as exc:
                decisions.append(getattr(exc, "code", type(exc).__name__))

        threads = [threading.Thread(target=decide, args=(True,)), threading.Thread(target=decide, args=(False,))]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=1)
        agent.release.set()
        confirmation_thread.join(timeout=1)

        self.assertCountEqual(decisions, ["accepted", "CONFIRMATION_RESOLVED"])
        self.assertTrue(confirmation_result)

    def test_sse_does_not_expose_inline_html_artifact(self) -> None:
        agent = FakeAgent()

        def run_stream(_text: str, _on_delta, **callbacks) -> str:
            tool_call = ToolCall(name="display_html", arguments={}, id="tool-html")
            callbacks["on_tool_start"](1, tool_call)
            callbacks["on_tool_result"](
                tool_call,
                ToolResult(
                    ok=True,
                    output="ok",
                    ui_artifact={
                        "type": "html",
                        "title": "private",
                        "html": '<script>const secret = "TOP_SECRET"</script>',
                    },
                ),
            )
            return "ok"

        agent.run_stream = run_stream
        with make_client(agent) as client:
            run_id = client.post(
                "/api/v1/runs", headers=AUTH_HEADERS, json={"message": "预览"}
            ).json()["data"]["run_id"]
            wait_for_run(client, run_id, {"completed"})
            stream = client.get(f"/api/v1/runs/{run_id}/events", headers=AUTH_HEADERS)

        self.assertNotIn("TOP_SECRET", stream.text)
        self.assertNotIn('"html"', stream.text)
        self.assertIn("event: artifact.available", stream.text)

    def test_run_failure_does_not_expose_exception_text(self) -> None:
        agent = FakeAgent()

        def run_stream(*_args, **_kwargs) -> str:
            raise RuntimeError("api_key=TOP_SECRET")

        agent.run_stream = run_stream
        with make_client(agent) as client:
            run_id = client.post(
                "/api/v1/runs", headers=AUTH_HEADERS, json={"message": "失败"}
            ).json()["data"]["run_id"]
            response = wait_for_run(client, run_id, {"failed"})
            stream = client.get(f"/api/v1/runs/{run_id}/events", headers=AUTH_HEADERS)

        self.assertNotIn("TOP_SECRET", response["error"])
        self.assertNotIn("TOP_SECRET", stream.text)
        self.assertEqual(response["error"], "生成任务失败。")

    def test_unexpected_http_error_does_not_expose_exception_text(self) -> None:
        app = create_app(config=APIConfig(bearer_token=TOKEN), agent_factory=FakeAgent)
        handler = next(
            registered_handler
            for exc_type, registered_handler in app.exception_handlers.items()
            if exc_type is Exception
        )
        scope = {"type": "http", "method": "GET", "path": "/api/v1/runtime", "headers": []}
        response = self._run_async(handler(Request(scope), RuntimeError("api_key=TOP_SECRET")))

        self.assertEqual(response.status_code, 500)
        self.assertNotIn("TOP_SECRET", response.body.decode("utf-8"))
        self.assertIn("服务器内部错误", response.body.decode("utf-8"))

    def test_management_endpoints_use_data_envelope_and_block_artifact_traversal(self) -> None:
        with make_client(FakeAgent()) as client:
            sessions = client.get("/api/v1/sessions", headers=AUTH_HEADERS)
            projects = client.get("/api/v1/projects", headers=AUTH_HEADERS)
            runtime = client.get("/api/v1/runtime", headers=AUTH_HEADERS)
            traversal = client.get(
                f"/api/v1/sessions/{SESSION_ID}/artifacts/../config.json",
                headers=AUTH_HEADERS,
            )

        self.assertEqual(sessions.status_code, 200)
        self.assertEqual(sessions.json()["data"][0]["session_id"], SESSION_ID)
        self.assertEqual(projects.json()["data"][0]["name"], "测试项目")
        self.assertEqual(runtime.json()["data"]["model"], "demo-model")
        self.assertIn(traversal.status_code, {400, 404})

    def test_missing_artifact_does_not_expose_internal_exception_text(self) -> None:
        agent = FakeAgent()
        agent.read_session_artifact_text = lambda *_args: (_ for _ in ()).throw(
            RuntimeError("api_key=TOP_SECRET")
        )

        with make_client(agent) as client:
            response = client.get(
                f"/api/v1/sessions/{SESSION_ID}/artifacts/missing.html",
                headers=AUTH_HEADERS,
            )

        self.assertEqual(response.status_code, 404)
        self.assertEqual(response.json()["error"]["code"], "ARTIFACT_NOT_FOUND")
        self.assertEqual(response.json()["error"]["message"], "会话 artifact 不存在。")
        self.assertNotIn("TOP_SECRET", response.text)

    def test_api_config_rejects_empty_token_wildcard_origin_and_non_loopback_host(self) -> None:
        for kwargs in (
            {"bearer_token": ""},
            {"bearer_token": TOKEN, "allowed_origins": ("*",)},
            {"bearer_token": TOKEN, "host": "0.0.0.0"},
        ):
            with self.assertRaises(ValueError):
                APIConfig(**kwargs)

    def test_agent_protocol_honors_cancel_check_between_stream_events(self) -> None:
        from omnicrawl.agent import AgentLLMProtocol

        class FakeCompletions:
            @staticmethod
            def create(**_kwargs):
                return [
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="第一段", tool_calls=[]))],
                        usage=None,
                    ),
                    SimpleNamespace(
                        choices=[SimpleNamespace(delta=SimpleNamespace(content="第二段", tool_calls=[]))],
                        usage=None,
                    ),
                ]

        client = SimpleNamespace(chat=SimpleNamespace(completions=FakeCompletions()))
        protocol = AgentLLMProtocol(
            client=client,
            model="demo-model",
            request_timeout_seconds=10,
            request_retry_count=1,
            workspace_root=Path.cwd(),
            system_prompt_provider=lambda: "system",
            prompt_cache_identity_provider=lambda: {},
            tools_provider=lambda: [],
            extra_body_provider=lambda: {},
            tool_name_from_function_name=lambda value: value,
            function_name_for_tool=lambda value: value,
        )
        checks = 0

        def cancel_check() -> None:
            nonlocal checks
            checks += 1
            if checks == 2:
                raise RuntimeError("cancelled")

        with self.assertRaisesRegex(RuntimeError, "cancelled"):
            protocol.request_reply_once(
                [],
                lambda _delta: None,
                lambda _input, _output, _cached: None,
                lambda: None,
                cancel_check,
            )

    @staticmethod
    def _run_async(coroutine):
        import asyncio

        return asyncio.run(coroutine)

    @staticmethod
    def _wait_for_confirmation(client: TestClient, run_id: str) -> str:
        for _ in range(100):
            events = client.get(
                f"/api/v1/runs/{run_id}/events",
                headers=AUTH_HEADERS,
                params={"follow": "false"},
            ).text
            for block in events.split("\n\n"):
                if "event: confirmation.required" not in block:
                    continue
                data_line = next(line for line in block.splitlines() if line.startswith("data: "))
                return json.loads(data_line[6:])["confirmation_id"]
            time.sleep(0.01)
        raise AssertionError("未收到确认请求")
