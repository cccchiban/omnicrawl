"""全屏斜杠命令分派的非 Textual 回归测试。"""

from __future__ import annotations

import unittest
from typing import Any

from omnicrawl.ui.fullscreen.commands import CommandDispatcher


class CommandDispatcherTests(unittest.TestCase):
    """锁定命令识别、执行类别和 UI 生命周期之间的边界。"""

    def setUp(self) -> None:
        class FakeAgent:
            workspace_root = "D:/workspace"
            skill_manager = None

            def __init__(self) -> None:
                self.reset_calls = 0
                self.workspace_calls: list[str] = []
                self.current_session_id = "20260813-144021-c613cb"

            def reset_conversation(self) -> None:
                self.reset_calls += 1

            def switch_workspace(self, workspace: str) -> None:
                self.workspace_calls.append(workspace)
                self.workspace_root = workspace

        self.agent = FakeAgent()

    def test_regular_text_is_not_handled(self) -> None:
        """自然语言必须继续进入 Agent 回合，不能被命令分派器吞掉。"""

        outcome = CommandDispatcher(self.agent).dispatch("请分析这个项目")

        self.assertFalse(outcome.handled)
        self.assertIsNone(outcome.message)

    def test_new_chat_is_immediate_and_requests_context_refresh(self) -> None:
        """新会话不需要 worker，但 UI 应刷新会话相关顶部摘要并清空对话视图。"""

        outcome = CommandDispatcher(self.agent).dispatch("/new")

        self.assertTrue(outcome.handled)
        self.assertEqual(
            outcome.message, "已新开会话，旧会话：20260813-144021-c613cb"
        )
        self.assertEqual(outcome.execution, "immediate")
        self.assertTrue(outcome.refresh_context)
        self.assertTrue(outcome.clear_conversation)
        self.assertEqual(self.agent.reset_calls, 1)

    def test_new_chat_without_session_id_degrades_message(self) -> None:
        """会话系统关闭（ID 为空）时，提示降级为不带旧会话 ID。"""

        self.agent.current_session_id = ""

        outcome = CommandDispatcher(self.agent).dispatch("/new")

        self.assertTrue(outcome.handled)
        self.assertEqual(outcome.message, "已新开会话。")
        self.assertTrue(outcome.clear_conversation)
        self.assertEqual(self.agent.reset_calls, 1)

    def test_quit_command_requests_tui_exit_without_agent_side_effect(self) -> None:
        """/quit 只请求退出当前 TUI，不触发 Agent 关闭或其他副作用。"""

        outcome = CommandDispatcher(self.agent).dispatch("/quit")

        self.assertTrue(outcome.handled)
        self.assertTrue(outcome.exit_requested)
        self.assertIsNone(outcome.message)
        self.assertEqual(outcome.execution, "immediate")
        self.assertEqual(self.agent.reset_calls, 0)

    def test_slash_command_options_include_quit(self) -> None:
        from omnicrawl.commands.slash import build_slash_command_options

        options = build_slash_command_options(self.agent)

        self.assertIn("/quit", {option["command"] for option in options})

    def test_workspace_query_is_immediate_but_switch_is_lazy(self) -> None:
        """查询工作区无需 worker；切换必须延迟到 UI 的慢命令 worker。"""

        dispatcher = CommandDispatcher(self.agent)
        query = dispatcher.dispatch("/workspace")
        switch = dispatcher.dispatch("/workspace D:/next")

        self.assertTrue(query.handled)
        self.assertEqual(query.execution, "immediate")
        self.assertIn("D:/workspace", query.message or "")
        self.assertTrue(switch.handled)
        self.assertEqual(switch.execution, "slow")
        self.assertTrue(switch.workspace_switch_requested)
        self.assertTrue(switch.refresh_context)
        self.assertEqual(self.agent.workspace_calls, [])
        self.assertIsNotNone(switch.command)
        self.assertEqual(switch.command(), "已切换工作区：D:/next")
        self.assertEqual(self.agent.workspace_calls, ["D:/next"])

    def test_mcp_command_remains_lazy(self) -> None:
        """可能触发连接的 /mcp 不能在分派阶段执行。"""

        calls: list[tuple[str, Any]] = []
        dispatcher = CommandDispatcher(
            self.agent,
            format_mcp=lambda agent: calls.append(("mcp", agent)) or "MCP 状态",
        )

        mcp = dispatcher.dispatch("/mcp")
        # /model 已移除：模型设置只能通过设置面板进入，分派不再识别。
        removed_model = dispatcher.dispatch("/model")
        removed_alias = dispatcher.dispatch("/models")
        removed_switch = dispatcher.dispatch("/model gpt-test")

        self.assertEqual(calls, [])
        self.assertEqual(mcp.execution, "slow")
        self.assertFalse(removed_model.handled)
        self.assertFalse(removed_alias.handled)
        self.assertFalse(removed_switch.handled)
        self.assertEqual(mcp.command(), "MCP 状态")
        self.assertEqual(calls, [("mcp", self.agent)])

    def test_settings_opens_chinese_settings_panel_only_for_plural_command(self) -> None:
        settings = CommandDispatcher(self.agent).dispatch("/settings")
        singular = CommandDispatcher(self.agent).dispatch("/setting")

        self.assertTrue(settings.handled)
        self.assertTrue(settings.open_settings)
        self.assertTrue(settings.refresh_context)
        self.assertFalse(singular.handled)

    def test_plugins_command_is_readonly_immediate(self) -> None:
        """/plugins 只读状态应即时返回，不走慢命令 worker。"""

        dispatcher = CommandDispatcher(
            self.agent,
            format_plugins=lambda _agent: "插件系统：已关闭",
        )
        outcome = dispatcher.dispatch("/plugins")
        self.assertTrue(outcome.handled)
        self.assertEqual(outcome.execution, "immediate")
        self.assertEqual(outcome.message, "插件系统：已关闭")

    def test_existing_command_handlers_keep_priority_and_context_refresh(self) -> None:
        """既有 Session、审批和推理处理器的返回语义不得变化。"""

        calls: list[tuple[str, str]] = []

        def session_handler(_agent, text: str) -> str | None:
            calls.append(("session", text))
            return "会话已恢复" if text == "/resume session-1" else None

        def approval_handler(_agent, text: str) -> str | None:
            calls.append(("approval", text))
            return "审批已切换" if text == "/approval:auto" else None

        dispatcher = CommandDispatcher(
            self.agent,
            handle_session=session_handler,
            handle_approval=approval_handler,
        )

        resumed = dispatcher.dispatch("/resume session-1")
        approved = dispatcher.dispatch("/approval:auto")

        self.assertTrue(resumed.handled)
        self.assertEqual(resumed.message, "会话已恢复")
        self.assertTrue(resumed.refresh_context)
        self.assertTrue(approved.handled)
        self.assertEqual(approved.message, "审批已切换")
        self.assertTrue(approved.refresh_context)
        self.assertEqual(
            calls,
            [
                ("session", "/resume session-1"),
                ("session", "/approval:auto"),
                ("approval", "/approval:auto"),
            ],
        )

    def test_subagent_task_command_is_immediate_and_uses_injected_handler(self) -> None:
        """TUI 分派层只展示控制面结果，不会启动新的 Agent 回合。"""

        calls: list[str] = []
        dispatcher = CommandDispatcher(
            self.agent,
            handle_subagent_task=lambda _agent, text: calls.append(text) or "子任务列表",
        )

        outcome = dispatcher.dispatch("/tasks")

        self.assertTrue(outcome.handled)
        self.assertEqual(outcome.execution, "immediate")
        self.assertEqual(outcome.message, "子任务列表")
        self.assertFalse(outcome.refresh_context)
        self.assertEqual(calls, ["/tasks"])

    def test_is_immediate_marks_readonly_commands_only(self) -> None:
        """生成期间仅纯 UI/只读命令可即时执行，其余命令必须排队。"""

        dispatcher = CommandDispatcher(self.agent)

        immediate = [
            "/quit", "退出", "结束", "再见",
            "/settings", "/skills", "/plugins", "/approval",
            "/sessions", "/archives", "/reasoning", "/tasks", "/workspace",
            "/history 关键词", "/task task-1",
        ]
        queued = [
            "请分析这个项目",
            "/new", "/mcp", "/workspace D:/next", "/memory:clean",
            "/approval:manual", "/auto-approve:on", "/auto-review:on",
            "/reasoning low", "/archive", "/undo", "/compact",
            "/rename 新标题", "/resume session-1", "/task cancel task-1",
        ]

        for command in immediate:
            with self.subTest(command=command):
                self.assertTrue(dispatcher.is_immediate(command))
        for command in queued:
            with self.subTest(command=command):
                self.assertFalse(dispatcher.is_immediate(command))
