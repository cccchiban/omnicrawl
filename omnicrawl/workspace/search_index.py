"""PRJ 范围内的后台文件名与内容索引服务。"""

from __future__ import annotations

import hashlib
import os
import sqlite3
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Iterator

from ..config.runtime import user_config_dir
from .usn import (
    USN_REASON_BASIC_INFO_CHANGE,
    USN_REASON_DATA_EXTEND,
    USN_REASON_DATA_OVERWRITE,
    USN_REASON_DATA_TRUNCATION,
    USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE,
    USN_REASON_RENAME_NEW_NAME,
    USN_REASON_RENAME_OLD_NAME,
    UsnJournalError,
    UsnJournalReader,
)

INDEX_SCHEMA_VERSION = 1
# 无 USN 时的低频核对周期：全量目录核对本身不读文件内容，
# 但内容索引需要重读变化的文件，周期不宜过短。
FALLBACK_RESCAN_INTERVAL_SECONDS = 300.0
USN_POLL_INTERVAL_SECONDS = 1.0
# 超过该大小的文件不建立内容索引（仍保留文件名条目），避免把大二进制/
# 大文本整体读入内存并让 trigram 索引无意义膨胀。
MAX_CONTENT_INDEX_BYTES = 2 * 1024 * 1024
# 短关键词（不足 3 字符无法生成 trigram）按 rowid 分批拉取内容，
# 控制单次搜索的内存峰值，而不是一次性把整个 FTS 表读入 Python。
SHORT_PATTERN_BATCH_SIZE = 500


@dataclass(frozen=True)
class SearchIndexStatus:
    file_state: str = "disabled"
    file_processed: int = 0
    file_total: int = 0
    content_state: str = "disabled"
    content_processed: int = 0
    content_total: int = 0
    detail: str = ""

    @property
    def active(self) -> bool:
        return self.file_state in {"loading", "building"} or self.content_state in {
            "loading",
            "building",
        }


@dataclass(frozen=True)
class _Entry:
    path: str
    is_dir: bool
    mtime_ns: int
    size: int
    file_reference: int
    parent_reference: int


