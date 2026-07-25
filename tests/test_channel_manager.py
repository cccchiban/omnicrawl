from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path

from textual.app import App, ComposeResult
from textual.widgets import Input, Select, Static

from omnicrawl.config.channels import (
    ChannelConfig,
    ChannelConfiguration,
    load_channel_configuration,
    save_channel_configuration,
)
from omnicrawl.ui.fullscreen.channel_manager import (
    ChannelEditorScreen,
    ChannelManagerResult,
    ChannelManagerScreen,
    ChannelSetupApp,
)


class ChannelManagerScreenTests(unittest.IsolatedAsyncioTestCase):
    def _write_channels(self, root: Path, *, with_key: bool = True) -> tuple[Path, Path]:
        config_path = root / "config.yaml"
        models_path = root / "models.yaml"
        config_path.write_text("version: 2\nllm: {}\n", encoding="utf-8")
        models_path.write_text("version: 1\nmodels: {}\n", encoding="utf-8")
        save_channel_configuration(
            ChannelConfiguration(
                channels=(
                    ChannelConfig(
                        key="openai-main",
                        name="OpenAI 主渠道",
                        profile_id="openai-main",
                        provider="openai",
                        protocol="openai_chat_completions",
                        base_url="https://api.openai.com/v1",
                        api_key="key-one" if with_key else "",
                        model_id="gpt-5.2",
                    ),
                    ChannelConfig(
                        key="anthropic-main",
                        name="Anthropic 主渠道",
                        profile_id="anthropic-main",
                        provider="anthropic",
                        protocol="anthropic_messages",
                        base_url="https://api.anthropic.com",
                        api_key="key-two",
                        model_id="claude-sonnet-4-5",
                    ),
                ),
                default_key="openai-main",
            ),
            config_path,
            models_path,
        )
        return config_path, models_path

    async def test_should_offer_all_three_request_methods_on_first_run(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(Path(temp_dir))
            loaded = load_channel_configuration(config_path, models_path)
            save_channel_configuration(
                ChannelConfiguration((loaded.channels[0],), loaded.channels[0].key),
                config_path,
                models_path,
            )

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(
                        ChannelManagerScreen(
                            config_path,
                            models_path,
                            required=True,
                        )
                    )

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                screen = app.screen
                self.assertEqual(
                    {item.provider for item in screen._channels},
                    {"openai", "anthropic", "gemini"},
                )
                self.assertTrue(screen._channels[0].enabled)
                self.assertFalse(screen._channels[1].enabled)
                self.assertFalse(screen._channels[2].enabled)

    async def test_should_complete_first_run_after_default_channel_key_is_entered(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(
                Path(temp_dir),
                with_key=False,
            )
            app = ChannelSetupApp(config_path, models_path)

            async with app.run_test(size=(110, 40)) as pilot:
                await pilot.pause()
                await pilot.press("enter")
                await pilot.pause()
                self.assertIsInstance(app.screen, ChannelEditorScreen)
                app.screen.query_one("#channel-editor-key", Input).value = "new-openai-key"
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, ChannelManagerScreen)
                await pilot.press("ctrl+s")
                await pilot.pause()

            loaded = load_channel_configuration(config_path, models_path)
            default = next(
                item for item in loaded.channels if item.key == loaded.default_key
            )
            self.assertEqual(default.api_key, "new-openai-key")
            self.assertTrue(app.return_value)

    async def test_should_save_selected_default_when_using_keyboard(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(Path(temp_dir))
            results: list[ChannelManagerResult | None] = []

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(
                        ChannelManagerScreen(config_path, models_path),
                        results.append,
                    )

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                await pilot.press("down", "f", "ctrl+s")
                await pilot.pause()

            loaded = load_channel_configuration(config_path, models_path)
            self.assertEqual(loaded.default_key, "anthropic-main")
            self.assertEqual(len(results), 1)
            self.assertIsNotNone(results[0])

    async def test_should_delete_channel_after_second_confirmation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(Path(temp_dir))

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(ChannelManagerScreen(config_path, models_path))

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                await pilot.press("d", "d")
                await pilot.pause()
                self.assertEqual(len(app.screen._channels), 1)
                self.assertEqual(app.screen._channels[0].key, "anthropic-main")

    async def test_should_reject_deleting_the_only_enabled_channel(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(Path(temp_dir))
            loaded = load_channel_configuration(config_path, models_path)
            save_channel_configuration(
                ChannelConfiguration(
                    (loaded.channels[0], replace(loaded.channels[1], enabled=False)),
                    loaded.channels[0].key,
                ),
                config_path,
                models_path,
            )

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(ChannelManagerScreen(config_path, models_path))

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                await pilot.press("d", "d")
                await pilot.pause()
                self.assertEqual(len(app.screen._channels), 2)
                status = str(
                    app.screen.query_one("#channel-manager-status", Static).content
                )
                self.assertIn("至少需要保留一个启用渠道", status)

    async def test_should_keep_last_enabled_channel_when_space_is_pressed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(Path(temp_dir))
            loaded = load_channel_configuration(config_path, models_path)
            only = ChannelConfiguration((loaded.channels[0],), loaded.channels[0].key)
            save_channel_configuration(only, config_path, models_path)

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(ChannelManagerScreen(config_path, models_path))

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                await pilot.press("space")
                await pilot.pause()
                screen = app.screen
                status = str(screen.query_one("#channel-manager-status", Static).content)
                self.assertIn("至少需要保留一个启用渠道", status)
                self.assertTrue(screen._channels[0].enabled)

    async def test_should_reject_enabled_channel_without_credentials_in_settings(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path, models_path = self._write_channels(
                Path(temp_dir),
                with_key=False,
            )

            class ManagerApp(App):
                def compose(self) -> ComposeResult:
                    yield Static("probe")

                def on_mount(self) -> None:
                    self.push_screen(ChannelManagerScreen(config_path, models_path))

            app = ManagerApp()
            async with app.run_test(size=(110, 38)) as pilot:
                await pilot.pause()
                await pilot.press("ctrl+s")
                await pilot.pause()
                self.assertIsInstance(app.screen, ChannelManagerScreen)
                status = str(
                    app.screen.query_one("#channel-manager-status", Static).content
                )
                self.assertIn("OpenAI 主渠道", status)
                self.assertIn("API Key", status)

    async def test_should_keep_actions_visible_in_compact_terminal(self) -> None:
        class EditorApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(ChannelEditorScreen(None, existing_keys=set()))

        app = EditorApp()
        async with app.run_test(size=(80, 24)) as pilot:
            await pilot.pause()
            dialog = app.screen.query_one("#channel-editor-dialog")
            actions = app.screen.query_one("#channel-editor-actions")
            self.assertLessEqual(
                actions.region.y + actions.region.height,
                dialog.region.y + dialog.region.height,
            )

    async def test_should_switch_protocol_options_when_provider_changes(self) -> None:
        results: list[ChannelConfig | None] = []

        class EditorApp(App):
            def compose(self) -> ComposeResult:
                yield Static("probe")

            def on_mount(self) -> None:
                self.push_screen(
                    ChannelEditorScreen(None, existing_keys=set()),
                    results.append,
                )

        app = EditorApp()
        async with app.run_test(size=(100, 40)) as pilot:
            await pilot.pause()
            screen = app.screen
            screen.query_one("#channel-editor-provider", Select).value = "gemini"
            await pilot.pause()
            self.assertEqual(
                screen.query_one("#channel-editor-protocol", Select).value,
                "gemini_generate_content",
            )
            self.assertEqual(
                screen.query_one("#channel-editor-url", Input).value,
                "https://generativelanguage.googleapis.com",
            )
            screen.query_one("#channel-editor-key", Input).value = "gemini-secret"
            await pilot.press("ctrl+s")
            await pilot.pause()

        self.assertEqual(len(results), 1)
        self.assertIsNotNone(results[0])
        self.assertEqual(results[0].provider, "gemini")


if __name__ == "__main__":
    unittest.main()
