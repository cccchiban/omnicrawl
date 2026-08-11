from __future__ import annotations

import unittest
from pathlib import Path

from omnicrawl.extensions.plugin_models import OMNICRAWL_VERSION


ROOT = Path(__file__).resolve().parent.parent


class PackagingEntryPointTest(unittest.TestCase):
    def test_distribution_name_is_distinct_from_python_import_package(self) -> None:
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        setup_cfg = (ROOT / "setup.cfg").read_text(encoding="utf-8")

        self.assertIn('name = "omnicrawl-agent"', pyproject)
        self.assertIn('version = "0.1.9"', pyproject)
        self.assertIn("name = omnicrawl-agent", setup_cfg)
        self.assertIn("version = 0.1.9", setup_cfg)
        self.assertIn(
            "Repository = https://github.com/cccchiban/omnicrawl",
            setup_cfg,
        )
        self.assertEqual(OMNICRAWL_VERSION, "0.1.9")
        self.assertIn('"docs/*.md"', pyproject)
        self.assertIn("docs/*.md", setup_cfg)
        self.assertFalse((ROOT / "docs").exists())
        self.assertTrue((ROOT / "omnicrawl" / "docs" / "MCP_USAGE.md").is_file())

        self.assertIn('ocl = "omnicrawl.__main__:main"', pyproject)
        self.assertIn('omnicrawl = "omnicrawl.__main__:main"', pyproject)
        self.assertIn("ocl = omnicrawl.__main__:main", setup_cfg)
        self.assertIn("omnicrawl = omnicrawl.__main__:main", setup_cfg)


if __name__ == "__main__":
    unittest.main()
