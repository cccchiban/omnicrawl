"""ChatWindow — QWebEngineView 容器，Persona 风格聊天界面。

核心架构：
  QWebEngineView 加载本地 HTML/CSS/JS 前端
  QWebChannel 实现双向通信
  BackendBridge 暴露给 JS 调用，同时通过 call_js() 调用前端回调
"""

from __future__ import annotations

import os
import queue
import threading

from PyQt5.QtCore import QUrl
from PyQt5.QtWidgets import QVBoxLayout, QWidget
from PyQt5.QtWebEngineWidgets import QWebEngineView, QWebEnginePage
from PyQt5.QtWebChannel import QWebChannel

from ._bridge import BackendBridge


class _CustomWebPage(QWebEnginePage):
    """自定义 WebEnginePage — 抑制 JS 控制台错误弹窗。"""

    def javaScriptConsoleMessage(self, level, message, line, _sourceID):
        prefix = {0: "INFO", 1: "WARN", 2: "ERROR"}.get(level, "LOG")
        print(f"[WebEngine:{prefix}] {message} (line {line})")


class ChatWindow(QWidget):
    """Persona 风格聊天主窗口 — QWebEngineView 容器。"""

    def __init__(self, bridge: BackendBridge, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._bridge = bridge
        self._input_queue: queue.Queue[str] = queue.Queue()
        self._cancel_event = threading.Event()
        self._export_queue: queue.Queue[str] = queue.Queue()
        self._closed = threading.Event()
        self._frontend_signals_connected = False

        # 关联桥和队列
        self._bridge.set_input_queue(self._input_queue)
        self._bridge.set_cancel_event(self._cancel_event)
        self._bridge.set_export_queue(self._export_queue)

        self.setWindowTitle("AI Voice Agent")
        self.setMinimumSize(1100, 700)
        self.resize(1600, 1080)

        self._init_ui()
        self._setup_channel()
        self._load_page()

    def _init_ui(self) -> None:
        root = QVBoxLayout(self)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)

        # ── Web 视图（全屏，标题栏由 HTML 渲染）────────────
        self._web_view = QWebEngineView(self)
        self._web_page = _CustomWebPage(self._web_view)
        self._web_view.setPage(self._web_page)
        self._web_view.loadFinished.connect(self._on_load_finished)
        self._bridge.close_requested.connect(self.close)

        # 传递 page 引用给 bridge
        self._bridge.set_web_page(self._web_page)

        root.addWidget(self._web_view, stretch=1)

    def _setup_channel(self) -> None:
        """设置 QWebChannel 双向通信。"""
        self._channel = QWebChannel(self)
        self._channel.registerObject("bridge", self._bridge)
        self._web_page.setWebChannel(self._channel)

    def _load_page(self) -> None:
        """加载本地 HTML 文件。"""
        self._bridge.reset_frontend_ready()
        web_dir = os.path.join(os.path.dirname(__file__), "web")
        html_path = os.path.join(web_dir, "index.html")
        url = QUrl.fromLocalFile(html_path)
        self._web_view.setUrl(url)

    def _on_load_finished(self, ok: bool) -> None:
        """Web 前端加载完成后释放积压的 Python → JS 调用。"""
        if ok:
            self._bridge.mark_frontend_ready()
            self._connect_frontend_signals_once()
        else:
            print("[WebEngine:ERROR] Qt HTML 前端加载失败")

    def _connect_frontend_signals_once(self) -> None:
        """只连接一次前端控制信号，避免页面 reload 后重复入队命令。"""

        if self._frontend_signals_connected:
            return
        self._bridge.model_changed.connect(self._on_model_changed)
        self._bridge.reasoning_effort_changed.connect(self._on_reasoning_effort_changed)
        self._bridge.model_list_refresh_requested.connect(self._on_model_list_refresh_requested)
        self._bridge.sessions_refresh_requested.connect(self._on_sessions_refresh_requested)
        self._bridge.new_session_requested.connect(self._on_new_session_requested)
        self._bridge.session_resume_requested.connect(self._on_session_resume_requested)
        self._bridge.session_rename_requested.connect(self._on_session_rename_requested)
        self._bridge.session_compact_requested.connect(self._on_session_compact_requested)
        self._bridge.session_delete_requested.connect(self._on_session_delete_requested)
        self._bridge.project_create_requested.connect(self._on_project_create_requested)
        self._bridge.project_import_requested.connect(self._on_project_import_requested)
        self._bridge.project_open_requested.connect(self._on_project_open_requested)
        self._bridge.project_switch_requested.connect(self._on_project_switch_requested)
        self._bridge.project_pin_requested.connect(self._on_project_pin_requested)
        self._bridge.project_rename_requested.connect(self._on_project_rename_requested)
        self._bridge.project_remove_requested.connect(self._on_project_remove_requested)
        self._bridge.project_explorer_requested.connect(self._on_project_explorer_requested)
        self._bridge.projects_refresh_requested.connect(self._on_projects_refresh_requested)
        self._frontend_signals_connected = True

    def _on_model_changed(self, model_id: str) -> None:
        """用户在前端下拉菜单选择了新模型。"""
        self._current_model = model_id
        # 通知后端模型已切换（通过输入队列发送特殊指令）
        if self._input_queue is not None:
            self._input_queue.put(f"/model {model_id}")

    def _on_model_list_refresh_requested(self) -> None:
        """用户打开模型下拉时，请后端按当前 base_url 刷新真实模型列表。"""
        if self._input_queue is not None:
            self._input_queue.put("__REFRESH_MODELS__")

    def _on_reasoning_effort_changed(self, effort: str) -> None:
        """用户在前端切换推理强度。"""
        if self._input_queue is not None:
            self._input_queue.put(f"/reasoning {effort}")

    def _on_sessions_refresh_requested(self) -> None:
        """用户请求刷新会话列表。"""
        if self._input_queue is not None:
            self._input_queue.put("__REFRESH_SESSIONS__")

    def _on_new_session_requested(self) -> None:
        """用户点击新对话。"""
        if self._input_queue is not None:
            self._input_queue.put("/new")

    def _on_session_resume_requested(self, session_id: str) -> None:
        """用户点击某个会话条目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__RESUME_SESSION__ {session_id}")

    def _on_session_rename_requested(self, title: str) -> None:
        """用户提交当前会话标题。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__RENAME_SESSION__ {title}")

    def _on_session_compact_requested(self) -> None:
        """用户点击手动压缩。"""
        if self._input_queue is not None:
            self._input_queue.put("/compact")

    def _on_session_delete_requested(self, session_id: str) -> None:
        """用户点击删除会话。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__DELETE_SESSION__ {session_id}")

    # ── 项目信号处理 ────────────────────────────────────────

    def _on_project_create_requested(self, name: str, path: str) -> None:
        """用户创建新项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__CREATE_PROJECT__ {name}|{path}")

    def _on_project_import_requested(self, name: str, path: str) -> None:
        """用户导入现有项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__IMPORT_PROJECT__ {name}|{path}")

    def _on_project_open_requested(self, project_path: str) -> None:
        """用户请求在资源管理器中打开项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__OPEN_PROJECT__ {project_path}")

    def _on_project_switch_requested(self, project_path: str) -> None:
        """用户切换到指定项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__SWITCH_PROJECT__ {project_path}")

    def _on_project_pin_requested(self, project_path: str) -> None:
        """用户置顶项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__PIN_PROJECT__ {project_path}")

    def _on_project_rename_requested(self, project_path: str, new_name: str) -> None:
        """用户重命名项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__RENAME_PROJECT__ {project_path}|{new_name}")

    def _on_project_remove_requested(self, project_path: str) -> None:
        """用户从列表中移除项目。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__REMOVE_PROJECT__ {project_path}")

    def _on_project_explorer_requested(self, project_path: str) -> None:
        """用户在资源管理器中打开项目目录。"""
        if self._input_queue is not None:
            self._input_queue.put(f"__OPEN_IN_EXPLORER__ {project_path}")

    def _on_projects_refresh_requested(self) -> None:
        """用户请求刷新项目列表。"""
        if self._input_queue is not None:
            self._input_queue.put("__REFRESH_PROJECTS__")

    # ── 输入 / 确认接口 ─────────────────────────────────────

    def wait_for_input(self, timeout: float | None = None) -> str | None:
        """阻塞等待用户输入。"""
        if self._closed.is_set():
            return "__WINDOW_CLOSED__"
        try:
            return self._input_queue.get(timeout=timeout)
        except queue.Empty:
            return None

    def is_closed(self) -> bool:
        return self._closed.is_set()

    def prepare_confirm(self, confirm_id: str) -> threading.Event:
        return self._bridge.prepare_confirm(confirm_id)

    def wait_for_confirm(
        self,
        confirm_id: str,
        event: threading.Event | None = None,
    ) -> bool:
        return self._bridge.wait_for_confirm(
            confirm_id, event, closed_event=self._closed
        )

    # ── 前端调用入口（由 QtUI 使用）─────────────────────────

    def append_user_msg(self, text: str) -> None:
        self._bridge.call_js("appendUserMsg", text)

    def append_ai_text(self, text: str) -> None:
        self._bridge.call_js("appendAIText", text)

    def finish_ai_msg(self) -> None:
        self._bridge.call_js("finishAIMsg")

    def set_status(self, message: str, italic: bool) -> None:
        self._bridge.call_js("setStatus", message, italic)

    def show_notice(self, message: str) -> None:
        self._bridge.call_js("showNotice", message)

    def show_startup(self, title: str, lines: list[str]) -> None:
        self._bridge.call_js("showStartup", title, lines)

    def show_tool_start(self, step: int, tool_name: str, args_json: str) -> None:
        self._bridge.call_js("showToolStart", step, tool_name, args_json)

    def show_tool_result(self, ok: bool, output: str, tool_name: str) -> None:
        self._bridge.call_js("showToolResult", ok, output, tool_name)

    def update_token_display(self, text: str) -> None:
        self._bridge.call_js("updateTokenDisplay", text)

    def show_confirm_dialog(self, confirm_id: str, prompt: str) -> None:
        self._bridge.call_js("showConfirmDialog", confirm_id, prompt)

    def hide_confirm_dialog(self) -> None:
        self._bridge.call_js("hideConfirmDialog")

    def clear_input(self) -> None:
        self._bridge.call_js("clearInput")

    def set_input_enabled(self, enabled: bool) -> None:
        self._bridge.call_js("setInputEnabled", enabled)

    def set_input_placeholder(self, text: str) -> None:
        self._bridge.call_js("setInputPlaceholder", text)

    def set_speaking(self, active: bool) -> None:
        self._bridge.call_js("setSpeaking", active)

    def set_listening(self, active: bool) -> None:
        self._bridge.call_js("setListening", active)

    def set_waiting(self, active: bool) -> None:
        self._bridge.call_js("setWaiting", active)

    def scroll_to_bottom(self) -> None:
        self._bridge.call_js("scrollToEnd")

    # ── 取消与导出 ─────────────────────────────────────────────

    def cancel_generation(self) -> None:
        """设置取消事件（由外部线程调用）。"""
        self._bridge.set_cancel_event(self._cancel_event)

    def get_export_text(self, timeout: float | None = None) -> str | None:
        """阻塞等待导出文本。"""
        if self._closed.is_set():
            return None
        try:
            return self._export_queue.get(timeout=timeout)
        except queue.Empty:
            return None

    def set_model_label(self, text: str) -> None:
        self._bridge.call_js("setModelLabel", text)

    def update_model_list(self, models: list[dict[str, str]], current_model: str) -> None:
        self._bridge.call_js("updateModelList", models, current_model)

    def set_current_model(self, model_id: str, model_name: str | None = None) -> None:
        self._bridge.call_js("setCurrentModel", model_id, model_name or model_id)

    def show_model_list_error(self, message: str) -> None:
        self._bridge.call_js("showModelListError", message)

    def update_session_list(self, sessions: list[dict[str, str | int | bool]]) -> None:
        self._bridge.call_js("updateSessionList", sessions)

    def render_session_messages(self, messages: list[dict[str, object]]) -> None:
        self._bridge.call_js("renderSessionMessages", messages)

    def set_current_session(self, session_id: str, title: str) -> None:
        self._bridge.call_js("setCurrentSession", session_id, title)

    def show_session_list_error(self, message: str) -> None:
        self._bridge.call_js("showSessionListError", message)

    def update_project_list(self, projects: list[dict[str, object]]) -> None:
        self._bridge.call_js("updateProjectList", projects)

    def set_current_project(self, project_path: str) -> None:
        self._bridge.call_js("setCurrentProject", project_path)

    # ── 窗口关闭 ─────────────────────────────────────────────

    def closeEvent(self, event) -> None:
        self._closed.set()
        self._input_queue.put("__WINDOW_CLOSED__")
        with self._bridge._confirm_lock:
            events = list(self._bridge._confirm_events.values())
        for confirm_event in events:
            confirm_event.set()
        super().closeEvent(event)
