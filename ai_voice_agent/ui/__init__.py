"""UI 前端工厂 — 根据 config 中的 frontend.type 创建对应的 UI 实例。"""

from __future__ import annotations

from .base import BaseUI, UIStartupError

__all__ = ["BaseUI", "UIStartupError", "create_ui"]


def create_ui(
    *,
    model_label: str | None = None,
    frontend_type: str = "tui",
) -> BaseUI:
    """根据 frontend_type 创建 UI 实例。

    Parameters
    ----------
    model_label : str | None
        模型名称标签，显示在状态行。
    frontend_type : str
        "tui" — 终端 UI（默认）；"qt" — Fluent Design 桌面 GUI。
    """
    if frontend_type == "qt":
        try:
            from .qt import QtUI
        except ModuleNotFoundError as exc:
            if _is_missing_qt_dependency(exc):
                raise UIStartupError(_qt_dependency_error_message()) from exc
            raise

        return QtUI(model_label=model_label)

    if frontend_type == "tui":
        from ..terminal_ui import TerminalUI

        return TerminalUI(model_label=model_label)

    raise ValueError(f"未知前端类型：{frontend_type!r}，可选值：tui、qt")


def _is_missing_qt_dependency(exc: ModuleNotFoundError) -> bool:
    """识别 Qt GUI 依赖缺失，避免把内部业务模块导入错误误报成安装问题。"""

    missing_name = getattr(exc, "name", "") or ""
    message = str(exc)
    return (
        missing_name.startswith("PyQt5")
        or missing_name.startswith("PyQtWebEngine")
        or "PyQt5" in message
        or "PyQtWebEngine" in message
    )


def _qt_dependency_error_message() -> str:
    """返回用户可直接执行的 Qt 依赖修复说明。"""

    return (
        "Qt GUI 依赖未安装完整。请在当前 Python 环境执行："
        "python -m pip install -r requirements.txt。"
        "如果只补本次缺失包，请执行：python -m pip install \"PyQtWebEngine>=5.15.0\"。"
        "在 PowerShell 中版本约束必须加引号，否则 >= 会被当作重定向符号。"
    )
