from __future__ import annotations

import builtins
from datetime import datetime
from pathlib import Path
import re
import tempfile
import unittest
from unittest.mock import patch

from ai_voice_agent.ui.qt.export import save_chat_export
from ai_voice_agent.ui.qt._bridge import BackendBridge
from ai_voice_agent.ui.qt.qt_ui import QtUI
from ai_voice_agent.qt_chat_session import _session_events_to_ui
from ai_voice_agent.session import SessionEvent


PROJECT_ROOT = Path(__file__).resolve().parents[1]
QT_WEB_DIR = PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "web"


class FakeWebPage:
    def __init__(self) -> None:
        self.scripts: list[str] = []

    def runJavaScript(self, script: str) -> None:
        self.scripts.append(script)


class FakeChatWindow:
    def __init__(self) -> None:
        self.user_messages: list[str] = []
        self.closed = False

    def append_user_msg(self, text: str) -> None:
        self.user_messages.append(text)

    def close(self) -> None:
        self.closed = True


class FakeCloseBridge:
    def __init__(self) -> None:
        self.close_requested = False

    def request_close(self) -> None:
        self.close_requested = True


class FakeModelWindow:
    def __init__(self) -> None:
        self.model_labels: list[str] = []
        self.current_models: list[tuple[str, str | None]] = []
        self.token_updates: list[str] = []
        self.html_previews: list[tuple[str, str]] = []

    def set_model_label(self, text: str) -> None:
        self.model_labels.append(text)

    def set_current_model(self, model_id: str, model_name: str | None = None) -> None:
        self.current_models.append((model_id, model_name))

    def update_token_display(self, text: str) -> None:
        self.token_updates.append(text)

    def show_html(self, title: str, html: str) -> None:
        self.html_previews.append((title, html))


class FakeSessionWindow:
    def __init__(self) -> None:
        self.session_lists: list[list[dict]] = []
        self.rendered_messages: list[list[dict]] = []
        self.current_sessions: list[tuple[str, str]] = []
        self.session_errors: list[str] = []

    def update_session_list(self, sessions: list[dict]) -> None:
        self.session_lists.append(sessions)

    def render_session_messages(self, messages: list[dict]) -> None:
        self.rendered_messages.append(messages)

    def set_current_session(self, session_id: str, title: str) -> None:
        self.current_sessions.append((session_id, title))

    def show_session_list_error(self, message: str) -> None:
        self.session_errors.append(message)


