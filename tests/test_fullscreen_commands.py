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

            def __init__(self) -> None:
                self.reset_calls = 0
                self.workspace_calls: list[str] = []

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
        """新会话不需要 worker，但 UI 应刷新会话相关顶部摘要。"""

        outcome = CommandDispatcher(self.agent).dispatch("/new")

        self.assertTrue(outcome.handled)
        self.assertEqual(outcome.message, "已开启新对话。")
        self.assertEqual(outcome.execution, "immediate")
        self.assertTrue(outcome.refresh_context)
        self.assertEqual(self.agent.reset_calls, 1)

    def test_exit_words_are_handled_without_agent_side_effect(self) -> None:
        """退出语义由 UI 执行，分派器仅显式返回退出意图。"""

        outcome = CommandDispatcher(self.agent).dispatch("结束")

        self.assertTrue(outcome.handled)
        self.assertTrue(outcome.exit_requested)
        self.assertIsNone(outcome.message)
        self.assertEqual(self.agent.reset_calls, 0)

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

    def test_mcp_and_model_commands_remain_lazy(self) -> None:
        """可能触发连接或模型探测的命令不能在分派阶段执行。"""

        calls: list[tuple[str, Any]] = []
        dispatcher = CommandDispatcher(
            self.agent,
            format_mcp=lambda agent: calls.append(("mcp", agent)) or "MCP 状态",
            handle_model=lambda agent, text: calls.append(("model", text)) or "模型状态",
        )

        mcp = dispatcher.dispatch("/mcp")
        # 裸 /model 打开双列选择界面，不在分派阶段执行探测。
        model = dispatcher.dispatch("/model")
        # 带参数时仍走慢命令 worker。
        switch = dispatcher.dispatch("/model gpt-test")

        self.assertEqual(calls, [])
        self.assertEqual(mcp.execution, "slow")
        self.assertTrue(model.open_model_picker)
        self.assertFalse(model.model_picker_refresh)
        self.assertIsNone(model.command)
        self.assertTrue(model.refresh_context)
        self.assertEqual(switch.execution, "slow")
        self.assertTrue(switch.refresh_context)
        self.assertEqual(mcp.command(), "MCP 状态")
        self.assertEqual(switch.command(), "模型状态")
        self.assertEqual(calls, [("mcp", self.agent), ("model", "/model gpt-test")])

    def test_model_refresh_opens_picker_with_refresh_flag(self) -> None:
        outcome = CommandDispatcher(self.agent).dispatch("/model --refresh")
        self.assertTrue(outcome.handled)
        self.assertTrue(outcome.open_model_picker)
        self.assertTrue(outcome.model_picker_refresh)

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
