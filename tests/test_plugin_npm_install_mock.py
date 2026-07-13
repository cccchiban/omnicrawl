from __future__ import annotations

import base64
import hashlib
import io
import json
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from omnicrawl.extensions.plugin_install import install_from_npm, verify_lockfile_hash, verify_store_integrity


FIXTURE = Path(__file__).resolve().parent / "fixtures" / "npm_plugins" / "sample-observe"


def _build_tarball_bytes(src: Path) -> bytes:
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for path in src.rglob("*"):
            if path.is_file():
                arcname = Path("package") / path.relative_to(src)
                archive.add(path, arcname=str(arcname).replace("\\", "/"))
    return buffer.getvalue()


class MockNpmInstallTest(unittest.TestCase):
    def test_install_from_npm_with_mocked_registry(self) -> None:
        if not FIXTURE.is_dir():
            self.skipTest("缺少 sample-observe fixture")

        tarball = _build_tarball_bytes(FIXTURE)
        integrity = "sha512-" + base64.b64encode(hashlib.sha512(tarball).digest()).decode("ascii")
        version_meta = {
            "name": "@omnicrawl-fixture/sample-observe",
            "version": "1.0.0",
            "dist": {
                "tarball": "https://registry.example.test/@omnicrawl-fixture/sample-observe/-/sample-observe-1.0.0.tgz",
                "integrity": integrity,
            },
        }

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            store = root / "store"
            registry = root / "plugins.json"

            def fake_urlopen(request, timeout=30):  # noqa: ANN001
                url = request.full_url if hasattr(request, "full_url") else str(request)
                if url.endswith(".tgz"):
                    return mock.Mock(
                        read=mock.Mock(return_value=tarball),
                        __enter__=lambda self: self,
                        __exit__=mock.Mock(return_value=False),
                    )
                # version metadata endpoint
                payload = json.dumps(version_meta).encode("utf-8")
                return mock.Mock(
                    read=mock.Mock(return_value=payload),
                    __enter__=lambda self: self,
                    __exit__=mock.Mock(return_value=False),
                )

            with mock.patch(
                "omnicrawl.extensions.plugin_install.project_registry_path",
                return_value=registry,
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.user_store_root",
                return_value=store,
            ), mock.patch(
                "omnicrawl.extensions.plugin_install.urllib.request.urlopen",
                side_effect=fake_urlopen,
            ):
                result = install_from_npm(
                    "@omnicrawl-fixture/sample-observe@1.0.0",
                    scope="project",
                    workspace_root=root,
                    enable=True,
                    yes=True,
                    allow_network=True,
                )

            self.assertEqual(result.name, "@omnicrawl-fixture/sample-observe")
            self.assertEqual(result.version, "1.0.0")
            self.assertEqual(result.integrity, integrity)
            self.assertTrue(Path(result.store_path).is_dir())
            verify_store_integrity(Path(result.store_path), integrity)
            # registry 应写入 active 指针
            data = json.loads(registry.read_text(encoding="utf-8"))
            active = data["plugins"][result.name]["active"]
            self.assertEqual(active["version"], "1.0.0")
            self.assertEqual(active["integrity"], integrity)
            verify_lockfile_hash(Path(active["storePath"]), active["lockfileHash"])


if __name__ == "__main__":
    unittest.main()