class QtUITest(unittest.TestCase):
    def test_start_reports_missing_webengine_dependency(self) -> None:
        original_import = builtins.__import__

        def fake_import(name, globals=None, locals=None, fromlist=(), level=0):
            if name == "PyQt5.QtWebEngineWidgets":
                raise ModuleNotFoundError("No module named 'PyQt5.QtWebEngineWidgets'")
            return original_import(name, globals, locals, fromlist, level)

        with patch("builtins.__import__", side_effect=fake_import):
            with self.assertRaisesRegex(RuntimeError, "PyQtWebEngine"):
                QtUI().start()

    def test_should_queue_js_callbacks_when_frontend_is_not_ready(self) -> None:
        bridge = BackendBridge()
        page = FakeWebPage()

        bridge._do_call_js("showStartup", ["AI 语音 Agent", ["frontend: Qt GUI"]])
        bridge.set_web_page(page)

        self.assertEqual(page.scripts, [])

        bridge.mark_frontend_ready()

        self.assertEqual(len(page.scripts), 1)
        self.assertIn("window.pyCallbacks.showStartup", page.scripts[0])
        self.assertIn("AI 语音 Agent", page.scripts[0])

    def test_should_pass_list_arguments_as_arrays_when_calling_js(self) -> None:
        bridge = BackendBridge()
        page = FakeWebPage()
        bridge.set_web_page(page)
        bridge.mark_frontend_ready()

        bridge.call_js("showStartup", "AI 语音 Agent", ["frontend: Qt GUI"])

        self.assertEqual(len(page.scripts), 1)
        self.assertIn('["AI 语音 Agent", ["frontend: Qt GUI"]]', page.scripts[0])
        self.assertNotIn('"\\"frontend: Qt GUI\\""', page.scripts[0])

    def test_should_append_user_message_immediately_when_enter_sends_input(self) -> None:
        source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")

        append_index = source.index("Messages.appendUserMsg(text)")
        send_index = source.index("window.bridge.onUserSend(text)")
        self.assertLess(append_index, send_index)
        self.assertIn("Notice.show", source)

    def test_should_show_slash_command_menu_and_support_tab_completion(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")
        input_css = (QT_WEB_DIR / "css" / "input-area.css").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )
        ui_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "qt_ui.py").read_text(
            encoding="utf-8"
        )
        qt_session_source = (PROJECT_ROOT / "ai_voice_agent" / "qt_chat_session.py").read_text(
            encoding="utf-8"
        )

        self.assertIn('id="slash-command-menu"', index_source)
        self.assertIn('aria-autocomplete="list"', index_source)
        self.assertIn("updateSlashCommands", callback_source)
        self.assertIn("updateSlashCommands", window_source)
        self.assertIn("def update_slash_commands", ui_source)
        self.assertIn("build_slash_command_options(agent)", qt_session_source)

        self.assertIn("function updateSlashCommands(commands)", input_source)
        self.assertIn("updateSlashCommands: updateSlashCommands", input_source)
        self.assertIn("function activeSlashToken(inp)", input_source)
        self.assertIn("function findSlashMatches(token)", input_source)
        self.assertIn("function handleSlashCommandKeydown(event, inp)", input_source)
        self.assertIn("event.key === 'Tab'", input_source)
        self.assertIn("event.key === 'ArrowDown'", input_source)
        self.assertIn("event.key === 'ArrowUp'", input_source)
        self.assertIn("'aria-activedescendant'", input_source)
        self.assertIn("'/skill:' + query.slice(1)", input_source)

        self.assertIn(".slash-command-menu", input_css)
        self.assertIn(".slash-command-option", input_css)
        self.assertIn(".slash-command-option.active", input_css)

    def test_should_define_message_time_formatter_when_rendering_messages(self) -> None:
        source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")

        self.assertIn("timeNow()", source)
        self.assertIn("function timeNow()", source)

    def test_confirm_card_should_allow_enter_to_approve_by_default(self) -> None:
        source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")

        self.assertIn("AppState.confirmId = confirmId", source)
        self.assertIn("document.addEventListener('keydown', handleConfirmKeydown, true)", source)
        self.assertIn("event.key === 'Enter' && !event.shiftKey", source)
        self.assertIn("submitConfirm(card, confirmId, true, handleConfirmKeydown)", source)
        self.assertIn("allowBtn.focus({ preventScroll: true })", source)

    def test_should_load_es2022_polyfills_before_marked_for_qt_webengine(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        polyfills_path = QT_WEB_DIR / "js" / "polyfills.js"

        self.assertTrue(polyfills_path.exists())
        self.assertLess(
            index_source.index('src="js/polyfills.js"'),
            index_source.index('src="vendor/marked.min.js"'),
        )

        polyfills_source = polyfills_path.read_text(encoding="utf-8")
        self.assertIn("Array.prototype.at", polyfills_source)
        self.assertIn("String.prototype.at", polyfills_source)

    def test_should_keep_input_disabled_until_qwebchannel_is_connected(self) -> None:
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")

        self.assertNotIn("Input.setEnabled(true);", app_source)
        self.assertIn("Input.setBridgeReady(false)", app_source)
        self.assertIn("bridge-ready", app_source)
        self.assertIn("setBridgeReady", input_source)
        self.assertIn("desiredEnabled", input_source)
        self.assertIn("bridgeReady", input_source)
        self.assertIn("dispatchEvent(new Event('bridge-ready'))", bridge_source)

    def test_should_expose_backend_slot_methods_when_qwebchannel_initializes(self) -> None:
        source = (QT_WEB_DIR / "qwebchannel.js").read_text(encoding="utf-8")

        self.assertIn("data.methods", source)
        self.assertRegex(source, r"object\[methodName\]\s*=\s*function")
        self.assertIn("QWebChannelMessageTypes.invokeMethod", source)

    def test_should_refresh_and_update_model_list_from_backend(self) -> None:
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")
        title_css = (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8")

        self.assertIn("window.bridge.onModelSelect()", app_source)
        self.assertIn("window.ModelSelector", app_source)
        self.assertIn("updateModelList", callback_source)
        self.assertIn("setCurrentModel", callback_source)
        self.assertIn("showModelListError", callback_source)
        self.assertIn("onModelSelect", bridge_source)
        self.assertIn("setReasoningEffort", bridge_source)
        self.assertIn(".model-option-message", title_css)

    def test_should_use_lobe_icons_for_model_provider_icons(self) -> None:
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        title_css = (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8")

        self.assertIn("@lobehub/icons-static-svg", app_source)
        self.assertIn("modelProviderIconSlug", app_source)
        self.assertIn("openai", app_source)
        self.assertIn("deepseek", app_source)
        self.assertIn("icon-fallback", app_source)
        self.assertIn(".model-icon img", title_css)
        self.assertIn(".model-icon-fallback", title_css)

    def test_should_render_ai_messages_with_current_model_identity(self) -> None:
        state_source = (QT_WEB_DIR / "js" / "state.js").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        messages_source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")
        title_css = (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8")
        messages_css = (QT_WEB_DIR / "css" / "messages.css").read_text(encoding="utf-8")
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")

        self.assertIn("currentModelName", state_source)
        self.assertIn("currentModelProvider", state_source)
        self.assertIn("setCurrentModelIdentity", state_source)
        self.assertIn("AppState.setCurrentModelIdentity", app_source)
        self.assertIn("modelAvatarHtml()", messages_source)
        self.assertIn("AppState.currentModelName", messages_source)
        self.assertIn("model-selector-icon", index_source)
        self.assertIn("document.getElementById('model-selector-icon')", app_source)
        self.assertIn("updateSelectorIcon", app_source)
        self.assertNotIn('<span class="msg-role">AI 助手</span>', messages_source)
        self.assertNotIn("**AI 助手**", app_source)
        self.assertIn("background: #ffffff", title_css)
        self.assertIn("background: #ffffff", messages_css)

    def test_should_sync_message_model_identity_when_qt_model_label_changes(self) -> None:
        ui = QtUI()
        window = FakeModelWindow()
        ui._window = window

        ui.set_model_label("deepseek-v4-flash")

        self.assertEqual(window.model_labels, ["deepseek-v4-flash"])
        self.assertEqual(window.current_models, [("deepseek-v4-flash", "deepseek-v4-flash")])
        self.assertIn("- deepseek-v4-flash", window.token_updates[-1])

    def test_should_request_window_close_through_bridge_signal(self) -> None:
        ui = QtUI()
        window = FakeChatWindow()
        bridge = FakeCloseBridge()
        ui._window = window
        ui._bridge = bridge

        ui.stop()

        self.assertTrue(bridge.close_requested)
        self.assertFalse(window.closed)

    def test_should_use_frameless_window_with_html_chrome_controls(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        title_css = (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8")
        backend_bridge = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "_bridge.py").read_text(
            encoding="utf-8"
        )
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("Qt.WindowType.FramelessWindowHint", window_source)
        self.assertIn("window_minimize_requested.connect(self.showMinimized)", window_source)
        self.assertIn("window_maximize_requested.connect(self._toggle_maximized)", window_source)
        self.assertIn("window_drag_requested.connect(self._start_window_drag)", window_source)
        self.assertIn("startSystemMove", window_source)
        self.assertIn("WM_NCHITTEST", window_source)

        self.assertIn('id="app-chrome"', index_source)
        self.assertIn('id="window-minimize-btn"', index_source)
        self.assertIn('id="window-maximize-btn"', index_source)
        self.assertIn('id="window-close-btn"', index_source)
        self.assertIn("initWindowChrome()", app_source)
        self.assertIn("window.bridge.onWindowDrag()", app_source)
        self.assertIn("window.bridge.onWindowMinimize()", app_source)
        self.assertIn("window.bridge.onWindowMaximize()", app_source)
        self.assertIn("window.bridge.onWindowClose()", app_source)
        self.assertIn("window.WindowChrome", app_source)

        self.assertIn("onWindowMinimize", bridge_source)
        self.assertIn("onWindowMaximize", bridge_source)
        self.assertIn("onWindowClose", bridge_source)
        self.assertIn("onWindowDrag", bridge_source)
        self.assertIn("window_minimize_requested", backend_bridge)
        self.assertIn("@pyqtSlot()\n    def onWindowMinimize", backend_bridge)
        self.assertIn("setWindowMaximized", callback_source)
        self.assertIn("#app-chrome", title_css)
        self.assertIn(".window-control.close:hover", title_css)

    def test_should_emit_export_request_when_frontend_sends_markdown(self) -> None:
        bridge = BackendBridge()
        received: list[str] = []
        bridge.export_requested.connect(received.append)

        bridge.onExportChat("# AI Voice Agent 对话记录\n")

        self.assertEqual(received, ["# AI Voice Agent 对话记录\n"])

    def test_should_expose_export_request_signal_from_qt_ui(self) -> None:
        ui = QtUI()
        received: list[str] = []
        ui.export_requested.connect(received.append)

        ui._bridge.onExportChat("# AI Voice Agent 对话记录\n")

        self.assertEqual(received, ["# AI Voice Agent 对话记录\n"])

    def test_should_save_exported_chat_markdown_to_agent_tmp_files(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = save_chat_export(
                "# AI Voice Agent 对话记录\n",
                workspace_root=Path(temp_dir),
                now=datetime(2026, 6, 16, 9, 30, 5),
            )

            saved_text = path.read_text(encoding="utf-8")

        self.assertEqual(path.name, "chat_export_20260616_093005.md")
        self.assertEqual(path.parent.name, "files")
        self.assertEqual(path.parent.parent.name, ".agent_tmp")
        self.assertEqual(saved_text, "# AI Voice Agent 对话记录\n")

    def test_should_expose_qt_session_controls_and_callbacks(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")
        backend_bridge = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "_bridge.py").read_text(
            encoding="utf-8"
        )
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )

        self.assertIn('id="session-list"', index_source)
        self.assertIn('id="nav-rename"', index_source)
        self.assertIn('id="nav-compact"', index_source)
        self.assertIn("window.SessionSidebar", app_source)
        self.assertIn("onRequestSessions", app_source)
        self.assertIn("onResumeSession", app_source)
        self.assertIn("onRenameSession", app_source)
        self.assertIn("onCompactSession", app_source)
        self.assertIn("renderSessionMessages", callback_source)
        self.assertIn("updateSessionList", callback_source)
        self.assertIn("onNewSession", bridge_source)
        self.assertIn("sessions_refresh_requested", backend_bridge)
        self.assertIn("_on_session_resume_requested", window_source)

    def test_should_integrate_session_search_from_sidebar_button(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        search_source = (QT_WEB_DIR / "js" / "search-dialog.js").read_text(encoding="utf-8")
        search_css = (QT_WEB_DIR / "css" / "search-dialog.css").read_text(encoding="utf-8")

        self.assertIn('id="nav-search"', index_source)
        self.assertIn('id="session-search-modal"', index_source)
        self.assertIn('id="session-search-input"', index_source)
        self.assertIn('src="js/search-dialog.js"', index_source)
        self.assertLess(
            index_source.index('src="js/search-dialog.js"'),
            index_source.index('src="js/py-callbacks.js"'),
        )

        self.assertIn("SessionSearch.init()", app_source)
        self.assertIn("window.SessionSearch.updateSessionList(sessions)", callback_source)
        self.assertIn("window.SessionSearch.updateProjectList(projects)", callback_source)
        self.assertIn("window.SessionSearch.setCurrentSession(sessionId, title)", callback_source)
        self.assertIn("window.bridge.onResumeSession(sessionId)", search_source)
        self.assertIn("function updateProjectList(projects)", search_source)
        self.assertIn("function render()", search_source)
        self.assertIn("modalEl.addEventListener('mousedown'", search_source)
        self.assertIn("if (event.target === modalEl)", search_source)
        self.assertIn("document.addEventListener('mousedown', handleDocumentMouseDown, true)", search_source)
        self.assertIn("!panel.contains(event.target)", search_source)
        self.assertIn(".session-search-panel", search_css)
        self.assertIn(".session-search-result", search_css)

    def test_should_delete_session_with_undo_bar_instead_of_confirm_modal(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        dialog_css = (QT_WEB_DIR / "css" / "dialog.css").read_text(encoding="utf-8")

        self.assertNotIn('id="delete-session-modal"', index_source)
        self.assertNotIn("delete-session-confirm", index_source)
        self.assertIn('id="delete-undo-bar"', index_source)
        self.assertIn('id="delete-undo-action"', index_source)

        self.assertIn("const DELETE_UNDO_DELAY_MS = 5000", app_source)
        self.assertIn("let pendingDelete = null", app_source)
        self.assertIn("function scheduleDelete(sessionId, title)", app_source)
        self.assertIn("function undoPendingDelete()", app_source)
        self.assertIn("function commitPendingDelete()", app_source)
        self.assertIn("window.bridge.onDeleteSession(target.sessionId)", app_source)
        self.assertIn("if (pendingDelete) {\n      commitPendingDelete();\n    }", app_source)
        self.assertNotIn("openDeleteModal", app_source)
        self.assertNotIn("delete-session-modal", app_source)

        self.assertIn(".delete-undo-bar", dialog_css)
        self.assertIn("bottom: 32px", dialog_css)

    def test_qt_ui_should_forward_session_updates_to_window(self) -> None:
        ui = QtUI()
        window = FakeSessionWindow()
        ui._window = window

        ui.update_session_list([{"id": "s1"}])
        ui.render_session_messages([{"type": "user", "content": "你好"}])
        ui.set_current_session("s1", "标题")
        ui.show_session_list_error("失败")

        self.assertEqual(window.session_lists, [[{"id": "s1"}]])
        self.assertEqual(window.rendered_messages, [[{"type": "user", "content": "你好"}]])
        self.assertEqual(window.current_sessions, [("s1", "标题")])
        self.assertEqual(window.session_errors, ["失败"])

    def test_qt_session_event_projection_replays_messages_and_tools(self) -> None:
        session_id = "20260616-093005-abcdef"
        events = [
            SessionEvent.create(session_id=session_id, event_type="user_message", payload={"content": "读文件"}),
            SessionEvent.create(
                session_id=session_id,
                event_type="tool_call_requested",
                payload={"tool": "read_file", "arguments": {"path": "README.md"}},
            ),
            SessionEvent.create(
                session_id=session_id,
                event_type="tool_result",
                payload={
                    "tool": "read_file",
                    "ok": True,
                    "output": "README 摘要",
                    "model_output": "README",
                    "artifact_path": "artifacts/session/tool_result.txt",
                },
            ),
            SessionEvent.create(session_id=session_id, event_type="assistant_message", payload={"content": "完成"}),
        ]

        projected = _session_events_to_ui(events)

        self.assertEqual(projected[0]["type"], "user")
        self.assertEqual(projected[1]["type"], "tool_start")
        self.assertEqual(projected[1]["step"], 1)
        self.assertEqual(projected[2]["type"], "tool_result")
        self.assertIn("完整输出 artifact", projected[2]["output"])
        self.assertIn("README", projected[2]["output"])
        self.assertEqual(projected[3]["type"], "assistant")

    def test_should_restore_html_preview_content_when_replaying_artifact_session(self) -> None:
        session_id = "20260620-172052-999ed2"
        html = "<!doctype html><html><body><h1>历史 HTML</h1></body></html>"
        artifact_path = "artifacts/20260620-172052-999ed2/html_preview_ab1025bf7b4f46a8.html"
        read_calls: list[tuple[str, str]] = []
        events = [
            SessionEvent.create(
                session_id=session_id,
                event_type="tool_result",
                payload={
                    "tool": "display_html",
                    "ok": True,
                    "output": "已发送到右侧 HTML 显示区。",
                    "ui_artifact": {
                        "type": "html",
                        "title": "历史预览",
                        "artifact_path": artifact_path,
                        "html_size_chars": len(html),
                    },
                },
            )
        ]

        def read_html_artifact(read_session_id: str, read_artifact_path: str) -> str:
            read_calls.append((read_session_id, read_artifact_path))
            return html if read_session_id == session_id and read_artifact_path == artifact_path else ""

        projected = _session_events_to_ui(
            events,
            read_html_artifact=read_html_artifact,
        )

        self.assertEqual(read_calls, [(session_id, artifact_path)])
        self.assertEqual(projected[0]["uiArtifact"]["html"], html)
        self.assertEqual(projected[0]["uiArtifact"]["artifact_path"], artifact_path)

    def test_should_keep_artifact_placeholder_when_html_artifact_is_missing(self) -> None:
        session_id = "20260620-172052-999ed2"
        artifact_path = "artifacts/20260620-172052-999ed2/html_preview_missing.html"
        events = [
            SessionEvent.create(
                session_id=session_id,
                event_type="tool_result",
                payload={
                    "tool": "display_html",
                    "ok": True,
                    "output": "已发送到右侧 HTML 显示区。",
                    "ui_artifact": {
                        "type": "html",
                        "title": "历史预览",
                        "artifact_path": artifact_path,
                    },
                },
            )
        ]

        projected = _session_events_to_ui(
            events,
            read_html_artifact=lambda _session_id, _path: (_ for _ in ()).throw(RuntimeError("missing")),
        )

        self.assertNotIn("html", projected[0]["uiArtifact"])
        self.assertEqual(projected[0]["uiArtifact"]["artifact_path"], artifact_path)

    def test_should_not_duplicate_user_message_when_frontend_already_echoed_input(self) -> None:
        ui = QtUI()
        window = FakeChatWindow()
        ui._window = window

        result = ui.inline_turn_base("你好")

        self.assertEqual(result, "")
        self.assertEqual(window.user_messages, [])

    def test_should_sanitize_layout_breaking_html_when_rendering_markdown(self) -> None:
        source = (QT_WEB_DIR / "js" / "markdown.js").read_text(encoding="utf-8")

        self.assertIn("sanitizeRenderedHtml", source)
        self.assertIn("FORBIDDEN_TAGS", source)
        self.assertIn("'style'", source)
        self.assertRegex(source, r"on\\w")

    def test_should_constrain_layout_when_content_is_long(self) -> None:
        messages_css = (QT_WEB_DIR / "css" / "messages.css").read_text(encoding="utf-8")
        dialog_css = (QT_WEB_DIR / "css" / "dialog.css").read_text(encoding="utf-8")
        title_css = (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8")
        status_css = (QT_WEB_DIR / "css" / "status-bar.css").read_text(encoding="utf-8")
        input_css = (QT_WEB_DIR / "css" / "input-area.css").read_text(encoding="utf-8")
        input_js = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")

        self.assertRegex(messages_css, r"\.bubble\s+img[^{]*{[^}]*max-width:\s*100%")
        self.assertRegex(messages_css, r"\.bubble\s+table[^{]*{[^}]*overflow-x:\s*auto")
        self.assertRegex(dialog_css, r"\.confirm-inline-body[^{]*{[^}]*max-height:\s*200px")
        self.assertRegex(dialog_css, r"\.confirm-inline-body[^{]*{[^}]*overflow-y:\s*auto")
        self.assertRegex(title_css, r"\.model-badge[^{]*{[^}]*display:\s*none")
        self.assertRegex(status_css, r"#status-text[^{]*{[^}]*text-overflow:\s*ellipsis")
        self.assertIn("--input-min-height", input_css)
        self.assertNotIn("'34px'", input_js)

    def test_should_render_content_as_cards_when_displayed_in_qt_webengine(self) -> None:
        messages_css = (QT_WEB_DIR / "css" / "messages.css").read_text(encoding="utf-8")

        self.assertIn("background: var(--bg-card)", messages_css)
        self.assertIn(".msg-row", messages_css)
        self.assertRegex(messages_css, r"\.bubble[^{]*{[^}]*color:\s*var\(--text-primary\)")
        self.assertRegex(messages_css, r"\.bubble[^{]*{[^}]*line-height:\s*1\.7")
        self.assertRegex(messages_css, r"\.startup-card[^{]*{[^}]*max-width:\s*640px")
        self.assertRegex(messages_css, r"\.startup-line[^{]*{[^}]*grid-template-columns:\s*minmax")
        self.assertIn(".model-badge:empty", (QT_WEB_DIR / "css" / "title-bar.css").read_text(encoding="utf-8"))

    def test_should_keep_qt_layout_compact_enough_for_standard_window(self) -> None:
        variables_css = (QT_WEB_DIR / "css" / "variables.css").read_text(encoding="utf-8")
        reset_css = (QT_WEB_DIR / "css" / "reset.css").read_text(encoding="utf-8")
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )

        def root_px(name: str) -> int:
            match = re.search(rf"{re.escape(name)}:\s*(\d+)px", variables_css)
            self.assertIsNotNone(match, f"{name} should be defined in variables.css")
            return int(match.group(1))

        font_size = re.search(r"html,\s*body\s*{[\s\S]*?font-size:\s*(\d+)px", reset_css)
        self.assertIsNotNone(font_size, "global font-size should stay explicit")
        self.assertLessEqual(int(font_size.group(1)), 18)

        self.assertLessEqual(root_px("--sidebar-width"), 300)
        self.assertLessEqual(root_px("--chat-max-width"), 880)
        self.assertLessEqual(root_px("--input-max-width"), 880)
        self.assertLessEqual(root_px("--input-min-height"), 56)

        minimum_size = re.search(r"setMinimumSize\((\d+),\s*(\d+)\)", window_source)
        self.assertIsNotNone(minimum_size, "Qt window should declare a minimum size")
        self.assertLessEqual(int(minimum_size.group(1)), 1100)
        self.assertLessEqual(int(minimum_size.group(2)), 700)

    def test_should_surface_workspace_info_when_startup_lines_are_rendered(self) -> None:
        messages_js = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")

        self.assertIn("Input.setWorkspaceInfo", messages_js)
        self.assertIn("setWorkspaceInfo", callback_source)

    def test_project_sidebar_should_expose_import_and_use_own_confirm_modal(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        project_source = (QT_WEB_DIR / "js" / "project-sidebar.js").read_text(encoding="utf-8")

        self.assertIn('id="project-confirm-modal"', index_source)
        self.assertIn("openModal('import')", project_source)
        self.assertIn("project-confirm-modal", project_source)
        self.assertIn("project-confirm-confirm", project_source)
        self.assertNotIn("delete-session-confirm", project_source)
        self.assertNotIn("archiveProject(", project_source)

    def test_should_connect_reasoning_effort_control_to_backend(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")
        messages_source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")
        backend_bridge = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "_bridge.py").read_text(
            encoding="utf-8"
        )
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )

        # reasoning-toggle 是 JS 动态创建的，检查 model-btn 按钮和 data-value 属性
        self.assertIn('id="model-btn"', index_source)
        self.assertIn('data-value="none"', input_source)
        self.assertIn('data-value="high"', input_source)
        self.assertIn("window.bridge.setReasoningEffort(value)", input_source)
        self.assertIn("setReasoningEffort: setReasoningEffort", input_source)
        self.assertIn("Input.setReasoningEffort", messages_source)
        self.assertIn("setReasoningEffort", bridge_source)
        self.assertIn("reasoning_effort_changed", backend_bridge)
        self.assertIn("_on_reasoning_effort_changed", window_source)
        self.assertIn("_connect_frontend_signals_once", window_source)
        self.assertIn("/reasoning", window_source)

    def test_should_connect_approval_mode_control_to_backend(self) -> None:
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")
        messages_source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")
        bridge_source = (QT_WEB_DIR / "bridge.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        backend_bridge = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "_bridge.py").read_text(
            encoding="utf-8"
        )
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )

        self.assertIn("window.bridge.setApprovalMode(value)", input_source)
        self.assertIn("setApprovalMode: setApprovalMode", input_source)
        self.assertIn("Input.setApprovalMode", messages_source)
        self.assertIn("setApprovalMode", callback_source)
        self.assertIn("setApprovalMode", bridge_source)
        self.assertIn("approval_mode_changed", backend_bridge)
        self.assertIn("_on_approval_mode_changed", window_source)
        self.assertIn("/approval:", window_source)

    def test_project_toolbar_dropdown_should_reuse_project_modal(self) -> None:
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")
        project_source = (QT_WEB_DIR / "js" / "project-sidebar.js").read_text(encoding="utf-8")

        self.assertIn("openProjectModal('create')", input_source)
        self.assertIn("openProjectModal('import')", input_source)
        self.assertIn("window.ProjectSidebar.openModal(mode)", input_source)
        self.assertNotIn("onCreateProject('新项目', '')", input_source)
        self.assertNotIn("onImportProject('新项目', '')", input_source)
        self.assertIn("openModal: openModal", project_source)

    def test_should_update_reasoning_label_when_backend_sets_initial_effort(self) -> None:
        input_source = (QT_WEB_DIR / "js" / "input.js").read_text(encoding="utf-8")

        set_effort_start = input_source.index("function setReasoningEffort(effort)")
        set_effort_end = input_source.index("function setWorkspaceInfo", set_effort_start)
        set_effort_body = input_source[set_effort_start:set_effort_end]

        self.assertIn("syncToolbarLabels();", set_effort_body)
        self.assertRegex(
            set_effort_body,
            r"currentReasoning\s*=\s*effort\s*\|\|\s*'none';[\s\S]*syncToolbarLabels\(\);",
        )

    def test_should_keep_approval_dropdown_readable_without_wrapping(self) -> None:
        input_css = (QT_WEB_DIR / "css" / "input-area.css").read_text(encoding="utf-8")

        self.assertRegex(
            input_css,
            r"#approval-dropdown\s*{[\s\S]*?min-width:\s*280px",
        )
        self.assertRegex(
            input_css,
            r"#approval-dropdown\s+\.reasoning-option\s*{[\s\S]*?grid-template-columns:\s*16px\s+64px\s+1fr",
        )
        self.assertRegex(
            input_css,
            r"#approval-dropdown\s+\.option-label\s*{[\s\S]*?white-space:\s*nowrap",
        )
        self.assertRegex(
            input_css,
            r"#approval-dropdown\s+\.option-desc\s*{[\s\S]*?white-space:\s*nowrap",
        )

    def test_should_use_windows_system_font_stack(self) -> None:
        variables_css = (QT_WEB_DIR / "css" / "variables.css").read_text(encoding="utf-8")

        self.assertIn("system-ui", variables_css)
        self.assertIn("'Segoe UI'", variables_css)
        self.assertIn("'Microsoft YaHei UI'", variables_css)
        self.assertNotIn("'楷体", variables_css)

    def test_should_keep_primary_qt_text_readable(self) -> None:
        reset_css = (QT_WEB_DIR / "css" / "reset.css").read_text(encoding="utf-8")
        messages_css = (QT_WEB_DIR / "css" / "messages.css").read_text(encoding="utf-8")
        input_css = (QT_WEB_DIR / "css" / "input-area.css").read_text(encoding="utf-8")
        layout_css = (QT_WEB_DIR / "css" / "layout.css").read_text(encoding="utf-8")

        self.assertRegex(reset_css, r"html,\s*body\s*{[\s\S]*?font-size:\s*18px")
        self.assertRegex(messages_css, r"\.bubble\s*{[\s\S]*?font-size:\s*18px")
        self.assertRegex(input_css, r"#chat-input\s*{[\s\S]*?font-size:\s*18px")
        self.assertRegex(layout_css, r"\.nav-item\s*{[\s\S]*?font-size:\s*16px")

    def test_sidebar_toggle_should_have_accessible_state_and_storage_fallback(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")

        self.assertIn('aria-expanded="true"', index_source)
        self.assertIn("setSidebarCollapsed", app_source)
        self.assertIn("readSidebarCollapsed", app_source)
        self.assertIn("writeSidebarCollapsed", app_source)
        self.assertIn("try", app_source)

    def test_should_add_right_side_html_preview_panel(self) -> None:
        index_source = (QT_WEB_DIR / "index.html").read_text(encoding="utf-8")
        app_source = (QT_WEB_DIR / "js" / "app.js").read_text(encoding="utf-8")
        callback_source = (QT_WEB_DIR / "js" / "py-callbacks.js").read_text(encoding="utf-8")
        preview_source = (QT_WEB_DIR / "js" / "html-preview.js").read_text(encoding="utf-8")
        preview_css = (QT_WEB_DIR / "css" / "html-preview.css").read_text(encoding="utf-8")
        variables_css = (QT_WEB_DIR / "css" / "variables.css").read_text(encoding="utf-8")
        window_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "window.py").read_text(
            encoding="utf-8"
        )
        ui_source = (PROJECT_ROOT / "ai_voice_agent" / "ui" / "qt" / "qt_ui.py").read_text(
            encoding="utf-8"
        )

        self.assertIn('id="html-preview-panel"', index_source)
        self.assertIn('id="html-preview-frame"', index_source)
        self.assertIn('sandbox="allow-scripts allow-forms allow-popups allow-modals"', index_source)
        self.assertIn('src="js/html-preview.js"', index_source)
        self.assertLess(
            index_source.index('src="js/html-preview.js"'),
            index_source.index('src="js/py-callbacks.js"'),
        )
        self.assertIn("HtmlPreview.init()", app_source)
        self.assertIn("showHtmlPreview", callback_source)
        self.assertIn("frame.srcdoc = currentHtml", preview_source)
        self.assertIn("noopener,noreferrer", preview_source)
        self.assertIn("win.opener = null", preview_source)
        self.assertIn("html-preview-collapsed", preview_source)
        self.assertIn(".html-preview-panel", preview_css)
        self.assertIn("--html-preview-width", variables_css)
        self.assertIn("self.resize(1920, 1018)", window_source)
        self.assertIn("def show_html", window_source)
        self.assertIn("showHtmlPreview", window_source)
        self.assertIn("def show_html", ui_source)

    def test_qt_ui_should_forward_html_preview_to_window(self) -> None:
        ui = QtUI()
        window = FakeModelWindow()
        ui._window = window

        ui.show_html("采集结果", "<html><body>ok</body></html>")

        self.assertEqual(window.html_previews, [("采集结果", "<html><body>ok</body></html>")])


if __name__ == "__main__":
    unittest.main()
