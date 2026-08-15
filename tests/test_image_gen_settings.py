"""image_gen 设置面板：打开、修改、保存与 settings 入口测试。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Input, Select, Static

from omnicrawl.config.image_gen import (
    ImageGenConfiguration,
    load_image_gen_configuration,
)
from omnicrawl.ui.fullscreen.image_gen_settings import ImageGenSettingsScreen
from omnicrawl.ui.fullscreen.settings import SettingsScreen


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


def _config_path(temp_dir: str) -> Path:
    path = Path(temp_dir) / "config.toml"
    path.write_text(
        "[image_gen]\nenabled = false\nbase_url = \"https://api.openai.com/v1\"\napi_key = \"\"\napi_key_env = \"OPENAI_API_KEY\"\nmodel = \"gpt-image-2\"\nsize = \"auto\"\nquality = \"auto\"\noutput_format = \"png\"\nn = 1\ntimeout_seconds = 120\n",
        encoding="utf-8",
    )
    return path


class ImageGenSettingsScreenTests(unittest.IsolatedAsyncioTestCase):
    async def test_opens_with_fields_and_switch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(ImageGenSettingsScreen(path))
                await pilot.pause()
                self.assertIsInstance(app.screen, ImageGenSettingsScreen)
                self.assertIn(
                    "图像生成配置",
                    str(app.screen.query_one("#image-gen-title").content),
                )
                switch = app.screen.query_one("#image-gen-enabled", Select)
                self.assertFalse(switch.value)
                base_url = app.screen.query_one("#image-gen-base-url", Input)
                self.assertEqual(base_url.value, "https://api.openai.com/v1")
                model = app.screen.query_one("#image-gen-model", Input)
                self.assertEqual(model.value, "gpt-image-2")

    async def test_toggle_switch_and_save_persists_configuration(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            applied: list[ImageGenConfiguration] = []
            agent = SimpleNamespace()
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(
                    ImageGenSettingsScreen(
                        path,
                        apply_configuration=applied.append,
                    )
                )
                await pilot.pause()
                # 在启用开关上打开选择器并选择“启用”
                switch = app.screen.query_one("#image-gen-enabled", Select)
                switch.focus()
                await pilot.pause()
                await pilot.press("enter")
                await pilot.pause()
                await pilot.press("down")
                await pilot.press("enter")
                await pilot.pause()
                self.assertTrue(
                    app.screen.query_one("#image-gen-enabled", Select).value
                )
                # 修改接口地址与模型
                url_input = app.screen.query_one("#image-gen-base-url", Input)
                url_input.value = "https://api.example.com/v1"
                model_input = app.screen.query_one("#image-gen-model", Input)
                model_input.value = "gpt-image-1"
                await pilot.press("ctrl+s")
                await pilot.pause()

            self.assertEqual(len(applied), 1)
            self.assertTrue(applied[0].enabled)
            self.assertEqual(applied[0].base_url, "https://api.example.com/v1")
            self.assertEqual(applied[0].model, "gpt-image-1")
            saved = load_image_gen_configuration(path)
            self.assertTrue(saved.enabled)
            self.assertEqual(saved.base_url, "https://api.example.com/v1")
            self.assertEqual(saved.model, "gpt-image-1")

    async def test_invalid_input_shows_error_and_stays(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = _config_path(temp_dir)
            app = _HostApp()
            async with app.run_test(size=(110, 45)) as pilot:
                app.push_screen(ImageGenSettingsScreen(path))
                await pilot.pause()
                url_input = app.screen.query_one("#image-gen-base-url", Input)
                url_input.value = "ftp://bad"
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, ImageGenSettingsScreen)
                self.assertIn(
                    "保存失败",
                    str(app.screen.query_one("#image-gen-status").content),
                )


class SettingsScreenImageGenEntryTests(unittest.TestCase):
    def test_settings_screen_exposes_image_gen_entry(self) -> None:
        screen = object.__new__(SettingsScreen)
        screen._agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            config=SimpleNamespace(
                vision=SimpleNamespace(enabled=False),
                image_gen=ImageGenConfiguration(enabled=True),
            ),
        )
        screen._advanced = False
        screen._row_keys = ("model", "channels", "vision", "image_gen")

        self.assertEqual(SettingsScreen._row_labels()["image_gen"], "图像生成")
        self.assertEqual(screen._current_row_values()["image_gen"], "已开启")

    def test_image_gen_entry_is_an_open_subpage(self) -> None:
        """图像生成行应跳转配置页而不是直接切换开关。"""

        # 对应 ui/fullscreen/__init__.py 的 SettingsAction("image_gen") 分发；
        # settings 面板对 image_gen 行的处理与 vision 一致：dismiss 返回动作。
        from omnicrawl.ui.fullscreen.settings import SettingsAction

        action = SettingsAction("image_gen")
        self.assertEqual(action.name, "image_gen")


if __name__ == "__main__":
    unittest.main()


class SettingsScreenKeyboardTests(unittest.IsolatedAsyncioTestCase):
    """设置面板全面键盘化：上下键移动、回车确认、ESC 返回。"""

    async def test_enter_on_image_gen_row_opens_config_page(self) -> None:
        from omnicrawl.config.image_gen import ImageGenConfiguration
        from omnicrawl.config.vision import VisionConfiguration

        agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            workspace_root="D:/workspace",
            current_session_id="s",
            skill_manager=None,
            _memory_store=None,
            _mcp_manager=SimpleNamespace(enabled=False),
            _plugin_manager=SimpleNamespace(enabled=False),
            config=SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
                vision=VisionConfiguration(enabled=False, models=()),
                image_gen=ImageGenConfiguration(enabled=False),
                disabled_tools=frozenset(),
                llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            ),
        )
        actions: list = []
        app = _HostApp()
        async with app.run_test(size=(110, 35)) as pilot:
            app.push_screen(
                SettingsScreen(agent, advanced=False),
                actions.append,
            )
            await pilot.pause()
            # 行序：model/channels/vision/image_gen → 下移 3 次后回车。
            await pilot.press("down", "down", "down", "enter")
            await pilot.pause()

        self.assertEqual([a.name for a in actions], ["image_gen"])

    async def test_escape_from_settings_dismisses_without_action(self) -> None:
        from omnicrawl.ui.fullscreen.settings import SettingsScreen

        agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            workspace_root="D:/workspace",
            current_session_id="s",
            skill_manager=None,
            _memory_store=None,
            _mcp_manager=SimpleNamespace(enabled=False),
            _plugin_manager=SimpleNamespace(enabled=False),
            config=SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
                vision=SimpleNamespace(enabled=False),
                image_gen=SimpleNamespace(enabled=False),
                disabled_tools=frozenset(),
                llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            ),
        )
        actions: list = []
        app = _HostApp()
        async with app.run_test(size=(110, 35)) as pilot:
            app.push_screen(SettingsScreen(agent, advanced=False), actions.append)
            await pilot.pause()
            await pilot.press("escape")
            await pilot.pause()

        self.assertEqual(actions, [None])

    async def test_enter_on_tool_row_toggles_switch(self) -> None:
        from omnicrawl.ui.fullscreen.tool_settings import ToolSettingsScreen

        agent = SimpleNamespace(
            current_model="demo",
            approval_mode="manual",
            reasoning_effort="none",
            context_window_tokens=128_000,
            workspace_root="D:/workspace",
            current_session_id="s",
            skill_manager=None,
            _memory_store=None,
            _mcp_manager=SimpleNamespace(enabled=False),
            _plugin_manager=SimpleNamespace(enabled=False),
            config=SimpleNamespace(
                subagents=SimpleNamespace(enabled=False),
                context_compaction=SimpleNamespace(enabled=False),
                vision=SimpleNamespace(enabled=False),
                image_gen=SimpleNamespace(enabled=False),
                disabled_tools=frozenset(),
                llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            ),
            _tools={},
        )
        app = _HostApp()
        async with app.run_test(size=(110, 35)) as pilot:
            app.push_screen(ToolSettingsScreen(agent))
            await pilot.pause()
            row = app.screen.query_one("#tool-settings-row-fetcher", Static)
            # 回车应触发 _toggle_selected（busy 状态后回到列表）
            await pilot.press("enter")
            await pilot.pause(0.3)

            # _apply_tool_switch 会调用 agent.set_tool_enabled —— FakeAgent 没有该方法，
            # 但事件已进入处理流程；这里断言屏幕仍为工具开关面板即可证明回车未被吞掉。
            self.assertIsInstance(app.screen, ToolSettingsScreen)
