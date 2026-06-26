from __future__ import annotations

import json
import os
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


PROJECTS_FILE_NAME = "projects.json"


class ProjectStoreError(RuntimeError):
    """项目列表持久化、路径校验或项目目录操作失败时抛出。"""


@dataclass(frozen=True)
class ProjectEntry:
    """`.agent_sessions/projects.json` 中的一条项目记录。

    项目列表只保存本地路径、展示名和轻量状态，不复制项目文件，也不改变
    Agent 当前工作区的安全边界；会话分组通过会话索引里的 workspace_root
    动态关联，避免在两个地方重复保存会话归属。
    """

    name: str
    path: str
    created_at: datetime
    updated_at: datetime
    pinned: bool = False
    source: str = "manual"

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "ProjectEntry":
        name = data.get("name", "")
        path = data.get("path", "")
        created_at = data.get("created_at", "")
        updated_at = data.get("updated_at", "")
        pinned = data.get("pinned", False)
        source = data.get("source", "manual")

        if not isinstance(name, str) or not name.strip():
            raise ProjectStoreError("项目名称必须是非空字符串。")
        if not isinstance(path, str) or not path.strip():
            raise ProjectStoreError("项目路径必须是非空字符串。")
        if not isinstance(source, str) or not source.strip():
            raise ProjectStoreError("项目来源必须是非空字符串。")

        return cls(
            name=_clean_project_name(name),
            path=_normalize_project_path(path),
            created_at=_parse_datetime(created_at),
            updated_at=_parse_datetime(updated_at),
            pinned=bool(pinned),
            source=source.strip(),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "path": self.path,
            "created_at": _format_datetime(self.created_at),
            "updated_at": _format_datetime(self.updated_at),
            "pinned": self.pinned,
            "source": self.source,
        }


