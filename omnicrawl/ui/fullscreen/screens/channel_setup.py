"""首次启动渠道配置向导（P3 自 channel_manager.py 拆出）。

ChannelSetupApp 是承载 ChannelManagerScreen 的轻量 Textual App，
与 channel_manager.py 中两个设置屏幕解耦，方便单独维护。
"""

from __future__ import annotations

from pathlib import Path

from textual.app import App

from ..terminal.select_compat import apply_textual_select_mount_patch
from .channel_manager import ChannelManagerResult, ChannelManagerScreen


class ChannelSetupApp(App[bool]):
    """首次启动时承载渠道管理 Screen 的轻量 Textual App。"""

    CSS = "Screen { background: transparent; }"

    def __init__(self, config_path: Path, models_path: Path) -> None:
        super().__init__()
        self._config_path = config_path
        self._models_path = models_path

    def on_mount(self) -> None:
        self.push_screen(
            ChannelManagerScreen(
                self._config_path,
                self._models_path,
                required=True,
            ),
            self._receive_result,
        )

    def _receive_result(self, result: ChannelManagerResult | None) -> None:
        self.exit(result is not None)


def run_channel_setup(config_path: Path, models_path: Path) -> bool:
    """在当前终端运行首次启动渠道配置向导。"""

    # 与全屏入口一致：先应用 Select 挂载竞态补丁（Textual #6581）。
    apply_textual_select_mount_patch()
    result = ChannelSetupApp(config_path, models_path).run()
    return bool(result)

