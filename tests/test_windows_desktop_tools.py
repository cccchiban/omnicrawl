from __future__ import annotations

import base64
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent.core import LocalToolAgent
from omnicrawl.agent.tools import build_agent_tools, public_tool_arguments
from omnicrawl.commands.slash import format_tool_confirmation
from omnicrawl.agent.types import ToolDefinition, ToolResult
from omnicrawl.agent.windows_desktop import WindowsDesktopTools


class _FakeWindowsDesktopTools(WindowsDesktopTools):
    """隔离真实桌面，验证结构化参数如何映射到各个原生能力。"""

    def __init__(self, screenshot_directory: Path | None = None) -> None:
        super().__init__(
            screenshot_directory=screenshot_directory,
            workspace_root=screenshot_directory.parent.parent.parent if screenshot_directory else None,
        )
        self.window_rows = [
            {
                "window_handle": "0x0000000000000010",
                "title": "Editor - OmniCrawl",
                "class_name": "EditorWindow",
                "process_id": 100,
                "is_minimized": False,
                "bounds": {
                    "left": 10,
                    "top": 20,
                    "right": 510,
                    "bottom": 420,
                    "width": 500,
                    "height": 400,
                },
            },
            {
                "window_handle": "0x0000000000000020",
                "title": "Terminal",
                "class_name": "ConsoleWindowClass",
                "process_id": 200,
                "is_minimized": False,
                "bounds": {
                    "left": 0,
                    "top": 0,
                    "right": 800,
                    "bottom": 600,
                    "width": 800,
                    "height": 600,
                },
            },
        ]
        self.calls: list[tuple[str, object]] = []

    def _ensure_windows(self) -> None:
        """测试替身不访问真实 Windows API。"""

    def _enumerate_windows(self) -> list[dict[str, object]]:
        return list(self.window_rows)

    def _describe_window(self, handle: int) -> dict[str, object]:
        for item in self.window_rows:
            if int(str(item["window_handle"]), 16) == handle:
                return dict(item)
        raise AssertionError(f"unexpected handle: {handle}")

    def _activate_window(self, handle: int) -> None:
        self.calls.append(("activate_window", handle))

    def _run_ui_automation(self, request: dict[str, object]) -> dict[str, object]:
        self.calls.append(("uia", request))
        return {"action": request["action"], "target": {"name": "Search"}}

    def _send_mouse_move(self, x: int, y: int) -> None:
        self.calls.append(("move", (x, y)))

    def _send_mouse_click(self, x: int, y: int, button: str, clicks: int) -> None:
        self.calls.append(("click", (x, y, button, clicks)))

    def _send_mouse_scroll(self, x: int | None, y: int | None, delta: int) -> None:
        self.calls.append(("scroll", (x, y, delta)))

    def _send_virtual_keys(self, keys: list[str], presses: int = 1) -> None:
        self.calls.append(("keys", (keys, presses)))

    def _send_unicode_text(self, text: str) -> None:
        self.calls.append(("text", text))

    def _read_clipboard_text(self) -> str:
        self.calls.append(("read_clipboard", None))
        return "copied text"

    def _write_clipboard_text(self, text: str) -> None:
        self.calls.append(("write_clipboard", text))

    def _clear_clipboard(self) -> None:
        self.calls.append(("clear_clipboard", None))

    def _virtual_desktop_bounds(self) -> dict[str, int]:
        return {
            "left": -100,
            "top": 0,
            "right": 1_900,
            "bottom": 1_000,
            "width": 2_000,
            "height": 1_000,
        }

    def _capture_screenshot_png(
        self,
        source_bounds: dict[str, int],
        output_path: Path,
        *,
        max_dimension: int,
    ) -> tuple[int, int, int]:
        self.calls.append(("screenshot", (dict(source_bounds), max_dimension)))
        png_bytes = b"\x89PNG\r\n\x1a\nfixture"
        output_path.write_bytes(png_bytes)
        return source_bounds["width"], source_bounds["height"], len(png_bytes)


class WindowsDesktopToolsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        screenshot_directory = Path(self.temp_dir.name) / ".omnicrawl" / ".agent_tmp" / "images"
        self.tools = _FakeWindowsDesktopTools(screenshot_directory)

    def test_window_list_filters_and_truncates_results(self) -> None:
        result = self.tools.run_window(
            {"action": "list", "title_contains": "editor", "max_results": 1}
        )

        self.assertTrue(result.ok)
        payload = json.loads(result.output)
        self.assertEqual(payload["matched_count"], 1)
        self.assertFalse(payload["truncated"])
        self.assertEqual(payload["windows"][0]["title"], "Editor - OmniCrawl")

    def test_window_activate_requires_handle_and_preserves_hex_handle(self) -> None:
        denied = self.tools.run_window({"action": "activate"})
        accepted = self.tools.run_window(
            {"action": "activate", "window_handle": "0x20"}
        )

        self.assertFalse(denied.ok)
        self.assertIn("window_handle", denied.output)
        self.assertTrue(accepted.ok)
        self.assertIn(("activate_window", 0x20), self.tools.calls)

    def test_control_set_value_passes_scoped_locator_without_echoing_value(self) -> None:
        result = self.tools.run_control(
            {
                "action": "set_value",
                "window_handle": "0x10",
                "automation_id": "searchBox",
                "value": "keyword",
            }
        )

        self.assertTrue(result.ok)
        request = next(value for name, value in self.tools.calls if name == "uia")
        self.assertIsInstance(request, dict)
        self.assertEqual(request["window_handle"], 0x10)
        self.assertEqual(request["automation_id"], "searchBox")
        self.assertEqual(request["value"], "keyword")
        self.assertNotIn("keyword", result.output)

    def test_control_action_rejects_unscoped_mutation(self) -> None:
        result = self.tools.run_control(
            {"action": "invoke", "window_handle": "0x10"}
        )

        self.assertFalse(result.ok)
        self.assertIn("定位条件", result.output)

    def test_input_actions_validate_and_dispatch(self) -> None:
        invalid = self.tools.run_input({"action": "click", "x": 10})
        click = self.tools.run_input(
            {"action": "click", "x": 10, "y": 20, "button": "right", "clicks": 2}
        )
        hotkey = self.tools.run_input({"action": "hotkey", "keys": ["ctrl", "shift", "s"]})
        typed = self.tools.run_input({"action": "type_text", "text": "中文 text"})

        self.assertFalse(invalid.ok)
        self.assertTrue(click.ok)
        self.assertTrue(hotkey.ok)
        self.assertTrue(typed.ok)
        self.assertIn(("click", (10, 20, "right", 2)), self.tools.calls)
        self.assertIn(("keys", (["ctrl", "shift", "S"], 1)), self.tools.calls)
        self.assertIn(("text", "中文 text"), self.tools.calls)

    def test_clipboard_text_operations_are_bounded_and_structured(self) -> None:
        read = self.tools.run_clipboard({"action": "read_text", "max_chars": 5})
        write = self.tools.run_clipboard({"action": "write_text", "text": "next value"})
        clear = self.tools.run_clipboard({"action": "clear"})

        self.assertTrue(read.ok)
        self.assertEqual(json.loads(read.output)["text"], "copie")
        self.assertTrue(json.loads(read.output)["truncated"])
        self.assertTrue(write.ok)
        self.assertTrue(clear.ok)
        self.assertIn(("write_clipboard", "next value"), self.tools.calls)
        self.assertIn(("clear_clipboard", None), self.tools.calls)

    def test_screenshot_supports_desktop_region_and_window_with_model_image(self) -> None:
        desktop = self.tools.run_screenshot({"target": "desktop"})
        region = self.tools.run_screenshot(
            {"target": "region", "x": -50, "y": 10, "width": 300, "height": 200}
        )
        window = self.tools.run_screenshot(
            {"target": "window", "window_handle": "0x10", "max_dimension": 1024}
        )

        for result in (desktop, region, window):
            self.assertTrue(result.ok, result.output)
            self.assertEqual(len(result.model_images), 1)
            self.assertTrue(
                base64.b64decode(result.model_images[0].data_base64).startswith(b"\x89PNG")
            )
            payload = json.loads(result.output)
            self.assertTrue((Path(self.temp_dir.name) / payload["path"]).is_file())
        self.assertEqual(json.loads(region.output)["source_bounds"]["left"], -50)
        self.assertEqual(json.loads(window.output)["source_bounds"]["width"], 500)

    def test_window_screenshot_clips_partially_offscreen_bounds(self) -> None:
        self.tools.window_rows[0]["bounds"] = {
            "left": -150,
            "top": -20,
            "right": 350,
            "bottom": 380,
            "width": 500,
            "height": 400,
        }

        result = self.tools.run_screenshot(
            {"target": "window", "window_handle": "0x10"}
        )

        self.assertTrue(result.ok, result.output)
        bounds = json.loads(result.output)["source_bounds"]
        self.assertEqual(bounds, {
            "left": -100,
            "top": 0,
            "right": 350,
            "bottom": 380,
            "width": 450,
            "height": 380,
        })

    def test_screenshot_rejects_out_of_desktop_region_and_target_mismatches(self) -> None:
        outside = self.tools.run_screenshot(
            {"target": "region", "x": 1800, "y": 0, "width": 200, "height": 100}
        )
        mismatched = self.tools.run_screenshot(
            {"target": "desktop", "window_handle": "0x10"}
        )

        self.assertFalse(outside.ok)
        self.assertIn("虚拟桌面", outside.output)
        self.assertFalse(mismatched.ok)
        self.assertIn("不支持参数", mismatched.output)

    def test_non_windows_environment_returns_a_clear_error(self) -> None:
        with patch("omnicrawl.agent.windows_desktop.os.name", "posix"):
            result = WindowsDesktopTools().run_window({"action": "list"})

        self.assertFalse(result.ok)
        self.assertIn("仅支持 Windows", result.output)


