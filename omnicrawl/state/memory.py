from __future__ import annotations

import json
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .memory_ranking import (
    DEFAULT_STORAGE_DIRECTORIES,
    directories_overlap as _directories_overlap,
    make_summary as _make_summary,
    merge_memory_content as _merge_memory_content,
    normalize_for_compare as _normalize_for_compare,
    score_related_entry as _score_related_entry_impl,
    score_search_entry as _score_search_entry_impl,
    text_similarity as _text_similarity,
    classify_storage_directory as _classify_storage_directory,
)
from .session_locking import replace_with_retry as _replace_with_retry


class MemoryStoreError(RuntimeError):
    """记忆索引、记忆文件读写或目录安全校验失败时抛出。"""


@dataclass(frozen=True)
class MemorySearchResult:
    """候选记忆摘要，供模型先低成本判断是否需要读取全文。"""

    id: str
    summary: str
    storage_directory: str
    related_directories: list[str]
    timestamp: datetime


@dataclass(frozen=True)
class MemoryRecord:
    """完整记忆内容，只有模型明确需要细节时才返回。"""

    id: str
    timestamp: datetime
    related_directories: list[str]
    content: str


@dataclass(frozen=True)
class MemoryWriteRequest:
    """模型写入长期记忆时提交的最小结构。"""

    content: str
    related_directories: list[str]
    storage_directory: str | None = None
    source_event: str | None = None


@dataclass(frozen=True)
class MemoryMigrationResult:
    """旧记忆目录迁移结果，供 Agent 启动日志和测试使用。"""

    migrated: bool
    destination: Path
    backup_path: Path | None = None
    imported_count: int = 0


@dataclass
class MemoryIndexEntry:
    """index.json 中的索引条目。

    Markdown 文件只保存设计文档要求的三部分：timestamp、关联目录和正文。
    touch_count 放在索引中，用于实现 7+N 天的清理规则。
    """

    id: str
    path: str
    storage_directory: str
    timestamp: datetime
    touch_count: int
    related_directories: list[str]
    summary: str

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "MemoryIndexEntry":
        try:
            raw_id = data["id"]
            raw_path = data["path"]
            raw_storage_directory = data["storage_directory"]
            raw_timestamp = data["timestamp"]
        except KeyError as exc:
            raise MemoryStoreError(f"记忆索引缺少字段：{exc.args[0]}。") from exc

        if not isinstance(raw_id, str) or not raw_id.strip():
            raise MemoryStoreError("记忆索引字段 id 必须是非空字符串。")
        if not isinstance(raw_path, str) or not raw_path.strip():
            raise MemoryStoreError(f"记忆 {raw_id} 的 path 必须是非空字符串。")
        if not isinstance(raw_storage_directory, str) or not raw_storage_directory.strip():
            raise MemoryStoreError(f"记忆 {raw_id} 的 storage_directory 必须是非空字符串。")
        if not isinstance(raw_timestamp, str) or not raw_timestamp.strip():
            raise MemoryStoreError(f"记忆 {raw_id} 的 timestamp 必须是非空字符串。")

        related = data.get("related_directories", [])
        if not isinstance(related, list) or not all(isinstance(item, str) for item in related):
            raise MemoryStoreError(f"记忆 {raw_id} 的 related_directories 必须是字符串列表。")

        touch_count = data.get("touch_count", 0)
        if isinstance(touch_count, bool) or not isinstance(touch_count, int) or touch_count < 0:
            raise MemoryStoreError(f"记忆 {raw_id} 的 touch_count 必须是非负整数。")

        summary = data.get("summary", "")
        if not isinstance(summary, str):
            raise MemoryStoreError(f"记忆 {raw_id} 的 summary 必须是字符串。")

        return cls(
            id=raw_id.strip(),
            path=_normalize_relative_file_path(raw_path),
            storage_directory=_normalize_directory(raw_storage_directory),
            timestamp=_parse_datetime(raw_timestamp),
            touch_count=touch_count,
            related_directories=_dedupe_directories(related),
            summary=summary.strip(),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "path": self.path,
            "storage_directory": self.storage_directory,
            "timestamp": _format_datetime(self.timestamp),
            "touch_count": self.touch_count,
            "related_directories": list(self.related_directories),
            "summary": self.summary,
        }


