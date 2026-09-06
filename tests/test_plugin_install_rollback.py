from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from omnicrawl.extensions.plugin_install import (
    collect_lockfile_integrity_report,
    rollback_plugin,
    verify_store_integrity,
)
from omnicrawl.extensions.plugin_models import PluginRecord, PluginRegistryDocument, PluginVersionRef
from omnicrawl.extensions.plugin_registry import save_registry_document


class InstallRollbackTest(unittest.TestCase):
    def test_collect_lockfile_integrity_report(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            lock = Path(temp_dir) / "package-lock.json"
            lock.write_text(
                json.dumps(
                    {
                        "packages": {
                            "": {"name": "root"},
                            "node_modules/a": {
                                "version": "1.0.0",
                                "resolved": "https://example/a.tgz",
                                "integrity": "sha512-abc",
                            },
                            "node_modules/b": {
                                "version": "2.0.0",
                                "resolved": "https://example/b.tgz",
                            },
                        }
                    }
                ),
                encoding="utf-8",
            )
            report = collect_lockfile_integrity_report(lock)
            self.assertEqual(report["packages"], 2)
            self.assertIn("node_modules/b", report["missingIntegrity"])
            self.assertTrue(report["lockfileHash"])

    def test_verify_store_integrity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            (root / "package.json").write_text("{}", encoding="utf-8")
            (root / ".omnicrawl-integrity").write_text("sha512-abc\n", encoding="utf-8")
            verify_store_integrity(root, "sha512-abc")
            with self.assertRaises(Exception):
                verify_store_integrity(root, "sha512-zzz")

    def test_rollback_keeps_active_when_smoke_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            prev_store = root / "prev"
            curr_store = root / "curr"
            prev_store.mkdir()
            curr_store.mkdir()
            (prev_store / "package.json").write_text("{}", encoding="utf-8")
            (curr_store / "package.json").write_text("{}", encoding="utf-8")
            (prev_store / ".omnicrawl-integrity").write_text("sha512-prev\n", encoding="utf-8")
            (curr_store / ".omnicrawl-integrity").write_text("sha512-curr\n", encoding="utf-8")

            registry = root / "plugins.json"
            doc = PluginRegistryDocument(
                plugins={
                    "@x/y": PluginRecord(
                        name="@x/y",
                        enabled=True,
                        active=PluginVersionRef(
                            version="2.0.0",
                            integrity="sha512-curr",
                            lockfile_hash="abc",
                            source="registry.npmjs.org",
                            store_path=str(curr_store),
                        ),
                        previous=PluginVersionRef(
                            version="1.0.0",
                            integrity="sha512-prev",
                            lockfile_hash="def",
                            source="registry.npmjs.org",
                            store_path=str(prev_store),
                        ),
                    )
                }
            )
            save_registry_document(registry, doc)

            with mock.patch(
                "omnicrawl.extensions.plugin_install._load_scope_registry",
                return_value=(registry, doc),
            ):
                with mock.patch(
                    "omnicrawl.extensions.plugin_install.smoke_test_worker",
                    side_effect=Exception("boom"),
                ):
                    with self.assertRaises(Exception):
                        rollback_plugin("@x/y", scope="user")

            # smoke 失败后不应改写 registry（save 未成功路径下 active 仍是 2.0.0）
            reloaded = json.loads(registry.read_text(encoding="utf-8"))
            active = reloaded["plugins"]["@x/y"]["active"]["version"]
            self.assertEqual(active, "2.0.0")


if __name__ == "__main__":
    unittest.main()
