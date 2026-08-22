"""全屏 TUI 的进程级入口：运行 App 并兜底恢复终端状态。"""

from __future__ import annotations

from ....agent import LocalToolAgent
from .._compat import resolve_facade
from ..terminal.handling import _disable_terminal_mouse_reporting
from .startup import FullscreenStartup


def run_fullscreen_tui(agent: LocalToolAgent, startup: FullscreenStartup) -> int:
    """运行默认全屏 TUI。

    ``OmniCrawlApp`` 通过 ``resolve_facade`` 从门面模块按名解析：测试会
    patch ``omnicrawl.ui.fullscreen.OmniCrawlApp``，必须调用时解析 patch
    才能生效，因此入口不直接导入 ``app/__init__.py`` 里的类。
    """

    app = resolve_facade("OmniCrawlApp")(agent, startup)
    try:
        app.run()
    finally:
        # Driver 正常会关闭鼠标报告，但退出期间的焦点/看门狗重启或 Driver
        # 内部清理异常可能让模式泄漏到后续 PowerShell Read-Host，必须再兜底一次。
        _disable_terminal_mouse_reporting()
    # Textual 会捕获定时器和消息处理异常并通过 return_code 报告，而不会重新抛出。
    # 必须向启动器透传，否则致命退出会被错误显示成“对话已结束”。
    return int(getattr(app, "return_code", 0) or 0)


__all__ = ["run_fullscreen_tui"]
