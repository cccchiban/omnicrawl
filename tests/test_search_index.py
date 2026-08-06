from __future__ import annotations

import shutil
import sqlite3
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.agent import AgentConfig
from omnicrawl.workspace.search_index import ProjectSearchIndex
from omnicrawl.workspace.tools import WorkspaceToolError, WorkspaceTools
from omnicrawl.workspace.usn import (
    USN_REASON_DATA_OVERWRITE,
    USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
    UsnJournalState,
    UsnRecord,
)


class _StableUsnReader:
    def __init__(self, _root: Path) -> None:
        self.state = UsnJournalState(
            journal_id=7, first_usn=0, next_usn=100, lowest_valid_usn=0,
        )

    def query_state(self) -> UsnJournalState:
        return self.state

    def read_records(self, **_kwargs):
        return iter(())


class _BrokenUsnReader:
    """read_records 始终抛错，模拟 USN 记录损坏/Journal 重建。"""

    def __init__(self, _root: Path) -> None:
        self.state = UsnJournalState(
            journal_id=7, first_usn=0, next_usn=100, lowest_valid_usn=0,
        )

    def query_state(self) -> UsnJournalState:
        return self.state

    def read_records(self, **_kwargs):
        raise UsnJournalError("模拟 USN 记录损坏")


class _RenameThenWriteReader:
    def __init__(
        self,
        *,
        directory_reference: int,
        root_reference: int,
        file_reference: int,
        new_directory: Path,
    ) -> None:
        self.directory_reference = directory_reference
        self.root_reference = root_reference
        self.file_reference = file_reference
        self.new_directory = new_directory

    def read_records(self, **_kwargs):
        yield UsnRecord(
            self.directory_reference,
            self.root_reference,
            101,
            USN_REASON_RENAME_OLD_NAME,
            0x10,
            "old",
        )
        yield UsnRecord(
            self.directory_reference,
            self.root_reference,
            102,
            USN_REASON_RENAME_NEW_NAME,
            0x10,
            "new",
        )
        (self.new_directory / "child.txt").write_text(
            "after rename keyword", encoding="utf-8"
        )
        yield UsnRecord(
            self.file_reference,
            self.directory_reference,
            103,
            USN_REASON_DATA_OVERWRITE,
            0,
            "child.txt",
        )


