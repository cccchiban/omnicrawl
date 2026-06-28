"""Qt UI Python 实现合并入口。

Qt 桥接、导出、窗口和 UI 包装集中到这里，减少代码文件数量。
web/ 静态资源保持原目录，模块别名兼容 omnicrawl.ui.qt.window 等旧路径。
"""

from __future__ import annotations

import sys as _sys

_THIS_MODULE = _sys.modules[__name__]
_QT_MODULE_ALIASES = (
    '_bridge',
    'export',
    'qt_ui',
    'window',
)
for _alias in _QT_MODULE_ALIASES:
    _sys.modules[f"{__name__}.{_alias}"] = _THIS_MODULE
    globals()[_alias] = _THIS_MODULE

# --- former module: _bridge.py ---
"""跨线程信号桥 — QWebChannel 双向通信桥。

后台线程通过 BackendBridge（QObject）的方法调用前端 JS 回调；
前端 JS 通过 QWebChannel 调用 BackendBridge 的槽方法与 Python 通信。
"""


import threading
from typing import Any

from PyQt5.QtCore import QObject, pyqtSlot, pyqtSignal


class BackendBridge(QObject):
    """暴露给 QWebChannel 的后端桥对象。

    前端 JS 通过 channel.objects.bridge 访问此对象的槽方法。
    后台线程通过 signal -> 主线程槽 -> JS 回调 的链路向前端推送消息。
    """

    # ── 内部信号（用于跨线程安全调用 _call_js）───────────────
    _sig_call_js = pyqtSignal(str, list)
    # 模型选择信号：通知主线程显示模型选择对话框
    model_select_requested = pyqtSignal()
    # 模型列表刷新信号：用户打开下拉菜单时触发后端按 base_url 拉取 /models
    model_list_refresh_requested = pyqtSignal()
    # 模型切换信号：用户在前端选择了新模型
    model_changed = pyqtSignal(str)
    # 推理强度切换信号：用户在前端选择了新的 reasoning_effort
    reasoning_effort_changed = pyqtSignal(str)
    # 审批模式切换信号：用户在前端选择了新的 approval.mode
    approval_mode_changed = pyqtSignal(str)
    # 导出请求信号：把前端传来的 Markdown 交给后端保存
    export_requested = pyqtSignal(str)
    # 会话控制信号：侧边栏会话列表、新建、恢复、重命名和压缩。
    sessions_refresh_requested = pyqtSignal()
    new_session_requested = pyqtSignal()
    session_resume_requested = pyqtSignal(str)
    session_rename_requested = pyqtSignal(str)
    session_compact_requested = pyqtSignal()
    session_delete_requested = pyqtSignal(str)
    # 项目控制信号：创建、导入、打开、切换、上下文菜单操作
    project_create_requested = pyqtSignal(str, str)
    project_import_requested = pyqtSignal(str, str)
    project_open_requested = pyqtSignal(str)
    project_switch_requested = pyqtSignal(str)
    project_pin_requested = pyqtSignal(str)
    project_rename_requested = pyqtSignal(str, str)
    project_remove_requested = pyqtSignal(str)
    project_explorer_requested = pyqtSignal(str)
    project_path_selected = pyqtSignal(str)
    projects_refresh_requested = pyqtSignal()
    # 窗口控制信号：HTML 自定义标题栏接管原生标题栏的按钮与拖拽。
    window_minimize_requested = pyqtSignal()
    window_maximize_requested = pyqtSignal()
    window_drag_requested = pyqtSignal()
    new_window_requested = pyqtSignal()
    workspace_open_requested = pyqtSignal(str)
    # 窗口关闭信号：允许后台线程通过 Qt 信号请求主线程关闭窗口
    close_requested = pyqtSignal()

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._web_page: Any = None  # QWebEnginePage，由 window.py 设置
        self._frontend_ready = False
        self._pending_js_calls: list[tuple[str, list[Any]]] = []
        self._lock = threading.Lock()
        self._confirm_results: dict[str, bool] = {}
        self._confirm_events: dict[str, threading.Event] = {}
        self._confirm_lock = threading.Lock()
        self._input_queue: Any = None  # queue.Queue，由 window.py 设置
        self._cancel_event: threading.Event | None = None
        self._export_queue: Any = None

        # 跨线程：_sig_call_js -> 主线程执行 _do_call_js
        self._sig_call_js.connect(self._do_call_js)

    def set_web_page(self, page: Any) -> None:
        """设置 QWebEnginePage 引用。"""
        self._web_page = page

    def reset_frontend_ready(self) -> None:
        """页面开始重新加载时暂停 JS 调用，避免首屏消息在前端注册前丢失。"""
        self._frontend_ready = False

    def mark_frontend_ready(self) -> None:
        """页面完成加载后释放积压的 JS 调用。

        QWebEngineView.setUrl() 是异步操作。启动阶段 Python 可能会先调用
        showStartup()/notice()，而 HTML 里的 window.pyCallbacks 尚未注册；
        这些调用如果直接 runJavaScript 会被静默跳过，所以这里缓存到页面
        loadFinished 后再按原顺序执行。
        """
        self._frontend_ready = True
        if not self._web_page:
            return

        pending_calls = self._pending_js_calls
        self._pending_js_calls = []
        for method, args in pending_calls:
            self._run_js_callback(method, args)

    def set_input_queue(self, q: Any) -> None:
        """设置输入队列。"""
        self._input_queue = q

    # ── 后端 → 前端：跨线程安全的 JS 调用 ────────────────────

    def call_js(self, method: str, *args: Any) -> None:
        """从任意线程调用前端 JS 的 pyCallbacks 方法。

        通过信号机制确保在 Qt 主线程执行 JavaScript。
        """
        serial_args = []
        for a in args:
            if isinstance(a, (str, int, float, bool, list, dict)) or a is None:
                serial_args.append(a)
            else:
                serial_args.append(str(a))
        self._sig_call_js.emit(method, serial_args)

    def _do_call_js(self, method: str, args: list) -> None:
        """在主线程中执行 JS 回调。"""
        if not self._web_page or not self._frontend_ready:
            self._pending_js_calls.append((method, args))
            return
        self._run_js_callback(method, args)

    def _run_js_callback(self, method: str, args: list) -> None:
        """在已就绪页面中执行前端回调。"""
        import json
        args_json = json.dumps(args, ensure_ascii=False)
        js_code = (
            f"if (window.pyCallbacks && window.pyCallbacks.{method}) "
            f"window.pyCallbacks.{method}(...{args_json});"
        )
        self._web_page.runJavaScript(js_code)

    # ── 前端 → 后端：槽方法（由 JS 通过 QWebChannel 调用）────

    @pyqtSlot(str)
    def onUserSend(self, text: str) -> None:
        """用户点击发送按钮。"""
        if self._input_queue is not None:
            self._input_queue.put(text)

    @pyqtSlot(str, bool)
    def onConfirmResult(self, confirm_id: str, result: bool) -> None:
        """用户在确认对话框中做出选择。"""
        with self._confirm_lock:
            self._confirm_results[confirm_id] = result
            event = self._confirm_events.get(confirm_id)
        if event:
            event.set()

    @pyqtSlot()
    def onCancel(self) -> None:
        """用户点击停止生成按钮。"""
        if self._cancel_event is not None:
            self._cancel_event.set()

    @pyqtSlot(str)
    def onExportChat(self, markdown_text: str) -> None:
        """用户点击导出对话按钮，前端传入 Markdown 格式的对话记录。"""
        self.export_requested.emit(markdown_text)
        if self._export_queue is not None:
            self._export_queue.put(markdown_text)

    @pyqtSlot()
    def onNewSession(self) -> None:
        """用户点击新对话按钮。"""
        self.new_session_requested.emit()

    @pyqtSlot()
    def onRequestSessions(self) -> None:
        """用户请求刷新会话列表。"""
        self.sessions_refresh_requested.emit()

    @pyqtSlot(str)
    def onResumeSession(self, session_id: str) -> None:
        """用户从侧边栏选择恢复某个会话。"""
        self.session_resume_requested.emit(session_id)

    @pyqtSlot(str)
    def onRenameSession(self, title: str) -> None:
        """用户为当前会话设置标题。"""
        self.session_rename_requested.emit(title)

    @pyqtSlot()
    def onCompactSession(self) -> None:
        """用户手动压缩当前会话。"""
        self.session_compact_requested.emit()

    @pyqtSlot(str)
    def onDeleteSession(self, session_id: str) -> None:
        """用户删除指定会话。"""
        self.session_delete_requested.emit(session_id)

    @pyqtSlot()
    def onModelSelect(self) -> None:
        """用户点击模型选择按钮，通知后端刷新当前 base_url 下的模型列表。"""
        self.model_select_requested.emit()
        self.model_list_refresh_requested.emit()

    @pyqtSlot(str)
    def onModelChange(self, model_id: str) -> None:
        """用户在前端选择新模型。"""
        self.model_changed.emit(model_id)

    @pyqtSlot(str)
    def setReasoningEffort(self, effort: str) -> None:
        """用户在前端选择推理强度。"""
        self.reasoning_effort_changed.emit(effort)

    @pyqtSlot(str)
    def setApprovalMode(self, mode: str) -> None:
        """用户在前端选择工具审批模式。"""
        self.approval_mode_changed.emit(mode)

    # ── 取消事件管理 ──────────────────────────────────────────

    def set_cancel_event(self, event: threading.Event) -> None:
        """设置取消事件，供 Agent 循环检测。"""
        self._cancel_event = event

    # ── 导出队列管理 ──────────────────────────────────────────

    def set_export_queue(self, q: Any) -> None:
        """设置导出队列。"""
        self._export_queue = q

    def request_close(self) -> None:
        """从任意线程请求主线程关闭 Qt 窗口。"""
        self.close_requested.emit()

    @pyqtSlot()
    def onWindowMinimize(self) -> None:
        """用户点击自定义标题栏的最小化按钮。"""
        self.window_minimize_requested.emit()

    @pyqtSlot()
    def onWindowMaximize(self) -> None:
        """用户点击自定义标题栏的最大化/还原按钮。"""
        self.window_maximize_requested.emit()

    @pyqtSlot()
    def onWindowClose(self) -> None:
        """用户点击自定义标题栏的关闭按钮。"""
        self.close_requested.emit()

    @pyqtSlot()
    def onWindowDrag(self) -> None:
        """用户从自定义标题栏拖动窗口。"""
        self.window_drag_requested.emit()

    @pyqtSlot()
    def onOpenNewWindow(self) -> None:
        """用户点击文件菜单中的“新窗口”。"""
        self.new_window_requested.emit()

    @pyqtSlot(str)
    def onOpenWorkspaceFolder(self, workspace_path: str) -> None:
        """用户点击文件菜单中的“打开文件夹...”。"""
        self.workspace_open_requested.emit(workspace_path)

    @pyqtSlot(str, str)
    def onCreateProject(self, name: str, path: str) -> None:
        """用户创建新项目。"""
        self.project_create_requested.emit(name, path)

    @pyqtSlot(str, str)
    def onImportProject(self, name: str, path: str) -> None:
        """用户导入现有项目。"""
        self.project_import_requested.emit(name, path)

    @pyqtSlot(str)
    def onOpenProject(self, project_path: str) -> None:
        """用户在资源管理器中打开项目。"""
        self.project_open_requested.emit(project_path)

    @pyqtSlot(str)
    def onSwitchProject(self, project_path: str) -> None:
        """用户切换到指定项目。"""
        self.project_switch_requested.emit(project_path)

    @pyqtSlot(str)
    def onPinProject(self, project_path: str) -> None:
        """用户置顶项目。"""
        self.project_pin_requested.emit(project_path)

    @pyqtSlot(str, str)
    def onRenameProject(self, project_path: str, new_name: str) -> None:
        """用户重命名项目。"""
        self.project_rename_requested.emit(project_path, new_name)

    @pyqtSlot(str)
    def onRemoveProject(self, project_path: str) -> None:
        """用户从列表中移除项目。"""
        self.project_remove_requested.emit(project_path)

    @pyqtSlot(str)
    def onOpenInExplorer(self, project_path: str) -> None:
        """用户在资源管理器中打开项目目录。"""
        self.project_explorer_requested.emit(project_path)

    @pyqtSlot()
    def onBrowseProjectPath(self) -> None:
        """用户在项目弹窗中请求选择项目目录。"""
        self.project_path_selected.emit("")

    @pyqtSlot()
    def onRequestProjects(self) -> None:
        """用户请求刷新项目列表。"""
        self.projects_refresh_requested.emit()

    # ── 确认对话框管理 ────────────────────────────────────────

    def prepare_confirm(self, confirm_id: str) -> threading.Event:
        """准备确认对话框的同步事件。"""
        event = threading.Event()
        with self._confirm_lock:
            self._confirm_events[confirm_id] = event
        return event

    def wait_for_confirm(
        self,
        confirm_id: str,
        event: threading.Event | None = None,
        closed_event: threading.Event | None = None,
    ) -> bool:
        """阻塞等待确认结果。"""
        if event is None:
            event = self.prepare_confirm(confirm_id)
        while True:
            if closed_event and closed_event.is_set():
                return False
            if event.wait(timeout=0.1):
                break
        with self._confirm_lock:
            result = self._confirm_results.pop(confirm_id, False)
            self._confirm_events.pop(confirm_id, None)
        return result


