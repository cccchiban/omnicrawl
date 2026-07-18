"""中文设置面板、持久化开关和 Agent 即时切换回归。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentError, LocalToolAgent
from omnicrawl.mcp.config import MCPConfig
from omnicrawl.config.settings import (
    SettingsConfigError,
    load_feature_enabled,
    save_feature_enabled,
)
from omnicrawl.config.subagents import SubAgentConfig
from omnicrawl.extensions.plugin_manager import PluginRuntime
from omnicrawl.ui.fullscreen.commands import CommandDispatcher
from omnicrawl.ui.fullscreen.settings import SettingsScreen


class SettingsConfigTests(unittest.TestCase):
    def test_feature_switch_round_trip_preserves_other_settings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                "memory:\n  enabled: true\n"
                "mcp:\n  enabled: false\n  servers:\n    demo:\n      enabled: true\n",
                encoding="utf-8",
            )

            saved = save_feature_enabled("mcp", True, path)
            save_feature_enabled("plugins", True, path)
            save_feature_enabled("subagents", True, path)

            self.assertEqual(saved, path)
            for section in ("memory", "mcp", "plugins", "subagents"):
                self.assertTrue(
                    load_feature_enabled(section, default=False, config_path=path)
                )
            text = path.read_text(encoding="utf-8")
            self.assertIn("demo:", text)
            self.assertIn("enabled: true", text)


class SettingsCommandTests(unittest.TestCase):
    def test_only_settings_opens_settings_panel(self) -> None:
        agent = SimpleNamespace(workspace_root=Path("D:/workspace"))
        dispatcher = CommandDispatcher(agent)

        settings = dispatcher.dispatch("/settings")
        singular = dispatcher.dispatch("/setting")

        self.assertTrue(settings.handled)
        self.assertTrue(settings.open_settings)
        self.assertFalse(settings.open_model_picker)
        self.assertFalse(settings.open_settings is False)
        self.assertFalse(singular.handled)

    def test_settings_screen_declares_navigation_bindings(self) -> None:
        descriptions = " ".join(binding[2] for binding in SettingsScreen.BINDINGS)
        self.assertIn("选择", descriptions)
        self.assertIn("取消", descriptions)
        self.assertIn("上一项", descriptions)


class SettingsScreenFailureTests(unittest.IsolatedAsyncioTestCase):
    async def test_should_report_when_feature_config_rollback_fails(self) -> None:
        """运行时切换失败且磁盘回滚失败时，界面不能伪装成普通设置失败。"""

        from textual.app import App, ComposeResult
        from textual.widgets import Static

        class FakeAgent:
            current_model = "demo-model"
            reasoning_effort = "none"
            approval_mode = "manual"
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)
            config = SimpleNamespace(subagents=SimpleNamespace(enabled=False))

            def set_memory_enabled(self, _enabled: bool) -> None:
                raise AgentError("运行时重建失败")

            def set_mcp_enabled(self, _enabled: bool) -> None:
                pass

            def set_plugin_enabled(self, _enabled: bool) -> None:
                pass

            def set_subagents_enabled(self, _enabled: bool) -> None:
                pass

        class SettingsApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(SettingsScreen(FakeAgent()))

        app = SettingsApp()
        with patch(
            "omnicrawl.ui.fullscreen.settings.save_feature_enabled",
            side_effect=[Path("config.yaml"), SettingsConfigError("回滚写入失败")],
        ):
            async with app.run_test(size=(100, 32)) as pilot:
                await pilot.pause()
                screen = app.screen
                worker = screen._apply_setting("memory", True)
                await worker.wait()

                self.assertIn("配置回滚失败", screen._status)
                self.assertIn("运行时重建失败", screen._status)


class AgentRuntimeSettingsTests(unittest.TestCase):
    def test_memory_toggle_rebuilds_tools_after_store_swap(self) -> None:
        agent = object.__new__(LocalToolAgent)
        old_store = object()
        next_store = object()
        agent.config = SimpleNamespace(memory_enabled=True)
        agent._memory_store = old_store
        agent._create_memory_store = Mock(return_value=next_store)
        agent._build_tools = Mock(return_value={"read_file": object()})

        LocalToolAgent.set_memory_enabled(agent, True)

        self.assertIs(agent._memory_store, next_store)
        self.assertTrue(agent.config.memory_enabled)
        agent._build_tools.assert_called_once()

    def test_mcp_toggle_prepares_new_manager_before_closing_old(self) -> None:
        events: list[str] = []

        class Manager:
            enabled = False

            def close(self) -> None:
                events.append("close")

        old = Manager()
        new = Manager()
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(mcp_config=MCPConfig(enabled=False))
        agent._mcp_manager = old
        agent._create_mcp_manager = Mock(side_effect=lambda: events.append("create") or new)
        agent._build_tools = Mock(return_value={})

        LocalToolAgent.set_mcp_enabled(agent, True)

        self.assertEqual(events, ["create", "close"])
        self.assertTrue(agent.config.mcp_config.enabled)
        self.assertIs(agent._mcp_manager, new)

    def test_subagent_enable_rebuilds_tools_after_coordinator_creation(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            subagents=SubAgentConfig(enabled=False, model_request_concurrency=2),
        )
        agent._subagent_coordinator = None
        agent._subagent_approval_broker = None
        agent._subagent_model_request_semaphore = None
        agent._refresh_subagent_definitions = Mock()
        agent._build_tools = Mock(return_value={"subagent": object()})

        LocalToolAgent.set_subagents_enabled(agent, True)

        self.assertTrue(agent.config.subagents.enabled)
        agent._refresh_subagent_definitions.assert_called_once()
        agent._build_tools.assert_called_once()
        self.assertIn("subagent", agent._tools)

    def test_subagent_disable_waits_for_tasks_before_removing_tool(self) -> None:
        coordinator = SimpleNamespace(
            cancel_and_wait=Mock(return_value=True),
        )
        broker = SimpleNamespace(close=Mock())
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            subagents=SubAgentConfig(enabled=True, model_request_concurrency=2),
        )
        agent._subagent_coordinator = coordinator
        agent._subagent_approval_broker = broker
        agent._subagent_model_request_semaphore = object()
        agent._build_tools = Mock(return_value={})

        LocalToolAgent.set_subagents_enabled(agent, False)

        coordinator.cancel_and_wait.assert_called_once()
        broker.close.assert_called_once()
        self.assertFalse(agent.config.subagents.enabled)
        self.assertIsNone(agent._subagent_coordinator)
        self.assertIsNone(agent._subagent_model_request_semaphore)

    def test_subagent_disable_timeout_keeps_current_runtime(self) -> None:
        coordinator = SimpleNamespace(cancel_and_wait=Mock(return_value=False))
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            subagents=SubAgentConfig(enabled=True, model_request_concurrency=2),
        )
        agent._subagent_coordinator = coordinator

        with self.assertRaisesRegex(AgentError, "仍有子任务未退出"):
            LocalToolAgent.set_subagents_enabled(agent, False)

        self.assertTrue(agent.config.subagents.enabled)
        self.assertIs(agent._subagent_coordinator, coordinator)


class PluginRuntimeSettingsTests(unittest.TestCase):
    def test_plugin_runtime_disable_closes_manager_and_can_reenable(self) -> None:
        runtime = PluginRuntime(workspace_root=Path.cwd())
        manager = Mock()
        runtime.manager = manager
        runtime._started = True
        runtime.config = runtime.config.__class__(enabled=True)

        with patch.object(runtime, "start") as start:
            runtime.set_enabled(False)
            self.assertFalse(runtime.config.enabled)
            manager.close.assert_called_once()
            self.assertIsNone(runtime.manager)

            runtime.set_enabled(True)
            start.assert_called_once()


if __name__ == "__main__":
    unittest.main()
