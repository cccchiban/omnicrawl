from __future__ import annotations

import importlib
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

from omnicrawl.memory import MemoryStore, MemoryWriteRequest
from omnicrawl.state import memory as memory_module
from omnicrawl.state import memory_ranking


class MemoryModuleBoundaryTests(unittest.TestCase):
    """锁定 Memory 纯策略与文件存储边界。"""

    def test_ranking_module_is_real_and_importable(self) -> None:
        package_path = Path(memory_module.__file__).resolve().parent
        module = importlib.import_module("omnicrawl.state.memory_ranking")
        self.assertEqual(module.__name__, "omnicrawl.state.memory_ranking")
        self.assertEqual(Path(module.__file__).resolve(), package_path / "memory_ranking.py")

    def test_classification_and_summary_are_pure(self) -> None:
        self.assertEqual(
            memory_ranking.classify_storage_directory("这个项目的仓库入口在 main.py"),
            "project-context/general",
        )
        self.assertEqual(
            memory_ranking.classify_storage_directory("修复了一个异常 bug"),
            "error-lessons/general",
        )
        long_text = "这是一段没有句号的很长摘要文本" * 10
        summary = memory_ranking.make_summary(long_text)
        self.assertLessEqual(len(summary), 120)
        self.assertTrue(summary.endswith("…"))

    def test_store_still_writes_and_reads_compatible_markdown(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            store = MemoryStore(Path(temp_dir))
            written = store.write(
                [
                    MemoryWriteRequest(
                        content="用户偏好简洁回答。",
                        related_directories=["user-preferences/general"],
                        storage_directory="user-preferences/general",
                    )
                ]
            )
            self.assertEqual(len(written), 1)
            results = store.search("偏好")
            self.assertEqual(results[0].id, written[0].id)
            records = store.read([written[0].id])
            self.assertEqual(records[0].content, "用户偏好简洁回答。")
            index_path = Path(temp_dir) / "index.json"
            self.assertTrue(index_path.is_file())

    def test_should_retry_memory_index_replace_when_windows_temporarily_denies_access(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            store = MemoryStore(Path(temp_dir))
            original_replace = Path.replace
            replace_attempts = 0

            def temporarily_denied(source: Path, destination: Path) -> Path:
                nonlocal replace_attempts
                replace_attempts += 1
                if replace_attempts < 3:
                    error = PermissionError(13, "Access is denied", str(destination))
                    error.winerror = 5
                    raise error
                return original_replace(source, destination)

            with (
                mock.patch.object(Path, "replace", autospec=True, side_effect=temporarily_denied),
                mock.patch("omnicrawl.state.session_locking.sys.platform", "win32"),
                mock.patch("omnicrawl.state.session_locking.time.sleep"),
            ):
                written = store.write(
                    [
                        MemoryWriteRequest(
                            content="短暂占用后仍应写入记忆索引。",
                            related_directories=["project-context/general"],
                            storage_directory="project-context/general",
                        )
                    ]
                )

            index_path = Path(temp_dir) / "index.json"
            self.assertEqual(replace_attempts, 3)
            self.assertEqual(len(written), 1)
            self.assertTrue(index_path.is_file())
            self.assertEqual(len(json.loads(index_path.read_text(encoding="utf-8"))["memories"]), 1)

    def test_search_scoring_prefers_token_hits(self) -> None:
        now = datetime(2026, 7, 12, tzinfo=timezone.utc)
        entry = memory_module.MemoryIndexEntry(
            id="demo",
            path="project-context/general/demo.md",
            storage_directory="project-context/general",
            timestamp=now,
            touch_count=0,
            related_directories=["project-context/general"],
            summary="仓库架构约束",
        )
        score = memory_ranking.score_search_entry(
            entry,
            "仓库架构",
            ["project-context/general"],
            now=now,
        )
        self.assertGreater(score, 5)
