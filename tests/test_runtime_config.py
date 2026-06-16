from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from ai_voice_agent.frontend_config import load_frontend_config
from ai_voice_agent.runtime_config import RuntimeConfigError
from ai_voice_agent.runtime_config import load_config_data, save_config_data


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


if __name__ == "__main__":
    unittest.main()
