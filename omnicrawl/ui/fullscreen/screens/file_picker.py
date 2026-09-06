"""TUI 文件选择弹层（DirectoryTree 选 wav 等音频文件）。

用于 TTS 语音克隆选择参考音频。纯键盘友好：目录树里方向键浏览，
Enter 选中文件；Esc 取消；顶部路径输入框可直接填路径跳转。
"""

from __future__ import annotations

from pathlib import Path
from typing import Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal
from textual.screen import ModalScreen
from textual.widgets import Button, DirectoryTree, Input, Static

from ..terminal.theme import terminal_css

_PICKER_CSS = """
FilePickerScreen { align: center middle; background: $terminal-overlay; }
#file-picker-dialog { width: 88; max-width: 96%; height: 34; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
#file-picker-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
#file-picker-path-input { height: 3; margin-bottom: 1; }
#file-picker-tree { height: 1fr; border: round $terminal-border; margin-bottom: 1; }
#file-picker-tree:focus { border: round $terminal-border-strong; }
#file-picker-hint { height: 1; color: $terminal-text-muted; margin-bottom: 1; }
#file-picker-actions { height: 3; align-horizontal: right; }
#file-picker-actions Button { margin-left: 1; }
"""


class AudioFileTree(DirectoryTree):
    """只展示目录与 .wav/.mp3/.flac/.ogg/.aac 音频文件的目录树。"""

    AUDIO_SUFFIXES = {".wav", ".mp3", ".flac", ".ogg", ".aac", ".m4a"}

    def filter_paths(self, paths):
        """过滤子路径：保留目录与音频文件，隐藏其它文件。"""
        return [
            path
            for path in paths
            if path.is_dir() or path.suffix.lower() in self.AUDIO_SUFFIXES
        ]


class FilePickerScreen(ModalScreen[Optional[Path]]):
    """选择音频文件；确认时以所选文件路径 dismiss，取消 dismiss(None)。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消", priority=True),
    ]

    CSS = terminal_css(_PICKER_CSS)

    def __init__(
        self,
        start_path: str | Path | None = None,
        *,
        title: str = "选择音频文件",
        id: str | None = None,
    ) -> None:
        super().__init__(id=id)
        self._start_path = Path(start_path or Path.home()).expanduser()
        self._title = title

    def compose(self) -> ComposeResult:
        with Container(id="file-picker-dialog"):
            yield Static(self._title, id="file-picker-title")
            yield Input(
                str(self._start_path),
                placeholder="输入目录或文件路径后回车跳转",
                id="file-picker-path-input",
            )
            self._tree = AudioFileTree(
                self._start_path if self._start_path.is_dir() else self._start_path.parent,
                id="file-picker-tree",
            )
            yield self._tree
            yield Static(
                "↑/↓ 浏览 · 回车或双击文件选择 · 输入路径回车跳转 · Esc 取消",
                id="file-picker-hint",
            )
            with Horizontal(id="file-picker-actions"):
                yield Button("取消", id="file-picker-cancel")
                yield Button("选择", variant="primary", id="file-picker-ok")

    def on_mount(self) -> None:
        self.query_one("#file-picker-path-input", Input).focus()

    def action_cancel(self) -> None:
        self.dismiss(None)

    def _choose(self, path: Path) -> None:
        if path.is_file() and path.suffix.lower() in AudioFileTree.AUDIO_SUFFIXES:
            self.dismiss(path)

    def on_directory_tree_file_selected(self, event: DirectoryTree.FileSelected) -> None:
        self._choose(event.path)

    def _choose_cursor(self) -> None:
        node = self._tree.cursor_node
        if node is None or node.data is None:
            return
        self._choose(Path(str(node.data.path)).expanduser())

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input.id != "file-picker-path-input":
            return
        candidate = Path(str(event.value).strip()).expanduser()
        if candidate.is_dir():
            self._tree.path = candidate
        elif candidate.is_file() and candidate.suffix.lower() in AudioFileTree.AUDIO_SUFFIXES:
            self._choose(candidate)
        else:
            self.notify("路径不存在或不是音频文件", severity="error")

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "file-picker-cancel":
            self.action_cancel()
        elif event.button.id == "file-picker-ok":
            self._choose_cursor()


__all__ = ["AudioFileTree", "FilePickerScreen"]
