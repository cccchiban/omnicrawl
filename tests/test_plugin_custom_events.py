from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from omnicrawl.extensions.plugin_install import register_local_dev_plugin
from omnicrawl.extensions.plugin_manager import PluginManager
from omnicrawl.extensions.plugin_models import PluginsConfig, validate_payload_against_schema
from omnicrawl.extensions.plugin_models import PluginManifestError


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "npm_plugins" / "sample-custom"


class CustomEventTest(unittest.TestCase):
    def test_schema_validation(self) -> None:
        schema = {
            "type": "object",
            "required": ["count"],
            "properties": {"count": {"type": "integer"}},
            "additionalProperties": False,
        }
        validate_payload_against_schema({"count": 1}, schema)
        with self.assertRaises(PluginManifestError):
            validate_payload_against_schema({"count": "x"}, schema)
        with self.assertRaises(PluginManifestError):
            validate_payload_against_schema({"count": 1, "extra": 1}, schema)

    def test_custom_emit_roundtrip(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 custom fixture")
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            register_local_dev_plugin(FIXTURE, scope="project", workspace_root=root, enable=True)
            manager = PluginManager(
                workspace_root=root,
                config=PluginsConfig(
                    enabled=True,
                    custom_event_max_depth=4,
                    default_timeout_ms=3000,
                    max_timeout_ms=10000,
                ),
                project_registry=root / ".omnicrawl" / "plugins.json",
                user_registry=root / "user-registry.json",
            )
            diagnostics = manager.bootstrap()
            self.assertTrue(any("已加载 1 个插件" in item for item in diagnostics), diagnostics)
            outcome = manager.dispatch(
                "turn.end",
                {"userText": "hi", "assistantText": "ok"},
                turn_id="t-custom",
            )
            self.assertFalse(outcome.denied)
            self.assertGreaterEqual(len(outcome.results), 1)
            # emit 成功时 annotations 中应带 emit.ok
            emit_ann = None
            for item in outcome.results:
                if item.annotations and "emit" in item.annotations:
                    emit_ann = item.annotations["emit"]
            self.assertIsNotNone(emit_ann, outcome.results)
            self.assertTrue(emit_ann.get("ok"))
            # private 事件：Worker 本地投递至少 1；Host crossPlugin 可为 0。
            total = int(emit_ann.get("delivered", 0)) + int(emit_ann.get("localDelivered", 0))
            self.assertGreaterEqual(total, 1, emit_ann)
            manager.close()

    def test_depth_limit(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 custom fixture")
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            register_local_dev_plugin(FIXTURE, scope="project", workspace_root=root, enable=True)
            manager = PluginManager(
                workspace_root=root,
                config=PluginsConfig(enabled=True, custom_event_max_depth=1),
                project_registry=root / ".omnicrawl" / "plugins.json",
                user_registry=root / "user-registry.json",
            )
            manager.bootstrap()
            with self.assertRaises(Exception):
                manager.emit_custom_event(
                    source_plugin="@omnicrawl-fixture/sample-custom",
                    event_name="plugin.omnicrawl-fixture-sample-custom.batch-flushed",
                    version=1,
                    payload={"count": 1},
                    depth=1,
                )
            manager.close()


if __name__ == "__main__":
    unittest.main()
