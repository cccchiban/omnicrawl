"""全屏 TUI 的进程级入口：运行 App 并兜底恢复终端状态。"""

from __future__ import annotations

import logging

from ....agent import LocalToolAgent
from ....config.core.runtime import user_config_dir
from ..terminal.handling import _disable_terminal_mouse_reporting
from ..terminal.select_compat import apply_textual_select_mount_patch
from .core import OmniCrawlApp
from .startup import FullscreenStartup

_TUI_LOG_DIRNAME = "logs"
_TUI_LOG_FILENAME = "tui.log"


def _attach_tui_log_file() -> logging.Handler | None:
    """把 TUI 运行期的 Python 日志落盘到用户 logs/ 目录。

    root logger 在 splash 结束后没有 handler：此后模块（TTS 引擎、连接器、
    回合控制器等）的 WARNING/ERROR 会经 Python ``lastResort`` 直接写 stderr，
    在 Textual 全屏下表现为覆盖界面的杂散行。挂一个 FileHandler 把日志收进
    文件，stderr 保持干净；返回 handler（失败返回 None，不影响 TUI）。
    """
    try:
        log_dir = user_config_dir() / _TUI_LOG_DIRNAME
        log_dir.mkdir(parents=True, exist_ok=True)
        handler = logging.FileHandler(
            log_dir / _TUI_LOG_FILENAME,
            encoding="utf-8",
        )
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
        )
        handler.setLevel(logging.WARNING)
        root = logging.getLogger()
        root.addHandler(handler)
        return handler
    except Exception:  # noqa: BLE001 - 日志落盘失败不阻断 TUI
        return None


def _detach_tui_log_file(handler: logging.Handler | None) -> None:
    """退出时移除 TUI 日志 FileHandler（幂等）。"""
    if handler is None:
        return
    root = logging.getLogger()
    root.removeHandler(handler)
    try:
        handler.close()
    except Exception:  # noqa: BLE001 - 关闭失败不影响退出
        pass


def run_fullscreen_tui(agent: LocalToolAgent, startup: FullscreenStartup) -> int:
    """运行默认全屏 TUI。"""

    # Textual 8.2.x Select 挂载竞态补丁（Windows 间歇 NoMatches 崩溃，
    # 上游 #6581 未修复）：必须在首个 Screen 挂载前应用。
    apply_textual_select_mount_patch()
    # TUI 全屏运行期把 Python 日志收进文件：避免 TTS/连接器等模块的
    # WARNING/ERROR 经 lastResort 直写 stderr，在画面上刷出杂散行。
    file_handler = _attach_tui_log_file()
    app = OmniCrawlApp(agent, startup)
    try:
        app.run()
    finally:
        # 释放回合自动朗读线程与 TTS 引擎，避免守护线程/临时文件残留。
        close_announcer = getattr(app, "_close_speech_announcer", None)
        if callable(close_announcer):
            try:
                close_announcer()
            except Exception:  # noqa: BLE001 - 清理失败不掩盖退出码
                pass
        _detach_tui_log_file(file_handler)
        # Driver 正常会关闭鼠标报告，但退出期间的焦点/看门狗重启或 Driver
        # 内部清理异常可能让模式泄漏到后续 PowerShell Read-Host，必须再兜底一次。
        _disable_terminal_mouse_reporting()
    # Textual 会捕获定时器和消息处理异常并通过 return_code 报告，而不会重新抛出。
    # 必须向启动器透传，否则致命退出会被错误显示成“对话已结束”。
    return int(getattr(app, "return_code", 0) or 0)


__all__ = [
    "run_fullscreen_tui",
    "_attach_tui_log_file",
    "_detach_tui_log_file",
    "_TUI_LOG_DIRNAME",
    "_TUI_LOG_FILENAME",
]
