"""文件选择弹层（语音克隆参考音频）冒烟测试。"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from textual.app import App
from textual.widgets import Input

from omnicrawl.ui.fullscreen.screens.file_picker import FilePickerScreen


class _PickerHost(App):
    """宿主：mount 时 push 文件选择器，把 dismiss 结果收集到 self.result。"""

    def __init__(self, picker: FilePickerScreen) -> None:
        super().__init__()
        self.picker = picker
        self.result: list[object] = []

    def on_mount(self) -> None:
        self.push_screen(self.picker, self.result.append)


class FilePickerSmokeTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp_path = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    async def test_mount_lists_audio_files_and_filters_others(self) -> None:
        (self.tmp_path / "voice.wav").write_bytes(b"RIFF")
        (self.tmp_path / "notes.txt").write_text("hello", encoding="utf-8")
        host = _PickerHost(FilePickerScreen(self.tmp_path, title="选择音频"))
        async with host.run_test(size=(100, 36)) as pilot:
            await pilot.pause()
            await pilot.pause()
            tree = host.picker.query_one("#file-picker-tree")
            for _ in range(40):
                if tree.root.children:
                    break
                await pilot.pause()
            labels = [str(node.label) for node in tree.root.children]
            self.assertIn("voice.wav", labels)
            self.assertNotIn("notes.txt", labels)

    async def test_escape_dismisses_none(self) -> None:
        host = _PickerHost(FilePickerScreen(self.tmp_path))
        async with host.run_test(size=(100, 36)) as pilot:
            await pilot.pause()
            await pilot.pause()
            host.picker.focus()
            await pilot.press("escape")
            await pilot.pause()
            self.assertEqual(host.result, [None])

    async def test_input_path_jump_and_select(self) -> None:
        audio = self.tmp_path / "clip.wav"
        audio.write_bytes(b"RIFF")
        host = _PickerHost(FilePickerScreen(self.tmp_path))
        async with host.run_test(size=(100, 36)) as pilot:
            await pilot.pause()
            await pilot.pause()
            path_input = host.picker.query_one("#file-picker-path-input", Input)
            path_input.value = str(audio)
            await pilot.press("enter")
            await pilot.pause()
            await pilot.pause()
            self.assertEqual(host.result, [audio])
