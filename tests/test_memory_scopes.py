from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent import LocalToolAgent
from omnicrawl.agent.llm_protocol import tool_parameters_schema
from omnicrawl.agent.tools import build_agent_tools
from omnicrawl.agent.types import ToolResult
from omnicrawl.memory import MemoryStore, MemoryWriteRequest, migrate_legacy_memory


class RecordingMemoryStore:
    def __init__(self) -> None:
        self.requests: list[MemoryWriteRequest] = []

    def write(self, memories: list[MemoryWriteRequest]):
        self.requests.extend(memories)
        return []


class MemoryScopeTest(unittest.TestCase):
    def test_legacy_memory_is_renamed_when_project_store_is_absent(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            legacy = MemoryStore(workspace / "memory")
            legacy.write(
                [
                    MemoryWriteRequest(
                        content="旧项目入口是 main.py。",
                        related_directories=["project-context/general"],
                        storage_directory="project-context/general",
                    )
                ]
            )

            result = migrate_legacy_memory(
                workspace / "memory",
                workspace / ".oclmemory",
            )

            self.assertTrue(result.migrated)
            self.assertFalse((workspace / "memory").exists())
            self.assertTrue((workspace / ".oclmemory" / "index.json").is_file())
            matches = MemoryStore(workspace / ".oclmemory").search("main.py")
            self.assertTrue(matches)

    def test_legacy_memory_is_imported_and_backed_up_when_target_exists(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            MemoryStore(workspace / ".oclmemory").write(
                [
                    MemoryWriteRequest(
                        content="新项目约束。",
                        related_directories=["project-context/general"],
                        storage_directory="project-context/general",
                    )
                ]
            )
            MemoryStore(workspace / "memory").write(
                [
                    MemoryWriteRequest(
                        content="旧项目决策。",
                        related_directories=["project-context/general"],
                        storage_directory="project-context/general",
                    )
                ]
            )

            result = migrate_legacy_memory(
                workspace / "memory",
                workspace / ".oclmemory",
            )

            self.assertTrue(result.migrated)
            self.assertEqual(result.imported_count, 1)
            self.assertIsNotNone(result.backup_path)
            self.assertTrue(result.backup_path.is_dir())
            target = MemoryStore(workspace / ".oclmemory")
            self.assertTrue(target.search("新项目约束"))
            self.assertTrue(target.search("旧项目决策"))

    def test_memory_store_paths_and_session_binding_are_isolated(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            workspace = root / "project"
            home = root / "home"
            workspace.mkdir()
            agent = object.__new__(LocalToolAgent)
            agent.workspace_root = workspace
            agent.config = SimpleNamespace(
                memory_directory=".oclmemory",
                memory_enabled=True,
            )
            agent._session_state = SimpleNamespace(session_id="session-one")
            agent._memory_user_data_root = lambda: home / ".omnicrawl"

            project, session_one, user = agent._create_memory_stores()
            self.assertEqual(project.root, (workspace / ".oclmemory").resolve())
            self.assertEqual(
                session_one.root,
                (home / ".omnicrawl" / "Session_memory" / "session-one").resolve(),
            )
            self.assertEqual(
                user.root,
                (home / ".omnicrawl" / "User_memory").resolve(),
            )
            session_one.write(
                [
                    MemoryWriteRequest(
                        content="仅会话一可见。",
                        related_directories=["task-history/general"],
                        storage_directory="task-history/general",
                    )
                ]
            )
            agent._session_memory_store = session_one
            agent._session_state = SimpleNamespace(session_id="session-two")
            agent._bind_current_session_memory_store()
            self.assertEqual(agent._session_memory_store.search("会话一"), [])

            agent._session_state = SimpleNamespace(session_id="session-one")
            agent._bind_current_session_memory_store()
            self.assertTrue(agent._session_memory_store.search("会话一"))

    def test_should_declare_memory_objects_when_building_scoped_write_tools(self) -> None:
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        common = {
            "mcp_manager": manager,
            "memory_enabled": True,
            "list": runner,
            "read": runner,
            "grep": runner,
            "replace_text": runner,
            "write_file": runner,
            "bash": runner,
            "powershell": runner,
            "monitor": runner,
            "memory_search": runner,
            "memory_read": runner,
            "memory_expand_related": runner,
            "memory_write": runner,
            "mcp_call": lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            "mcp_read_resource": lambda _uri: ToolResult(ok=True, output="ok"),
            "mcp_get_prompt": lambda _name, _arguments: ToolResult(ok=True, output="ok"),
        }
        scoped = {
            f"{scope}_memory_{operation}": runner
            for scope in ("project", "session", "user")
            for operation in ("search", "read", "expand_related", "write")
        }

        tools = build_agent_tools(**common, **scoped)

        self.assertNotIn("memory_search", tools)
        self.assertNotIn("memory_write", tools)
        self.assertEqual(
            {
                name
                for name in tools
                if name.startswith(("project_memory_", "session_memory_", "user_memory_"))
            },
            set(scoped),
        )
        for scope in ("project", "session", "user"):
            schema = tool_parameters_schema(tools[f"{scope}_memory_write"])
            memories = schema["properties"]["memories"]
            self.assertEqual(memories["type"], "array")
            self.assertEqual(memories["items"]["type"], "object")
            self.assertIn("content", memories["items"]["properties"])

    def test_should_write_read_and_merge_when_using_scoped_memory_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            agent = object.__new__(LocalToolAgent)
            agent._project_memory_store = MemoryStore(root / "project")
            agent._session_memory_store = MemoryStore(root / "session")
            agent._user_memory_store = MemoryStore(root / "user")
            agent._memory_store = agent._project_memory_store
            cases = {
                "project": (
                    "项目入口是 main.py。",
                    "project-context/architecture",
                ),
                "session": (
                    "当前会话下一步执行回归测试。",
                    "task-history/current-session",
                ),
                "user": (
                    "用户偏好中文交付摘要。",
                    "user-preferences/communication-style",
                ),
            }

            for scope, (content, directory) in cases.items():
                payload = {
                    "memories": [
                        {
                            "content": content,
                            "related_directories": [directory],
                            "storage_directory": directory,
                            "source_event": "scope-contract-test",
                        }
                    ]
                }
                write = getattr(agent, f"_tool_{scope}_memory_write")
                read = getattr(agent, f"_tool_{scope}_memory_read")
                search = getattr(agent, f"_tool_{scope}_memory_search")

                first = write(payload)
                second = write(payload)
                self.assertTrue(first.ok, first.output)
                self.assertTrue(second.ok, second.output)
                first_record = json.loads(first.output)[0]
                second_record = json.loads(second.output)[0]
                self.assertEqual(second_record["id"], first_record["id"])

                recalled = read({"memory_ids": [first_record["id"]]})
                self.assertTrue(recalled.ok, recalled.output)
                self.assertEqual(json.loads(recalled.output)[0]["content"], content)
                matches = search({"query": content, "reason": "验证作用域写入回读"})
                self.assertTrue(matches.ok, matches.output)
                self.assertEqual(json.loads(matches.output)[0]["id"], first_record["id"])

            for scope, (expected_content, _directory) in cases.items():
                store = getattr(agent, f"_{scope}_memory_store")
                entries = store.search("", max_results=20)
                records = store.read([entry.id for entry in entries])
                self.assertEqual({record.content for record in records}, {expected_content})

    def test_compaction_writes_only_current_session_memory(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent._project_memory_store = RecordingMemoryStore()
        agent._session_memory_store = RecordingMemoryStore()
        agent._user_memory_store = RecordingMemoryStore()

        agent._write_compaction_memories(
            {
                "structured": {
                    "objective": ["完成当前目标"],
                    "constraints": [{"text": "保持兼容"}],
                    "decisions": [],
                    "completed": [{"text": "已实现"}],
                    "current_state": ["待验证"],
                    "open_issues": [{"text": "执行测试"}],
                    "artifacts": [{"text": "omnicrawl/agent/core.py"}],
                }
            }
        )

        self.assertEqual(agent._project_memory_store.requests, [])
        self.assertEqual(agent._user_memory_store.requests, [])
        self.assertEqual(len(agent._session_memory_store.requests), 2)


if __name__ == "__main__":
    unittest.main()
