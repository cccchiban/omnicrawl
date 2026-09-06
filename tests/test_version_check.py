from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from omnicrawl.version_check import (
    VersionCheckResult,
    check_latest_version,
    is_newer_version,
    parse_latest_version,
)


RSS_XML = b"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <item><title>0.1.2</title></item>
    <item><title>0.1.1</title></item>
  </channel>
</rss>
"""


class VersionCheckTest(unittest.TestCase):
    def test_should_parse_latest_release_from_pypi_rss(self) -> None:
        self.assertEqual(parse_latest_version(RSS_XML), "0.1.2")

    def test_should_compare_numeric_and_prerelease_versions(self) -> None:
        self.assertTrue(is_newer_version("0.1.10", "0.1.9"))
        self.assertTrue(is_newer_version("0.2.0", "0.2.0rc1"))
        self.assertFalse(is_newer_version("0.2.0rc1", "0.2.0"))
        self.assertFalse(is_newer_version("invalid", "0.1.1"))

    def test_should_reuse_fresh_cache_without_network_request(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cache_path = Path(temp_dir) / "version-check.json"
            cache_path.write_text(
                json.dumps({"checked_at": 1_000.0, "latest_version": "0.1.2"}),
                encoding="utf-8",
            )
            calls: list[tuple[str, float]] = []

            def fetcher(url: str, timeout: float) -> bytes:
                calls.append((url, timeout))
                return RSS_XML

            result = check_latest_version(
                "0.1.1",
                cache_path=cache_path,
                now=1_000.0 + 86_399,
                fetcher=fetcher,
            )

        self.assertEqual(calls, [])
        self.assertEqual(
            result,
            VersionCheckResult(current_version="0.1.1", latest_version="0.1.2"),
        )
        self.assertTrue(result.update_available)

    def test_should_refresh_stale_cache_and_persist_latest_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cache_path = Path(temp_dir) / "version-check.json"
            cache_path.write_text(
                json.dumps({"checked_at": 1.0, "latest_version": "0.1.0"}),
                encoding="utf-8",
            )

            result = check_latest_version(
                "0.1.1",
                cache_path=cache_path,
                now=100_000.0,
                fetcher=lambda _url, _timeout: RSS_XML,
            )
            cached = json.loads(cache_path.read_text(encoding="utf-8"))

        self.assertEqual(result.latest_version, "0.1.2")
        self.assertEqual(cached["latest_version"], "0.1.2")
        self.assertEqual(cached["checked_at"], 100_000.0)

    def test_network_failure_should_silently_keep_current_version(self) -> None:
        def fail(_url: str, _timeout: float) -> bytes:
            raise RuntimeError("offline")

        with tempfile.TemporaryDirectory() as temp_dir:
            result = check_latest_version(
                "0.1.1",
                cache_path=Path(temp_dir) / "version-check.json",
                fetcher=fail,
            )

        self.assertEqual(
            result,
            VersionCheckResult(current_version="0.1.1", latest_version=None),
        )
        self.assertFalse(result.update_available)


if __name__ == "__main__":
    unittest.main()
