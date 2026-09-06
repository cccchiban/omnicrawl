"""ask_user 工具超时清理回归测试。

锁定 2026-09 修复：ask_user 工具等待用户回答期间若超过工具批次超时
（AgentConfig.tool_timeout_seconds），UI 提问面板必须自动关闭、等待线程
退出，不能把“输入框上方的提问”残留到回合结束后。
"""

from __future__ import annotations

import threading
import time
import unittest
from types import SimpleNamespace

from omnicrawl.agent.controllers.tools.implementations import ToolImplementationsMixin
from omnicrawl.agent.types import AskUserRequest, ToolResult
from omnicrawl.ui.fullscreen.input.editing import InputMixin


class _AskUserUiStub(InputMixin):
    """不触碰 Textual DOM 的 InputMixin 替身：同步执行 UI 回调并记录清理。"""

    def __init__(self) -> None:
        self._ask_user_event = threading.Event()
        self._cancel_requested = threading.Event()
        self._ask_user_answer: str | None = None
        self._ask_user_request: AskUserRequest | None = None
        self.close_calls = 0

    def call_from_thread(self, callback, *args, **kwargs):  # type: ignore[no-untyped-def]
        return callback(*args, **kwargs)

    def _set_ask_user_request(  # type: ignore[override]
        self, request: AskUserRequest | None
    ) -> None:
        self._ask_user_request = request
        if request is None:
            self.close_calls += 1
        self._ask_user_answer = None

    def _answer(self, text: str) -> None:
        self._ask_user_answer = text
        self._ask_user_event.set()


class AskUserRequestTimeoutTests(unittest.TestCase):
    """AskUserRequest 超时字段的默认与传播。"""

    def test_timeout_seconds_defaults_to_none(self) -> None:
        request = AskUserRequest(question="问题", kind="confirm", options=("是", "否"))
        self.assertIsNone(request.timeout_seconds)

    def test_tool_ask_user_passes_timeout_to_handler(self) -> None:
        captured: list[AskUserRequest] = []

        def handler(request: AskUserRequest) -> str:
            captured.append(request)
            return "是"

        agent = object.__new__(ToolImplementationsMixin)
        agent.config = SimpleNamespace(tool_timeout_seconds=321)
        agent._ask_user_handler = handler

        result = ToolImplementationsMixin._tool_ask_user(
            agent,
            {"kind": "confirm", "question": "确认？", "options": ["是", "否"]},
        )
        self.assertTrue(result.ok)
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0].timeout_seconds, 321)

    def test_tool_ask_user_timeout_none_without_config(self) -> None:
        captured: list[AskUserRequest] = []

        def handler(request: AskUserRequest) -> str:
            captured.append(request)
            return "是"

        agent = object.__new__(ToolImplementationsMixin)
        agent.config = None
        agent._ask_user_handler = handler

        result = ToolImplementationsMixin._tool_ask_user(
            agent,
            {"kind": "confirm", "question": "确认？", "options": ["是", "否"]},
        )
        self.assertTrue(result.ok)
        self.assertIsNone(captured[0].timeout_seconds)


class AskUserUiTimeoutTests(unittest.TestCase):
    """InputMixin._ask_user 在超时/取消/回答后的退出与面板关闭行为。"""

    def test_answer_wakes_and_closes_panel(self) -> None:
        stub = _AskUserUiStub()
        request = AskUserRequest(
            question="确认？",
            kind="confirm",
            options=("是", "否"),
            timeout_seconds=60,
        )

        def deliver() -> None:
            time.sleep(0.05)
            stub._answer("是")

        thread = threading.Thread(target=deliver, daemon=True)
        thread.start()
        answer = stub._ask_user(request)
        thread.join(timeout=1.0)
        self.assertEqual(answer, "是")
        self.assertIsNone(stub._ask_user_request)
        # 正常路径也关闭面板：_ask_user 返回前调用 _set_ask_user_request(None)。
        self.assertEqual(stub.close_calls, 1)

    def test_timeout_closes_panel_and_returns_none(self) -> None:
        stub = _AskUserUiStub()
        request = AskUserRequest(
            question="确认？",
            kind="confirm",
            options=("是", "否"),
            timeout_seconds=0.15,
        )
        started = time.monotonic()
        answer = stub._ask_user(request)
        elapsed = time.monotonic() - started
        self.assertIsNone(answer)
        # 面板必须已被关闭（超时清理）。
        self.assertIsNone(stub._ask_user_request)
        self.assertGreaterEqual(stub.close_calls, 1)
        # 不应无限等待：需在超时附近返回。
        self.assertGreaterEqual(elapsed, 0.1)
        self.assertLess(elapsed, 2.0)

    def test_cancel_closes_panel_and_returns_none(self) -> None:
        stub = _AskUserUiStub()
        request = AskUserRequest(
            question="确认？",
            kind="confirm",
            options=("是", "否"),
            timeout_seconds=60,
        )

        def cancel() -> None:
            time.sleep(0.05)
            stub._cancel_requested.set()

        thread = threading.Thread(target=cancel, daemon=True)
        thread.start()
        answer = stub._ask_user(request)
        thread.join(timeout=1.0)
        self.assertIsNone(answer)
        self.assertIsNone(stub._ask_user_request)
        self.assertEqual(stub.close_calls, 1)


if __name__ == "__main__":
    unittest.main()
