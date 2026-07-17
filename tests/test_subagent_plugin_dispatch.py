from __future__ import annotations

import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.subagents.definitions import AgentDefinition
from omnicrawl.agent.subagents.execution import SubAgentExecutionContext
from omnicrawl.config.subagents import SubAgentConfig
from omnicrawl.extensions.plugin_manager import (
    HookDispatcher,
    PluginDispatchContext,
    activate_plugin_dispatch_context,
    get_active_plugin_dispatch_context,
)
from omnicrawl.extensions.plugin_models import (
    HookResult,
    PluginsConfig,
    ResolvedHandler,
)


def _handler(plugin_name: str, hook: str = "tool.execute.before") -> ResolvedHandler:
    return ResolvedHandler(
        key=f"{plugin_name}/h",
        plugin_name=plugin_name,
        plugin_version="1",
        handler_id=f"{plugin_name}-h",
        hook=hook,
        mode="guard",
        priority=10,
        scope="user",
        timeout_ms=500,
    )


class PluginDispatchContextTests(unittest.TestCase):
    def test_freeze_prefers_turn_plan_and_survives_set_execution_plan(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        first = _handler("plugin-a")
        second = _handler("plugin-b")
        dispatcher.set_execution_plan([first])
        dispatcher.begin_turn()
        frozen = dispatcher.freeze_dispatch_context()
        self.assertEqual(frozen.source, "parent-turn")
        self.assertEqual([item.plugin_name for item in frozen.handlers], ["plugin-a"])
        dispatcher.set_execution_plan([second])
        dispatcher.end_turn()
        self.assertEqual([item.plugin_name for item in frozen.handlers], ["plugin-a"])
        self.assertEqual(dispatcher.current_plan(), (second,))

    def test_active_context_blocks_fallback_to_live_plan(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        live = _handler("live-plugin", hook="tool.call.before")
        frozen = _handler("frozen-plugin", hook="tool.call.before")
        dispatcher.set_execution_plan([live])
        seen: list[str] = []

        def _invoke(handler, *, event, payload):
            seen.append(handler.plugin_name)
            return HookResult.continue_result()

        with patch.object(dispatcher, "_invoke_handler", side_effect=_invoke):
            with activate_plugin_dispatch_context(
                PluginDispatchContext(handlers=(frozen,), source="parent-turn")
            ):
                outcome = dispatcher.dispatch("tool.call.before", {"arguments": {}})
            self.assertFalse(outcome.denied)
            self.assertEqual(seen, ["frozen-plugin"])
            seen.clear()
            outcome = dispatcher.dispatch("tool.call.before", {"arguments": {}})
            self.assertFalse(outcome.denied)
            self.assertEqual(seen, ["live-plugin"])

    def test_empty_active_context_does_not_use_parent_plan(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        live = _handler("live-plugin", hook="tool.call.before")
        dispatcher.set_execution_plan([live])
        seen: list[str] = []

        def _invoke(handler, *, event, payload):
            seen.append(handler.plugin_name)
            return HookResult.continue_result()

        with patch.object(dispatcher, "_invoke_handler", side_effect=_invoke):
            with activate_plugin_dispatch_context(
                PluginDispatchContext(handlers=(), source="none")
            ):
                outcome = dispatcher.dispatch("tool.call.before", {"arguments": {}})
        self.assertFalse(outcome.denied)
        self.assertEqual(seen, [])

    def test_prepare_execution_freezes_plugin_dispatch_from_parent_manager(self) -> None:
        agent = LocalToolAgent.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(subagents=SubAgentConfig(enabled=True))
        agent._plugin_manager = SimpleNamespace(
            freeze_dispatch_context=lambda: PluginDispatchContext(
                handlers=(_handler("parent-frozen"),),
                source="parent-turn",
            )
        )
        agent._active_fork_context_messages = None
        definition = AgentDefinition(
            name="explore",
            description="read only",
            tools=("list_files", "read_file"),
            model="inherit",
            permission_mode="delegated-read-only",
            max_turns=2,
            max_tool_calls=4,
            isolation="shared",
            background=False,
        )
        with patch.object(agent, "_freeze_subagent_model_snapshot", return_value=None):
            context = agent._prepare_subagent_execution(
                definition=definition,
                context="fresh",
                task_model="",
            )
        self.assertIsNotNone(context.plugin_dispatch)
        self.assertEqual(context.plugin_dispatch.source, "parent-turn")
        self.assertEqual(
            [item.plugin_name for item in context.plugin_dispatch.handlers],
            ["parent-frozen"],
        )

    def test_execute_subagent_task_activates_frozen_context_and_never_begins_turn(self) -> None:
        agent = LocalToolAgent.__new__(LocalToolAgent)
        agent.config = SimpleNamespace(
            subagents=SubAgentConfig(enabled=True, default_timeout_seconds=5),
            llm=None,
        )
        agent._tools = []
        agent._subagent_verify_tools = []
        agent._plugin_manager = SimpleNamespace(
            begin_turn=lambda: (_ for _ in ()).throw(AssertionError("begin_turn")),
            end_turn=lambda: (_ for _ in ()).throw(AssertionError("end_turn")),
        )
        agent._active_runtime_snapshot = None
        agent._subagent_model_request_semaphore = None
        agent.workspace_root = Path(".").resolve()
        definition = AgentDefinition(
            name="explore",
            description="read only",
            tools=("list_files",),
            model="inherit",
            permission_mode="delegated-read-only",
            max_turns=1,
            max_tool_calls=1,
            isolation="shared",
            background=False,
        )
        frozen = PluginDispatchContext(
            handlers=(_handler("child-frozen"),),
            source="parent-turn",
        )
        execution_context = SubAgentExecutionContext(
            context="fresh",
            plugin_dispatch=frozen,
        )
        observed = {}

        def fake_run(**_kwargs):
            observed["active"] = get_active_plugin_dispatch_context()
            return SimpleNamespace(final_text="ok", model_turns=1, tool_calls=0)

        from omnicrawl.agent import core as core_mod

        class _FakeRunner:
            def run(self, **kwargs):
                return fake_run(**kwargs)

        with patch.object(core_mod, "AgentLoopRunner", _FakeRunner):
            with patch.object(
                agent,
                "_subagent_llm_protocol",
                return_value=SimpleNamespace(runtime_manager=None),
            ):
                with patch.object(
                    agent,
                    "_runtime_manager_for_protocol",
                    return_value=None,
                ):
                    result = agent._execute_subagent_task(
                        definition=definition,
                        child_tools={},
                        description="inspect",
                        prompt="inspect",
                        cancel_check=lambda: None,
                        execution_context=execution_context,
                    )
        self.assertEqual(result.final_text, "ok")
        active = observed.get("active")
        self.assertIsInstance(active, PluginDispatchContext)
        self.assertEqual([item.plugin_name for item in active.handlers], ["child-frozen"])
        self.assertIsNone(get_active_plugin_dispatch_context())

    def test_parent_turn_plan_not_overwritten_by_child_dispatch_context(self) -> None:
        dispatcher = HookDispatcher(config=PluginsConfig(enabled=True))
        parent_handler = _handler("parent", hook="tool.call.before")
        child_handler = _handler("child", hook="tool.call.before")
        dispatcher.set_execution_plan([parent_handler])
        dispatcher.begin_turn()
        parent_before = dispatcher.current_plan()
        child_ctx = PluginDispatchContext(
            handlers=(child_handler,),
            source="parent-turn",
        )
        barrier = threading.Barrier(2)
        child_seen: list[str] = []
        parent_seen: list[str] = []

        def _invoke(handler, *, event, payload):
            owner = payload.get("owner")
            if owner == "child":
                child_seen.append(handler.plugin_name)
            else:
                parent_seen.append(handler.plugin_name)
            return HookResult.continue_result()

        def child_worker():
            barrier.wait(timeout=2)
            with activate_plugin_dispatch_context(child_ctx):
                barrier.wait(timeout=2)
                dispatcher.dispatch(
                    "tool.call.before",
                    {"arguments": {}, "owner": "child"},
                )
                self.assertEqual(dispatcher.current_plan(), parent_before)
                time.sleep(0.05)

        with patch.object(dispatcher, "_invoke_handler", side_effect=_invoke):
            thread = threading.Thread(target=child_worker)
            thread.start()
            barrier.wait(timeout=2)
            barrier.wait(timeout=2)
            dispatcher.dispatch(
                "tool.call.before",
                {"arguments": {}, "owner": "parent"},
            )
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

        self.assertEqual(child_seen, ["child"])
        self.assertEqual(parent_seen, ["parent"])
        self.assertEqual(dispatcher.current_plan(), parent_before)
        dispatcher.end_turn()


if __name__ == "__main__":
    unittest.main()
