from __future__ import annotations

import unittest

from omnicrawl.extensions.plugin_manager import HookDispatcher, PluginWorkerState
from omnicrawl.extensions.plugin_models import (
    HandlerRegistration,
    HookResult,
    PluginManifest,
    PluginProtocolError,
    PluginRecord,
    PluginsConfig,
    ResolvedHandler,
    parse_hook_result,
)


class FakeClient:
    def __init__(self, responses: dict[str, dict]):
        self.responses = responses

    def invoke_handler(self, *, handler_id: str, event, timeout_ms: int):
        return self.responses.get(handler_id, {"action": "continue"})


def _worker(name: str, handlers: list[HandlerRegistration], client: FakeClient) -> PluginWorkerState:
    manifest = PluginManifest(
        name=name,
        version="1.0.0",
        api_version="1",
        entry="x.js",
        permissions=tuple(f"hook:{h.hook}" for h in handlers),
        hooks=tuple(handlers),
        engines_omnicrawl=">=0.1",
        engines_node=">=20",
    )
    return PluginWorkerState(
        name=name,
        manifest=manifest,
        scope="user",
        record=PluginRecord(name=name, enabled=True),
        root=".",
        client=client,  # type: ignore[arg-type]
        active=True,
    )


class PluginSecurityTest(unittest.TestCase):
    def test_approve_action_rejected_by_parser(self) -> None:
        with self.assertRaises(PluginProtocolError):
            parse_hook_result({"action": "approve"}, handler_key="@p/h")

    def test_non_guard_deny_is_ignored(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        handler = ResolvedHandler(
            key="@p/obs",
            plugin_name="@p",
            plugin_version="1",
            handler_id="obs",
            hook="tool.approval.after",
            mode="notify",
            priority=1,
            scope="user",
            timeout_ms=200,
        )
        client = FakeClient({"obs": {"action": "deny", "reason": "nope"}})
        worker = _worker(
            "@p",
            [HandlerRegistration(id="obs", hook="tool.approval.after", mode="notify")],
            client,
        )
        dispatcher.set_execution_plan([handler], {"@p": worker})
        outcome = dispatcher.dispatch(
            "tool.approval.after",
            {"tool": "bash", "approved": False},
        )
        # notify deny 不能改 Host 结果
        self.assertFalse(outcome.denied)

    def test_guard_can_only_deny_not_approve_host_tool(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        handler = ResolvedHandler(
            key="@p/guard",
            plugin_name="@p",
            plugin_version="1",
            handler_id="guard",
            hook="tool.approval.before",
            mode="guard",
            priority=1,
            scope="user",
            timeout_ms=200,
        )
        client = FakeClient({"guard": {"action": "deny", "reason": "blocked by plugin"}})
        worker = _worker(
            "@p",
            [HandlerRegistration(id="guard", hook="tool.approval.before", mode="guard")],
            client,
        )
        dispatcher.set_execution_plan([handler], {"@p": worker})
        outcome = dispatcher.dispatch(
            "tool.approval.before",
            {"tool": "bash", "arguments": {"command": "rm -rf /"}},
        )
        self.assertTrue(outcome.denied)
        self.assertIn("blocked", outcome.deny_reason)

    def test_invalid_transform_patch_does_not_enter_host_state(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        handler = ResolvedHandler(
            key="@p/t",
            plugin_name="@p",
            plugin_version="1",
            handler_id="t",
            hook="tool.execute.after",
            mode="transform",
            priority=1,
            scope="user",
            timeout_ms=200,
        )
        client = FakeClient(
            {
                "t": {
                    "action": "patch",
                    "patch": [{"op": "replace", "path": "/payload/secrets", "value": "x"}],
                }
            }
        )
        worker = _worker(
            "@p",
            [HandlerRegistration(id="t", hook="tool.execute.after", mode="transform")],
            client,
        )
        dispatcher.set_execution_plan([handler], {"@p": worker})
        outcome = dispatcher.dispatch(
            "tool.execute.after",
            {"displayText": "ok", "annotations": {}},
        )
        self.assertEqual(outcome.payload["displayText"], "ok")
        self.assertFalse(outcome.denied)


if __name__ == "__main__":
    unittest.main()
