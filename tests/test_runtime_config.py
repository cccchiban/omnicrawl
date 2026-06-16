from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from main import _parse_args
from ai_voice_agent.frontend_config import load_frontend_config
from ai_voice_agent.runtime_config import RuntimeConfigError
from ai_voice_agent.runtime_config import load_config_data, save_config_data
from ai_voice_agent.windows_launcher import launch_in_powershell_window


class RuntimeConfigTest(unittest.TestCase):
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

    def test_load_frontend_config_defaults_to_tui(self) -> None:
        with patch("ai_voice_agent.frontend_config.load_config_data", return_value={}):
            config = load_frontend_config()

        self.assertEqual(config.type, "tui")

    def test_load_frontend_config_accepts_qt(self) -> None:
        with patch(
            "ai_voice_agent.frontend_config.load_config_data",
            return_value={"frontend": {"type": "qt"}},
        ):
            config = load_frontend_config()

        self.assertEqual(config.type, "qt")

    def test_load_frontend_config_rejects_unknown_type(self) -> None:
        with patch(
            "ai_voice_agent.frontend_config.load_config_data",
            return_value={"frontend": {"type": "browser"}},
        ):
            with self.assertRaisesRegex(RuntimeConfigError, "frontend.type"):
                load_frontend_config()

    def test_parse_args_accepts_resume_session_id(self) -> None:
        args = _parse_args(["--resume", "20260616-201530-a1b2c3"])

        self.assertEqual(args.resume, "20260616-201530-a1b2c3")

    def test_windows_launcher_forwards_resume_argument(self) -> None:
        popen_calls: list[dict[str, object]] = []

        def fake_popen(command, **kwargs):
            popen_calls.append({"command": command, **kwargs})
            return object()

        with patch("ai_voice_agent.windows_launcher.os.name", "nt"):
            with patch("ai_voice_agent.windows_launcher._running_in_powershell_child", return_value=False):
                with patch("ai_voice_agent.windows_launcher.subprocess.Popen", side_effect=fake_popen):
                    with patch("ai_voice_agent.windows_launcher.subprocess.CREATE_NEW_CONSOLE", 16, create=True):
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
