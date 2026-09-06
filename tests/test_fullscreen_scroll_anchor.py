"""会话区锚定跟随回归测试。

锁定 2026-09 修复：`RenderingMixin._scroll_conversation_if_following`
不得在已锚定的会话容器上跨刷新排队 ``scroll_end``。Textual 的
``scroll_end`` 执行时会先清除 ``_anchor_released``（用户手动滚动产生的
释放标记）再贴底；思考流式高频更新期间排队的迟到回调会在用户上滑后
仍把会话拉回底部（“思考中上滑被频繁拉回”）。
"""

from __future__ import annotations

import unittest
from unittest import mock

from textual.app import App, ComposeResult
from textual.containers import VerticalScroll
from textual.widgets import Static

from omnicrawl.ui.fullscreen.rendering.pipeline import RenderingMixin


class _ConversationApp(App[None]):
    """最小会话容器：内容足够长以允许滚动。"""

    def compose(self) -> ComposeResult:
        with VerticalScroll(id="conversation") as box:
            for index in range(120):
                yield Static(f"历史消息行 {index}", classes="message")
        self.box = box


class _FakeConversation:
    """模拟已锚定的会话容器，记录是否被要求跨刷新排队 scroll_end。"""

    def __init__(self) -> None:
        self.max_scroll_y = 100
        self.is_anchored = True
        self.queued_call_after_refresh = 0

    def anchor(self, _anchor: bool = True) -> None:
        raise AssertionError("已锚定容器不应再次调用 anchor()")

    def call_after_refresh(self, callback: object, *args: object, **kwargs: object) -> None:
        self.queued_call_after_refresh += 1


class ConversationAnchorContractTest(unittest.TestCase):
    """纯契约：已锚定容器上跟随调用不得排队迟到 scroll_end。"""

    def test_anchored_container_never_queues_scroll_end(self) -> None:
        conversation = _FakeConversation()
        RenderingMixin._scroll_conversation_if_following(
            conversation,
            follow_latest=True,
        )
        self.assertEqual(
            conversation.queued_call_after_refresh,
            0,
            "已锚定容器不得 call_after_refresh(scroll_end)：迟到回调会吞掉"
            "用户滚动释放的锚定并把页面拉回底部",
        )

    def test_user_scrolled_away_not_followed(self) -> None:
        conversation = _FakeConversation()
        # 用户上滑后调用方捕获 follow_latest=False：不得做任何滚动/锚定动作
        with mock.patch.object(
            RenderingMixin,
            "_scroll_conversation_if_following",
            wraps=RenderingMixin._scroll_conversation_if_following,
        ) as wrapped:
            RenderingMixin._scroll_conversation_if_following(
                conversation,
                follow_latest=False,
            )
            wrapped.assert_called_once()
        self.assertEqual(conversation.queued_call_after_refresh, 0)


class ConversationAnchorRegressionTest(unittest.IsolatedAsyncioTestCase):
    async def test_following_when_at_bottom_keeps_pinned_to_latest(self) -> None:
        """位于底部时新增内容仍应跟随贴底（不破坏自动跟随）。"""

        app = _ConversationApp()
        async with app.run_test(size=(100, 24)) as pilot:
            conversation = app.query_one("#conversation", VerticalScroll)
            await pilot.pause()
            # 初始滚动到底部（等价于用户正在查看最新内容）
            conversation.scroll_end(animate=False)
            await pilot.pause()
            self.assertTrue(conversation.is_vertical_scroll_end)

            RenderingMixin._scroll_conversation_if_following(
                conversation,
                follow_latest=True,
            )
            conversation.mount(Static("新增内容行\n" * 60, classes="message"))
            await pilot.pause()

            # 布局期 compositor 自动跟随底部
            self.assertTrue(conversation.is_vertical_scroll_end)
            self.assertEqual(conversation.scroll_y, conversation.max_scroll_y)

    async def test_user_scrolled_up_keeps_position_during_stream(self) -> None:
        """思考流式中用户上滑后，后续分片到达不应把会话拉回底部。"""

        app = _ConversationApp()
        async with app.run_test(size=(100, 24)) as pilot:
            conversation = app.query_one("#conversation", VerticalScroll)
            await pilot.pause()
            # 建立锚定（等价流式路径首次跟随）
            conversation.scroll_end(animate=False)
            RenderingMixin._scroll_conversation_if_following(
                conversation,
                follow_latest=True,
            )
            await pilot.pause()
            self.assertTrue(conversation.is_anchored)

            # 用户上滑离开底部
            conversation.scroll_up(animate=False)
            await pilot.pause()
            position_after_user_scroll = conversation.scroll_y
            self.assertFalse(conversation.is_vertical_scroll_end)
            self.assertTrue(conversation._anchor_released)

            # 内容继续追加（思考分片/新消息），调用方按滚动前状态仍会传入
            # follow_latest=True；布局刷新后用户位置必须保持。
            for _ in range(3):
                conversation.mount(Static("持续流入的思考内容\n" * 20, classes="message"))
                RenderingMixin._scroll_conversation_if_following(
                    conversation,
                    follow_latest=True,
                )
                await pilot.pause()

            self.assertEqual(
                conversation.scroll_y,
                position_after_user_scroll,
                "思考分片到达不应把已上滑的用户拉回底部",
            )
            self.assertTrue(conversation._anchor_released)


if __name__ == "__main__":
    unittest.main()
