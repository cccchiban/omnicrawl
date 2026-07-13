from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from omnicrawl.extensions.plugin_install import register_local_dev_plugin
from omnicrawl.extensions.plugin_manager import PluginManager
from omnicrawl.extensions.plugin_models import PluginsConfig
from omnicrawl.extensions.plugin_protocol import PluginWorkerClient, build_worker_env


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "npm_plugins" / "sample-observe"


class PluginProtocolTest(unittest.TestCase):
    def test_worker_env_strips_secrets(self) -> None:
        env = build_worker_env(
            {
                "PATH": "/usr/bin",
                "OPENAI_API_KEY": "sk-secret",
                "MY_TOKEN": "abc",
                "SAFE_FLAG": "1",
            }
        )
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertNotIn("MY_TOKEN", env)
        self.assertEqual(env.get("SAFE_FLAG"), "1")
        self.assertEqual(env.get("OMNICRAWL_PLUGIN_WORKER"), "1")

    def test_local_plugin_handshake_and_dispatch(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 fixture 插件")
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            register_local_dev_plugin(FIXTURE, scope="project", workspace_root=root, enable=True)
            manager = PluginManager(
                workspace_root=root,
                config=PluginsConfig(enabled=True),
                project_registry=root / ".omnicrawl" / "plugins.json",
                user_registry=root / "user-registry.json",
            )
            diagnostics = manager.bootstrap()
            self.assertTrue(any("已加载 1 个插件" in item for item in diagnostics), diagnostics)
            outcome = manager.dispatch(
                "turn.end",
                {"userText": "hi", "assistantText": "ok"},
                turn_id="t-1",
            )
            self.assertFalse(outcome.denied)
            self.assertEqual(len(outcome.results), 1)
            self.assertEqual(outcome.results[0].annotations.get("seen"), True)
            manager.close()

    def test_worker_client_ping(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 fixture 插件")
        client = PluginWorkerClient(
            plugin_root=FIXTURE,
            plugin_name="@omnicrawl-fixture/sample-observe",
            timeout_ms=5000,
        )
        try:
            client.start()
            initialized = client.initialize(
                {
                    "apiVersion": "1",
                    "omnicrawlVersion": "0.1.0",
                    "permissions": ["hook:turn.end"],
                },
                timeout_ms=5000,
            )
            self.assertIn("handlers", initialized)
            pong = client.ping(timeout_ms=2000)
            self.assertTrue(pong.get("ok"))
            client.shutdown(timeout_ms=2000)
        finally:
            client.close(force=True)


if __name__ == "__main__":
    unittest.main()
