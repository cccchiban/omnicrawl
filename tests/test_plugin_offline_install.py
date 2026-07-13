from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest import mock

from omnicrawl.extensions.plugin_install import (
    doctor,
    install_from_local_package,
    list_plugins,
    verify_lockfile_hash,
    verify_store_integrity,
)
from omnicrawl.extensions.plugin_manager import PluginManager
from omnicrawl.extensions.plugin_models import PluginsConfig


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "npm_plugins" / "sample-observe"


class OfflineInstallE2ETest(unittest.TestCase):
    def test_install_from_local_package_store_and_dispatch(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 sample-observe fixture")

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            store = root / "store"
            registry = root / "plugins.json"
            user_registry = root / "user-registry.json"

            with mock.patch(
                "omnicrawl.extensions.plugin_install.project_registry_path",
                return_value=registry,
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.user_store_root",
                return_value=store,
            ):
                result = install_from_local_package(
                    FIXTURE,
                    scope="project",
                    workspace_root=root,
                    enable=True,
                    yes=True,
                    store_root=store,
                )

                self.assertEqual(result.name, "@omnicrawl-fixture/sample-observe")
                self.assertTrue(result.enabled)
                self.assertTrue(Path(result.store_path).is_dir())
                self.assertTrue((Path(result.store_path) / ".omnicrawl-integrity").is_file())
                self.assertTrue((Path(result.store_path) / "package-lock.json").is_file())
                verify_store_integrity(Path(result.store_path), result.integrity)

                rows = list_plugins(scope="project", workspace_root=root)
                self.assertEqual(len(rows), 1, rows)
                active = rows[0]["active"]
                self.assertIsNotNone(active)
                assert isinstance(active, dict)
                verify_lockfile_hash(Path(active["storePath"]), active["lockfileHash"])

                report = doctor(result.name, workspace_root=root)
                self.assertTrue(report["ok"], report)

            manager = PluginManager(
                workspace_root=root,
                config=PluginsConfig(enabled=True),
                project_registry=registry,
                user_registry=user_registry,
                store_root=store,
            )
            diagnostics = manager.bootstrap()
            self.assertTrue(any("已加载" in item for item in diagnostics), diagnostics)
            outcome = manager.dispatch("turn.end", {"userText": "hi", "assistantText": "ok"})
            self.assertFalse(outcome.denied)
            self.assertGreaterEqual(len(outcome.results), 1)
            manager.close()


if __name__ == "__main__":
    unittest.main()
