"""中文设置面板、持久化开关和 Agent 即时切换回归。"""

from __future__ import annotations

import tempfile
import unittest

import yaml
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentError, LocalToolAgent
from omnicrawl.config.context_compaction import ContextCompactionConfig
from omnicrawl.mcp.config import MCPConfig
from omnicrawl.config.settings import (
    SettingsConfigError,
    load_feature_enabled,
    save_context_window_tokens,
    save_feature_enabled,
    save_subagent_setting,
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
            save_feature_enabled("context_compaction", True, path)

            self.assertEqual(saved, path)
            for section in (
                "memory",
                "mcp",
                "plugins",
                "subagents",
                "context_compaction",
            ):
                self.assertTrue(
                    load_feature_enabled(section, default=False, config_path=path)
                )
            text = path.read_text(encoding="utf-8")
            self.assertIn("demo:", text)
            self.assertIn("enabled: true", text)
    def test_subagent_advanced_setting_round_trip_preserves_other_settings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            path.write_text(
                "subagents:\n"
                "  enabled: true\n"
                "  allow_worktree: false\n"
                "  result_summary_chars: 2400\n",
                encoding="utf-8",
            )

            saved = save_subagent_setting("max_concurrency", 4, path)
            data = yaml.safe_load(path.read_text(encoding="utf-8"))

        self.assertEqual(saved, path)
        self.assertEqual(data["subagents"]["max_concurrency"], 4)
        self.assertTrue(data["subagents"]["enabled"])
        self.assertFalse(data["subagents"]["allow_worktree"])
        self.assertEqual(data["subagents"]["result_summary_chars"], 2400)

    def test_subagent_advanced_setting_rejects_excluded_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.yaml"
            for key in (
                "result_summary_chars",
                "max_total_tasks",
                "default_max_turns",
                "default_max_tool_calls",
            ):
                with self.assertRaises(SettingsConfigError):
                    save_subagent_setting(key, 1, path)


class SettingsScreenAdvancedTests(unittest.TestCase):
    def test_subagent_advanced_rows_exclude_result_and_execution_budget_fields(self) -> None:
        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(
            config=SimpleNamespace(
                subagents=SubAgentConfig(
                    max_concurrency=2,
                    max_tasks_per_batch=4,
                    default_timeout_seconds=3600,
                    model_request_concurrency=2,
                    verify_command_timeout_seconds=120,
                    task_retention_minutes=60,
                )
            )
        )
        screen._row_keys = ("subagents_advanced",)

        labels = SettingsScreen._row_labels()
        self.assertIn("subagents_advanced", labels)
        self.assertIn("最大并发数", SettingsScreen._subagent_advanced_labels().values())
        self.assertNotIn("result_summary_chars", SettingsScreen._subagent_advanced_keys())
        self.assertNotIn("default_max_turns", SettingsScreen._subagent_advanced_keys())
        self.assertNotIn("default_max_tool_calls", SettingsScreen._subagent_advanced_keys())

    def test_subagent_advanced_setting_updates_runtime_config(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            subagents=SubAgentConfig(enabled=False, max_concurrency=2)
        )

        LocalToolAgent.set_subagent_advanced_setting(agent, "max_concurrency", 4)

        self.assertEqual(agent.config.subagents.max_concurrency, 4)



    def test_custom_model_context_window_is_saved_to_models_yaml(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            models_path = Path(temp_dir) / "models.yaml"
            models_path.write_text(
                "version: 1\nmodels:\n  demo:\n"
                "    display_name: Demo\n"
                "    profile: openai\n"
                "    model_id: demo-model\n"
                "    protocol: openai_chat_completions\n"
                "    context_window_tokens: 128000\n",
                encoding="utf-8",
            )

            saved = save_context_window_tokens(
                256000,
                model_source="custom",
                catalog_key="demo",
                models_path=models_path,
            )
            data = yaml.safe_load(models_path.read_text(encoding="utf-8"))

        self.assertEqual(saved, models_path)
        self.assertEqual(data["models"]["demo"]["context_window_tokens"], 256000)
        self.assertEqual(data["models"]["demo"]["capabilities"]["context_window_tokens"], 256000)

    def test_detected_model_context_window_is_saved_to_yaml_defaults(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text(
                "llm:\n  defaults:\n    reasoning_effort: low\n"
                "  profiles:\n    openai:\n      provider: openai\n",
                encoding="utf-8",
            )

            saved = save_context_window_tokens(
                512000,
                model_source="detected",
                config_path=config_path,
            )
            data = yaml.safe_load(config_path.read_text(encoding="utf-8"))

        self.assertEqual(saved, config_path)
        self.assertEqual(data["llm"]["defaults"]["context_window_tokens"], 512000)
        self.assertEqual(data["llm"]["defaults"]["reasoning_effort"], "low")


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

    def test_settings_screen_exposes_model_channel_manager(self) -> None:
        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(current_model="demo")
        screen._advanced = False
        screen._row_keys = ("model", "channels")

        self.assertEqual(SettingsScreen._row_labels()["channels"], "模型渠道")
        self.assertEqual(screen._current_row_values()["channels"], "管理")

        descriptions = " ".join(
            binding.description if hasattr(binding, "description") else binding[2]
            for binding in SettingsScreen.BINDINGS
        )
        self.assertIn("选择", descriptions)
        self.assertIn("取消", descriptions)
        self.assertIn("上一项", descriptions)


class SettingsScreenContextTests(unittest.IsolatedAsyncioTestCase):
    async def test_context_window_cycle_applies_k_value_and_persists(self) -> None:
        from textual.app import App, ComposeResult
        from textual.widgets import Static

        class FakeAgent:
            current_model = "demo"
            reasoning_effort = "none"
            approval_mode = "manual"
            context_window_tokens = 128000
            config = SimpleNamespace(
                llm=SimpleNamespace(model_source="detected", catalog_key=""),
                subagents=SimpleNamespace(enabled=False),
            )
            _memory_store = None
            _mcp_manager = SimpleNamespace(enabled=False)
            _plugin_manager = SimpleNamespace(enabled=False)

            def set_context_window_tokens(self, tokens: int) -> int:
                self.context_window_tokens = tokens
                return tokens

        agent = FakeAgent()

        class SettingsApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(SettingsScreen(agent))

        app = SettingsApp()
        with patch(
            "omnicrawl.ui.fullscreen.settings.save_context_window_tokens",
            return_value=Path("config.yaml"),
        ) as save_context:
            async with app.run_test(size=(100, 32)) as pilot:
                await pilot.pause()
                screen = app.screen
                screen._selected = screen._row_keys.index("context")
                worker = screen._apply_setting("context", 256000)
                await worker.wait()

        self.assertEqual(agent.context_window_tokens, 256000)
        save_context.assert_called_once()
        self.assertIn("256K", screen._status)


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
    def test_context_compaction_toggle_rebuilds_evidence_tool(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(context_window_tokens=128_000, max_output_tokens=8_192),
            context_compaction=ContextCompactionConfig(enabled=False),
        )
        agent._tools = {"read_file": object()}
        agent._build_tools = Mock(return_value={"recall_session_evidence": object()})
        agent._context_compaction_service_instance = object()

        LocalToolAgent.set_context_compaction_enabled(agent, True)

        self.assertTrue(agent.config.context_compaction.enabled)
        self.assertIn("recall_session_evidence", agent._tools)
        self.assertNotIn("_context_compaction_service_instance", agent.__dict__)

    def test_context_compaction_toggle_rejects_unsafe_context_window(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(context_window_tokens=80_000, max_output_tokens=8_192),
            context_compaction=ContextCompactionConfig(enabled=False),
        )
        agent._tools = {}
        agent._build_tools = Mock(return_value={})

        with self.assertRaisesRegex(AgentError, "上下文窗口必须大于"):
            LocalToolAgent.set_context_compaction_enabled(agent, True)

        self.assertFalse(agent.config.context_compaction.enabled)
        agent._build_tools.assert_not_called()

    def test_context_compaction_disable_removes_evidence_tool(self) -> None:
        agent = object.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(context_window_tokens=128_000, max_output_tokens=8_192),
            context_compaction=ContextCompactionConfig(enabled=True),
        )
        agent._tools = {"recall_session_evidence": object()}
        agent._build_tools = Mock(return_value={"read_file": object()})

        LocalToolAgent.set_context_compaction_enabled(agent, False)

        self.assertFalse(agent.config.context_compaction.enabled)
        self.assertNotIn("recall_session_evidence", agent._tools)

    def test_context_compaction_tool_rebuild_failure_restores_runtime(self) -> None:
        agent = object.__new__(LocalToolAgent)
        previous_tools = {"read_file": object()}
        previous_service = object()
        agent.config = SimpleNamespace(
            llm=SimpleNamespace(context_window_tokens=128_000, max_output_tokens=8_192),
            context_compaction=ContextCompactionConfig(enabled=False),
        )
        agent._tools = previous_tools
        agent._context_compaction_service_instance = previous_service
        agent._build_tools = Mock(side_effect=RuntimeError("工具表重建失败"))

        with self.assertRaisesRegex(RuntimeError, "工具表重建失败"):
            LocalToolAgent.set_context_compaction_enabled(agent, True)

        self.assertFalse(agent.config.context_compaction.enabled)
        self.assertIs(agent._tools, previous_tools)
        self.assertIs(agent._context_compaction_service_instance, previous_service)

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
