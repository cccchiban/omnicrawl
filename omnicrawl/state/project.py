from __future__ import annotations

import json
import os
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable


PROJECTS_FILE_NAME = "projects.json"
# Agent 隔离工作树宿主根目录名（主 Agent 隔离区 aw-*/SubAgent 隔离区 sw-*）。
# 与 workspace/agent_isolation.py 的 DEFAULT_WORKTREES_ROOT 保持同值；
# state 层不反向依赖 workspace，因此在此独立定义同构常量。
AGENT_WORKTREES_DIR_NAME = "agent-worktrees"


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
        """返回项目列表，置顶项目优先，其余按更新时间倒序排列。

        Agent 隔离工作树（`~/.omnicrawl/agent-worktrees/` 下的 aw-*/sw-*）
        不属于用户项目，读取时兜底过滤，避免历史残留继续出现在界面。
        """

        self.ensure()
        entries = [
            entry for entry in self._load_entries() if not _under_agent_worktrees(entry.path)
        ]
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

        只补齐缺失路径，不删除用户手动导入的项目（手动项目目录暂时不可用
        时列表仍保留）。以下候选会被忽略、历史扫描残留会被移除：
        - Agent 隔离工作树（`~/.omnicrawl/agent-worktrees/` 下 aw-*/sw-*）；
        - 系统临时目录下的测试/临时残留（`<temp>/tmp*`，目录名以 tmp 开头）；
        - 已不存在的目录（历史残留会话结束后目录已被清理）。
        """

        self.ensure()
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        loaded = self._load_entries()
        entries = [
            entry
            for entry in loaded
            if entry.source != "scanned" or not _is_scan_excluded(entry.path)
        ]
        by_path = {_path_key(entry.path): entry for entry in entries}

        candidates: list[str | Path] = []
        if current_workspace is not None:
            candidates.append(current_workspace)
        candidates.extend(workspace_roots)

        changed = len(entries) != len(loaded)
        for raw_path in candidates:
            project_path = _normalize_project_path(raw_path)
            if _is_scan_excluded(project_path):
                continue
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

    def project_overview(
        self,
        session_entries: Iterable[Any],
        *,
        recent_limit: int = 3,
        sort_limit: int = 0,
    ) -> list[dict[str, Any]]:
        """只读聚合项目总览：显式项目 + 会话索引中的稳定目录。

        - ``session_entries``：会话索引条目（需有 workspace_root/updated_at/session_id/title）；
        - 聚合键 = git 仓库根（若在 git 工作树内）否则目录自身 → 子目录会话归并到仓库；
        - 自动排除隔离工作树、临时目录、解释器库目录（见 ``_is_scan_excluded``）；
        - 显式项目（created/imported/pinned）恒保留（即使 session_count=0）；
        - scanned 且 session_count=0 的目录不展示（只剩历史引用）；
        - 不写盘、不修改 projects.json（只读聚合，避免回灌）。

        返回按 pinned → 最近活动 → 名称 排序的视图字典列表：
        name/path/source/pinned/session_count/recent_at/recent_sessions。
        """

        self.ensure()
        explicit = [
            entry
            for entry in self._load_entries()
            if entry.source != "scanned" or not _is_scan_excluded(entry.path)
        ]

        def _make(name: str, path: str, source: str, pinned: bool) -> dict[str, Any]:
            return {
                "name": name,
                "path": path,
                "source": source,
                "pinned": pinned,
                "session_count": 0,
                "recent_at": "",
                "recent_sessions": [],
            }

        def _agg_key(path: str) -> tuple[str, str]:
            """返回 (聚合键, 展示路径)。git 工作树内上卷到仓库根。"""
            root = _git_root(path)
            target = root if root else _normalize_project_path(path)
            return _path_key(target), target

        merged: dict[str, dict[str, Any]] = {}
        # 显式项目先按仓库根归并（保留显式 source/pinned/name）
        for entry in explicit:
            key, target = _agg_key(entry.path)
            item = merged.get(key)
            if item is None:
                item = _make(entry.name, target, entry.source, entry.pinned)
                merged[key] = item
            elif item["source"] == "scanned":
                item["source"] = entry.source
                item["pinned"] = item["pinned"] or entry.pinned
                item["name"] = entry.name

        # 会话索引聚合（同一目录多会话合并计数）
        for entry in session_entries:
            raw = getattr(entry, "workspace_root", None)
            if not raw:
                continue
            try:
                path = _normalize_project_path(raw)
            except ProjectStoreError:
                continue
            if _is_scan_excluded(path):
                continue
            key, target = _agg_key(path)
            item = merged.get(key)
            if item is None:
                item = _make(Path(target).name or target, target, "scanned", False)
                merged[key] = item
            item["session_count"] += 1
            raw_up = getattr(entry, "updated_at", None)
            stamp = _format_datetime(raw_up) if raw_up is not None else ""
            if not item["recent_at"] or (stamp and stamp > item["recent_at"]):
                item["recent_at"] = stamp
            if len(item["recent_sessions"]) < max(1, recent_limit):
                item["recent_sessions"].append(
                    {
                        "session_id": getattr(entry, "session_id", ""),
                        "title": getattr(entry, "title", "") or "",
                        "updated_at": stamp,
                    }
                )

        result = [
            item
            for item in merged.values()
            if item["source"] != "scanned" or item["session_count"] > 0
        ]
        if sort_limit and len(result) > sort_limit:
            # 只对头部做 git 探测等昂贵增强时保留前 N（本方法不做探测，供调用方参考）
            result = result[:sort_limit]
        result.sort(
            key=lambda item: (
                not item["pinned"],
                -_datetime_to_millis(_ensure_timezone(_parse_datetime(item["recent_at"])))
                if item["recent_at"]
                else 0,
                item["name"].casefold(),
            )
        )
        return result

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
        if _under_agent_worktrees(normalized_path):
            raise ProjectStoreError(
                f"Agent 隔离工作树目录不能加入项目列表：{normalized_path}"
            )
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


def _git_root(path: str) -> str | None:
    """向上查找有效 git 仓库根（含 .git 目录且含 HEAD，或 gitfile），找不到返回 None。

    - 仅目录名 .git 但无 HEAD（git init 中断残留）不算仓库，避免把容器目录误当项目；
    - worktree 场景 .git 是文件（指向真实仓库），仍视为同一仓库根。
    """
    try:
        cursor = Path(path).expanduser().resolve(strict=False)
    except OSError:
        return None
    while True:
        dot_git = cursor / ".git"
        try:
            if dot_git.is_dir():
                if (dot_git / "HEAD").exists():
                    return str(cursor.resolve())
            elif dot_git.is_file():
                return str(cursor.resolve())
        except OSError:
            pass
        parent = cursor.parent
        if parent == cursor:
            return None
        cursor = parent


def _under_agent_worktrees(path: str) -> bool:
    """路径是否位于 Agent 隔离工作树宿主根（~/.omnicrawl/agent-worktrees/）下。

    aw-*/sw-* 隔离目录是进程级临时工作区，不应作为用户项目展示或持久化。
    平台无关比较（Windows 路径大小写不敏感）。
    """
    try:
        normalized = os.path.normcase(Path(path).expanduser())
    except OSError:
        return False
    worktrees_root = os.path.normcase(Path.home() / ".omnicrawl" / AGENT_WORKTREES_DIR_NAME)
    try:
        Path(normalized).relative_to(Path(worktrees_root))
        return True
    except ValueError:
        return False


# 自动扫描/聚合忽略的解释器与用户配置目录片段（大小写不敏感，命中即排除）。
# 这些目录由工具链生成（site-packages、Python 安装树、AppData 解释器目录），不是用户项目。
_SCAN_EXCLUDED_FRAGMENTS = (
    os.sep + "site-packages",
    os.sep + "python" + os.sep + "python",  # AppData/Roaming/Python/Python39 等安装树
    os.sep + "anaconda3",
    os.sep + "miniconda3",
    os.sep + "node_modules",
    os.sep + ".venv",
    os.sep + "venv",
)


def _is_scan_excluded(path: str) -> bool:
    """自动扫描候选是否应被忽略：隔离工作树 / 系统临时残留 / 解释器库目录 / 已删除目录。"""
    if _under_agent_worktrees(path):
        return True
    expanded = Path(path).expanduser()
    # Windows 下 gettempdir 可能是 8.3 短路径：realpath 展开为长路径再比较
    try:
        temp_root = os.path.normcase(os.path.realpath(tempfile.gettempdir()))
        normalized = os.path.normcase(os.path.realpath(str(expanded)))
        in_temp = normalized == temp_root or normalized.startswith(temp_root + os.sep)
    except OSError:
        in_temp = False
    if in_temp and (normalized == temp_root or expanded.name.startswith("tmp")):
        return True
    normed = os.path.normcase(str(expanded))
    if any(fragment in normed for fragment in _SCAN_EXCLUDED_FRAGMENTS):
        return True
    return not Path(path).exists()


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
