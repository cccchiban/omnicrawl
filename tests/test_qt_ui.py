from __future__ import annotations

import builtins
from datetime import datetime
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from ai_voice_agent.ui.qt.export import save_chat_export
from ai_voice_agent.ui.qt._bridge import BackendBridge
from ai_voice_agent.ui.qt.qt_ui import QtUI


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

    def test_should_define_message_time_formatter_when_rendering_messages(self) -> None:
        source = (QT_WEB_DIR / "js" / "messages.js").read_text(encoding="utf-8")

        self.assertIn("timeNow()", source)
        self.assertIn("function timeNow()", source)

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
        self.assertIn(".model-option-message", title_css)

    def test_should_request_window_close_through_bridge_signal(self) -> None:
        ui = QtUI()
        window = FakeChatWindow()
        bridge = FakeCloseBridge()
        ui._window = window
        ui._bridge = bridge

        ui.stop()

        self.assertTrue(bridge.close_requested)
        self.assertFalse(window.closed)

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


if __name__ == "__main__":
    unittest.main()