# --- former module: export.py ---
"""Qt 对话导出保存工具。"""


from datetime import datetime
from pathlib import Path


def save_chat_export(
    markdown_text: str,
    *,
    workspace_root: Path,
    now: datetime | None = None,
) -> Path:
    """将前端导出的 Markdown 对话记录保存到工作区临时文件目录。

    导出内容属于用户主动产生的一次性文件，默认放入 `.agent_tmp/files/`，
    既不会污染项目根目录，也会沿用项目已有的临时目录清理约定。
    """

    root = workspace_root.expanduser().resolve()
    export_dir = root / ".agent_tmp" / "files"
    export_dir.mkdir(parents=True, exist_ok=True)

    timestamp = (now or datetime.now()).strftime("%Y%m%d_%H%M%S")
    path = export_dir / f"chat_export_{timestamp}.md"
    path.write_text(markdown_text, encoding="utf-8")
    return path


# --- former module: qt_ui.py ---
"""QtUI — BaseUI 的 QWebEngineView 实现，Persona 风格。"""


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


# --- former module: window.py ---
"""ChatWindow — QWebEngineView 容器，Persona 风格聊天界面。

核心架构：
  QWebEngineView 加载本地 HTML/CSS/JS 前端
  QWebChannel 实现双向通信
  BackendBridge 暴露给 JS 调用，同时通过 call_js() 调用前端回调
"""


