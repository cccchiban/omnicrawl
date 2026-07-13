from __future__ import annotations

import unittest

from omnicrawl.extensions.plugin_manager import HookDispatcher
from omnicrawl.extensions.plugin_models import (
    HookResult,
    PluginsConfig,
    ResolvedHandler,
)
from omnicrawl.extensions.plugin_manager import PluginWorkerState
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


if __name__ == "__main__":
    unittest.main()
