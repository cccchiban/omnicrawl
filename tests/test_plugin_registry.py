from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.extensions.plugin_models import (
    HandlerRegistration,
    PluginManifest,
    PluginRecord,
    PluginRegistryDocument,
    ResolvedHandler,
)
from omnicrawl.extensions.plugin_registry import (
    add_tombstone,
    atomic_write_json,
    build_execution_plan,
    load_registry_document,
    merge_registry_documents,
    resolve_replacements,
    save_registry_document,
    user_plugins_root,
)


def _manifest(name: str, handlers: list[HandlerRegistration]) -> PluginManifest:
    return PluginManifest(
        name=name,
        version="1.0.0",
        api_version="1",
        entry="index.js",
        permissions=tuple(f"hook:{h.hook}" for h in handlers),
        hooks=tuple(handlers),
        engines_omnicrawl=">=0.1 <0.2",
        engines_node=">=20",
    )


class PluginRegistryTest(unittest.TestCase):
    def test_user_plugins_root_is_under_shared_user_config_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir)
            with patch("omnicrawl.extensions.plugin_registry.Path.home", return_value=home):
                self.assertEqual(user_plugins_root(), home / ".OmniCrawl" / "plugins")

    def test_atomic_write_and_load(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "registry.json"
            doc = PluginRegistryDocument(
                plugins={
                    "@a/b": PluginRecord(name="@a/b", enabled=True),
                }
            )
            save_registry_document(path, doc)
            loaded = load_registry_document(path)
            self.assertIn("@a/b", loaded.plugins)

    def test_project_overrides_user(self) -> None:
        user = PluginRegistryDocument(
            plugins={"p": PluginRecord(name="p", enabled=False, local_path="/user")}
        )
        project = PluginRegistryDocument(
            plugins={"p": PluginRecord(name="p", enabled=True, local_path="/project")}
        )
        merged = merge_registry_documents(user_doc=user, project_doc=project)
        self.assertTrue(merged.plugins["p"].enabled)
        self.assertEqual(merged.plugins["p"].local_path, "/project")

    def test_tombstone_and_sort(self) -> None:
        handlers = [
            HandlerRegistration(id="a", hook="turn.end", mode="notify", priority=1),
            HandlerRegistration(id="b", hook="turn.end", mode="notify", priority=10),
        ]
        manifests = {
            "@z/p": (
                _manifest("@z/p", handlers),
                "user",
                PluginRecord(name="@z/p", enabled=True, dev_mode=True, local_path="/x"),
            ),
            "@a/p": (
                _manifest(
                    "@a/p",
                    [HandlerRegistration(id="c", hook="turn.end", mode="notify", priority=10)],
                ),
                "project",
                PluginRecord(name="@a/p", enabled=True, dev_mode=True, local_path="/y"),
            ),
        }
        plan = build_execution_plan(
            manifests=manifests,
            disabled_handlers=["@z/p/a"],
            max_timeout_ms=5000,
        )
        keys = [item.key for item in plan]
        self.assertNotIn("@z/p/a", keys)
        # priority 相同：project 先于 user；同 scope 按包名升序。
        self.assertEqual(keys[0], "@a/p/c")

    def test_replace_conflict(self) -> None:
        handlers = [
            ResolvedHandler(
                key="@a/p/new",
                plugin_name="@a/p",
                plugin_version="1",
                handler_id="new",
                hook="tool.execute.after",
                mode="transform",
                priority=1,
                scope="user",
                timeout_ms=1000,
                replaces=("@old/p/old",),
            ),
            ResolvedHandler(
                key="@b/p/new2",
                plugin_name="@b/p",
                plugin_version="1",
                handler_id="new2",
                hook="tool.execute.after",
                mode="transform",
                priority=1,
                scope="user",
                timeout_ms=1000,
                replaces=("@old/p/old",),
            ),
            ResolvedHandler(
                key="@old/p/old",
                plugin_name="@old/p",
                plugin_version="1",
                handler_id="old",
                hook="tool.execute.after",
                mode="transform",
                priority=1,
                scope="user",
                timeout_ms=1000,
            ),
        ]
        resolved = resolve_replacements(handlers)
        keys = {item.key for item in resolved}
        # 冲突替换方被禁用，目标仍保留。
        self.assertIn("@old/p/old", keys)
        self.assertNotIn("@a/p/new", keys)
        self.assertNotIn("@b/p/new2", keys)

    def test_add_tombstone_rejects_sealed(self) -> None:
        doc = PluginRegistryDocument()
        with self.assertRaises(Exception):
            add_tombstone(doc, "core/approval")


if __name__ == "__main__":
    unittest.main()
