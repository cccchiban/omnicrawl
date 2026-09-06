from __future__ import annotations

import ast
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
PACKAGE = ROOT / "omnicrawl" / "agent" / "context_compaction"


class ContextCompactionModuleBoundaryTest(unittest.TestCase):
    def test_context_compaction_modules_do_not_import_core_history_state_or_ui(self) -> None:
        forbidden = {
            "omnicrawl.agent.core",
            "omnicrawl.agent.session.history",
            "omnicrawl.state",
            "omnicrawl.ui",
        }
        violations: list[str] = []

        for path in PACKAGE.glob("*.py"):
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    names = [alias.name for alias in node.names]
                elif isinstance(node, ast.ImportFrom):
                    names = [node.module or ""]
                else:
                    continue
                for name in names:
                    if any(name == item or name.startswith(f"{item}.") for item in forbidden):
                        violations.append(f"{path.name}: {name}")

        self.assertEqual(violations, [])

    def test_summary_prompt_is_declared_in_both_package_manifests(self) -> None:
        expected = "agent/context_compaction/summary_prompt.md"
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        setup_cfg = (ROOT / "setup.cfg").read_text(encoding="utf-8")

        self.assertIn(expected, pyproject)
        self.assertIn(expected, setup_cfg)

    def test_production_modules_stay_below_eight_hundred_lines(self) -> None:
        oversized = {
            path.name: len(path.read_text(encoding="utf-8").splitlines())
            for path in PACKAGE.glob("*.py")
            if len(path.read_text(encoding="utf-8").splitlines()) > 800
        }

        self.assertEqual(oversized, {})


if __name__ == "__main__":
    unittest.main()
