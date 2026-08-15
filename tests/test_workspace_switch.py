from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentConfig, AgentError, LocalToolAgent
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.config.subagents import SubAgentConfig
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
            context_compaction=ContextCompactionConfig(enabled=False),
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
                    directory=".omnicrawl/.agent_tmp",
                ),
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    agent.switch_workspace(target_workspace)
                    temp_dirs = list(target_workspace.iterdir())
                    self.assertTrue(
                        any(d.name == ".omnicrawl" for d in temp_dirs if d.is_dir()) and (target_workspace / ".omnicrawl" / ".agent_tmp").is_dir()
                    )
                finally:
                    agent.close()

    def test_switch_workspace_keeps_history(self) -> None:
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
                    # 会话已解除工作区绑定：切换工作区保留对话上下文。
                    self.assertEqual(
                        agent._history,
                        [
                            {"role": "user", "content": "hello"},
                            {"role": "assistant", "content": "hi"},
                        ],
                    )
                finally:
                    agent.close()

    def test_should_refresh_context_compaction_service_when_workspace_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._context_compaction_service_instance = object()
                try:
                    agent.switch_workspace(target_workspace)
                    self.assertNotIn(
                        "_context_compaction_service_instance",
                        agent.__dict__,
                    )
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
                    read_tool = agent._tools.get("read")
                    self.assertIsNotNone(read_tool)
                    result = read_tool.run({"path": "hello.txt"})
                    self.assertTrue(result.ok)
                    self.assertIn("content", result.output)
                finally:
                    agent.close()

    @unittest.skipUnless(os.name == "nt", "Windows 桌面工具仅在 Windows 注册")
    def test_switch_workspace_rebinds_screenshot_directory_to_new_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    agent.switch_workspace(target_workspace)
                    screenshot_tool = agent._tools["windows_screenshot"]
                    toolbox = screenshot_tool.run.__self__
                    self.assertEqual(
                        toolbox._screenshot_directory,
                        (target_workspace / ".omnicrawl/.agent_tmp" / "images").resolve(),
                    )
                finally:
                    agent.close()

    def test_agent_registers_shell_monitor_and_extended_read_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    # 默认开关：powershell 关闭，其余 shell/读取工具启用。
                    self.assertNotIn("powershell", agent._tools)
                    self.assertTrue(
                        {"find", "read", "bash", "monitor"}.issubset(
                            agent._tools
                        )
                    )
                    self.assertNotIn("run_command", agent._tools)
                    schema = agent._tools["read"].argument_schema
                    self.assertIn("function_name", schema)
                    self.assertIn("context_lines", schema)
                finally:
                    agent.close()

    def test_agent_registers_powershell_when_explicitly_enabled(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace, disabled_tools=frozenset())
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                try:
                    self.assertIn("powershell", agent._tools)
                    self.assertIn("bash", agent._tools)
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

    def test_close_closes_legacy_openai_client(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            client = SimpleNamespace(close=Mock())
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._client = client
                agent.close()

        client.close.assert_called_once_with()
        self.assertIsNone(agent._client)

    def test_close_cancels_subagents_before_shared_resources(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            order = []
            coordinator = SimpleNamespace(
                cancel_and_wait=lambda **_kwargs: order.append("subagents") or True,
            )
            mcp_manager = SimpleNamespace(close=lambda: order.append("mcp"))
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._subagent_coordinator = coordinator
                agent._mcp_manager = mcp_manager
                agent.close()

        self.assertEqual(order[:2], ["subagents", "mcp"])

    def test_close_timeout_defers_resource_release_until_subagents_idle(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            config = self._make_config(workspace)
            state = {"drained": False, "idle_callback": None}
            coordinator = SimpleNamespace(
                cancel_and_wait=lambda **_kwargs: state["drained"],
                call_when_idle=lambda callback: state.__setitem__(
                    "idle_callback",
                    callback,
                ),
            )
            mcp_manager = SimpleNamespace(close=Mock())
            external_runtime_close = Mock()
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._subagent_coordinator = coordinator
                agent._mcp_manager = mcp_manager
                agent.add_close_callback(external_runtime_close)
                agent.close()

                self.assertTrue(agent._closing)
                self.assertFalse(agent._closed)
                mcp_manager.close.assert_not_called()
                external_runtime_close.assert_not_called()

                state["drained"] = True
                state["idle_callback"]()
                self.assertTrue(agent._closed)

        mcp_manager.close.assert_called_once_with()
        external_runtime_close.assert_called_once_with()

    def test_switch_workspace_rejects_unresolved_subagent_worktree(self) -> None:
        """旧项目的待处理 Worktree 不能进入新工作区控制面。"""

        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            session = SimpleNamespace(
                task_id="task-old-workspace",
                branch_name="subagent/task-old-workspace",
                worktree_path=orig_workspace / ".agent_worktrees" / "task-old",
                base_ref="HEAD",
                repo_root=orig_workspace,
            )

            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._subagent_worktree_sessions = {
                    session.task_id: session,
                    session.branch_name: session,
                }
                try:
                    with self.assertRaisesRegex(
                        AgentError,
                        "未处理的 SubAgent worktree",
                    ):
                        agent.switch_workspace(target_workspace)

                    self.assertEqual(agent.workspace_root, orig_workspace.resolve())
                    self.assertEqual(
                        [item["branch"] for item in agent.list_subagent_worktrees()],
                        [session.branch_name],
                    )
                finally:
                    agent.close()

    def test_switch_workspace_timeout_keeps_old_workspace_intact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(orig_workspace)
            state = {"drained": False, "resume_calls": 0}
            coordinator = SimpleNamespace(
                cancel_and_wait=lambda **_kwargs: state["drained"],
                resume_accepting_when_idle=lambda: state.__setitem__(
                    "resume_calls",
                    state["resume_calls"] + 1,
                ),
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                agent._subagent_coordinator = coordinator
                with patch.object(agent, "_prepare_workspace_switch") as prepare:
                    with self.assertRaises(AgentError) as ctx:
                        agent.switch_workspace(target_workspace)
                self.assertIn("已保留原工作区", str(ctx.exception))
                self.assertEqual(agent.workspace_root, orig_workspace.resolve())
                self.assertEqual(state["resume_calls"], 1)
                prepare.assert_not_called()

                state["drained"] = True
                agent.close()

    def test_plugin_switch_failure_keeps_new_workspace_coordinator_available(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            orig_workspace = Path(temp_dir) / "project_a"
            target_workspace = Path(temp_dir) / "project_b"
            orig_workspace.mkdir()
            target_workspace.mkdir()
            config = self._make_config(
                orig_workspace,
                subagents=SubAgentConfig(enabled=True),
            )
            with patch("openai.OpenAI", return_value=SimpleNamespace()):
                agent = LocalToolAgent(config)
                old_coordinator = agent._subagent_coordinator
                agent._on_workspace_switched = lambda _root: (_ for _ in ()).throw(
                    RuntimeError("plugin switch failed")
                )
                try:
                    with self.assertRaises(AgentError):
                        agent.switch_workspace(target_workspace)
                    self.assertEqual(agent.workspace_root, target_workspace.resolve())
                    self.assertIsNot(agent._subagent_coordinator, old_coordinator)
                    self.assertTrue(agent._subagent_coordinator._accepting)
                    self.assertIsNone(agent._plugin_manager)
                finally:
                    agent._on_workspace_switched = None
                    agent.close()

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
                    # 会话已解除工作区绑定：切换工作区保持同一会话。
                    self.assertEqual(new_session_id, old_session_id)
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
                        agent._memory_store.root.resolve(),
                        (target_workspace / ".omnicrawl" / ".oclmemory").resolve(),
                    )
                finally:
                    agent.close()


if __name__ == "__main__":
    unittest.main()
