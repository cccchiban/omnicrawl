"""Select 挂载竞态补丁（Textual #6581）回归测试。"""

from __future__ import annotations

import unittest

import textual.app
import textual.widgets as w
from textual.widgets._select import (
    NonSelectableStatic,
    SelectCurrent,
    SelectOverlay,
    Select,
)

from omnicrawl.ui.fullscreen.terminal.select_compat import (
    apply_textual_select_mount_patch,
    restore_textual_select,
)

_APPLIED_BY_TEST = False


class _SlowLabelSelectCurrent(SelectCurrent):
    """SelectCurrent 的 #label 延迟一个 refresh 挂载（复现上游竞态窗口）。"""

    def compose(self):
        yield NonSelectableStatic("▼", classes="arrow down-arrow")
        yield NonSelectableStatic("▲", classes="arrow up-arrow")

    def on_mount(self):
        self.call_after_refresh(
            self.mount, NonSelectableStatic(self.placeholder, id="label")
        )


class _SlowLabelSelect(Select):
    def compose(self):
        yield _SlowLabelSelectCurrent(self.prompt)
        yield SelectOverlay(type_to_search=self._type_to_search).data_bind(
            compact=w.Select.compact
        )


class _SlowOverlaySelect(Select):
    """SelectOverlay 延迟一个 refresh 挂载（用户崩溃变体）。"""

    def compose(self):
        yield SelectCurrent(self.prompt)
        self.call_after_refresh(
            self.mount, SelectOverlay(type_to_search=self._type_to_search)
        )


class _HostApp(textual.app.App):
    def __init__(self, *widgets) -> None:
        super().__init__()
        self._widgets = widgets

    def compose(self):
        yield from self._widgets


def setUpModule() -> None:
    global _APPLIED_BY_TEST
    _APPLIED_BY_TEST = apply_textual_select_mount_patch()


def tearDownModule() -> None:
    # 恢复原始方法，避免 patch 泄漏影响同进程其它测试文件。
    restore_textual_select()


class SelectMountPatchTests(unittest.IsolatedAsyncioTestCase):
    async def test_patch_is_idempotent(self) -> None:
        self.assertTrue(_APPLIED_BY_TEST)
        self.assertFalse(apply_textual_select_mount_patch())

    async def test_slow_label_select_mounts_without_crash(self) -> None:
        app = _HostApp(
            _SlowLabelSelect([("a", "a"), ("b", "b")], allow_blank=False, value="a")
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            await pilot.pause()
            select = app.query_one(Select)
            self.assertEqual(select.value, "a")
            self.assertTrue(select.query("SelectOverlay"))

    async def test_slow_overlay_select_mounts_without_crash(self) -> None:
        app = _HostApp(
            _SlowOverlaySelect(
                [("停用", False), ("启用", True)], allow_blank=False, value=False
            )
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            await pilot.pause()
            select = app.query_one(Select)
            self.assertIs(select.value, False)
            self.assertTrue(select.query("SelectOverlay"))

    async def test_stock_select_unaffected(self) -> None:
        app = _HostApp(
            Select([("x", "x"), ("y", "y")], allow_blank=False, value="x", id="stock")
        )
        async with app.run_test() as pilot:
            await pilot.pause()
            select = app.query_one("#stock", Select)
            self.assertEqual(select.value, "x")


if __name__ == "__main__":
    unittest.main()