import os
import queue
import subprocess
import threading
import sys
from pathlib import Path

from PyQt5.QtCore import QEvent, QPoint, QUrl, Qt
from PyQt5.QtWidgets import QFileDialog, QVBoxLayout, QWidget
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
        self.setWindowFlags(self.windowFlags() | Qt.WindowType.FramelessWindowHint)
        self.setMinimumSize(1100, 700)
        self.resize(1920, 1018)

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
        self._web_view.setContextMenuPolicy(Qt.ContextMenuPolicy.NoContextMenu)
        self._web_view.loadFinished.connect(self._on_load_finished)
        self._bridge.close_requested.connect(self.close)
        self._bridge.window_minimize_requested.connect(self.showMinimized)
        self._bridge.window_maximize_requested.connect(self._toggle_maximized)
        self._bridge.window_drag_requested.connect(self._start_window_drag)
        self._bridge.new_window_requested.connect(self._open_new_window)
        self._bridge.workspace_open_requested.connect(self._open_workspace_folder)

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
            self._notify_window_state()
        else:
            print("[WebEngine:ERROR] Qt HTML 前端加载失败")

    def _toggle_maximized(self) -> None:
        """切换最大化状态，并同步按钮文案/图标给 HTML 标题栏。"""
        if self.isMaximized():
            self.showNormal()
        else:
            self.showMaximized()
        self._notify_window_state()

    def _start_window_drag(self) -> None:
        """让 HTML 顶栏拖拽表现接近原生窗口标题栏。

        Qt 5.15 的 QWindow.startSystemMove() 会把移动交给系统窗口管理器，
        比手动按鼠标坐标移动更稳定，也能保留 Windows 的吸附/贴边体验。
        """
        handle = self.windowHandle()
        if handle is not None and hasattr(handle, "startSystemMove"):
            handle.startSystemMove()

    def _notify_window_state(self) -> None:
        """告知前端当前是否最大化，用于更新还原/最大化按钮状态。"""
        self._bridge.call_js("setWindowMaximized", self.isMaximized())

    def _open_new_window(self) -> None:
        """用独立进程打开一个新的 Qt 窗口。"""
        script_path = Path(__file__).resolve().parents[3] / "main.py"
        launch_cwd = Path.cwd().resolve()
        env = os.environ.copy()
        env["AI_VOICE_CHAT_IN_QT_WINDOW"] = "1"
        try:
            popen_kwargs = {"cwd": str(launch_cwd), "env": env}
            if os.name == "nt":
                popen_kwargs["creationflags"] = getattr(subprocess, "CREATE_NO_WINDOW", 0)
            subprocess.Popen([sys.executable, str(script_path)], **popen_kwargs)
            self._bridge.call_js("showNotice", "已打开新窗口")
        except OSError as exc:
            self._bridge.call_js("showNotice", f"新窗口打开失败：{exc}")

    def _open_workspace_folder(self, workspace_path: str) -> None:
        """用系统文件管理器打开当前工作区目录。"""
        path_text = str(workspace_path or "").strip()
        if not path_text:
            self._bridge.call_js("showNotice", "当前没有可打开的工作区目录")
            return

        path = Path(path_text).expanduser().resolve(strict=False)
        if not path.exists() or not path.is_dir():
            self._bridge.call_js("showNotice", f"工作区目录无效：{path}")
            return
        try:
            if os.name == "nt":
                os.startfile(str(path))  # type: ignore[attr-defined]
            elif sys.platform == "darwin":
                subprocess.Popen(["open", str(path)])
            else:
                subprocess.Popen(["xdg-open", str(path)])
            self._bridge.call_js("showNotice", f"已打开工作区：{path}")
        except OSError as exc:
            self._bridge.call_js("showNotice", f"打开工作区失败：{exc}")

    def changeEvent(self, event) -> None:
        if event.type() == QEvent.Type.WindowStateChange:
            self._notify_window_state()
        super().changeEvent(event)

    def nativeEvent(self, eventType, message):
        """为无边框窗口补回四周缩放热区，避免去标题栏后丢失基础窗口能力。"""
        if os.name != "nt":
            return super().nativeEvent(eventType, message)

        try:
            import ctypes
            from ctypes import wintypes

            msg = wintypes.MSG.from_address(int(message))
        except (ImportError, TypeError, ValueError):
            return super().nativeEvent(eventType, message)

        WM_NCHITTEST = 0x0084
        if msg.message != WM_NCHITTEST or self.isMaximized():
            return super().nativeEvent(eventType, message)

        border = 8
        x = ctypes.c_short(msg.lParam & 0xFFFF).value
        y = ctypes.c_short((msg.lParam >> 16) & 0xFFFF).value
        pos = self.mapFromGlobal(QPoint(x, y))
        width = self.width()
        height = self.height()
        on_left = 0 <= pos.x() < border
        on_right = width - border <= pos.x() < width
        on_top = 0 <= pos.y() < border
        on_bottom = height - border <= pos.y() < height

        HTLEFT = 10
        HTRIGHT = 11
        HTTOP = 12
        HTTOPLEFT = 13
        HTTOPRIGHT = 14
        HTBOTTOM = 15
        HTBOTTOMLEFT = 16
        HTBOTTOMRIGHT = 17

        if on_top and on_left:
            return True, HTTOPLEFT
        if on_top and on_right:
            return True, HTTOPRIGHT
        if on_bottom and on_left:
            return True, HTBOTTOMLEFT
        if on_bottom and on_right:
            return True, HTBOTTOMRIGHT
        if on_left:
            return True, HTLEFT
        if on_right:
            return True, HTRIGHT
        if on_top:
            return True, HTTOP
        if on_bottom:
            return True, HTBOTTOM
        return super().nativeEvent(eventType, message)

    def _connect_frontend_signals_once(self) -> None:
        """只连接一次前端控制信号，避免页面 reload 后重复入队命令。"""

        if self._frontend_signals_connected:
            return
        self._bridge.model_changed.connect(self._on_model_changed)
        self._bridge.reasoning_effort_changed.connect(self._on_reasoning_effort_changed)
        self._bridge.approval_mode_changed.connect(self._on_approval_mode_changed)
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
        self._bridge.project_path_selected.connect(self._on_project_path_selected)
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

    def _on_approval_mode_changed(self, mode: str) -> None:
        """用户在前端切换工具审批模式。"""
        if self._input_queue is not None:
            self._input_queue.put(f"/approval:{mode}")

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

    def _on_project_path_selected(self, _unused: str = "") -> None:
        """打开原生目录选择器，并把结果回填到项目弹窗路径输入框。"""
        selected = QFileDialog.getExistingDirectory(self, "选择项目文件夹", os.getcwd())
        if selected:
            self._bridge.call_js("setProjectModalPath", selected)

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

    def update_slash_commands(self, commands: list[dict[str, str]]) -> None:
        self._bridge.call_js("updateSlashCommands", commands)

    def set_speaking(self, active: bool) -> None:
        self._bridge.call_js("setSpeaking", active)

    def set_listening(self, active: bool) -> None:
        self._bridge.call_js("setListening", active)

    def set_waiting(self, active: bool) -> None:
        self._bridge.call_js("setWaiting", active)

    def set_generating(self, active: bool) -> None:
        self._bridge.call_js("setGenerating", active)

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

    def show_html(self, title: str, html: str) -> None:
        self._bridge.call_js("showHtmlPreview", title, html)

    # ── 窗口关闭 ─────────────────────────────────────────────

    def closeEvent(self, event) -> None:
        self._closed.set()
        self._input_queue.put("__WINDOW_CLOSED__")
        with self._bridge._confirm_lock:
            events = list(self._bridge._confirm_events.values())
        for confirm_event in events:
            confirm_event.set()
        super().closeEvent(event)


# --- former module: __init__.py ---
"""Qt Fluent GUI 前端。"""

from .qt_ui import QtUI

__all__ = ["QtUI"]
