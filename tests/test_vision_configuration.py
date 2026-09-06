from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib

from omnicrawl.config.models.llm import ActiveModelRef
from omnicrawl.config.models.vision import (
    VisionConfigError,
    VisionConfiguration,
    load_vision_configuration,
    save_vision_configuration,
)


class VisionConfigurationTests(unittest.TestCase):
    def test_round_trip_preserves_order_and_other_sections(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text(
                "[llm.active_model]\nsource = \"custom\"\nkey = \"main\"\n\n[memory]\nenabled = true\n",
                encoding="utf-8",
            )
            configuration = VisionConfiguration(
                enabled=True,
                models=(
                    ActiveModelRef(source="custom", key="vision-first"),
                    ActiveModelRef(
                        source="detected",
                        profile="anthropic-main",
                        model_id="claude-sonnet",
                        protocol="anthropic_messages",
                    ),
                ),
            )

            self.assertEqual(save_vision_configuration(configuration, path), path)
            loaded = load_vision_configuration(path)
            data = tomllib.loads(path.read_text(encoding="utf-8"))

        self.assertEqual(loaded, configuration)
        self.assertEqual(data["llm"]["active_model"]["key"], "main")
        self.assertTrue(data["memory"]["enabled"])
        self.assertEqual(
            [item["key"] for item in data["vision"]["models"][:1]],
            ["vision-first"],
        )
        self.assertEqual(
            data["vision"]["models"][1]["profile"],
            "anthropic-main",
        )

    def test_missing_section_defaults_to_disabled_and_empty(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text("[llm]\nmodel = \"demo\"\n", encoding="utf-8")
            self.assertEqual(load_vision_configuration(path), VisionConfiguration())

    def test_rejects_duplicate_and_malformed_model_refs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            path.write_text(
                "[vision]\nenabled = true\nmodels = [\n    {source = \"custom\", key = \"duplicate\"},\n    {source = \"custom\", key = \"duplicate\"},\n]\n",
                encoding="utf-8",
            )
            with self.assertRaises(VisionConfigError):
                load_vision_configuration(path)

            path.write_text(
                "[vision]\nenabled = true\nmodels = [{source = \"detected\", profile = \"only-profile\"}]\n",
                encoding="utf-8",
            )
            with self.assertRaises(VisionConfigError):
                load_vision_configuration(path)


if __name__ == "__main__":
    unittest.main()
