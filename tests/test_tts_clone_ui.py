"""TTS 设置面板语音克隆 UI 冒烟测试。

验证：克隆区块控件存在、音色下拉合并自定义克隆音色。
不依赖真实 ONNX 模型（模型未就绪时克隆按钮会被禁用逻辑拦截，但控件仍渲染）。
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any

from textual.app import App, ComposeResult
from textual.widgets import Button, Input, Select, Static

from omnicrawl.ui.fullscreen.screens.tts_settings import TTSSettingsPane


class _PaneHost(App):
    def __init__(self, config_path: Path, voice_store: Path | None = None) -> None:
        super().__init__()
        self.config_path = config_path
        self.pane = None
        self._voice_store = voice_store

    def compose(self) -> ComposeResult:
        self.pane = TTSSettingsPane(self.config_path)
        yield self.pane


class TtsCloneUiTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self._tmp.name)
        self.config_path = self.tmp_path / "config.toml"
        self.config_path.write_text("", encoding="utf-8")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    async def test_clone_controls_present(self) -> None:
        app = _PaneHost(self.config_path)
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            pane = app.pane
            pane.query_one("#tts-clone-name", Input)
            pane.query_one("#tts-clone-audio", Input)
            pane.query_one("#tts-clone-browse", Button)
            pane.query_one("#tts-clone", Button)
            pane.query_one("#tts-clone-note", Static)

    async def test_clone_pane_renders_narrow(self) -> None:
        """右侧 pane 较窄（70 列）时克隆区不崩溃。"""
        app = _PaneHost(self.config_path)
        async with app.run_test(size=(70, 40)) as pilot:
            await pilot.pause()
            await pilot.pause()
            pane = app.pane
            pane.query_one("#tts-clone-name", Input)
            pane.query_one("#tts-clone-audio", Input)
            pane.query_one("#tts-clone", Button)

    async def test_voice_select_contains_custom_voice(self) -> None:
        import omnicrawl.tts.custom_voices as cv

        from omnicrawl.tts.custom_voices import add_custom_voice

        store = self.tmp_path / "custom_voices.json"
        original = cv.custom_voices_path
        cv.custom_voices_path = lambda: store
        try:
            add_custom_voice(voice="Fairy", prompt_audio_codes=[[1, 2], [3, 4]])
            app = _PaneHost(self.config_path)
            async with app.run_test(size=(110, 50)) as pilot:
                await pilot.pause()
                await pilot.pause()
                select = app.pane.query_one("#tts-voice", Select)
                option_values = [value for _label, value in select._options]
                self.assertIn("Fairy", option_values)
        finally:
            cv.custom_voices_path = original

    async def test_clone_button_blocked_without_model(self) -> None:
        """模型未就绪时点击克隆应提示先下载，不抛异常。"""
        app = _PaneHost(self.config_path)
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            pane = app.pane
            # 等 GPU 检查结束（_gpu_busy 复位），避免误判为“请等待当前任务”。
            for _ in range(50):
                if not pane._gpu_busy:
                    break
                await pilot.pause()
            name_input = pane.query_one("#tts-clone-name", Input)
            name_input.value = "MyVoice"
            status = pane.query_one("#tts-status", Static)
            # 与真实模型存在性解耦：强制视为未就绪。
            pane._models_ready = lambda: False
            # 直接调用 _start_clone（绕过按钮，模型未就绪应走提示分支）。
            pane._start_clone()
            await pilot.pause()
            self.assertIn("模型未下载", str(status.content))


class TtsDeleteVoiceUiTests(unittest.IsolatedAsyncioTestCase):
    """TTS 删除自定义音色 UI 冒烟测试（不依赖真实模型）。"""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self._tmp.name)
        self.config_path = self.tmp_path / "config.toml"
        self.config_path.write_text("", encoding="utf-8")
        self._voice_store = self.tmp_path / "custom_voices.json"
        import omnicrawl.tts.custom_voices as cv

        self._orig_path = cv.custom_voices_path
        cv.custom_voices_path = lambda: self._voice_store

    def tearDown(self) -> None:
        import omnicrawl.tts.custom_voices as cv

        cv.custom_voices_path = self._orig_path
        self._tmp.cleanup()

    async def _wait_gpu_idle(self, pilot: Any) -> None:
        """等待后台 GPU 检查结束（_gpu_busy 复位），避免误拦截删除。"""
        for _ in range(50):
            if not self.app.pane._gpu_busy:
                break
            await pilot.pause()

    async def test_delete_controls_present(self) -> None:
        """删除区控件（下拉+按钮）存在。"""
        app = _PaneHost(self.config_path)
        self.app = app
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            pane = app.pane
            pane.query_one("#tts-delete-voice", Select)
            pane.query_one("#tts-delete", Button)
            pane.query_one("#tts-delete-note", Static)

    async def test_delete_removes_custom_voice(self) -> None:
        """选中克隆音色点删除：自定义库移除且下拉与音色下拉同步。"""
        from omnicrawl.tts.custom_voices import add_custom_voice

        add_custom_voice(voice="Fairy", prompt_audio_codes=[[1, 2], [3, 4]])
        app = _PaneHost(self.config_path)
        self.app = app
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            await pilot.pause()
            await self._wait_gpu_idle(pilot)
            pane = app.pane
            delete_select = pane.query_one("#tts-delete-voice", Select)
            self.assertIn("Fairy", [v for _l, v in delete_select._options])
            pane._start_delete()
            await pilot.pause()
            from omnicrawl.tts.custom_voices import list_custom_voice_names

            self.assertEqual(list_custom_voice_names(), [])
            self.assertNotIn(
                "Fairy",
                [v for _l, v in pane.query_one("#tts-voice", Select)._options],
            )
            status = pane.query_one("#tts-status", Static)
            self.assertIn("已删除", str(status.content))

    async def test_delete_without_selection_hints(self) -> None:
        """无自定义音色时点删除：提示先选择，不抛异常。"""
        app = _PaneHost(self.config_path)
        self.app = app
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            await pilot.pause()
            await self._wait_gpu_idle(pilot)
            pane = app.pane
            pane._start_delete()
            await pilot.pause()
            status = pane.query_one("#tts-status", Static)
            self.assertIn("请先", str(status.content))

    async def test_clone_finish_adds_to_delete_select(self) -> None:
        """克隆成功后删除下拉同步出现新音色候选。"""
        from omnicrawl.tts.custom_voices import add_custom_voice

        add_custom_voice(voice="Fairy", prompt_audio_codes=[[1, 2]])
        app = _PaneHost(self.config_path)
        self.app = app
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            await pilot.pause()
            await self._wait_gpu_idle(pilot)
            pane = app.pane
            # 直接走克隆成功回调（无需真实模型），验证下拉刷新同步
            pane._clone_finished("Fairy", None)
            await pilot.pause()
            delete_select = pane.query_one("#tts-delete-voice", Select)
            self.assertIn("Fairy", [v for _l, v in delete_select._options])
            # 同时语音下拉也已包含
            self.assertIn(
                "Fairy",
                [v for _l, v in pane.query_one("#tts-voice", Select)._options],
            )

    async def test_delete_config_voice_falls_back(self) -> None:
        """删除的正是当前配置音色时，配置回退到首个可用音色。"""
        from omnicrawl.tts.custom_voices import add_custom_voice
        from omnicrawl.config.features.tts import TTSConfiguration

        add_custom_voice(voice="Fairy", prompt_audio_codes=[[1, 2]])
        config = self.config_path
        config.write_text(
            "[tts]\nenabled=true\nvoice='Fairy'\nmodel_dir=''\n",
            encoding="utf-8",
        )
        app = _PaneHost(config)
        self.app = app
        async with app.run_test(size=(110, 50)) as pilot:
            await pilot.pause()
            await pilot.pause()
            await self._wait_gpu_idle(pilot)
            pane = app.pane
            # 配置加载后 voice 应为 Fairy
            pane._start_delete()
            await pilot.pause()
            # 配置 voice 不应再是已删除的 Fairy（回退 Junhao）
            self.assertNotEqual(pane._configuration.voice, "Fairy")
