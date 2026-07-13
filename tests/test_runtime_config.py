from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.entry import _parse_args, run_application
from omnicrawl.runtime_config import default_config_path, load_config_data, save_config_data
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.windows_launcher import launch_in_powershell_window


class RuntimeConfigTest(unittest.TestCase):
    def test_default_config_path_points_to_project_root_config(self) -> None:
        expected_path = Path(__file__).resolve().parent.parent / "config.json"

        self.assertEqual(default_config_path(), expected_path)

    def test_load_config_data_accepts_utf8_bom(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            payload = json.dumps({"llm": {"model": "demo"}}).encode("utf-8")
            config_path.write_bytes(b"\xef\xbb\xbf" + payload)

            data = load_config_data(config_path)

        self.assertEqual(data["llm"]["model"], "demo")

    def test_save_config_data_writes_utf8_without_bom(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"

            save_config_data({"approval": {"mode": "auto"}}, config_path)
            raw = config_path.read_bytes()

        self.assertFalse(raw.startswith(b"\xef\xbb\xbf"))
        self.assertEqual(json.loads(raw.decode("utf-8"))["approval"]["mode"], "auto")

    def test_parse_args_accepts_resume_session_id(self) -> None:
        args = _parse_args(["--resume", "20260616-201530-a1b2c3"])

        self.assertEqual(args.resume, "20260616-201530-a1b2c3")

    def test_main_reports_readable_error_when_fullscreen_ui_dependency_is_missing(self) -> None:
        with patch("omnicrawl.entry.configure_console_encoding"):
            with patch(
                "omnicrawl.entry._load_fullscreen_ui",
                side_effect=UIStartupError("缺少可选终端界面依赖：textual。请执行 pip install -r requirements.txt。"),
            ):
                with patch("builtins.print") as print_mock:
                    code = run_application([])

        self.assertEqual(code, 1)

        print_mock.assert_called_once_with("界面启动失败：缺少可选终端界面依赖：textual。请执行 pip install -r requirements.txt。")

    def test_main_starts_fullscreen_ui_and_closes_agent(self) -> None:
        config = SimpleNamespace(
            thinking_enabled=True,
            reasoning_effort="max",
        )
        project_context = SimpleNamespace(
            workspace_root=Path("D:/workspace"),
            detection_summary="workspace",
            source="fallback_start",
            marker="",
        )
        agent = Mock()

        with patch("omnicrawl.entry.configure_console_encoding"):
            with patch("omnicrawl.entry.load_llm_config", return_value=config):
                with patch("omnicrawl.entry.load_approval_mode", return_value="manual"):
                    with patch("omnicrawl.entry.load_agent_temp_workspace_config", return_value="temp-config"):
                        with patch("omnicrawl.entry.detect_project_context", return_value=project_context):
                            with patch("omnicrawl.entry.agent_temp_status_label", return_value=".agent_tmp"):
                                with patch("omnicrawl.entry.AgentConfig") as agent_config_class:
                                    with patch("omnicrawl.entry.LocalToolAgent", return_value=agent) as agent_class:
                                        with patch("omnicrawl.entry._load_fullscreen_ui") as load_fullscreen_ui:
                                            fullscreen_startup = Mock()
                                            run_fullscreen_tui = Mock()
                                            load_fullscreen_ui.return_value = (fullscreen_startup, run_fullscreen_tui)
                                            code = run_application(["--resume", "session-demo"])

        self.assertEqual(code, 0)

        agent_config_class.assert_called_once_with(
            llm=config,
            workspace_root=Path("D:/workspace"),
            workspace_detection_summary="workspace",
            approval_mode="manual",
            temp_workspace="temp-config",
            resume_session_id="session-demo",
        )
        agent_class.assert_called_once()
        agent_config = agent_class.call_args.args[0]
        self.assertEqual(agent_config, agent_config_class.return_value)
        fullscreen_startup.assert_called_once()
        run_fullscreen_tui.assert_called_once_with(agent, fullscreen_startup.return_value)
        agent.close.assert_called_once()

    def test_windows_launcher_forwards_resume_argument(self) -> None:
        popen_calls: list[dict[str, object]] = []

        def fake_popen(command, **kwargs):
            popen_calls.append({"command": command, **kwargs})
            return object()

        with patch("omnicrawl.ui.windows_launcher.os.name", "nt"):
            with patch("omnicrawl.ui.windows_launcher._running_in_powershell_child", return_value=False):
                with patch("omnicrawl.ui.windows_launcher.subprocess.Popen", side_effect=fake_popen):
                    with patch("omnicrawl.ui.windows_launcher.subprocess.CREATE_NEW_CONSOLE", 16, create=True):
                        launched = launch_in_powershell_window(
                            Path("main.py"),
                            ["--resume", "20260616-201530-a1b2c3"],
                        )

        self.assertTrue(launched)
        command = popen_calls[0]["command"]
        self.assertIsInstance(command, list)
        power_shell_command = command[-1]
        self.assertIn("'--resume' '20260616-201530-a1b2c3'", power_shell_command)


if __name__ == "__main__":
    unittest.main()
