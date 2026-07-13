"""全屏 Agent 回合控制器的非 UI 回归测试。"""

from __future__ import annotations

import threading
import unittest
from types import SimpleNamespace
from typing import Any

from omnicrawl.ui.fullscreen.turns import AgentTurnCallbacks, AgentTurnController


class AgentTurnControllerTests(unittest.TestCase):
    """锁定 Agent 协议转发与协作式取消，不依赖 Textual 事件循环。"""

    @staticmethod
    def _callbacks(events: list[tuple[Any, ...]]) -> AgentTurnCallbacks:
        return AgentTurnCallbacks(
            on_delta=lambda delta: events.append(("delta", delta)),
            on_status=lambda status: events.append(("status", status)),
            on_tool_start=lambda step, tool_call: events.append(
                ("tool_start", step, tool_call.name)
            ),
            on_tool_result=lambda tool_call, result: events.append(
                ("tool_result", tool_call.name, result.output)
            ),
            on_token_usage=lambda incoming, outgoing, cached: events.append(
                ("token_usage", incoming, outgoing, cached)
            ),
            on_protocol_wait=lambda: events.append(("protocol_wait",)),
            on_retry_status=lambda status: events.append(("retry_status", status)),
            on_reasoning_delta=lambda delta: events.append(("reasoning", delta)),
        )

    def test_run_forwards_full_stream_protocol_without_reordering_events(self) -> None:
        """控制器必须保留 Agent 的全部流式回调和原有调用顺序。"""

        observed: dict[str, Any] = {}

        class FakeAgent:
            def preload_mcp_tools(self) -> None:
                raise AssertionError("本测试不应调用 MCP 预热")

            def run_stream(self, text: str, on_delta, **callbacks: Any) -> str:
                observed["text"] = text
                observed["callback_names"] = set(callbacks)
                on_delta("回复分片")
                callbacks["on_status"]("正在思考")
                callbacks["on_reasoning_delta"]("分析中")
                callbacks["on_protocol_wait"]()
                callbacks["on_tool_start"](1, SimpleNamespace(name="read_file"))
                callbacks["on_tool_result"](
                    SimpleNamespace(name="read_file"),
                    SimpleNamespace(output="读取完成"),
                )
                callbacks["on_token_usage"](12, 8, 3)
                callbacks["on_retry_status"]("正在重试")
                callbacks["cancel_check"]()
                return "最终回复"

        events: list[tuple[Any, ...]] = []
        controller = AgentTurnController(FakeAgent(), threading.Event())

        self.assertEqual(controller.run("问题", self._callbacks(events)), "最终回复")
        self.assertEqual(observed["text"], "问题")
        self.assertEqual(
            observed["callback_names"],
            {
                "on_status",
                "on_tool_start",
                "on_tool_result",
                "on_token_usage",
                "on_protocol_wait",
                "on_retry_status",
                "cancel_check",
                "on_reasoning_delta",
            },
        )
        self.assertEqual(
            events,
            [
                ("delta", "回复分片"),
                ("status", "正在思考"),
                ("reasoning", "分析中"),
                ("protocol_wait",),
                ("tool_start", 1, "read_file"),
                ("tool_result", "read_file", "读取完成"),
                ("token_usage", 12, 8, 3),
                ("retry_status", "正在重试"),
            ],
        )

    def test_run_cancels_at_agent_protocol_checkpoint(self) -> None:
        """预先请求取消时，控制器传入的检查函数必须立即中断 Agent。"""

        class FakeAgent:
            continued_after_cancel = False

            def preload_mcp_tools(self) -> None:
                raise AssertionError("本测试不应调用 MCP 预热")

            def run_stream(self, _text: str, _on_delta, **callbacks: Any) -> str:
                callbacks["cancel_check"]()
                self.continued_after_cancel = True
                return "不应返回"

        cancellation = threading.Event()
        cancellation.set()
        agent = FakeAgent()
        controller = AgentTurnController(agent, cancellation)

        with self.assertRaisesRegex(KeyboardInterrupt, "用户取消当前任务"):
            controller.run("需要取消", self._callbacks([]))
        self.assertFalse(agent.continued_after_cancel)

    def test_preload_delegates_to_agent_without_swallowing_failures(self) -> None:
        """MCP 预热归控制器转发，异常仍交由 UI 层按照既有文案分类。"""

        class FakeAgent:
            preload_calls = 0

            def preload_mcp_tools(self) -> None:
                self.preload_calls += 1
                raise RuntimeError("MCP 不可用")

            def run_stream(self, _text: str, _on_delta, **_callbacks: Any) -> str:
                raise AssertionError("本测试不应运行 Agent 回合")

        agent = FakeAgent()
        controller = AgentTurnController(agent, threading.Event())

        with self.assertRaisesRegex(RuntimeError, "MCP 不可用"):
            controller.preload_mcp_tools()
        self.assertEqual(agent.preload_calls, 1)
