from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.temp_workspace import AgentTempWorkspaceConfig


class WorkspaceSwitchTest(unittest.TestCase):
    def _make_config(self, workspace: Path, **kwargs) -> AgentConfig:
        defaults = dict(
            llm=SimpleNamespace(
                api_key="test-key",
                base_url="https://example.test/v1",
                model="test-model",
            ),
            workspace_root=workspace,
            memory_enabled=False,
            session_enabled=False,
            skills_enabled=False,
            mcp_config=None,
            temp_workspace=AgentTempWorkspaceConfig(cleanup_enabled=False),
        )
        defaults.update(kwargs)
        return AgentConfig(**defaults)

    def test_switch_workspace_rejects_nonexistent_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    with self.assertRaises(AgentError) as ctx:
                        agent.switch_workspace(workspace / "nonexistent")
                    self.assertIn("无法解析", str(ctx.exception))
                finally:
                    agent.close()

    def test_switch_workspace_rejects_file_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "file.txt").write_text("hello", encoding="utf-8")
            config = self._make_config(workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    with self.assertRaises(AgentError) as ctx:
                        agent.switch_workspace(workspace / "file.txt")
                    self.assertIn("不是目录", str(ctx.exception))
                finally:
                    agent.close()

    def test_switch_workspace_idempotent_same_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    new_root = agent.switch_workspace(str(workspace))
                    self.assertEqual(new_root, workspace.resolve())
                    self.assertEqual(agent.workspace_root, workspace.resolve())
                finally:
                    agent.close()

    def test_switch_workspace_updates_workspace_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    self.assertEqual(
                        agent.workspace_root.resolve(),
                        orig_workspace.resolve(),
                    )
                    agent.switch_workspace(target_workspace)
                    self.assertEqual(
                        agent.workspace_root.resolve(),
                        target_workspace.resolve(),
                    )
                finally:
                    agent.close()

    def test_switch_workspace_creates_temp_workspace_in_new_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()

            config = self._make_config(
                orig_workspace,
                temp_workspace=AgentTempWorkspaceConfig(
                    cleanup_enabled=False,
                    directory=".agent_tmp",
                ),
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    agent.switch_workspace(target_workspace)
                    temp_dirs = list(target_workspace.iterdir())
                    self.assertTrue(
                        any(d.name == ".agent_tmp" for d in temp_dirs if d.is_dir())
                    )
                finally:
                    agent.close()

    def test_switch_workspace_clears_history(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._history = [
                    {"role": "user", "content": "hello"},
                    {"role": "assistant", "content": "hi"},
                ]
                try:
                    agent.switch_workspace(target_workspace)
                    self.assertEqual(agent._history, [])
                    self.assertIsNone(agent._pending_user_text)
                    self.assertEqual(agent._active_skills, [])
                finally:
                    agent.close()

    def test_switch_workspace_rebuilds_workspace_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            (target_workspace / "hello.txt").write_text("content", encoding="utf-8")
            config = self._make_config(orig_workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    agent.switch_workspace(target_workspace)
                    read_tool = agent._tools.get("read_file")
                    self.assertIsNotNone(read_tool)
                    result = read_tool.run({"path": "hello.txt"})
                    self.assertTrue(result.ok)
                    self.assertIn("content", result.output)
                finally:
                    agent.close()

    def test_agent_registers_shell_monitor_and_extended_read_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    self.assertTrue({"read_file", "bash", "powershell", "monitor"}.issubset(agent._tools))
                    self.assertNotIn("run_command", agent._tools)
                    schema = agent._tools["read_file"].argument_schema
                    self.assertIn("function_name", schema)
                    self.assertIn("context_lines", schema)
                finally:
                    agent.close()

    def test_switch_workspace_closes_monitor_manager(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            monitor_manager = SimpleNamespace(close=Mock())
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._monitor_manager = monitor_manager
                try:
                    agent.switch_workspace(target_workspace)
                    monitor_manager.close.assert_called_once_with()
                    self.assertFalse(hasattr(agent, "_monitor_manager"))
                finally:
                    agent.close()

    def test_close_closes_monitor_manager(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            monitor_manager = SimpleNamespace(close=Mock())
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._monitor_manager = monitor_manager
                agent.close()

        monitor_manager.close.assert_called_once_with()

    def test_switch_workspace_with_sessions_enabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(
                orig_workspace,
                session_enabled=True,
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                old_session_id = agent.current_session_id
                self.assertTrue(bool(old_session_id))
                try:
                    agent.switch_workspace(target_workspace)
                    new_session_id = agent.current_session_id
                    self.assertTrue(bool(new_session_id))
                    self.assertNotEqual(new_session_id, old_session_id)
                finally:
                    agent.close()

    def test_switch_workspace_with_memory_enabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(
                orig_workspace,
                memory_enabled=True,
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    agent.switch_workspace(target_workspace)
                    self.assertIsNotNone(agent._memory_store)
                    self.assertEqual(
                        agent._memory_store.root.resolve().parent,
                        target_workspace.resolve(),
                    )
                finally:
                    agent.close()


if __name__ == "__main__":
    unittest.main()