@dataclass(frozen=True)
class _PreparedMemoryWrite:
    """已完成校验和目录归一化的写入请求。

    批量写入会先把所有请求整理成该结构，再开始落盘，避免后续请求校验失败时，
    前面请求已经写出 Markdown 但 index.json 没有同步更新。
    """

    content: str
    storage_directory: str
    related_directories: list[str]


class MemoryStore:
    """基于 Markdown 文件和 index.json 的长期记忆存储。

    该类刻意不依赖向量数据库或第三方 YAML 库：课程项目当前依赖很轻，
    第一版先把文件结构、索引、基础检索、读取加深和清理机制做稳。
    后续如果要接入嵌入模型，相似度检索可以只替换 search 的打分层。
    """

    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.index_path = self.root / "index.json"

    def search(
        self,
        query: str,
        candidate_directories: list[str] | None = None,
        max_results: int = 5,
    ) -> list[MemorySearchResult]:
        """按查询文本和候选目录返回摘要，不读取完整记忆正文。"""

        entries = self._load_entries()
        cleaned_query = query.strip()
        directories = _dedupe_directories(candidate_directories or [])
        limit = _clamp(max_results, minimum=1, maximum=20)

        scored: list[tuple[float, MemoryIndexEntry]] = []
        for entry in entries:
            if not self._memory_path(entry).is_file():
                continue
            score = self._score_search_entry(entry, cleaned_query, directories)
            if score > 0 or (not cleaned_query and not directories):
                scored.append((score, entry))

        scored.sort(key=lambda item: (item[0], item[1].timestamp, item[1].touch_count), reverse=True)
        return [self._to_search_result(entry) for _score, entry in scored[:limit]]

    def read(self, memory_ids: list[str]) -> list[MemoryRecord]:
        """读取指定记忆全文，并对实际读到的记忆执行加深回忆。"""

        ids = _dedupe_strings(memory_ids)
        if not ids:
            return []

        touched_entries = self._touch_entries(ids)
        records: list[MemoryRecord] = []
        for memory_id in ids:
            entry = touched_entries.get(memory_id)
            if entry is None:
                continue
            path = self._memory_path(entry)
            if not path.is_file():
                continue
            records.append(
                MemoryRecord(
                    id=entry.id,
                    timestamp=entry.timestamp,
                    related_directories=list(entry.related_directories),
                    content=_read_markdown_body(path),
                )
            )

        return records

    def expand_related(
        self,
        memory_ids: list[str],
        max_depth: int = 1,
        max_results: int = 5,
    ) -> list[MemorySearchResult]:
        """沿关联目录扩展候选摘要，默认只展开一层关系。"""

        ids = set(_dedupe_strings(memory_ids))
        if not ids:
            return []

        entries = self._load_entries()
        entry_by_id = {entry.id: entry for entry in entries}
        frontier = self._initial_related_frontier(ids, entry_by_id)
        if not frontier:
            return []

        limit = _clamp(max_results, minimum=1, maximum=20)
        depth_limit = _clamp(max_depth, minimum=1, maximum=3)
        found: dict[str, tuple[float, MemoryIndexEntry]] = {}
        seen_ids = set(ids)

        for depth in range(depth_limit):
            next_frontier: set[str] = set()
            for entry in entries:
                if entry.id in seen_ids or not self._memory_path(entry).is_file():
                    continue
                score = self._score_related_entry(entry, frontier, depth)
                if score <= 0:
                    continue
                found[entry.id] = (score, entry)
                seen_ids.add(entry.id)
                next_frontier.update(entry.related_directories)
                next_frontier.add(entry.storage_directory)

            if not next_frontier or len(found) >= limit:
                break
            frontier = next_frontier

        ranked = sorted(
            found.values(),
            key=lambda item: (item[0], item[1].timestamp, item[1].touch_count),
            reverse=True,
        )
        return [self._to_search_result(entry) for _score, entry in ranked[:limit]]

    def write(self, memories: list[MemoryWriteRequest]) -> list[MemoryRecord]:
        """写入或合并长期记忆，并在写入后执行一次过期清理。"""

        if not memories:
            return []

        prepared_memories = [self._prepare_write_request(request) for request in memories]
        self.root.mkdir(parents=True, exist_ok=True)
        entries = self._load_entries()
        records: list[MemoryRecord] = []
        created_paths: list[Path] = []
        updated_file_backups: dict[Path, str | None] = {}

        try:
            for prepared in prepared_memories:
                existing = self._find_duplicate_entry(
                    entries,
                    prepared.content,
                    prepared.storage_directory,
                    prepared.related_directories,
                )
                if existing is not None:
                    path = self._memory_path(existing)
                    if path not in updated_file_backups:
                        updated_file_backups[path] = path.read_text(encoding="utf-8") if path.is_file() else None
                    record = self._update_existing_memory(
                        existing,
                        prepared.content,
                        prepared.related_directories,
                    )
                else:
                    record = self._create_memory(
                        prepared.content,
                        prepared.storage_directory,
                        prepared.related_directories,
                        entries,
                    )
                    created_paths.append(self.root / prepared.storage_directory / f"{record.id}.md")
                    entries.append(
                        MemoryIndexEntry(
                            id=record.id,
                            path=f"{prepared.storage_directory}/{record.id}.md",
                            storage_directory=prepared.storage_directory,
                            timestamp=record.timestamp,
                            touch_count=0,
                            related_directories=list(record.related_directories),
                            summary=_make_summary(record.content),
                        )
                    )

                records.append(record)

            self._save_entries(entries)
        except Exception:
            self._rollback_file_changes(created_paths, updated_file_backups)
            raise

        self.clean_expired_memories()
        return records

    def _prepare_write_request(self, request: MemoryWriteRequest) -> _PreparedMemoryWrite:
        content = _normalize_content(request.content)
        if not content:
            raise MemoryStoreError("写入记忆的 content 不能为空。")

        storage_directory = self._choose_storage_directory(
            content=content,
            requested_directory=request.storage_directory,
        )
        related_directories = _dedupe_directories(request.related_directories)
        if storage_directory not in related_directories:
            related_directories.append(storage_directory)
        return _PreparedMemoryWrite(
            content=content,
            storage_directory=storage_directory,
            related_directories=related_directories,
        )

    def _rollback_file_changes(
        self,
        created_paths: list[Path],
        updated_file_backups: dict[Path, str | None],
    ) -> None:
        """批量写入失败时尽力回滚已落盘文件，避免未索引记忆残留。"""

        for path in created_paths:
            try:
                if path.is_file():
                    path.unlink()
            except OSError:
                pass

        for path, previous_text in updated_file_backups.items():
            try:
                if previous_text is None:
                    if path.is_file():
                        path.unlink()
                else:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(previous_text, encoding="utf-8")
            except OSError:
                pass

    def clean_expired_memories(self) -> list[str]:
        """按 7+touch_count 天规则清理过期记忆，并移除空目录。"""

        entries = self._load_entries()
        if not entries:
            return []

        now = _now()
        kept: list[MemoryIndexEntry] = []
        deleted_paths: list[str] = []
        for entry in entries:
            age_days = (now - entry.timestamp).total_seconds() / 86400
            expire_days = 7 + entry.touch_count
            if age_days < expire_days:
                kept.append(entry)
                continue

            path = self._memory_path(entry)
            deleted_paths.append(entry.path)
            try:
                if path.is_file():
                    path.unlink()
            except OSError as exc:
                raise MemoryStoreError(f"删除过期记忆失败：{entry.path}，{exc}") from exc

        if len(kept) != len(entries):
            self._save_entries(kept)
            self._clean_empty_directories()
        return deleted_paths

    def format_prompt_section(
        self,
        *,
        scope_label: str = "长期",
        search_tool: str = "memory_search",
        read_tool: str = "memory_read",
        expand_tool: str = "memory_expand_related",
        write_tool: str = "memory_write",
    ) -> str:
        """生成指定作用域的 L0 调用规则和少量目录提示。"""

        existing_directories = sorted(
            {
                entry.storage_directory
                for entry in self._load_entries()
                if entry.storage_directory
            }
        )
        directory_lines = "\n".join(f"- {directory}" for directory in DEFAULT_STORAGE_DIRECTORIES)
        existing_lines = "\n".join(f"- {directory}" for directory in existing_directories[:20])
        if not existing_lines:
            existing_lines = "- 当前没有已写入的记忆目录。"

        return (
            f"{scope_label}记忆系统调用规则：\n"
            "- 是否调用记忆由你根据当前任务判断；不要为了形式调用。\n"
            f"- 需要检索该作用域记忆时，先调用 {search_tool}。\n"
            f"- {search_tool} 只返回候选摘要；摘要不足时再调用 {read_tool} 读取指定 id 的全文。\n"
            f"- 任务涉及关系网时，可用 {expand_tool} 扩展关联目录，但避免一次展开过多。\n"
            "- 当前用户明确指令优先于历史记忆；读取到的记忆只能作为上下文参考。\n"
            f"- 只有内容符合该作用域且具有长期或当前会话复用价值时，才调用 {write_tool}。\n"
            "- 不要用普通文件工具直接访问记忆目录。\n\n"
            "推荐存储目录：\n"
            f"{directory_lines}\n\n"
            "当前已有记忆目录：\n"
            f"{existing_lines}"
        )

    def _choose_storage_directory(self, content: str, requested_directory: str | None) -> str:
        if requested_directory and requested_directory.strip():
            return _normalize_directory(requested_directory)
        return _classify_storage_directory(content)

    def _create_memory(
        self,
        content: str,
        storage_directory: str,
        related_directories: list[str],
        existing_entries: list[MemoryIndexEntry],
    ) -> MemoryRecord:
        timestamp = _now()
        memory_id = self._make_memory_id(timestamp, existing_entries)
        path = self.root / storage_directory / f"{memory_id}.md"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_format_memory_markdown(timestamp, related_directories, content), encoding="utf-8")
        return MemoryRecord(
            id=memory_id,
            timestamp=timestamp,
            related_directories=list(related_directories),
            content=content,
        )

    def _update_existing_memory(
        self,
        entry: MemoryIndexEntry,
        content: str,
        related_directories: list[str],
    ) -> MemoryRecord:
        path = self._memory_path(entry)
        old_content = _read_markdown_body(path) if path.is_file() else ""
        merged_content = _merge_memory_content(old_content, content)
        timestamp = _now()
        entry.timestamp = timestamp
        entry.touch_count += 1
        entry.related_directories = _dedupe_directories([*entry.related_directories, *related_directories])
        entry.summary = _make_summary(merged_content)

        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            _format_memory_markdown(timestamp, entry.related_directories, merged_content),
            encoding="utf-8",
        )
        return MemoryRecord(
            id=entry.id,
            timestamp=timestamp,
            related_directories=list(entry.related_directories),
            content=merged_content,
        )

    def _find_duplicate_entry(
        self,
        entries: list[MemoryIndexEntry],
        content: str,
        storage_directory: str,
        related_directories: list[str],
    ) -> MemoryIndexEntry | None:
        normalized_content = _normalize_for_compare(content)
        for entry in entries:
            if entry.storage_directory != storage_directory and not _directories_overlap(
                entry.related_directories,
                related_directories,
            ):
                continue

            path = self._memory_path(entry)
            old_content = _read_markdown_body(path) if path.is_file() else entry.summary
            if _normalize_for_compare(old_content) == normalized_content:
                return entry
            if _text_similarity(old_content, content) >= 0.9:
                return entry
        return None

    def _score_search_entry(
        self,
        entry: MemoryIndexEntry,
        query: str,
        candidate_directories: list[str],
    ) -> float:
        return _score_search_entry_impl(entry, query, candidate_directories, now=_now())

    def _score_related_entry(
        self,
        entry: MemoryIndexEntry,
        directories: set[str],
        depth: int,
    ) -> float:
        return _score_related_entry_impl(entry, directories, depth)

    def _initial_related_frontier(
        self,
        ids: set[str],
        entry_by_id: dict[str, MemoryIndexEntry],
    ) -> set[str]:
        directories: set[str] = set()
        for memory_id in ids:
            entry = entry_by_id.get(memory_id)
            if entry is None:
                continue
            directories.add(entry.storage_directory)
            directories.update(entry.related_directories)
        return directories

    def _touch_entries(self, memory_ids: list[str]) -> dict[str, MemoryIndexEntry]:
        entries = self._load_entries()
        id_set = set(memory_ids)
        touched: dict[str, MemoryIndexEntry] = {}
        now = _now()
        for entry in entries:
            if entry.id not in id_set:
                continue
            path = self._memory_path(entry)
            if not path.is_file():
                continue

            entry.timestamp = now
            entry.touch_count += 1
            content = _read_markdown_body(path)
            path.write_text(_format_memory_markdown(now, entry.related_directories, content), encoding="utf-8")
            touched[entry.id] = entry

        if touched:
            self._save_entries(entries)
        return touched

    def _to_search_result(self, entry: MemoryIndexEntry) -> MemorySearchResult:
        return MemorySearchResult(
            id=entry.id,
            summary=entry.summary,
            storage_directory=entry.storage_directory,
            related_directories=list(entry.related_directories),
            timestamp=entry.timestamp,
        )

    def _load_entries(self) -> list[MemoryIndexEntry]:
        if not self.index_path.is_file():
            return []

        try:
            data = json.loads(self.index_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise MemoryStoreError(
                f"记忆索引 JSON 解析失败：{self.index_path}，第 {exc.lineno} 行第 {exc.colno} 列：{exc.msg}"
            ) from exc
        except OSError as exc:
            raise MemoryStoreError(f"读取记忆索引失败：{self.index_path}，{exc}") from exc

        memories = data.get("memories", [])
        if not isinstance(memories, list):
            raise MemoryStoreError("记忆索引顶层字段 memories 必须是列表。")
        return [MemoryIndexEntry.from_dict(item) for item in memories if isinstance(item, dict)]

    def _save_entries(self, entries: list[MemoryIndexEntry]) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        entries.sort(key=lambda entry: (entry.storage_directory, entry.path))
        payload = {"memories": [entry.to_dict() for entry in entries]}
        tmp_path = self.index_path.with_suffix(".json.tmp")
        try:
            # 先落同目录临时文件，再带 Windows WinError 5 短重试地原子替换 index。
            tmp_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
            _replace_with_retry(tmp_path, self.index_path)
        except OSError as exc:
            if tmp_path.exists():
                try:
                    tmp_path.unlink()
                except OSError:
                    pass
            raise MemoryStoreError(f"写入记忆索引失败：{self.index_path}，{exc}") from exc

    def _make_memory_id(self, timestamp: datetime, existing_entries: list[MemoryIndexEntry]) -> str:
        base_id = timestamp.strftime("%Y%m%d-%H%M%S")
        memory_id = base_id
        suffix = 1
        existing_ids = {entry.id for entry in existing_entries}
        while memory_id in existing_ids or self._memory_file_exists(memory_id):
            suffix += 1
            memory_id = f"{base_id}-{suffix:03d}"
        return memory_id

    def _memory_file_exists(self, memory_id: str) -> bool:
        if not self.root.is_dir():
            return False
        return any(self.root.rglob(f"{memory_id}.md"))

    def _memory_path(self, entry: MemoryIndexEntry) -> Path:
        path = (self.root / entry.path).resolve()
        if not _is_relative_to(path, self.root):
            raise MemoryStoreError(f"记忆路径越界：{entry.path}")
        return path

    def _clean_empty_directories(self) -> None:
        if not self.root.is_dir():
            return
        directories = [path for path in self.root.rglob("*") if path.is_dir()]
        for directory in sorted(directories, key=lambda path: len(path.parts), reverse=True):
            if directory == self.root:
                continue
            try:
                directory.rmdir()
            except OSError:
                pass


def search_result_to_dict(result: MemorySearchResult) -> dict[str, Any]:
    return {
        "id": result.id,
        "summary": result.summary,
        "storage_directory": result.storage_directory,
        "related_directories": result.related_directories,
        "timestamp": _format_datetime(result.timestamp),
    }


def record_to_dict(record: MemoryRecord) -> dict[str, Any]:
    return {
        "id": record.id,
        "timestamp": _format_datetime(record.timestamp),
        "related_directories": record.related_directories,
        "content": record.content,
    }


def _format_memory_markdown(
    timestamp: datetime,
    related_directories: list[str],
    content: str,
) -> str:
    lines = [
        "---",
        f'timestamp: "{_format_datetime(timestamp)}"',
        "related_directories:",
    ]
    for directory in related_directories:
        lines.append(f'  - "{_escape_frontmatter_string(directory)}"')
    lines.extend(["---", "", content.rstrip(), ""])
    return "\n".join(lines)


def _read_markdown_body(path: Path) -> str:
    try:
        text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        raise MemoryStoreError(f"记忆文件不是 UTF-8 文本：{path}") from exc
    except OSError as exc:
        raise MemoryStoreError(f"读取记忆文件失败：{path}，{exc}") from exc

    normalized = text.replace("\r\n", "\n").replace("\r", "\n")
    if not normalized.startswith("---\n"):
        return normalized.strip()

    end_index = normalized.find("\n---", 4)
    if end_index < 0:
        return normalized.strip()
    return normalized[end_index + 4 :].strip()


def _normalize_directory(raw_directory: str) -> str:
    directory = raw_directory.strip().replace("\\", "/")
    directory = re.sub(r"\s+", "-", directory)
    directory = re.sub(r"/{2,}", "/", directory).strip("/")
    if not directory or directory in {".", ".."}:
        raise MemoryStoreError("记忆目录不能为空或为相对跳转目录。")
    if directory.startswith("../") or "/../" in directory or directory.endswith("/.."):
        raise MemoryStoreError(f"记忆目录不能包含上级跳转：{raw_directory}")
    if re.match(r"^[a-zA-Z]:/", directory) or directory.startswith("/"):
        raise MemoryStoreError(f"记忆目录必须是相对路径：{raw_directory}")

    segments = []
    for segment in directory.split("/"):
        safe = re.sub(r"[^0-9A-Za-z\u4e00-\u9fff_.-]+", "-", segment).strip(".-")
        if not safe:
            continue
        segments.append(safe.lower())
    if not segments:
        raise MemoryStoreError(f"记忆目录无有效片段：{raw_directory}")
    return "/".join(segments)


def _normalize_relative_file_path(raw_path: str) -> str:
    path = raw_path.strip().replace("\\", "/")
    if not path.endswith(".md"):
        raise MemoryStoreError(f"记忆文件必须是 Markdown：{raw_path}")
    directory = _normalize_directory(str(Path(path).parent).replace("\\", "/"))
    filename = Path(path).name
    safe_name = re.sub(r"[^0-9A-Za-z_.-]+", "-", filename).strip(".-")
    if not safe_name or safe_name in {".", ".."}:
        raise MemoryStoreError(f"记忆文件名无效：{raw_path}")
    return f"{directory}/{safe_name}"


def _dedupe_directories(directories: list[str]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for directory in directories:
        if not isinstance(directory, str) or not directory.strip():
            continue
        normalized = _normalize_directory(directory)
        if normalized in seen:
            continue
        seen.add(normalized)
        result.append(normalized)
    return result


def _dedupe_strings(values: list[str]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        if not isinstance(value, str):
            continue
        cleaned = value.strip()
        if not cleaned or cleaned in seen:
            continue
        seen.add(cleaned)
        result.append(cleaned)
    return result


def _normalize_content(content: str) -> str:
    return content.replace("\r\n", "\n").replace("\r", "\n").strip()


def _format_datetime(value: datetime) -> str:
    if value.tzinfo is None:
        value = value.replace(tzinfo=timezone.utc)
    return value.astimezone().isoformat(timespec="seconds")


def _parse_datetime(raw_value: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(raw_value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise MemoryStoreError(f"记忆时间戳格式无效：{raw_value}") from exc
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone()


def _now() -> datetime:
    return datetime.now().astimezone()


def migrate_legacy_memory(
    source_root: Path,
    destination_root: Path,
) -> MemoryMigrationResult:
    """将旧 ``memory/`` 迁移到项目级记忆目录并保留失败可恢复性。

    目标不存在时直接改名，完整保留旧索引、时间戳和触碰次数。目标已存在时，
    通过 MemoryStore 导入旧正文，再把源目录改名为带时间戳的备份；导入失败
    时不移动源目录，避免启动过程造成不可逆数据丢失。
    """

    source = source_root.expanduser().resolve()
    destination = destination_root.expanduser().resolve()
    if source == destination or not source.exists():
        return MemoryMigrationResult(False, destination)
    if not source.is_dir():
        raise MemoryStoreError(f"旧记忆路径不是目录：{source}")
    if destination.exists() and not destination.is_dir():
        raise MemoryStoreError(f"项目级记忆路径不是目录：{destination}")

    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        try:
            source.rename(destination)
        except OSError as exc:
            raise MemoryStoreError(f"迁移旧记忆目录失败：{source} -> {destination}，{exc}") from exc
        return MemoryMigrationResult(True, destination)

    source_store = MemoryStore(source)
    destination_store = MemoryStore(destination)
    source_entries = source_store._load_entries()
    requests: list[MemoryWriteRequest] = []
    for entry in source_entries:
        path = source_store._memory_path(entry)
        if not path.is_file():
            continue
        requests.append(
            MemoryWriteRequest(
                content=_read_markdown_body(path),
                related_directories=entry.related_directories,
                storage_directory=entry.storage_directory,
                source_event="legacy_memory_migration",
            )
        )
    if requests:
        destination_store.write(requests)

    backup_path = _next_memory_backup_path(source)
    try:
        source.rename(backup_path)
    except OSError as exc:
        raise MemoryStoreError(f"备份旧记忆目录失败：{source} -> {backup_path}，{exc}") from exc
    return MemoryMigrationResult(
        migrated=True,
        destination=destination,
        backup_path=backup_path,
        imported_count=len(requests),
    )


def _next_memory_backup_path(source: Path) -> Path:
    stamp = _now().strftime("%Y%m%d-%H%M%S")
    candidate = source.with_name(f"{source.name}.migrated-{stamp}")
    suffix = 1
    while candidate.exists():
        suffix += 1
        candidate = source.with_name(f"{source.name}.migrated-{stamp}-{suffix:03d}")
    return candidate


def _escape_frontmatter_string(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def _clamp(value: int, *, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        return minimum
    return max(minimum, min(maximum, value))


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
