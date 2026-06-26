"""QtUI — BaseUI 的 QWebEngineView 实现，Persona 风格。"""

from __future__ import annotations

import json
import sys
from typing import Any

from ..base import BaseUI, UIStartupError
from ._bridge import BackendBridge


class QtUI(BaseUI):
    """基于 QWebEngineView + QWebChannel 的桌面 GUI 前端。

    后台线程通过 BackendBridge.call_js() 将 UI 调用安全转发到 Qt 主线程，
    再由 QWebChannel 执行前端 JS 回调。
    对话循环在后台线程中运行，通过 wait_for_input() 阻塞等待用户输入。

    调用顺序：init() → start() → 对话循环(后台线程) → exec_and_wait()
    """

    def __init__(self, *, model_label: str | None = None) -> None:
        super().__init__(model_label=model_label)
        self._bridge = BackendBridge()
        self._window: Any = None  # ChatWindow，延迟创建
        self._app: Any = None  # QApplication
        self._started = False
        self._confirm_counter = 0

    def start(self) -> None:
        """创建窗口并显示，不阻塞。必须在主线程调用。"""
        if self._started:
            return

        # QtWebEngineWidgets 必须在 QApplication 创建之前导入，否则
        # OpenGL 上下文共享会失败。PyQt 的 WebEngine 是单独发行包
        # PyQtWebEngine，缺失时给出可操作提示，避免启动阶段只抛原始
        # ModuleNotFoundError，用户不知道应安装哪个包。
        try:
            import PyQt5.QtWebEngineWidgets  # noqa: F401
        except ModuleNotFoundError as exc:
            if (
                getattr(exc, "name", "") == "PyQt5.QtWebEngineWidgets"
                or "PyQt5.QtWebEngineWidgets" in str(exc)
            ):
                raise UIStartupError(
                    "缺少 Qt WebEngine 组件：PyQt5.QtWebEngineWidgets。"
                    "请执行 python -m pip install \"PyQtWebEngine>=5.15.0\" 后重试。"
                    "注意 PowerShell 中版本约束需要加引号。"
                ) from exc
            raise

        from PyQt5.QtWidgets import QApplication
        from .window import ChatWindow

        self._app = QApplication.instance()
        if self._app is None:
            self._app = QApplication(sys.argv)

        self._window = ChatWindow(self._bridge)
        self._window.show()
        self._started = True

    def exec_and_wait(self) -> None:
        """启动 Qt 事件循环，阻塞直到窗口关闭。"""
        if self._app is not None:
            self._app.exec_()

    def stop(self) -> None:
        """关闭窗口。"""
        if self._window is not None:
            self._bridge.request_close()

    def is_closed(self) -> bool:
        """返回用户是否已经关闭 Qt 窗口。"""
        if self._window is None:
            return False
        return bool(self._window.is_closed())

    def wait_for_input(self, timeout: float | None = None) -> str | None:
        """阻塞等待用户通过 GUI 输入（线程安全）。"""
        if self._window is not None:
            return self._window.wait_for_input(timeout)
        return None

    # ── BaseUI 抽象方法实现 ──────────────────────────────────

    def update_token_usage(
        self,
        input_tokens: int,
        output_tokens: int,
        cached_input_tokens: int = 0,
    ) -> None:
        """更新 token 统计并同步刷新前端状态栏。"""
        super().update_token_usage(input_tokens, output_tokens, cached_input_tokens)
        self._window.update_token_display(self.prompt_status_line())

    def print_startup_panel(self, title: str, lines: list[str]) -> None:
        self._window.show_startup(title, lines)

    def print_tool_call_start(
        self,
        step: int,
        tool_name: str,
        arguments: dict[str, Any],
        *,
        leading_blank: bool = True,
    ) -> Any:
        args_json = json.dumps(arguments, ensure_ascii=False, indent=2)
        self._window.show_tool_start(step, tool_name, args_json)
        return None

    def print_tool_result_record(
        self,
        ok: bool,
        output: str | None = None,
        *,
        tool_name: str = "",
        display_state: Any | None = None,
    ) -> None:
        self._window.show_tool_result(ok, output or "", tool_name)

    def write_markdown_delta(self, delta: str, state: Any) -> None:
        self._window.append_ai_text(delta)

    def flush_markdown(self, state: Any) -> None:
        self._window.finish_ai_msg()

    def print_ai_prefix(self) -> None:
        pass  # AI 前缀由气泡角色标签处理

    def write(self, text: str) -> None:
        self._window.append_ai_text(text)

    def newline(self) -> None:
        self._window.finish_ai_msg()

    def status(self, message: str, *, leading_blank: bool = True, italic: bool = False) -> None:
        self._window.set_status(message, italic)

    def set_waiting(self, active: bool) -> None:
        self._window.set_waiting(active)

    def set_generating(self, active: bool) -> None:
        self._window.set_generating(active)

    def notice(self, message: str) -> None:
        self._window.show_notice(message)

    def mark_transient_output_start(self) -> bool:
        return False

    def clear_transient_output(self) -> None:
        pass

    def prompt(self) -> str:
        return "▸ "

    def prompt_yes_no(self, prompt: str, confirmed_label: str = "") -> bool:
        if self._window is None or self.is_closed():
            return False

        with self._lock:
            self._confirm_counter += 1
            confirm_id = str(self._confirm_counter)

        event = self._window.prepare_confirm(confirm_id)
        if self.is_closed():
            event.set()
            return False
        self._window.show_confirm_dialog(confirm_id, prompt)
        return self._window.wait_for_confirm(confirm_id, event)

    def inline_turn_base(self, user_text: str) -> str:
        return ""

    def replace_current_input_with_status(self, message: str) -> None:
        self._window.set_status(message, False)

    def clear_current_input_status(self) -> None:
        self._window.set_status("", False)

    def prompt_status_line(self) -> str:
        parts: list[str] = []
        if self.model_label:
            parts.append(f"- {self.model_label}")
        parts.append(f"in:{self._input_tokens} cache:{self._cached_input_tokens} out:{self._output_tokens}")
        return " ".join(parts)

    def print_prompt_status(self, cursor_column: int = 0) -> None:
        self._window.update_token_display(self.prompt_status_line())

    def clear_prompt_status(self) -> None:
        self._window.update_token_display("")

    def set_model_label(self, text: str) -> None:
        """更新 Qt 顶栏和状态栏中的当前模型名称。"""

        super().set_model_label(text)
        if self._window is not None:
            self._window.set_model_label(self.model_label or text)
            self._window.set_current_model(self.model_label or text, self.model_label or text)
            self._window.update_token_display(self.prompt_status_line())

    def set_workspace_info(self, path: str, status: str) -> None:
        """同步当前工作区信息到前端输入栏，供菜单和状态栏共用。"""
        if self._window is not None:
            self._window._bridge.call_js("setWorkspaceInfo", path, status)

    def show_html(self, title: str, html: str) -> None:
        """把 HTML 内容推送到 Qt 右侧显示区。"""

        if self._window is not None:
            self._window.show_html(title, html)

    # ── Qt 特有方法 ──────────────────────────────────────────

    def set_speaking(self, active: bool) -> None:
        """设置语音朗读状态指示。"""
        self._window.set_speaking(active)

    def set_listening(self, active: bool) -> None:
        """设置语音录音状态指示。"""
        self._window.set_listening(active)

    def set_input_enabled(self, enabled: bool) -> None:
        """启用/禁用输入框。"""
        self._window.set_input_enabled(enabled)

    def set_input_placeholder(self, text: str) -> None:
        """更新输入框占位文字。"""
        self._window.set_input_placeholder(text)

    def update_slash_commands(self, commands: list[dict[str, str]]) -> None:
        """更新 Qt 输入框的斜杠命令候选列表。"""
        if self._window is not None:
            self._window.update_slash_commands(commands)

    def get_cancel_event(self) -> Any:
        """返回取消事件（由窗口管理），供 chat_session 检测用户停止请求。"""
        if self._window is not None:
            return self._window._cancel_event
        return None

    @property
    def export_requested(self) -> Any:
        """返回前端导出请求信号，供 Qt 对话循环连接保存逻辑。"""
        return self._bridge.export_requested

    def update_model_list(self, models: list[dict[str, str]], current_model: str) -> None:
        if self._window is not None:
            self._window.update_model_list(models, current_model)

    def set_current_model(self, model_id: str, model_name: str | None = None) -> None:
        if self._window is not None:
            self._window.set_current_model(model_id, model_name)

    def show_model_list_error(self, message: str) -> None:
        if self._window is not None:
            self._window.show_model_list_error(message)

    def update_session_list(self, sessions: list[dict[str, Any]]) -> None:
        if self._window is not None:
            self._window.update_session_list(sessions)

    def render_session_messages(self, messages: list[dict[str, Any]]) -> None:
        if self._window is not None:
            self._window.render_session_messages(messages)

    def set_current_session(self, session_id: str, title: str) -> None:
        if self._window is not None:
            self._window.set_current_session(session_id, title)

    def show_session_list_error(self, message: str) -> None:
        if self._window is not None:
            self._window.show_session_list_error(message)

    def update_project_list(self, projects: list[dict[str, Any]]) -> None:
        if self._window is not None:
            self._window.update_project_list(projects)

    def set_current_project(self, project_path: str) -> None:
        if self._window is not None:
            self._window.set_current_project(project_path)