class WindowsDesktopToolIntegrationTest(unittest.TestCase):
    def test_agent_registers_all_windows_tools_with_manual_confirmation(self) -> None:
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        tools = build_agent_tools(
            mcp_manager=manager,
            memory_enabled=False,
            list=runner,
            read=runner,
            grep=runner,
            replace_text=runner,
            write_file=runner,
            bash=runner,
            powershell=runner,
            monitor=runner,
            memory_search=runner,
            memory_read=runner,
            memory_expand_related=runner,
            memory_write=runner,
            mcp_call=lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            mcp_read_resource=lambda _uri: ToolResult(ok=True, output="ok"),
            mcp_get_prompt=lambda _name, _arguments: ToolResult(ok=True, output="ok"),
            windows_window=runner,
            windows_control=runner,
            windows_input=runner,
            windows_clipboard=runner,
            windows_screenshot=runner,
        )

        self.assertNotIn("bb_browser_cli", tools)
        for name in (
            "windows_window",
            "windows_control",
            "windows_input",
            "windows_clipboard",
            "windows_screenshot",
        ):
            with self.subTest(tool=name):
                self.assertIn(name, tools)
                self.assertTrue(tools[name].requires_confirmation)

    def test_sensitive_desktop_arguments_are_not_persisted_or_shown_verbatim(self) -> None:
        clipboard = public_tool_arguments(
            "windows_clipboard",
            {"action": "write_text", "text": "password=never-save-this"},
        )
        control = public_tool_arguments(
            "windows_control",
            {
                "action": "set_value",
                "window_handle": "0x10",
                "automation_id": "passwordBox",
                "value": "never-save-this",
            },
        )
        typed = public_tool_arguments(
            "windows_input",
            {"action": "type_text", "text": "never-save-this"},
        )

        self.assertNotIn("never-save-this", str(clipboard))
        self.assertNotIn("never-save-this", str(control))
        self.assertNotIn("never-save-this", str(typed))
        self.assertEqual(clipboard["text_length"], len("password=never-save-this"))
        self.assertEqual(control["value_length"], len("never-save-this"))
        self.assertEqual(typed["text_length"], len("never-save-this"))

        confirmation = format_tool_confirmation(
            "windows_control",
            {
                "action": "set_value",
                "window_handle": "0x10",
                "automation_id": "passwordBox",
                "value": "never-save-this",
            },
        )
        self.assertIn("写入文本：15 字符（内容不展示）", confirmation)
        self.assertNotIn("never-save-this", confirmation)

    def test_desktop_tools_are_serial_batch_barriers(self) -> None:
        for name in (
            "windows_window",
            "windows_control",
            "windows_input",
            "windows_clipboard",
            "windows_screenshot",
        ):
            with self.subTest(tool=name):
                definition = ToolDefinition(name, "", "{}", True, lambda _args: ToolResult(True, ""))
                self.assertTrue(LocalToolAgent._tool_call_requires_serial_execution(definition, {}))


if __name__ == "__main__":
    unittest.main()
