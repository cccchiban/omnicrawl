from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.context_compaction import (
    RECALL_SESSION_EVIDENCE_TOOL_NAME,
    SessionEvidenceRecallService,
    SourceEvent,
    estimate_json_tokens,
)
from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.tools import build_agent_tools
from omnicrawl.agent.types import ToolDefinition, ToolResult
from omnicrawl.agent.windows_desktop import WindowsDesktopTools
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.session import SessionStore


def _summary(
    event_id: str,
    source_event_ids: list[str],
) -> SourceEvent:
    return SourceEvent(
        event_id,
        "compact_summary",
        {
            "schema_version": 2,
            "model_generated": True,
            "covered_event_ids": source_event_ids,
            "structured": {
                "objective": ["继续任务"],
                "constraints": [],
                "decisions": [],
                "completed": [],
                "current_state": [],
                "open_issues": [],
                "artifacts": [],
                "exact_evidence": [
                    {
                        "text": "需要原始证据",
                        "source_event_ids": source_event_ids,
                    }
                ],
            },
        },
    )


class SessionEvidenceRecallServiceTest(unittest.TestCase):
    def test_recalls_authorized_event_and_derived_text_artifact(self) -> None:
        events = (
            SourceEvent(
                "e1",
                "tool_result",
                {
                    "tool": "bash",
                    "ok": False,
                    "output": "完整结果见 artifact",
                    "artifact_path": "artifacts/session-1/result.txt",
                    "output_sha256": "abc123",
                },
            ),
            _summary("s1", ["e1"]),
        )
        requested_paths: list[str] = []

        def read_artifact(path: str) -> str:
            requested_paths.append(path)
            return "line one\nline two"

        result = SessionEvidenceRecallService().recall(
            events=events,
            event_ids=["e1"],
            artifact_reader=read_artifact,
        )

        self.assertTrue(result["ok"])
        self.assertEqual(result["summary_event_id"], "s1")
        self.assertEqual(requested_paths, ["artifacts/session-1/result.txt"])
        self.assertEqual(result["items"][0]["status"], "ok")
        self.assertIn("line one", result["items"][0]["artifacts"][0]["content"])

    def test_only_latest_summary_authorizes_events(self) -> None:
        events = (
            SourceEvent("e1", "user_message", {"content": "old"}),
            _summary("s1", ["e1"]),
            SourceEvent("e2", "user_message", {"content": "new"}),
            _summary("s2", ["e2"]),
        )

        result = SessionEvidenceRecallService().recall(
            events=events,
            event_ids=["e1", "e2"],
            artifact_reader=lambda _path: "",
        )

        self.assertEqual(result["summary_event_id"], "s2")
        self.assertEqual(
            [item["status"] for item in result["items"]],
            ["unauthorized", "ok"],
        )

    def test_limits_request_to_eight_unique_event_ids(self) -> None:
        event_ids = [f"e{index}" for index in range(10)]
        events = tuple(
            SourceEvent(event_id, "user_message", {"content": event_id})
            for event_id in event_ids
        ) + (_summary("s1", event_ids),)

        result = SessionEvidenceRecallService().recall(
            events=events,
            event_ids=event_ids,
            artifact_reader=lambda _path: "",
        )

        self.assertEqual(len(result["items"]), 8)
        self.assertTrue(result["truncated"])
        self.assertIn("item_limit_exceeded", {
            diagnostic["code"] for diagnostic in result["diagnostics"]
        })

    def test_enforces_total_token_budget(self) -> None:
        events = (
            SourceEvent("e1", "tool_result", {"output": "x" * 50_000}),
            _summary("s1", ["e1"]),
        )
        service = SessionEvidenceRecallService(max_output_tokens=4_000)

        result = service.recall(
            events=events,
            event_ids=["e1"],
            artifact_reader=lambda _path: "",
        )

        self.assertLessEqual(estimate_json_tokens(result), 4_000)
        self.assertTrue(result["truncated"])
        self.assertTrue(result["items"][0]["content_truncated"])

    def test_binary_or_cross_session_artifact_returns_metadata_diagnostic(self) -> None:
        events = (
            SourceEvent(
                "e1",
                "tool_result",
                {
                    "artifact_path": "artifacts/other-session/data.bin",
                    "output_size_chars": 123,
                    "output_sha256": "deadbeef",
                },
            ),
            _summary("s1", ["e1"]),
        )

        def reject_artifact(_path: str) -> str:
            raise UnicodeDecodeError("utf-8", b"\xff", 0, 1, "invalid")

        result = SessionEvidenceRecallService().recall(
            events=events,
            event_ids=["e1"],
            artifact_reader=reject_artifact,
        )

        artifact = result["items"][0]["artifacts"][0]
        self.assertEqual(artifact["status"], "metadata_only")
        self.assertEqual(artifact["path"], "artifacts/other-session/data.bin")
        self.assertNotIn("content", artifact)
        self.assertEqual(artifact["diagnostic"]["code"], "artifact_not_text")

    def test_missing_and_unauthorized_events_return_structured_diagnostics(self) -> None:
        events = (
            SourceEvent("e1", "user_message", {"content": "allowed"}),
            _summary("s1", ["e1", "missing"]),
        )

        result = SessionEvidenceRecallService().recall(
            events=events,
            event_ids=["missing", "other"],
            artifact_reader=lambda _path: "",
        )

        self.assertFalse(result["ok"])
        self.assertEqual(
            [item["status"] for item in result["items"]],
            ["missing", "unauthorized"],
        )


