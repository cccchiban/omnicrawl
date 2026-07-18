from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.extensions.plugin_manager import (
    HookDispatcher,
    PluginManager,
    PluginRuntime,
    PluginWorkerState,
)
from omnicrawl.extensions.plugin_models import (
    HookResult,
    PluginsConfig,
    ResolvedHandler,
)
from omnicrawl.extensions.plugin_models import (
    PluginManifest,
    PluginRecord,
    HandlerRegistration,
)


class FakeClient:
    def __init__(self, responses: dict[str, dict]):
        self.responses = responses
        self.calls: list[str] = []

    def invoke_handler(self, *, handler_id: str, event, timeout_ms: int):
        self.calls.append(handler_id)
        return self.responses.get(handler_id, {"action": "continue"})


class HookDispatcherTest(unittest.TestCase):
    def test_no_handlers_passthrough(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        outcome = dispatcher.dispatch("turn.start", {"userText": "hi", "tags": []})
        self.assertFalse(outcome.denied)
        self.assertEqual(outcome.payload["userText"], "hi")

    def test_disabled_config_skips(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=False))
        handler = ResolvedHandler(
            key="@p/h",
            plugin_name="@p",
            plugin_version="1",
            handler_id="h",
            hook="turn.start",
            mode="guard",
            priority=1,
            scope="user",
            timeout_ms=500,
        )
        dispatcher.set_execution_plan([handler], {})
        outcome = dispatcher.dispatch("turn.start", {"userText": "x", "tags": []})
        self.assertFalse(outcome.denied)

    def test_guard_deny_and_transform_patch(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        guard = ResolvedHandler(
            key="@p/guard",
            plugin_name="@p",
            plugin_version="1",
            handler_id="guard",
            hook="turn.start",
            mode="guard",
            priority=10,
            scope="user",
            timeout_ms=500,
        )
        transform = ResolvedHandler(
            key="@p/tag",
            plugin_name="@p",
            plugin_version="1",
            handler_id="tag",
            hook="turn.start",
            mode="transform",
            priority=5,
            scope="user",
            timeout_ms=500,
        )
        client = FakeClient(
            {
                "guard": {"action": "continue"},
                "tag": {
                    "action": "patch",
                    "patch": [{"op": "add", "path": "/payload/tags/-", "value": "reviewed"}],
                },
            }
        )
        manifest = PluginManifest(
            name="@p",
            version="1",
            api_version="1",
            entry="x.js",
            permissions=("hook:turn.start",),
            hooks=(
                HandlerRegistration(id="guard", hook="turn.start", mode="guard"),
                HandlerRegistration(id="tag", hook="turn.start", mode="transform"),
            ),
            engines_omnicrawl=">=0.1",
            engines_node=">=20",
        )
        worker = PluginWorkerState(
            name="@p",
            manifest=manifest,
            scope="user",
            record=PluginRecord(name="@p", enabled=True),
            root=".",
            client=client,  # type: ignore[arg-type]
            active=True,
        )
        dispatcher.set_execution_plan([guard, transform], {"@p": worker})
        outcome = dispatcher.dispatch("turn.start", {"userText": "hello", "tags": []})
        self.assertFalse(outcome.denied)
        self.assertIn("reviewed", outcome.payload.get("tags", []))

    def test_turn_snapshot_immutable(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        h1 = ResolvedHandler(
            key="@p/a",
            plugin_name="@p",
            plugin_version="1",
            handler_id="a",
            hook="turn.end",
            mode="notify",
            priority=1,
            scope="user",
            timeout_ms=100,
        )
        dispatcher.set_execution_plan([h1], {})
        dispatcher.begin_turn()
        h2 = ResolvedHandler(
            key="@p/b",
            plugin_name="@p",
            plugin_version="1",
            handler_id="b",
            hook="turn.end",
            mode="notify",
            priority=1,
            scope="user",
            timeout_ms=100,
        )
        dispatcher.set_execution_plan([h2], {})
        # 当前 turn 仍使用旧计划
        self.assertEqual([item.key for item in dispatcher.current_plan()], ["@p/a"])
        dispatcher.end_turn()
        self.assertEqual([item.key for item in dispatcher.current_plan()], ["@p/b"])

    def test_plugin_agent_definition_symlink_cannot_escape_plugin_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            parent = Path(temp_dir)
            root = parent / "plugin"
            link = root / "agents" / "reviewer.md"
            link.parent.mkdir(parents=True)
            outside = parent / "outside.md"
            outside.write_text("secret", encoding="utf-8")
            try:
                link.symlink_to(outside)
            except OSError as exc:
                self.skipTest(f"当前环境不能创建文件符号链接：{exc}")
            manager = PluginManager(workspace_root=root)
            manifest = PluginManifest(
                name="@p/agents",
                version="1.0.0",
                api_version="1",
                entry="x.js",
                permissions=("agent:definitions",),
                hooks=(),
                engines_omnicrawl=">=0.1",
                engines_node=">=20",
                agents=("agents/reviewer.md",),
            )
            manager._workers = {
                "@p/agents": PluginWorkerState(
                    name="@p/agents",
                    manifest=manifest,
                    scope="project",
                    record=PluginRecord(
                        name="@p/agents",
                        enabled=True,
                        approved_permissions=["agent:definitions"],
                    ),
                    root=root,
                    active=True,
                )
            }

            paths = manager.agent_definition_paths()

        self.assertEqual(paths, [])

    def test_agent_definition_paths_only_include_active_approved_declarations(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            definition_path = root / "agents" / "reviewer.md"
            definition_path.parent.mkdir(parents=True)
            definition_path.write_text("---\nname: reviewer\ndescription: review\n---\nbody\n", encoding="utf-8")
            manager = PluginManager(workspace_root=root)
            manifest = PluginManifest(
                name="@p/agents",
                version="1.0.0",
                api_version="1",
                entry="x.js",
                permissions=("agent:definitions",),
                hooks=(),
                engines_omnicrawl=">=0.1",
                engines_node=">=20",
                agents=("agents/reviewer.md",),
            )
            manager._workers = {
                "@p/agents": PluginWorkerState(
                    name="@p/agents",
                    manifest=manifest,
                    scope="project",
                    record=PluginRecord(
                        name="@p/agents",
                        enabled=True,
                        approved_permissions=["agent:definitions"],
                    ),
                    root=root,
                    active=True,
                )
            }

            paths = manager.agent_definition_paths()

        self.assertEqual(paths, [("@p/agents", definition_path.resolve())])

    def test_runtime_start_closes_candidate_when_start_hook_denies(self) -> None:
        candidate = Mock()
        candidate.bootstrap.return_value = []
        candidate.dispatch.return_value = SimpleNamespace(
            denied=True,
            deny_reason="plugin denied startup",
        )
        runtime = PluginRuntime(
            config=PluginsConfig(enabled=True),
            workspace_root=Path("D:/workspace"),
        )

        with patch(
            "omnicrawl.extensions.plugin_manager.PluginManager",
            return_value=candidate,
        ):
            with self.assertRaisesRegex(Exception, "plugin denied startup"):
                runtime.start()

        self.assertIsNone(runtime.manager)
        self.assertFalse(runtime._started)
        candidate.close.assert_called_once_with()

    def test_runtime_enable_notifies_app_started_after_rebuilding_manager(self) -> None:
        """运行中重新开启插件时，也必须补齐 app.start.after 生命周期钩子。"""

        runtime = PluginRuntime(
            config=PluginsConfig(enabled=False),
            workspace_root=Path("D:/workspace"),
        )
        # 启动时 plugins.enabled=false 仍会保留一个无插件 Manager，因此重新开启
        # 会走 switch_workspace 分支而不是 start 分支。
        runtime.manager = Mock()
        runtime._started = True

        with patch.object(runtime, "switch_workspace") as switch_workspace, patch.object(
            runtime, "notify_app_started"
        ) as notify_app_started:
            runtime.set_enabled(True)

        switch_workspace.assert_called_once_with(runtime.workspace_root)
        notify_app_started.assert_called_once_with()

    def test_runtime_workspace_switch_keeps_old_manager_when_candidate_bootstrap_fails(self) -> None:
        old_root = Path("D:/old").resolve()
        new_root = Path("D:/new").resolve()
        old_manager = Mock()
        candidate = Mock()
        candidate.bootstrap.side_effect = RuntimeError("bootstrap failed")
        runtime = PluginRuntime(
            config=PluginsConfig(enabled=True),
            workspace_root=old_root,
        )
        runtime.manager = old_manager
        runtime._started = True

        with patch(
            "omnicrawl.extensions.plugin_manager.PluginManager",
            return_value=candidate,
        ):
            with self.assertRaisesRegex(RuntimeError, "bootstrap failed"):
                runtime.switch_workspace(new_root)

        self.assertIs(runtime.manager, old_manager)
        self.assertEqual(runtime.workspace_root, old_root)
        old_manager.close.assert_not_called()
        candidate.close.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