class WorkspaceSearchContractTest(unittest.TestCase):
    def test_find_files_matches_names_and_paths_without_reading_content(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "nested").mkdir()
            (workspace / "nested" / "AgentCore.py").write_text(
                "content must not be read", encoding="utf-8"
            )
            tools = WorkspaceTools(workspace)
            with patch.object(
                tools, "read_text", side_effect=AssertionError("unexpected read")
            ):
                output = tools.find_files({"pattern": "agent", "kind": "file"})

        self.assertIn("nested", output)
        self.assertIn("AgentCore.py", output)

    def test_search_text_treats_regex_metacharacters_as_literal_keyword(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.txt").write_text("axb\na.*b\n", encoding="utf-8")
            output = WorkspaceTools(workspace).search_text({"pattern": "a.*b"})

        self.assertIn("sample.txt:2: a.*b", output)
        self.assertNotIn("sample.txt:1: axb", output)

    def test_search_text_rejects_user_home_itself(self) -> None:
        tools = WorkspaceTools(Path.home())
        with self.assertRaisesRegex(WorkspaceToolError, "具体项目子目录"):
            tools.search_text({"pattern": "needle", "path": "."})

    def test_search_indexes_are_disabled_by_default(self) -> None:
        config = AgentConfig(
            llm=SimpleNamespace(
                api_key="key", context_window_tokens=128_000, max_output_tokens=8_192,
            )
        )
        self.assertFalse(config.file_name_index_enabled)
        self.assertFalse(config.content_index_enabled)


class ProjectSearchIndexTest(unittest.TestCase):
    def test_background_index_supports_name_content_and_immediate_write_refresh(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            (workspace / "src").mkdir()
            (workspace / "src" / "AgentCore.py").write_text(
                "class AgentCore:\n    keyword = 1\n", encoding="utf-8"
            )
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                self.assertEqual(index.status().file_state, "ready")
                self.assertEqual(index.status().content_state, "ready")
                self.assertIn(
                    "AgentCore.py", tools.find_files({"pattern": "agentcore"})
                )
                self.assertIn("keyword", tools.search_text({"pattern": "keyword"}))

                tools.write_file(
                    {"path": "src/new.txt", "content": "fresh indexed value"}
                )
                self.assertIn("new.txt", tools.find_files({"pattern": "new.txt"}))
                self.assertIn(
                    "fresh indexed value", tools.search_text({"pattern": "fresh"})
                )
            finally:
                index.close()

    def test_should_close_database_connection_when_context_exits(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            index = ProjectSearchIndex(Path(temp_dir), storage_root=Path(cache_dir),)
            index.database_path.parent.mkdir(parents=True, exist_ok=True)
            with index._connect() as connection:  # type: ignore[attr-defined]
                connection.execute("CREATE TABLE probe(value INTEGER)")
            try:
                with self.assertRaises(sqlite3.ProgrammingError):
                    connection.execute("SELECT 1")
            finally:
                connection.close()

    def test_should_rebuild_when_persisted_database_is_corrupt(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            (workspace / "recover.py").write_text("recovered keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            index.database_path.parent.mkdir(parents=True, exist_ok=True)
            index.database_path.write_bytes(b"not-a-sqlite-database")
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                self.assertEqual(index.status().file_state, "ready")
                self.assertIn("recover.py", tools.find_files({"pattern": "recover"}))
                self.assertIn(
                    "recovered keyword",
                    tools.search_text({"pattern": "recovered keyword"}),
                )
            finally:
                index.close()

    def test_should_not_start_background_scan_for_content_only_forbidden_root(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            with patch(
                "omnicrawl.workspace.search_index.is_forbidden_content_search_root",
                return_value=True,
            ):
                index = ProjectSearchIndex(
                    workspace,
                    content_enabled=True,
                    storage_root=Path(cache_dir),
                    usn_reader_factory=_StableUsnReader,
                )
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(1))
                self.assertEqual(index.status().content_state, "unavailable")
                self.assertIsNone(index._thread)
                self.assertFalse(index.database_path.exists())
            finally:
                index.close()

    def test_should_not_return_search_root_itself_when_name_index_is_ready(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            source = workspace / "source"
            source.mkdir()
            (source / "child.py").write_text("pass", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                self.assertEqual(
                    tools.find_files({"pattern": "source", "path": "source"}),
                    "source/child.py",
                )
            finally:
                index.close()

    def test_should_follow_child_updates_after_directory_rename_in_same_usn_batch(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            old_directory = workspace / "old"
            old_directory.mkdir()
            old_file = old_directory / "child.txt"
            old_file.write_text("before", encoding="utf-8")
            root_reference = int(workspace.stat().st_ino)
            directory_reference = int(old_directory.stat().st_ino)
            file_reference = int(old_file.stat().st_ino)
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            self.assertTrue(index.wait_until_ready(5))
            new_directory = workspace / "new"
            old_directory.rename(new_directory)

            reader = _RenameThenWriteReader(
                directory_reference=directory_reference,
                root_reference=root_reference,
                file_reference=file_reference,
                new_directory=new_directory,
            )
            try:
                index._apply_usn_range(  # type: ignore[attr-defined]
                    reader, 100, 104, 7,
                )
                self.assertIn(
                    "after rename keyword",
                    tools.search_text({"pattern": "after rename keyword"}),
                )
            finally:
                index.close()

    def test_should_remove_descendant_entries_when_directory_is_deleted(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            removed = workspace / "removed"
            removed.mkdir()
            (removed / "nested.py").write_text("stale keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                shutil.rmtree(removed)
                index.refresh_path(removed)

                self.assertEqual(
                    tools.find_files({"pattern": "nested.py"}), "未找到匹配结果。",
                )
                self.assertEqual(
                    tools.search_text({"pattern": "stale keyword"}), "未找到匹配结果。",
                )
            finally:
                index.close()

    def test_persisted_snapshot_restores_without_full_rebuild_when_usn_is_contiguous(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            cache = Path(cache_dir)
            (workspace / "cached.py").write_text("persisted keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            first = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=cache,
                usn_reader_factory=_StableUsnReader,
            )
            first.start()
            self.assertTrue(first.wait_until_ready(5))
            first.close()

            second = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=cache,
                usn_reader_factory=_StableUsnReader,
            )
            rebuild = Mock(side_effect=AssertionError("snapshot should be restored"))
            second._rebuild = rebuild  # type: ignore[method-assign]
            tools.search_index = second
            second.start()
            try:
                self.assertTrue(second.wait_until_ready(5))
                self.assertEqual(second.status().file_state, "ready")
                self.assertFalse(rebuild.called)
                self.assertIn("cached.py", tools.find_files({"pattern": "cached"}))
                self.assertIn(
                    "persisted keyword", tools.search_text({"pattern": "persisted"})
                )
            finally:
                second.close()

    def test_usn_replay_failure_degrades_to_rebuild_instead_of_error(self) -> None:
        """快照恢复时 USN 回放失败应降级为全量重建，而不是让索引永久 error。"""

        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            cache = Path(cache_dir)
            (workspace / "recover.py").write_text("replay failure keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            first = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=cache,
                usn_reader_factory=_StableUsnReader,
            )
            first.start()
            self.assertTrue(first.wait_until_ready(5))
            first.close()

            second = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=cache,
                usn_reader_factory=_BrokenUsnReader,
            )
            tools.search_index = second
            second.start()
            try:
                self.assertTrue(second.wait_until_ready(5))
                self.assertEqual(second.status().file_state, "ready")
                self.assertEqual(second.status().content_state, "ready")
                self.assertNotEqual(second.status().file_state, "error")
                self.assertIn("recover.py", tools.find_files({"pattern": "recover"}))
                self.assertIn(
                    "replay failure keyword",
                    tools.search_text({"pattern": "replay failure keyword"}),
                )
            finally:
                second.close()

    def test_fallback_rebuild_reuses_unchanged_file_content(self) -> None:
        """低频核对时内容未变化的文件不应被重新读取，避免每次全量重读。"""

        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            (workspace / "stable.txt").write_text("stable content", encoding="utf-8")
            (workspace / "stable2.txt").write_text("stable content two", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                with patch.object(
                    index, "_read_indexable_text", wraps=index._read_indexable_text
                ) as read_text:
                    # 修改一个文件：核对时只应重读这个变化的文件。
                    (workspace / "stable2.txt").write_text(
                        "changed content", encoding="utf-8"
                    )
                    index._rebuild(None)  # type: ignore[attr-defined]
                    self.assertEqual(
                        read_text.call_count, 1, "只有变化的文件需要重读",
                    )
                    # 第二次核对：文件未变化，不应重读任何内容。
                    index._rebuild(None)  # type: ignore[attr-defined]
                    self.assertEqual(
                        read_text.call_count, 1,
                        "未变化文件不应在核对时被重新读取",
                    )
                    self.assertIn(
                        "changed content", tools.search_text({"pattern": "changed"})
                    )
                    self.assertIn(
                        "stable content", tools.search_text({"pattern": "stable"})
                    )
            finally:
                index.close()

    def test_short_pattern_search_matches_without_trigram(self) -> None:
        """不足 3 字符的关键词走分批全表扫描，结果与直接扫描一致。"""

        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            (workspace / "note.txt").write_text("ab keyword\nkeep it\n", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                self.assertIn("ab", tools.search_text({"pattern": "ab"}))
                self.assertIn("ke", tools.search_text({"pattern": "ke"}))
                self.assertEqual(
                    tools.search_text({"pattern": "xy"}), "未找到匹配结果。",
                )
            finally:
                index.close()

    def test_large_files_keep_name_entries_but_skip_content(self) -> None:
        """超过大小上限的文件只保留文件名条目，内容不进 FTS。"""

        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            big = workspace / "big.bin"
            big.write_bytes(b"x" * (2 * 1024 * 1024 + 1))
            (workspace / "small.txt").write_text("small keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)
            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_skip_path,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                self.assertIn("big.bin", tools.find_files({"pattern": "big.bin"}))
                # 内容不应进入 FTS：直接查询索引，不能命中 big.bin。
                self.assertEqual(
                    index.search_text(
                        "x" * 8, root=workspace, case_sensitive=False, max_results=5,
                    ),
                    [],
                )
            finally:
                index.close()

    def test_index_excluded_directories_keep_tool_results_consistent(self) -> None:
        """索引排除目录：索引快照不收录，但开启索引后工具结果与直接扫描一致。"""

        with tempfile.TemporaryDirectory() as temp_dir, tempfile.TemporaryDirectory() as cache_dir:
            workspace = Path(temp_dir)
            excluded = workspace / "build"
            excluded.mkdir()
            (excluded / "artifact.py").write_text("excluded dir keyword", encoding="utf-8")
            (workspace / "src").mkdir()
            (workspace / "src" / "main.py").write_text("normal keyword", encoding="utf-8")
            tools = WorkspaceTools(workspace)

            # 索引关闭时：直接扫描能命中排除目录。
            self.assertIn("artifact.py", tools.find_files({"pattern": "artifact"}))
            self.assertIn(
                "excluded dir keyword", tools.search_text({"pattern": "excluded"})
            )

            index = ProjectSearchIndex(
                workspace,
                file_name_enabled=True,
                content_enabled=True,
                should_skip=tools.should_index_skip,
                storage_root=Path(cache_dir),
                usn_reader_factory=_StableUsnReader,
            )
            tools.search_index = index
            index.start()
            try:
                self.assertTrue(index.wait_until_ready(5))
                # 索引快照本身不收录排除目录。
                self.assertEqual(
                    index.search_files(
                        "artifact", root=workspace, kind="file",
                        case_sensitive=False, max_results=10,
                    ),
                    [],
                )
                # 工具层合并排除目录的补充扫描，结果与直接扫描一致。
                self.assertIn("artifact.py", tools.find_files({"pattern": "artifact"}))
                self.assertIn(
                    "excluded dir keyword", tools.search_text({"pattern": "excluded"})
                )
                self.assertIn("main.py", tools.find_files({"pattern": "main"}))
            finally:
                index.close()


if __name__ == "__main__":
    unittest.main()
