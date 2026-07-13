from __future__ import annotations

import unittest

from omnicrawl.extensions.plugin_models import (
    PluginManifestError,
    apply_json_patch,
    parse_plugin_manifest,
    validate_json_patch,
)


VALID_PACKAGE = {
    "name": "@example/omnicrawl-redactor",
    "version": "1.2.3",
    "type": "module",
    "main": "dist/plugin.js",
    "omnicrawl": {
        "apiVersion": "1",
        "engines": {"omnicrawl": ">=0.1 <0.2", "node": ">=20"},
        "entry": "dist/plugin.js",
        "timeoutMs": 2000,
        "permissions": ["hook:tool.execute.after"],
        "hooks": [
            {
                "id": "redact-tool-output",
                "hook": "tool.execute.after",
                "mode": "transform",
                "priority": 100,
            }
        ],
    },
}


class PluginManifestTest(unittest.TestCase):
    def test_parse_valid_manifest(self) -> None:
        manifest = parse_plugin_manifest(VALID_PACKAGE)
        self.assertEqual(manifest.name, "@example/omnicrawl-redactor")
        self.assertEqual(manifest.hooks[0].id, "redact-tool-output")

    def test_reject_unknown_api_version(self) -> None:
        package = {
            **VALID_PACKAGE,
            "omnicrawl": {**VALID_PACKAGE["omnicrawl"], "apiVersion": "2"},
        }
        with self.assertRaises(PluginManifestError):
            parse_plugin_manifest(package)

    def test_reject_entry_escape(self) -> None:
        package = {
            **VALID_PACKAGE,
            "omnicrawl": {**VALID_PACKAGE["omnicrawl"], "entry": "../../payload.js"},
        }
        with self.assertRaises(PluginManifestError):
            parse_plugin_manifest(package)

    def test_reject_missing_permission(self) -> None:
        package = {
            **VALID_PACKAGE,
            "omnicrawl": {
                **VALID_PACKAGE["omnicrawl"],
                "permissions": ["hook:turn.start"],
            },
        }
        with self.assertRaises(PluginManifestError):
            parse_plugin_manifest(package)

    def test_reject_sealed_replace(self) -> None:
        package = {
            **VALID_PACKAGE,
            "omnicrawl": {
                **VALID_PACKAGE["omnicrawl"],
                "hooks": [
                    {
                        "id": "x",
                        "hook": "tool.execute.after",
                        "mode": "transform",
                        "replaces": ["core/approval"],
                    }
                ],
            },
        }
        with self.assertRaises(PluginManifestError):
            parse_plugin_manifest(package)

    def test_json_patch_allowlist_and_apply(self) -> None:
        patch = validate_json_patch(
            [{"op": "replace", "path": "/payload/displayText", "value": "redacted"}],
            hook="tool.execute.after",
        )
        updated = apply_json_patch(
            {"payload": {"displayText": "secret", "annotations": {}}},
            patch,
        )
        self.assertEqual(updated["payload"]["displayText"], "redacted")

    def test_json_patch_rejects_unknown_path(self) -> None:
        with self.assertRaises(PluginManifestError):
            validate_json_patch(
                [{"op": "replace", "path": "/payload/secrets", "value": "x"}],
                hook="tool.execute.after",
            )


if __name__ == "__main__":
    unittest.main()
