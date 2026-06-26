from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.bb_browser_cli import (
    DEFAULT_BB_BROWSER_TIMEOUT_SECONDS,
    BBBrowserCLI,
    _resolve_bb_browser_command,
)


class BBBrowserCLITest(unittest.TestCase):
    def test_run_passes_args_without_shell(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            cli = BBBrowserCLI(workspace)

            with patch(
                "omnicrawl.bb_browser_cli._resolve_bb_browser_command",
                return_value=["bb-browser"],
            ), patch("omnicrawl.bb_browser_cli.subprocess.run") as run:
                run.return_value.returncode = 0
                run.return_value.stdout = '{"running":true}'
                run.return_value.stderr = ""

                result = cli.run({"args": ["status", "--json"], "timeout_seconds": 5})

        self.assertTrue(result.ok)
        self.assertEqual(result.output, '{"running":true}')
        run.assert_called_once()
        call_kwargs = run.call_args.kwargs
        self.assertFalse(call_kwargs.get("shell", False))
        self.assertEqual(run.call_args.args[0], ["bb-browser", "status", "--json"])
        self.assertEqual(call_kwargs["timeout"], 5)

    def test_run_rejects_empty_or_non_string_args(self) -> None:
        cli = BBBrowserCLI(Path.cwd())

        empty = cli.run({"args": []})
        invalid = cli.run({"args": ["status", 1]})
        blank = cli.run({"args": [""]})

        self.assertFalse(empty.ok)
        self.assertIn("args 不能为空", empty.output)
        self.assertFalse(invalid.ok)
        self.assertIn("args 必须是字符串数组", invalid.output)
        self.assertFalse(blank.ok)
        self.assertIn("args 不能包含空字符串", blank.output)

    def test_run_preserves_argument_whitespace(self) -> None:
        cli = BBBrowserCLI(Path.cwd())

        with patch.object(BBBrowserCLI, "_run_bb_browser", return_value="ok") as run:
            result = cli.run({"args": ["fill", "input-ref", "  keep padded text  "]})

        self.assertTrue(result.ok)
        run.assert_called_once_with(
            ["fill", "input-ref", "  keep padded text  "],
            DEFAULT_BB_BROWSER_TIMEOUT_SECONDS,
        )

    def test_ensure_started_uses_daemon_start(self) -> None:
        cli = BBBrowserCLI(Path.cwd())

        with patch.object(BBBrowserCLI, "_run_bb_browser", return_value='{"running":true}') as run:
            ok, output = cli.ensure_started()

        self.assertTrue(ok)
        self.assertEqual(output, '{"running":true}')
        run.assert_called_once_with(
            ["daemon", "start", "--json"],
            DEFAULT_BB_BROWSER_TIMEOUT_SECONDS,
        )

    def test_resolve_prefers_local_bin(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            bin_dir = workspace / "node_modules" / ".bin"
            bin_dir.mkdir(parents=True)
            expected = bin_dir / "bb-browser.cmd"
            expected.write_text("@echo off\n", encoding="utf-8")

            resolved = _resolve_bb_browser_command(workspace)

        self.assertEqual(resolved, [str(expected)])


if __name__ == "__main__":
    unittest.main()