class ProjectSearchIndex:
    """维护持久化快照，并把文件名快照加载为进程内查询表。

    初始构建和外部变更同步均在守护线程执行。Windows NTFS 优先读取 USN
    Journal；其他文件系统或权限不足时使用低频完整核对。Agent 自身写入通过
    :meth:`refresh_path` 立即提交，不需要等待后台轮询。
    """

    def __init__(
        self,
        workspace_root: Path,
        *,
        file_name_enabled: bool = False,
        content_enabled: bool = False,
        should_skip: Callable[[Path], bool] | None = None,
        storage_root: Path | None = None,
        usn_reader_factory: Callable[[Path], UsnJournalReader] = UsnJournalReader,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve()
        self.file_name_enabled = bool(file_name_enabled)
        self.content_enabled = bool(content_enabled)
        self._content_root_allowed = not is_forbidden_content_search_root(
            self.workspace_root
        )
        self._should_skip = should_skip or (lambda _path: False)
        cache_root = storage_root or user_config_dir() / "search-index"
        workspace_key = hashlib.sha256(
            os.path.normcase(str(self.workspace_root)).encode("utf-8")
        ).hexdigest()[:24]
        self.database_path = Path(cache_root) / f"{workspace_key}.sqlite3"
        self._usn_reader_factory = usn_reader_factory
        self._lock = threading.RLock()
        self._status = SearchIndexStatus(
            file_state="loading" if self.file_name_enabled else "disabled",
            content_state=(
                "loading"
                if self.content_enabled and self._content_root_allowed
                else "unavailable"
                if self.content_enabled
                else "disabled"
            ),
            detail=(
                "用户主目录或文件系统根目录不建立项目级内容索引。"
                if self.content_enabled and not self._content_root_allowed
                else ""
            ),
        )
        self._name_entries: tuple[_Entry, ...] = ()
        self._dirty_paths: set[Path] = set()
        self._stop_event = threading.Event()
        self._ready_event = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        effective_content_enabled = self.content_enabled and self._content_root_allowed
        if not (self.file_name_enabled or effective_content_enabled):
            self._ready_event.set()
            return
        with self._lock:
            if self._thread is not None and self._thread.is_alive():
                return
            self._thread = threading.Thread(
                target=self._run,
                name=f"omnicrawl-search-index-{self.database_path.stem}",
                daemon=True,
            )
            self._thread.start()

    def close(self) -> None:
        self._stop_event.set()
        thread = self._thread
        if thread is not None and thread is not threading.current_thread():
            # 给构建/增量应用留足退出时间，避免切换开关后旧线程仍与新实例
            # 并发写同一个 sqlite 文件（WAL 下会表现为反复等待写锁）。
            thread.join(timeout=10.0)

    def wait_until_ready(self, timeout: float = 10.0) -> bool:
        return self._ready_event.wait(timeout=max(0.0, timeout))

    def status(self) -> SearchIndexStatus:
        with self._lock:
            return self._status

    @property
    def file_name_ready(self) -> bool:
        return self.file_name_enabled and self.status().file_state == "ready"

    @property
    def content_ready(self) -> bool:
        return (
            self.content_enabled
            and self._content_root_allowed
            and self.status().content_state == "ready"
        )

    def search_files(
        self,
        pattern: str,
        *,
        root: Path,
        kind: str,
        case_sensitive: bool,
        max_results: int,
    ) -> list[tuple[str, bool]] | None:
        """查询内存名称索引；尚未就绪时返回 None 让调用方直接扫描。"""

        if not self.file_name_ready:
            return None
        relative_root = self._relative(root)
        needle = pattern if case_sensitive else pattern.casefold()
        results: list[tuple[str, bool]] = []
        with self._lock:
            entries = self._name_entries
        for entry in entries:
            if not _is_relative_entry(entry.path, relative_root):
                continue
            if root.is_dir() and relative_root != "." and entry.path == relative_root:
                continue
            if kind == "file" and entry.is_dir:
                continue
            if kind == "directory" and not entry.is_dir:
                continue
            candidate = entry.path if case_sensitive else entry.path.casefold()
            name = Path(entry.path).name
            candidate_name = name if case_sensitive else name.casefold()
            if needle not in candidate and needle not in candidate_name:
                continue
            results.append((entry.path, entry.is_dir))
            if len(results) >= max_results:
                break
        return results

    def search_text(
        self, pattern: str, *, root: Path, case_sensitive: bool, max_results: int,
    ) -> list[tuple[str, int, str]] | None:
        """用 trigram FTS 找候选文件，再逐行执行精确子串复核。

        短模式（不足 3 字符无法生成 trigram）按 rowid 分批拉取内容并边读边
        匹配，避免把整个 FTS 内容一次性读入 Python 内存。
        """

        if not self.content_ready:
            return None
        relative_root = self._relative(root)
        needle = pattern if case_sensitive else pattern.casefold()
        results: list[tuple[str, int, str]] = []
        try:
            with self._connect() as connection:
                if len(pattern) >= 3:
                    quoted = '"' + pattern.replace('"', '""') + '"'
                    rows = connection.execute(
                        "SELECT path, content FROM content_fts "
                        "WHERE content_fts MATCH ?",
                        (quoted,),
                    ).fetchall()
                    rows.sort(key=lambda row: str(row[0]).casefold())
                    for relative_path, content in rows:
                        self._append_line_matches(
                            results,
                            relative_path,
                            content,
                            relative_root=relative_root,
                            root=root,
                            needle=needle,
                            case_sensitive=case_sensitive,
                            limit=max_results,
                        )
                else:
                    last_rowid = 0
                    while True:
                        batch = connection.execute(
                            "SELECT rowid, path, content FROM content_fts "
                            "WHERE rowid > ? ORDER BY rowid LIMIT ?",
                            (last_rowid, SHORT_PATTERN_BATCH_SIZE),
                        ).fetchall()
                        if not batch:
                            break
                        for _rowid, relative_path, content in batch:
                            self._append_line_matches(
                                results,
                                relative_path,
                                content,
                                relative_root=relative_root,
                                root=root,
                                needle=needle,
                                case_sensitive=case_sensitive,
                                limit=None,
                            )
                        last_rowid = batch[-1][0]
        except sqlite3.Error:
            return None
        # 短模式按 rowid 顺序收集，最后统一按路径排序后截断，
        # 与长模式（排序遍历 + 提前截断）的结果语义一致。
        results.sort(key=lambda match: str(match[0]).casefold())
        return results[:max_results]

    @staticmethod
    def _append_line_matches(
        results: list[tuple[str, int, str]],
        relative_path: object,
        content: object,
        *,
        relative_root: str,
        root: Path,
        needle: str,
        case_sensitive: bool,
        limit: int | None,
    ) -> None:
        """在单文件内容上逐行匹配；limit 非空时满额即停止收集。"""

        relative_path = str(relative_path)
        if not _is_relative_entry(relative_path, relative_root):
            return
        if root.is_file() and relative_path != relative_root:
            return
        for line_number, line in enumerate(str(content).splitlines(), start=1):
            candidate = line if case_sensitive else line.casefold()
            if needle in candidate:
                results.append((relative_path, line_number, line))
                if limit is not None and len(results) >= limit:
                    return

    def refresh_path(self, path: Path) -> None:
        """同步 Agent 自身的写入；构建未完成时登记为构建后的补偿更新。"""

        resolved = Path(path).resolve()
        if not _is_relative_to(resolved, self.workspace_root):
            return
        with self._lock:
            if not self._ready_event.is_set():
                self._dirty_paths.add(resolved)
                return
        try:
            self._refresh_absolute_path(resolved)
            self._reload_name_entries_if_enabled()
        except (OSError, sqlite3.Error):
            with self._lock:
                self._dirty_paths.add(resolved)

    def _run(self) -> None:
        reader: UsnJournalReader | None = None
        try:
            self.database_path.parent.mkdir(parents=True, exist_ok=True)
            try:
                self._initialize_database()
            except sqlite3.DatabaseError:
                self._discard_corrupt_database()
                self._initialize_database()
            try:
                reader = self._usn_reader_factory(self.workspace_root)
            except (OSError, UsnJournalError):
                reader = None

            # 快照恢复（含 USN 增量回放）失败不能杀死索引线程：
            # 降级为全量重建，一次记录损坏不应让索引永久停留在 error。
            try:
                restored = self._restore_snapshot(reader)
            except (OSError, sqlite3.Error, UsnJournalError):
                restored = False
                reader = None
            if not restored:
                try:
                    self._rebuild(reader)
                except (OSError, sqlite3.Error, UsnJournalError) as exc:
                    # 只有数据库级故障（磁盘损坏、文件系统异常）才进入 error；
                    # 普通 USN 问题已在上面降级为无 USN 的完整核对。
                    self._set_error(f"索引重建失败：{exc}")
                    self._ready_event.set()
                    return
            self._drain_dirty_paths()
            self._mark_ready()
            self._ready_event.set()

            last_fallback_scan = time.monotonic()
            while not self._stop_event.wait(
                USN_POLL_INTERVAL_SECONDS if reader is not None else 1.0
            ):
                if reader is not None:
                    try:
                        self._apply_usn_updates(reader)
                    except (OSError, sqlite3.Error, UsnJournalError):
                        # 增量失效：立即降级为低频核对（last_fallback_scan=0
                        # 让下一轮循环直接执行一次重建），而不是等满一个周期。
                        reader = None
                        last_fallback_scan = 0.0
                elif (
                    time.monotonic() - last_fallback_scan
                    >= FALLBACK_RESCAN_INTERVAL_SECONDS
                ):
                    try:
                        self._rebuild(None)
                    except (OSError, sqlite3.Error, UsnJournalError) as exc:
                        # 核对失败只记录状态并继续等下一周期，线程不退出。
                        self._set_error(f"索引核对失败：{exc}")
                    else:
                        self._mark_ready()
                    last_fallback_scan = time.monotonic()
                self._drain_dirty_paths()
        except Exception as exc:  # noqa: BLE001 - 后台索引必须隔离故障
            self._set_error(str(exc))
            self._ready_event.set()

    def _restore_snapshot(self, reader: UsnJournalReader | None) -> bool:
        with self._connect() as connection:
            meta = dict(connection.execute("SELECT key, value FROM meta"))
            snapshot_complete = meta.get("snapshot_complete") == "1"
            schema_matches = meta.get("schema_version") == str(INDEX_SCHEMA_VERSION)
            root_matches = meta.get("workspace_root") == str(self.workspace_root)
            content_complete = meta.get("content_complete") == "1"
        if not (snapshot_complete and schema_matches and root_matches):
            return False
        if self.content_enabled and self._content_root_allowed and not content_complete:
            return False
        if reader is None:
            return False

        try:
            state = reader.query_state()
            journal_id = int(meta.get("usn_journal_id", "-1"))
            cursor = int(meta.get("usn_cursor", "-1"))
        except (TypeError, ValueError, UsnJournalError):
            return False
        if (
            journal_id != state.journal_id
            or cursor < state.lowest_valid_usn
            or cursor > state.next_usn
        ):
            return False

        self._set_status(
            file_state="loading", content_state=self._content_loading_state()
        )
        self._load_name_entries()
        self._apply_usn_range(reader, cursor, state.next_usn, state.journal_id)
        return True

    def _rebuild(self, reader: UsnJournalReader | None) -> None:
        start_state = None
        if reader is not None:
            try:
                start_state = reader.query_state()
            except UsnJournalError:
                reader = None

        file_state = "building" if self.file_name_enabled else "disabled"
        self._set_status(
            file_state=file_state,
            file_processed=0,
            file_total=0,
            content_state=self._content_building_state(),
            content_processed=0,
            content_total=0,
        )
        entries = self._scan_entries()
        if self._stop_event.is_set():
            return
        files = [entry for entry in entries if not entry.is_dir]
        self._set_status(
            file_processed=len(entries),
            file_total=len(entries),
            content_total=len(files),
        )

        content_enabled = self.content_enabled and self._content_root_allowed
        with self._connect() as connection:
            connection.execute("BEGIN IMMEDIATE")
            # 记录旧快照中“路径 → (mtime, size)”映射，用于判断内容是否需要重读；
            # 未变化的文件保留现有 content_fts 行，使低频核对只重读真正变化的文件，
            # 而不是把整个项目的内容全部重读一遍重建 trigram。
            previous_meta = {
                str(path): (int(mtime_ns), int(size))
                for path, mtime_ns, size in connection.execute(
                    "SELECT path, mtime_ns, size FROM entries WHERE is_dir = 0"
                )
            }
            previous_content_paths = {
                str(path) for path, in connection.execute("SELECT path FROM content_fts")
            }
            disk_paths = {entry.path for entry in files}

            connection.execute("DELETE FROM entries")
            connection.executemany(
                "INSERT INTO entries(path, is_dir, mtime_ns, size, file_reference, parent_reference) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                [
                    (
                        entry.path,
                        int(entry.is_dir),
                        entry.mtime_ns,
                        entry.size,
                        str(entry.file_reference),
                        str(entry.parent_reference),
                    )
                    for entry in entries
                ],
            )
            content_processed = 0
            failed: list[str] = []
            if content_enabled:
                # 删除磁盘上已不存在的文件残留内容；随后逐文件核对。
                connection.executemany(
                    "DELETE FROM content_fts WHERE path = ?",
                    [(path,) for path in sorted(previous_content_paths - disk_paths)],
                )
                for entry in files:
                    if self._stop_event.is_set():
                        connection.rollback()
                        return
                    if (
                        entry.path in previous_content_paths
                        and previous_meta.get(entry.path)
                        == (entry.mtime_ns, entry.size)
                    ):
                        # 内容未变化：保留旧 FTS 行，跳过重读。
                        content_processed += 1
                        continue
                    content = self._read_indexable_text(
                        self.workspace_root / entry.path
                    )
                    if content is not None:
                        connection.execute(
                            "DELETE FROM content_fts WHERE path = ?", (entry.path,),
                        )
                        connection.execute(
                            "INSERT INTO content_fts(path, content) VALUES (?, ?)",
                            (entry.path, content),
                        )
                    else:
                        # 内容读不到（二进制/过大/瞬态锁）：移除旧行，登记重试。
                        connection.execute(
                            "DELETE FROM content_fts WHERE path = ?", (entry.path,),
                        )
                        failed.append(entry.path)
                    content_processed += 1
                    if content_processed % 25 == 0 or content_processed == len(files):
                        self._set_status(content_processed=content_processed)
            else:
                connection.execute("DELETE FROM content_fts")
            root_stat = self.workspace_root.stat()
            meta_values = {
                "schema_version": str(INDEX_SCHEMA_VERSION),
                "workspace_root": str(self.workspace_root),
                "snapshot_complete": "1",
                "content_complete": "1" if content_enabled else "0",
                "root_file_reference": str(int(root_stat.st_ino)),
            }
            connection.executemany(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)",
                meta_values.items(),
            )
            connection.commit()

        self._load_name_entries()
        # 内容读取失败的文件登记为补偿更新，由 _drain_dirty_paths 在后续轮询
        # 中重试，而不是静默丢失内容条目。
        if failed:
            with self._lock:
                self._dirty_paths.update(self.workspace_root / path for path in failed)
        if reader is not None and start_state is not None:
            try:
                end_state = reader.query_state()
                if end_state.journal_id != start_state.journal_id:
                    raise UsnJournalError("索引构建期间 USN Journal 已重建。")
                self._apply_usn_range(
                    reader, start_state.next_usn, end_state.next_usn, end_state.journal_id,
                )
            except (OSError, sqlite3.Error, UsnJournalError):
                # 回放失败不清空已建好的快照，只丢弃游标，
                # 让下次启动回到无 USN 的完整核对路径。
                self._save_usn_cursor(None, None)
        else:
            self._save_usn_cursor(None, None)

    def _scan_entries(self) -> list[_Entry]:
        entries: list[_Entry] = []
        processed = 0
        for dirpath, dirnames, filenames in os.walk(self.workspace_root):
            if self._stop_event.is_set():
                break
            current = Path(dirpath)
            kept_directories: list[str] = []
            for name in sorted(dirnames, key=str.casefold):
                path = current / name
                if path.is_symlink() or self._skip(path):
                    continue
                entry = self._entry_for_path(path, is_dir=True)
                if entry is not None:
                    entries.append(entry)
                    kept_directories.append(name)
                    processed += 1
            dirnames[:] = kept_directories
            for name in sorted(filenames, key=str.casefold):
                path = current / name
                if self._skip(path):
                    continue
                entry = self._entry_for_path(path, is_dir=False)
                if entry is not None:
                    entries.append(entry)
                    processed += 1
            if processed:
                self._set_status(file_processed=processed)
        entries.sort(key=lambda entry: entry.path.casefold())
        return entries

    def _entry_for_path(
        self, path: Path, *, is_dir: bool | None = None
    ) -> _Entry | None:
        try:
            resolved = path.resolve()
            if not _is_relative_to(resolved, self.workspace_root):
                return None
            stat = path.stat()
            parent_stat = path.parent.stat()
            return _Entry(
                path=self._relative(path),
                is_dir=path.is_dir() if is_dir is None else is_dir,
                mtime_ns=int(stat.st_mtime_ns),
                size=0
                if (path.is_dir() if is_dir is None else is_dir)
                else int(stat.st_size),
                file_reference=int(stat.st_ino),
                parent_reference=int(parent_stat.st_ino),
            )
        except OSError:
            return None

    def _apply_usn_updates(self, reader: UsnJournalReader) -> None:
        with self._connect() as connection:
            meta = dict(connection.execute("SELECT key, value FROM meta"))
        state = reader.query_state()
        try:
            journal_id = int(meta.get("usn_journal_id", "-1"))
            cursor = int(meta.get("usn_cursor", "-1"))
        except ValueError as exc:
            raise UsnJournalError("持久化 USN 游标无效。") from exc
        if journal_id != state.journal_id or cursor < state.lowest_valid_usn:
            raise UsnJournalError("USN Journal 已重建或索引游标已过期。")
        self._apply_usn_range(reader, cursor, state.next_usn, state.journal_id)

    def _apply_usn_range(
        self, reader: UsnJournalReader, start_usn: int, stop_usn: int, journal_id: int,
    ) -> None:
        if start_usn >= stop_usn:
            self._save_usn_cursor(journal_id, stop_usn)
            return
        references = self._reference_paths()
        pending_renames: dict[int, str] = {}
        changed = False
        for record in reader.read_records(
            start_usn=start_usn, journal_id=journal_id, stop_usn=stop_usn,
        ):
            if self._stop_event.is_set():
                return
            old_path = references.get(record.file_reference)
            if record.reason & USN_REASON_RENAME_OLD_NAME and old_path:
                pending_renames[record.file_reference] = old_path
                continue
            parent_path = references.get(record.parent_reference)
            new_path = (
                _join_relative(parent_path, record.name)
                if parent_path is not None
                else None
            )
            if record.reason & USN_REASON_RENAME_NEW_NAME:
                previous = pending_renames.pop(record.file_reference, old_path)
                if previous:
                    self._delete_relative_path(previous, include_descendants=True)
                if new_path is not None:
                    absolute = self.workspace_root / new_path
                    self._refresh_absolute_path(absolute, recursive=record.is_directory)
                    if record.is_directory:
                        references = self._reference_paths()
                    else:
                        references[record.file_reference] = new_path
                else:
                    references.pop(record.file_reference, None)
                changed = True
                continue
            if record.reason & USN_REASON_FILE_DELETE:
                if old_path:
                    self._delete_relative_path(
                        old_path, include_descendants=record.is_directory
                    )
                    references.pop(record.file_reference, None)
                    if record.is_directory:
                        references = self._reference_paths()
                    changed = True
                continue
            if record.reason & (
                USN_REASON_FILE_CREATE
                | USN_REASON_DATA_OVERWRITE
                | USN_REASON_DATA_EXTEND
                | USN_REASON_DATA_TRUNCATION
                | USN_REASON_BASIC_INFO_CHANGE
            ):
                relative = old_path or new_path
                if relative is not None:
                    self._refresh_absolute_path(
                        self.workspace_root / relative,
                        recursive=record.is_directory
                        and bool(record.reason & USN_REASON_FILE_CREATE),
                    )
                    if record.is_directory:
                        references = self._reference_paths()
                    else:
                        references[record.file_reference] = relative
                    changed = True
        for file_reference, previous in pending_renames.items():
            self._delete_relative_path(previous, include_descendants=True)
            references.pop(file_reference, None)
            changed = True
        self._save_usn_cursor(journal_id, stop_usn)
        if changed:
            self._reload_name_entries_if_enabled()

    def _reference_paths(self) -> dict[int, str]:
        with self._connect() as connection:
            rows = connection.execute(
                "SELECT file_reference, path FROM entries"
            ).fetchall()
            meta = dict(connection.execute("SELECT key, value FROM meta"))
        references = {int(reference): str(path) for reference, path in rows}
        root_reference = meta.get("root_file_reference")
        if root_reference is not None:
            references[int(root_reference)] = "."
        return references

    def _refresh_absolute_path(self, path: Path, *, recursive: bool = False) -> bool:
        """刷新单个路径；返回 False 表示内容条目读取失败（需要后续重试）。"""

        if not _is_relative_to(path.resolve(strict=False), self.workspace_root):
            return True
        relative = self._relative(path)
        if not path.exists() or self._skip(path):
            self._delete_relative_path(relative, include_descendants=True)
            return True
        if recursive and path.is_dir():
            self._delete_relative_path(relative, include_descendants=True)
            ok = True
            for entry in self._scan_subtree(path):
                if not self._upsert_entry(entry):
                    ok = False
            return ok
        entry = self._entry_for_path(path)
        if entry is None:
            return True
        return self._upsert_entry(entry)

    def _scan_subtree(self, root: Path) -> Iterable[_Entry]:
        root_entry = self._entry_for_path(root)
        if root_entry is not None and root != self.workspace_root:
            yield root_entry
        if root.is_file():
            return
        for dirpath, dirnames, filenames in os.walk(root):
            current = Path(dirpath)
            kept: list[str] = []
            for name in sorted(dirnames, key=str.casefold):
                path = current / name
                if path.is_symlink() or self._skip(path):
                    continue
                kept.append(name)
                entry = self._entry_for_path(path, is_dir=True)
                if entry is not None:
                    yield entry
            dirnames[:] = kept
            for name in sorted(filenames, key=str.casefold):
                path = current / name
                if self._skip(path):
                    continue
                entry = self._entry_for_path(path, is_dir=False)
                if entry is not None:
                    yield entry

    def _upsert_entry(self, entry: _Entry) -> bool:
        """写入文件名条目并尝试刷新内容条目；返回内容是否成功写入。"""

        content_ok = True
        with self._connect() as connection:
            connection.execute(
                "INSERT OR REPLACE INTO entries"
                "(path, is_dir, mtime_ns, size, file_reference, parent_reference) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (
                    entry.path,
                    int(entry.is_dir),
                    entry.mtime_ns,
                    entry.size,
                    str(entry.file_reference),
                    str(entry.parent_reference),
                ),
            )
            connection.execute("DELETE FROM content_fts WHERE path = ?", (entry.path,))
            if not entry.is_dir and self.content_enabled and self._content_root_allowed:
                content = self._read_indexable_text(self.workspace_root / entry.path)
                if content is not None:
                    connection.execute(
                        "INSERT INTO content_fts(path, content) VALUES (?, ?)",
                        (entry.path, content),
                    )
                else:
                    content_ok = False
            connection.commit()
        return content_ok

    def _delete_relative_path(
        self, relative: str, *, include_descendants: bool
    ) -> None:
        with self._connect() as connection:
            if include_descendants:
                prefix = _escape_like(relative.rstrip("/") + "/") + "%"
                paths = [
                    row[0]
                    for row in connection.execute(
                        "SELECT path FROM entries WHERE path = ? OR path LIKE ? ESCAPE '\\'",
                        (relative, prefix),
                    )
                ]
            else:
                paths = [relative]
            connection.executemany(
                "DELETE FROM content_fts WHERE path = ?", [(path,) for path in paths],
            )
            connection.executemany(
                "DELETE FROM entries WHERE path = ?", [(path,) for path in paths],
            )
            connection.commit()

    def _drain_dirty_paths(self) -> None:
        with self._lock:
            paths = tuple(self._dirty_paths)
            self._dirty_paths.clear()
        pending: list[Path] = []
        for path in paths:
            if self._stop_event.is_set():
                return
            try:
                # 内容读取失败（文件被独占锁定、瞬态 IO 错误）返回 False，
                # 登记到下一轮轮询重试，而不是静默丢失内容条目。
                if not self._refresh_absolute_path(path):
                    pending.append(path)
            except (OSError, sqlite3.Error):
                pending.append(path)
        if pending:
            with self._lock:
                self._dirty_paths.update(pending)
        if paths:
            self._reload_name_entries_if_enabled()

    def _load_name_entries(self) -> None:
        if not self.file_name_enabled:
            return
        with self._connect() as connection:
            rows = connection.execute(
                "SELECT path, is_dir, mtime_ns, size, file_reference, parent_reference "
                "FROM entries ORDER BY path COLLATE NOCASE"
            ).fetchall()
        entries = tuple(
            _Entry(
                path=str(path),
                is_dir=bool(is_dir),
                mtime_ns=int(mtime_ns),
                size=int(size),
                file_reference=int(file_reference),
                parent_reference=int(parent_reference),
            )
            for path, is_dir, mtime_ns, size, file_reference, parent_reference in rows
        )
        with self._lock:
            self._name_entries = entries

    def _reload_name_entries_if_enabled(self) -> None:
        if self.file_name_enabled:
            self._load_name_entries()

    def _save_usn_cursor(self, journal_id: int | None, cursor: int | None) -> None:
        with self._connect() as connection:
            values = {
                "usn_journal_id": "" if journal_id is None else str(journal_id),
                "usn_cursor": "" if cursor is None else str(cursor),
            }
            connection.executemany(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?, ?)", values.items(),
            )
            connection.commit()

    def _discard_corrupt_database(self) -> None:
        """仅清理当前 PRJ 的缓存数据库；项目文件和其他工作区快照不受影响。"""

        for suffix in ("", "-wal", "-shm"):
            candidate = Path(str(self.database_path) + suffix)
            try:
                candidate.unlink()
            except FileNotFoundError:
                continue

    def _initialize_database(self) -> None:
        with self._connect() as connection:
            connection.execute(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
            )
            connection.execute(
                "CREATE TABLE IF NOT EXISTS entries ("
                "path TEXT PRIMARY KEY, is_dir INTEGER NOT NULL, mtime_ns INTEGER NOT NULL, "
                "size INTEGER NOT NULL, file_reference TEXT NOT NULL, parent_reference TEXT NOT NULL)"
            )
            connection.execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS content_fts "
                "USING fts5(path UNINDEXED, content, tokenize='trigram')"
            )
            connection.commit()

    @contextmanager
    def _connect(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.database_path, timeout=30.0)
        try:
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("PRAGMA synchronous=NORMAL")
            yield connection
        finally:
            connection.close()

    def _read_indexable_text(self, path: Path) -> str | None:
        """读取可索引文本；非 UTF-8、超过大小上限或读取失败返回 None。

        OSError（Windows 独占锁、瞬态 IO）会短暂重试一次再放弃，
        由调用方把路径登记为补偿更新，后续轮询继续尝试。
        """

        try:
            if path.stat().st_size > MAX_CONTENT_INDEX_BYTES:
                return None
        except OSError:
            return None
        try:
            return path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            return None
        except OSError:
            time.sleep(0.05)
            try:
                return path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                return None

    def _skip(self, path: Path) -> bool:
        try:
            return bool(self._should_skip(path))
        except Exception:
            return True

    def _relative(self, path: Path) -> str:
        try:
            relative = path.resolve(strict=False).relative_to(self.workspace_root)
        except ValueError:
            return str(path)
        text = relative.as_posix()
        return text or "."

    def _mark_ready(self) -> None:
        self._set_status(
            file_state="ready" if self.file_name_enabled else "disabled",
            content_state=(
                "ready"
                if self.content_enabled and self._content_root_allowed
                else "unavailable"
                if self.content_enabled
                else "disabled"
            ),
        )

    def _set_error(self, detail: str) -> None:
        self._set_status(
            file_state="error" if self.file_name_enabled else "disabled",
            content_state=(
                "error"
                if self.content_enabled and self._content_root_allowed
                else "unavailable"
                if self.content_enabled
                else "disabled"
            ),
            detail=detail[:300],
        )

    def _content_loading_state(self) -> str:
        if not self.content_enabled:
            return "disabled"
        return "loading" if self._content_root_allowed else "unavailable"

    def _content_building_state(self) -> str:
        if not self.content_enabled:
            return "disabled"
        return "building" if self._content_root_allowed else "unavailable"

    def _set_status(self, **changes: object) -> None:
        with self._lock:
            current = self._status
            values = {
                "file_state": current.file_state,
                "file_processed": current.file_processed,
                "file_total": current.file_total,
                "content_state": current.content_state,
                "content_processed": current.content_processed,
                "content_total": current.content_total,
                "detail": current.detail,
            }
            values.update(changes)
            self._status = SearchIndexStatus(**values)


def is_forbidden_content_search_root(path: Path) -> bool:
    """用户主目录或文件系统根目录本身不允许内容关键词搜索。"""

    resolved = Path(path).expanduser().resolve()
    try:
        home = Path.home().resolve()
    except OSError:
        home = Path.home()
    return resolved == home or resolved.parent == resolved


def _join_relative(parent: str | None, name: str) -> str | None:
    if parent is None:
        return None
    return name if parent == "." else f"{parent.rstrip('/')}/{name}"


def _is_relative_entry(path: str, root: str) -> bool:
    if root == ".":
        return True
    return path == root or path.startswith(root.rstrip("/") + "/")


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def _escape_like(value: str) -> str:
    return value.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")


__all__ = [
    "ProjectSearchIndex",
    "SearchIndexStatus",
    "is_forbidden_content_search_root",
]