class ProjectStore:
    """基于 `.agent_sessions/projects.json` 的项目列表存储。

    这个存储只负责"项目列表"本身：扫描会话索引得到项目路径、创建目录、
    导入已有目录，以及把列表落盘。会话数据仍然由 SessionStore 维护，二者
    通过规范化后的绝对路径关联。
    """

    def __init__(self, session_root: Path) -> None:
        self.root = session_root.resolve()
        self.path = self.root / PROJECTS_FILE_NAME

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        if not self.path.exists():
            self._save_entries([])

    def list_projects(self) -> list[ProjectEntry]:
        """返回项目列表，置顶项目优先，其余按更新时间倒序排列。"""

        self.ensure()
        entries = self._load_entries()
        entries.sort(
            key=lambda entry: (
                not entry.pinned,
                -_datetime_to_millis(entry.updated_at),
                entry.name.casefold(),
            )
        )
        return entries

    def scan_projects(
        self,
        workspace_roots: Iterable[str | Path],
        *,
        current_workspace: str | Path | None = None,
        now: datetime | None = None,
    ) -> list[ProjectEntry]:
        """把会话索引里出现过的工作区同步进项目列表。

        扫描不会删除用户手动导入的项目；它只补齐缺失路径。这样即使某个
        项目目录暂时不可用，`projects.json` 里的手动列表也不会被刷新误删。
        """

        self.ensure()
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        entries = self._load_entries()
        by_path = {_path_key(entry.path): entry for entry in entries}

        candidates: list[str | Path] = []
        if current_workspace is not None:
            candidates.append(current_workspace)
        candidates.extend(workspace_roots)

        changed = False
        for raw_path in candidates:
            project_path = _normalize_project_path(raw_path)
            key = _path_key(project_path)
            if key in by_path:
                continue
            entry = ProjectEntry(
                name=_project_name_from_path(project_path),
                path=project_path,
                created_at=timestamp,
                updated_at=timestamp,
                pinned=False,
                source="scanned",
            )
            entries.append(entry)
            by_path[key] = entry
            changed = True

        if changed:
            self._save_entries(entries)
        return self.list_projects()

    def create_project(
        self,
        *,
        name: str,
        path: str | Path,
        now: datetime | None = None,
    ) -> ProjectEntry:
        """创建项目目录并写入项目列表；已有目录会作为幂等导入处理。"""

        cleaned_name = _clean_project_name(name)
        project_path = _normalize_project_path(path)
        candidate = Path(project_path)
        if candidate.exists() and not candidate.is_dir():
            raise ProjectStoreError(f"项目路径已存在但不是目录：{project_path}")
        try:
            candidate.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            raise ProjectStoreError(f"创建项目目录失败：{project_path}，{exc}") from exc
        return self._upsert_project(
            name=cleaned_name,
            path=project_path,
            source="created",
            now=now,
        )

    def import_project(
        self,
        *,
        name: str,
        path: str | Path,
        now: datetime | None = None,
    ) -> ProjectEntry:
        """导入已有项目目录并写入项目列表。"""

        cleaned_name = _clean_project_name(name)
        project_path = _normalize_project_path(path)
        candidate = Path(project_path)
        if not candidate.exists():
            raise ProjectStoreError(f"导入项目路径不存在：{project_path}")
        if not candidate.is_dir():
            raise ProjectStoreError(f"导入项目路径必须是目录：{project_path}")
        return self._upsert_project(
            name=cleaned_name,
            path=project_path,
            source="imported",
            now=now,
        )

    def rename_project(
        self,
        *,
        path: str | Path,
        name: str,
        now: datetime | None = None,
    ) -> ProjectEntry:
        """只修改项目列表中的展示名，不重命名磁盘目录。"""

        project_path = _normalize_project_path(path)
        cleaned_name = _clean_project_name(name)
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        entries = self._load_entries()
        updated: list[ProjectEntry] = []
        renamed: ProjectEntry | None = None
        for entry in entries:
            if _path_key(entry.path) != _path_key(project_path):
                updated.append(entry)
                continue
            renamed = ProjectEntry(
                name=cleaned_name,
                path=entry.path,
                created_at=entry.created_at,
                updated_at=timestamp,
                pinned=entry.pinned,
                source=entry.source,
            )
            updated.append(renamed)
        if renamed is None:
            raise ProjectStoreError(f"未找到项目：{project_path}")
        self._save_entries(updated)
        return renamed

    def pin_project(
        self,
        path: str | Path,
        *,
        pinned: bool = True,
        now: datetime | None = None,
    ) -> ProjectEntry:
        """设置项目置顶状态。"""

        project_path = _normalize_project_path(path)
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        entries = self._load_entries()
        updated: list[ProjectEntry] = []
        pinned_entry: ProjectEntry | None = None
        for entry in entries:
            if _path_key(entry.path) != _path_key(project_path):
                updated.append(entry)
                continue
            pinned_entry = ProjectEntry(
                name=entry.name,
                path=entry.path,
                created_at=entry.created_at,
                updated_at=timestamp,
                pinned=bool(pinned),
                source=entry.source,
            )
            updated.append(pinned_entry)
        if pinned_entry is None:
            raise ProjectStoreError(f"未找到项目：{project_path}")
        self._save_entries(updated)
        return pinned_entry

    def toggle_project_pin(
        self,
        path: str | Path,
        *,
        now: datetime | None = None,
    ) -> ProjectEntry:
        """切换项目置顶状态，供 UI 的单按钮置顶/取消置顶使用。"""

        project_path = _normalize_project_path(path)
        entries = self._load_entries()
        for entry in entries:
            if _path_key(entry.path) == _path_key(project_path):
                return self.pin_project(project_path, pinned=not entry.pinned, now=now)
        raise ProjectStoreError(f"未找到项目：{project_path}")

    def remove_project(self, path: str | Path) -> None:
        """从项目列表移除记录，不删除磁盘上的项目目录或会话文件。"""

        project_path = _normalize_project_path(path)
        entries = self._load_entries()
        remaining = [entry for entry in entries if _path_key(entry.path) != _path_key(project_path)]
        if len(remaining) == len(entries):
            raise ProjectStoreError(f"未找到项目：{project_path}")
        self._save_entries(remaining)

    def _upsert_project(
        self,
        *,
        name: str,
        path: str,
        source: str,
        now: datetime | None,
    ) -> ProjectEntry:
        self.ensure()
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        normalized_path = _normalize_project_path(path)
        entries = self._load_entries()
        updated: list[ProjectEntry] = []
        saved: ProjectEntry | None = None
        for entry in entries:
            if _path_key(entry.path) != _path_key(normalized_path):
                updated.append(entry)
                continue
            saved = ProjectEntry(
                name=name,
                path=entry.path,
                created_at=entry.created_at,
                updated_at=timestamp,
                pinned=entry.pinned,
                source=source,
            )
            updated.append(saved)
        if saved is None:
            saved = ProjectEntry(
                name=name,
                path=normalized_path,
                created_at=timestamp,
                updated_at=timestamp,
                pinned=False,
                source=source,
            )
            updated.append(saved)
        self._save_entries(updated)
        return saved

    def _load_entries(self) -> list[ProjectEntry]:
        if not self.path.exists():
            return []
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise ProjectStoreError(f"项目列表不是合法 JSON：{self.path}") from exc
        except UnicodeDecodeError as exc:
            raise ProjectStoreError(f"项目列表不是 UTF-8 文本：{self.path}") from exc
        except OSError as exc:
            raise ProjectStoreError(f"读取项目列表失败：{self.path}，{exc}") from exc

        projects = data.get("projects", []) if isinstance(data, dict) else []
        if not isinstance(projects, list):
            raise ProjectStoreError("项目列表顶层字段 projects 必须是列表。")
        entries: list[ProjectEntry] = []
        for item in projects:
            if isinstance(item, dict):
                entries.append(ProjectEntry.from_dict(item))
        return entries

    def _save_entries(self, entries: list[ProjectEntry]) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        data = {"projects": [entry.to_dict() for entry in entries]}
        temp_path = self.path.with_suffix(".json.tmp")
        try:
            temp_path.write_text(
                json.dumps(data, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            temp_path.replace(self.path)
        except OSError as exc:
            raise ProjectStoreError(f"写入项目列表失败：{self.path}，{exc}") from exc


def _normalize_project_path(raw_path: str | Path) -> str:
    if isinstance(raw_path, Path):
        candidate = raw_path
    elif isinstance(raw_path, str) and raw_path.strip():
        candidate = Path(os.path.expandvars(raw_path.strip())).expanduser()
    else:
        raise ProjectStoreError("项目路径必须是非空字符串。")
    try:
        return str(candidate.resolve(strict=False))
    except OSError as exc:
        raise ProjectStoreError(f"项目路径无效：{raw_path}") from exc


def _clean_project_name(value: str) -> str:
    if not isinstance(value, str):
        raise ProjectStoreError("项目名称必须是非空字符串。")
    name = " ".join(value.strip().split())
    if not name:
        raise ProjectStoreError("项目名称不能为空。")
    if len(name) > 80:
        return name[:77] + "..."
    return name


def _project_name_from_path(path: str) -> str:
    name = Path(path).name.strip()
    return name or path


def _path_key(path: str) -> str:
    return os.path.normcase(path).casefold()


def _parse_datetime(raw_value: Any) -> datetime:
    if not isinstance(raw_value, str) or not raw_value.strip():
        raise ProjectStoreError("项目时间戳必须是非空字符串。")
    try:
        parsed = datetime.fromisoformat(raw_value.strip().replace("Z", "+00:00"))
    except ValueError as exc:
        raise ProjectStoreError(f"项目时间戳格式无效：{raw_value}") from exc
    return _ensure_timezone(parsed)


def _format_datetime(value: datetime) -> str:
    return _ensure_timezone(value).isoformat()


def _datetime_to_millis(value: datetime) -> int:
    return int(_ensure_timezone(value).timestamp() * 1000)


def _ensure_timezone(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)
