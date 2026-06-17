"""跨线程信号桥 — QWebChannel 双向通信桥。

后台线程通过 BackendBridge（QObject）的方法调用前端 JS 回调；
前端 JS 通过 QWebChannel 调用 BackendBridge 的槽方法与 Python 通信。
"""

from __future__ import annotations

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
    projects_refresh_requested = pyqtSignal()
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
