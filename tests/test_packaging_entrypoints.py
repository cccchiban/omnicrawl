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
        self.assertRegex(pyproject, r'version = "0\.1\.\d+"')
        self.assertIn("name = omnicrawl-agent", setup_cfg)
        self.assertRegex(setup_cfg, r"version = 0\.1\.\d+")
        self.assertIn(
            "Repository = https://github.com/cccchiban/omnicrawl",
            setup_cfg,
        )
        import re

        pyproject_version = re.search(r'version = "(0\.1\.\d+)"', pyproject)
        setup_cfg_version = re.search(r"version = (0\.1\.\d+)", setup_cfg)
        self.assertIsNotNone(pyproject_version)
        self.assertIsNotNone(setup_cfg_version)
        self.assertEqual(pyproject_version.group(1), setup_cfg_version.group(1))
        self.assertEqual(OMNICRAWL_VERSION, pyproject_version.group(1))
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
