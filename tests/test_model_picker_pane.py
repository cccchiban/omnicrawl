"""ModelPickerPane 切换成功后的 UI 状态复位测试。

回归：设置页右侧内嵌模型选择器切换成功后，``_switching`` 必须复位、
状态行必须离开“正在切换 …”，否则键盘门卫（Enter/Esc/刷新）全部拦截，
表现为“一直卡在正在切换 …”。
"""

from __future__ import annotations

import asyncio
import time
import unittest

from textual.app import App, ComposeResult
from textual.widgets import Static

from omnicrawl.config.models.model_catalog import CatalogModel
from omnicrawl.ui.fullscreen.screens.model_picker import (
    ModelPickerPane,
    ModelPickerResult,
)

_CUSTOM_ITEM = CatalogModel(
    source="custom",
    key="demo/deepseek-v4-flash",
    profile_id="demo",
    provider="openai",
    protocol="openai",
    model_id="deepseek-v4-flash",
    display_name="deepseek-v4-flash",
)


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


def _make_item(name: str = "deepseek-v4-flash") -> CatalogModel:
    return CatalogModel(
        source="custom",
        key=f"demo/{name}",
        profile_id="demo",
        provider="openai",
        protocol="openai",
        model_id=name,
        display_name=name,
    )


def _new_pane(switched: list[str], committed: list[ModelPickerResult]):
    pane = ModelPickerPane(
        object(),
        switch_model=lambda token: switched.append(token),
        persist_selection=lambda item: f"已切换为 {item.model_id}，并写入测试。",
    )
    pane.bind_pane_events(
        on_back=lambda: None,
        on_commit=lambda result: committed.append(result),
    )
    return pane


async def _wait_switch_done(pane: ModelPickerPane, pilot, timeout: float = 8.0) -> None:
    deadline = time.monotonic() + timeout
    while pane._switching and time.monotonic() < deadline:
        await pilot.pause()
        await asyncio.sleep(0.02)


class ModelPickerSwitchResetTests(unittest.IsolatedAsyncioTestCase):
    """切换成功路径必须复位 _switching 并离开“正在切换”状态。"""

    async def test_switch_success_resets_state_and_reports(self) -> None:
        switched: list[str] = []
        committed: list[ModelPickerResult] = []
        pane = _new_pane(switched, committed)
        # 隔离目录加载：只测切换链路。
        pane._load_catalog = lambda refresh=False: None

        app = _HostApp()
        async with app.run_test(size=(100, 30)) as pilot:
            await app.mount(pane)
            await pilot.pause()

            pane._switch_to(_CUSTOM_ITEM)
            await _wait_switch_done(pane, pilot)

            self.assertFalse(pane._switching, "切换成功后 _switching 必须复位")
            self.assertNotIn("正在切换", pane._status)
            self.assertIn("已切换", pane._status)
            self.assertEqual(switched, ["demo/deepseek-v4-flash"])
            self.assertEqual(len(committed), 1)

    async def test_switch_success_allows_escape_back(self) -> None:
        switched: list[str] = []
        committed: list[ModelPickerResult] = []
        back: list[bool] = []
        pane = _new_pane(switched, committed)
        pane.bind_pane_events(
            on_back=lambda: back.append(True),
            on_commit=lambda result: committed.append(result),
        )
        pane._load_catalog = lambda refresh=False: None

        app = _HostApp()
        async with app.run_test(size=(100, 30)) as pilot:
            await app.mount(pane)
            await pilot.pause()

            pane._switch_to(_CUSTOM_ITEM)
            await _wait_switch_done(pane, pilot)
            self.assertFalse(pane._switching)

            pane.focus()
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(back, [True], "切换成功后 Esc 应能返回上一级")

    async def test_switch_success_allows_second_switch(self) -> None:
        switched: list[str] = []
        committed: list[ModelPickerResult] = []
        pane = _new_pane(switched, committed)
        pane._load_catalog = lambda refresh=False: None

        app = _HostApp()
        async with app.run_test(size=(100, 30)) as pilot:
            await app.mount(pane)
            await pilot.pause()

            pane._switch_to(_make_item("alpha"))
            await _wait_switch_done(pane, pilot)
            pane._switch_to(_make_item("beta"))
            await _wait_switch_done(pane, pilot)

            self.assertEqual(
                switched,
                ["demo/alpha", "demo/beta"],
                "切换成功后应允许再次切换（无卡死）",
            )
            self.assertFalse(pane._switching)


if __name__ == "__main__":
    unittest.main()