class EvidenceRecallToolIntegrationTest(unittest.TestCase):
    @staticmethod
    def _build_tools(evidence_recall=None):
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        return build_agent_tools(
            mcp_manager=manager,
            memory_enabled=False,
            list_files=runner,
            read_file=runner,
            grep=runner,
            replace_text=runner,
            write_file=runner,
            bash=runner,
            powershell=runner,
            monitor=runner,
            memory_search=runner,
            memory_read=runner,
            memory_expand_related=runner,
            memory_write=runner,
            mcp_call=lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            mcp_read_resource=lambda _uri: ToolResult(ok=True, output="ok"),
            mcp_get_prompt=lambda _name, _arguments: ToolResult(ok=True, output="ok"),
            evidence_recall=evidence_recall,
        )

    def test_registers_read_only_recall_tool_only_when_runner_is_available(self) -> None:
        without_runner = self._build_tools()
        with_runner = self._build_tools(
            lambda _arguments: ToolResult(ok=True, output='{"ok":true}')
        )

        self.assertNotIn(RECALL_SESSION_EVIDENCE_TOOL_NAME, without_runner)
        tool = with_runner[RECALL_SESSION_EVIDENCE_TOOL_NAME]
        self.assertFalse(tool.requires_confirmation)
        schema = json.loads(tool.argument_schema)
        self.assertEqual(schema["properties"]["event_ids"]["maxItems"], 8)
        self.assertEqual(schema["required"], ["event_ids"])

    def test_core_registers_recall_tool_only_when_compaction_is_enabled(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._session_store = object()
        agent._memory_store = None
        agent._mcp_manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        agent._subagent_coordinator = None
        agent._windows_desktop_tools = None
        agent.config = SimpleNamespace(
            context_compaction=ContextCompactionConfig(enabled=True)
        )
        with patch.object(WindowsDesktopTools, "is_supported", return_value=False):
            enabled_tools = agent._build_tools()
        agent.config = SimpleNamespace(
            context_compaction=ContextCompactionConfig(enabled=False)
        )
        with patch.object(WindowsDesktopTools, "is_supported", return_value=False):
            disabled_tools = agent._build_tools()

        self.assertIn(RECALL_SESSION_EVIDENCE_TOOL_NAME, enabled_tools)
        self.assertNotIn(RECALL_SESSION_EVIDENCE_TOOL_NAME, disabled_tools)

    def test_core_binds_artifact_reads_to_current_session(self) -> None:
        session_id = "20260720-120000-abcdef"
        events = [
            SimpleNamespace(
                event_id="e1",
                type="tool_result",
                payload={"artifact_path": f"artifacts/{session_id}/result.txt"},
            ),
            SimpleNamespace(
                event_id="s1",
                type="compact_summary",
                payload={"covered_event_ids": ["e1"], "structured": {}},
            ),
        ]
        reads: list[tuple[str, str]] = []
        store = SimpleNamespace(
            read_session_events=lambda actual_session_id: (
                events if actual_session_id == session_id else []
            ),
            read_artifact_text=lambda actual_session_id, path: (
                reads.append((actual_session_id, path)) or "evidence text"
            ),
        )
        agent = object.__new__(LocalToolAgent)
        agent._session_store = store
        agent._session_state = SimpleNamespace(session_id=session_id)

        tool_result = agent._tool_recall_session_evidence({"event_ids": ["e1"]})
        payload = json.loads(tool_result.output)

        self.assertTrue(tool_result.ok)
        self.assertTrue(payload["ok"])
        self.assertEqual(
            reads,
            [(session_id, f"artifacts/{session_id}/result.txt")],
        )

    def test_core_recalls_persisted_tool_result_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            source = store.append_event(
                state.session_id,
                "tool_result",
                {"tool": "bash", "ok": True, "output": "evidence-" * 2_000},
            )
            store.append_event(
                state.session_id,
                "compact_summary",
                {
                    "content": "summary",
                    "schema_version": 2,
                    "covered_event_ids": [source.event_id],
                    "remaining_message_count": 0,
                    "structured": {
                        "exact_evidence": [
                            {
                                "text": "evidence",
                                "source_event_ids": [source.event_id],
                            }
                        ]
                    },
                },
            )
            agent = object.__new__(LocalToolAgent)
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)

            result = agent._tool_recall_session_evidence(
                {"event_ids": [source.event_id]}
            )
            payload = json.loads(result.output)

        self.assertTrue(result.ok)
        artifact = payload["items"][0]["artifacts"][0]
        self.assertEqual(artifact["status"], "ok")
        self.assertTrue(artifact["path"].startswith(f"artifacts/{state.session_id}/"))
        self.assertIn("evidence-", artifact["content"])
        self.assertLessEqual(estimate_json_tokens(payload), 4_000)

    def test_undo_reverts_evidence_authorization_to_previous_summary(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            store = SessionStore(workspace / ".agent_sessions")
            state = store.start_session(workspace)
            first_user = store.append_event(
                state.session_id,
                "user_message",
                {"content": "first"},
            )
            store.append_event(
                state.session_id,
                "assistant_message",
                {"content": "first done"},
            )
            store.append_event(
                state.session_id,
                "compact_summary",
                {
                    "content": "first summary",
                    "covered_event_ids": [first_user.event_id],
                    "remaining_message_count": 0,
                    "structured": {},
                },
            )
            second_user = store.append_event(
                state.session_id,
                "user_message",
                {"content": "second"},
            )
            store.append_event(
                state.session_id,
                "assistant_message",
                {"content": "second done"},
            )
            store.append_event(
                state.session_id,
                "compact_summary",
                {
                    "content": "second summary",
                    "covered_event_ids": [first_user.event_id, second_user.event_id],
                    "remaining_message_count": 0,
                    "structured": {},
                },
            )
            store.undo_last_turn(state.session_id)
            agent = object.__new__(LocalToolAgent)
            agent._session_store = store
            agent._session_state = store.load_session(state.session_id)

            result = agent._tool_recall_session_evidence(
                {"event_ids": [first_user.event_id, second_user.event_id]}
            )
            payload = json.loads(result.output)

        self.assertEqual(
            [item["status"] for item in payload["items"]],
            ["ok", "unauthorized"],
        )

    def test_core_returns_structured_diagnostic_when_session_read_fails(self) -> None:
        def fail_read(_session_id):
            raise RuntimeError("do not leak this detail")

        agent = object.__new__(LocalToolAgent)
        agent._session_store = SimpleNamespace(read_session_events=fail_read)
        agent._session_state = SimpleNamespace(session_id="20260720-120000-abcdef")

        result = agent._tool_recall_session_evidence({"event_ids": ["e1"]})
        payload = json.loads(result.output)

        self.assertFalse(result.ok)
        self.assertEqual(payload["diagnostics"][0]["code"], "evidence_unavailable")
        self.assertNotIn("do not leak", result.output)

    def test_recall_tool_uses_its_own_token_budget_not_generic_char_limit(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(max_tool_output_chars=10)
        agent._dispatch_plugin_hook = lambda _name, payload: payload
        long_output = json.dumps({"content": "证据" * 100}, ensure_ascii=False)
        tool = ToolDefinition(
            name=RECALL_SESSION_EVIDENCE_TOOL_NAME,
            description="test",
            argument_schema="{}",
            requires_confirmation=False,
            run=lambda _arguments: ToolResult(ok=True, output=long_output),
            model_output_is_bounded=True,
        )

        result = agent._execute_approved_tool(tool, {})

        self.assertEqual(result.output, long_output)


if __name__ == "__main__":
    unittest.main()
