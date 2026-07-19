from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from omnicrawl.cli import build_parser, run_plugin_command


class PluginCLITest(unittest.TestCase):
    def test_parser_plugin_install(self) -> None:
        parser = build_parser()
        args = parser.parse_args(["plugin", "install", "@x/y@1.0.0", "--user", "--yes"])
        self.assertEqual(args.command, "plugin")
        self.assertEqual(args.plugin_command, "install")
        self.assertEqual(args.package_spec, "@x/y@1.0.0")

    def test_list_empty(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            parser = build_parser()
            args = parser.parse_args(["plugin", "list", "--json", "--user"])
            with mock.patch("omnicrawl.cli.list_plugins", return_value=[]):
                code = run_plugin_command(args, workspace_root=root)
            self.assertEqual(code, 0)

    def test_system_enable_writes_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.yaml"
            config_path.write_text("{}", encoding="utf-8")
            parser = build_parser()
            args = parser.parse_args(["plugin", "system", "enable"])
            with mock.patch("omnicrawl.cli.load_config_data", return_value={}):
                with mock.patch("omnicrawl.cli.save_config_data") as save:
                    save.return_value = config_path
                    code = run_plugin_command(args, workspace_root=Path(temp_dir))
            self.assertEqual(code, 0)
            save.assert_called_once()
            written = save.call_args[0][0]
            self.assertTrue(written["plugins"]["enabled"])


if __name__ == "__main__":
    unittest.main()
