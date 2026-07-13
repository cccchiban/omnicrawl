"""会话 JSONL 转录与 index.json 的一致性检查、诊断与索引重建。

本模块只处理可确定、可幂等的索引修复问题，不改写历史 JSONL，
也不删除无法判断归属的 artifact 或损坏转录。SESSION-DEBT-004 的
崩溃恢复入口建立在这里：先报告，再按调用方确认写回索引。
"""

from __future__ import annotations

import re
import shutil
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable

from .session_models import (
    MESSAGE_EVENT_TYPES,
    SESSION_ID_PATTERN,
    SessionEvent,
    SessionIndexEntry,
    SessionStoreError,
    clean_title,
    format_datetime,
    is_relative_to,
    normalize_relative_file_path,
    normalize_session_id,
    utc_now,
)


# 诊断码保持稳定，便于测试、日志和未来 API/TUI 展示复用。
ISSUE_EVENT_COUNT_MISMATCH = "event_count_mismatch"
ISSUE_MESSAGE_COUNT_MISMATCH = "message_count_mismatch"
ISSUE_TITLE_MISMATCH = "title_mismatch"
ISSUE_LAST_EVENT_MISMATCH = "last_event_type_mismatch"
ISSUE_UPDATED_AT_MISMATCH = "updated_at_mismatch"
ISSUE_PATH_MISMATCH = "path_mismatch"
ISSUE_ARCHIVED_MISMATCH = "archived_at_mismatch"
ISSUE_WORKSPACE_MISMATCH = "workspace_root_mismatch"
ISSUE_MISSING_TRANSCRIPT = "missing_transcript"
ISSUE_ORPHAN_TRANSCRIPT = "orphan_transcript"
ISSUE_ORPHAN_INDEX = "orphan_index"
ISSUE_ORPHAN_ARTIFACT = "orphan_artifact"
ISSUE_UNREADABLE_TRANSCRIPT = "unreadable_transcript"
ISSUE_EMPTY_TRANSCRIPT = "empty_transcript"
ISSUE_MISSING_WORKSPACE = "missing_workspace_root"

SEVERITY_ERROR = "error"
SEVERITY_WARNING = "warning"
SEVERITY_INFO = "info"

_SESSION_FILENAME_PATTERN = re.compile(
    r"^(?P<session_id>\d{8}-\d{6}-[a-f0-9]{6})\.jsonl$",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class SessionConsistencyIssue:
    """单条一致性诊断。

    `repairable=True` 表示 `rebuild_index(apply=True)` 能在不删除
    用户数据的前提下，通过重写 index.json 消除该问题。
    """

    code: str
    severity: str
    message: str
    session_id: str | None = None
    path: str | None = None
    details: dict[str, Any] = field(default_factory=dict)
    repairable: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "code": self.code,
            "severity": self.severity,
            "message": self.message,
            "session_id": self.session_id,
            "path": self.path,
            "details": dict(self.details),
            "repairable": self.repairable,
        }


@dataclass(frozen=True)
class SessionConsistencyReport:
    """一次一致性扫描或索引重建预览的结果。"""

    issues: tuple[SessionConsistencyIssue, ...]
    scanned_index_entries: int
    scanned_transcripts: int
    scanned_artifact_dirs: int
    proposed_entries: tuple[SessionIndexEntry, ...] = ()
    applied: bool = False
    backup_path: str | None = None

    @property
    def ok(self) -> bool:
        return not any(issue.severity == SEVERITY_ERROR for issue in self.issues)

    @property
    def repairable_issues(self) -> tuple[SessionConsistencyIssue, ...]:
        return tuple(issue for issue in self.issues if issue.repairable)

    def to_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "applied": self.applied,
            "backup_path": self.backup_path,
            "scanned_index_entries": self.scanned_index_entries,
            "scanned_transcripts": self.scanned_transcripts,
            "scanned_artifact_dirs": self.scanned_artifact_dirs,
            "issue_count": len(self.issues),
            "repairable_count": len(self.repairable_issues),
            "issues": [issue.to_dict() for issue in self.issues],
            "proposed_entries": [entry.to_dict() for entry in self.proposed_entries],
        }


@dataclass(frozen=True)
class TranscriptLocation:
    """磁盘上发现的一份会话转录。"""

    session_id: str
    relative_path: str
    absolute_path: Path


def discover_transcripts(root: Path) -> list[TranscriptLocation]:
    """扫描 sessions/ 与 archive/ 下的 JSONL 转录。

    只认符合 session_id 命名规则的文件名，忽略导出、临时文件和其他
    非会话数据，避免把无关文件纳入索引重建。
    """

    resolved_root = root.resolve()
    found: list[TranscriptLocation] = []
    for relative_dir in ("sessions", "archive"):
        directory = resolved_root / relative_dir
        if not directory.is_dir():
            continue
        for path in sorted(directory.glob("*.jsonl")):
            match = _SESSION_FILENAME_PATTERN.match(path.name)
            if match is None:
                continue
            session_id = match.group("session_id")
            # 规范化大小写，保证与索引里的 session_id 规则一致。
            try:
                session_id = normalize_session_id(session_id)
            except SessionStoreError:
                continue
            relative_path = f"{relative_dir}/{path.name}"
            found.append(
                TranscriptLocation(
                    session_id=session_id,
                    relative_path=relative_path,
                    absolute_path=path.resolve(),
                )
            )
    return found


def discover_artifact_session_ids(artifacts_dir: Path) -> list[str]:
    """列出 artifact 目录下看起来像 session_id 的一级子目录。"""

    if not artifacts_dir.is_dir():
        return []
    session_ids: list[str] = []
    for child in sorted(artifacts_dir.iterdir()):
        if not child.is_dir():
            continue
        if not SESSION_ID_PATTERN.match(child.name):
            continue
        try:
            session_ids.append(normalize_session_id(child.name))
        except SessionStoreError:
            continue
    return session_ids


def read_events_from_path(path: Path, session_id: str) -> list[SessionEvent]:
    """从指定 JSONL 路径读取合法事件，跳过损坏行。

    委托 session_records 解码与版本迁移；坏行不阻断其余有效历史。
    """

    from .session_records import read_session_events_with_diagnostics

    result = read_session_events_with_diagnostics(
        path,
        session_id=session_id,
        relative_path=path.name,
    )
    return list(result.events)


def build_index_entry_from_events(
    *,
    session_id: str,
    relative_path: str,
    events: list[SessionEvent],
) -> SessionIndexEntry:
    """根据完整事件流重建索引核心字段。

    标题更新规则与 SessionStore._update_entry_after_event 保持一致：
    首条用户消息可覆盖默认标题，之后的 session_renamed 覆盖最终标题。
    归档状态以文件实际位置为准：位于 archive/ 则视为已归档。
    """

    normalized_id = normalize_session_id(session_id)
    normalized_path = normalize_relative_file_path(relative_path)
    if not events:
        raise SessionStoreError(f"无法从空转录重建索引：{normalized_id}")

    title = "新会话"
    workspace_root = ""
    message_count = 0
    archived_at: datetime | None = None
    first_user_title_applied = False

    for event in events:
        if event.type == "session_started":
            workspace = event.payload.get("workspace_root", "")
            if isinstance(workspace, str) and workspace.strip():
                workspace_root = workspace.strip()
            started_title = event.payload.get("title", "")
            if isinstance(started_title, str) and started_title.strip():
                title = clean_title(started_title) or title
        if not first_user_title_applied and event.type == "user_message":
            content = event.payload.get("content", "")
            if isinstance(content, str) and content.strip():
                title = clean_title(content)
                first_user_title_applied = True
        elif event.type == "session_renamed":
            renamed_title = event.payload.get("title", "")
            if isinstance(renamed_title, str) and renamed_title.strip():
                title = clean_title(renamed_title)
        if event.type in MESSAGE_EVENT_TYPES:
            message_count += 1
        if event.type == "session_archived":
            archived_at = event.created_at
        elif event.type == "session_unarchived":
            archived_at = None

    if normalized_path.startswith("archive/"):
        if archived_at is None:
            archived_at = events[-1].created_at
    else:
        # 活跃目录中的文件视为未归档，即使历史中出现过归档事件。
        archived_at = None

    if not workspace_root:
        # 索引模型要求非空 workspace；用可识别占位值，并在扫描时单独告警。
        workspace_root = "(unknown)"

    return SessionIndexEntry(
        session_id=normalized_id,
        title=title,
        workspace_root=workspace_root,
        path=normalized_path,
        created_at=events[0].created_at,
        updated_at=events[-1].created_at,
        event_count=len(events),
        message_count=message_count,
        last_event_type=events[-1].type,
        archived_at=archived_at,
    )


def compare_index_entry(
    current: SessionIndexEntry,
    expected: SessionIndexEntry,
) -> list[SessionConsistencyIssue]:
    """比较现有索引条目与从转录重建的期望值。"""

    issues: list[SessionConsistencyIssue] = []
    session_id = current.session_id

    def _add(
        code: str,
        message: str,
        *,
        current_value: Any,
        expected_value: Any,
        severity: str = SEVERITY_ERROR,
    ) -> None:
        issues.append(
            SessionConsistencyIssue(
                code=code,
                severity=severity,
                message=message,
                session_id=session_id,
                path=current.path,
                details={
                    "current": current_value,
                    "expected": expected_value,
                },
                repairable=True,
            )
        )

    if current.path != expected.path:
        _add(
            ISSUE_PATH_MISMATCH,
            f"会话路径不一致：索引为 {current.path}，转录位于 {expected.path}。",
            current_value=current.path,
            expected_value=expected.path,
        )
    if current.event_count != expected.event_count:
        _add(
            ISSUE_EVENT_COUNT_MISMATCH,
            f"事件数不一致：索引 {current.event_count}，转录 {expected.event_count}。",
            current_value=current.event_count,
            expected_value=expected.event_count,
        )
    if current.message_count != expected.message_count:
        _add(
            ISSUE_MESSAGE_COUNT_MISMATCH,
            f"消息数不一致：索引 {current.message_count}，转录 {expected.message_count}。",
            current_value=current.message_count,
            expected_value=expected.message_count,
        )
    if current.title != expected.title:
        _add(
            ISSUE_TITLE_MISMATCH,
            f"标题不一致：索引为 {current.title!r}，转录推导为 {expected.title!r}。",
            current_value=current.title,
            expected_value=expected.title,
            severity=SEVERITY_WARNING,
        )
    if current.last_event_type != expected.last_event_type:
        _add(
            ISSUE_LAST_EVENT_MISMATCH,
            f"最后事件类型不一致：索引为 {current.last_event_type!r}，转录为 {expected.last_event_type!r}。",
            current_value=current.last_event_type,
            expected_value=expected.last_event_type,
        )
    if current.updated_at != expected.updated_at:
        _add(
            ISSUE_UPDATED_AT_MISMATCH,
            "更新时间与转录最后事件时间不一致。",
            current_value=format_datetime(current.updated_at),
            expected_value=format_datetime(expected.updated_at),
            severity=SEVERITY_WARNING,
        )
    if current.workspace_root != expected.workspace_root:
        _add(
            ISSUE_WORKSPACE_MISMATCH,
            "工作区路径与 session_started 事件不一致。",
            current_value=current.workspace_root,
            expected_value=expected.workspace_root,
            severity=SEVERITY_WARNING,
        )
    if current.archived_at != expected.archived_at:
        _add(
            ISSUE_ARCHIVED_MISMATCH,
            "归档状态与转录路径/事件不一致。",
            current_value=format_datetime(current.archived_at) if current.archived_at else None,
            expected_value=format_datetime(expected.archived_at) if expected.archived_at else None,
        )
    return issues


def build_consistency_report(
    *,
    root: Path,
    index_entries: Iterable[SessionIndexEntry],
    read_events,
) -> SessionConsistencyReport:
    """扫描索引、转录和 artifact 目录，生成诊断与建议索引。

    `read_events(path, session_id)` 由调用方注入，便于复用 SessionStore
    的路径边界检查，并在测试中注入故障。
    """

    resolved_root = root.resolve()
    entries = list(index_entries)
    index_by_id = {entry.session_id: entry for entry in entries}
    transcripts = discover_transcripts(resolved_root)
    transcripts_by_id: dict[str, TranscriptLocation] = {}
    issues: list[SessionConsistencyIssue] = []

    for location in transcripts:
        previous = transcripts_by_id.get(location.session_id)
        if previous is not None:
            # 同 ID 同时出现在 sessions 与 archive 时，优先保留 archive
            # 之外的活跃副本，并报告冲突，避免自动删除任一文件。
            preferred = location if location.relative_path.startswith("sessions/") else previous
            other = previous if preferred is location else location
            transcripts_by_id[location.session_id] = preferred
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_PATH_MISMATCH,
                    severity=SEVERITY_ERROR,
                    message=(
                        f"会话 {location.session_id} 同时存在多份转录："
                        f"{previous.relative_path} 与 {location.relative_path}。"
                    ),
                    session_id=location.session_id,
                    path=other.relative_path,
                    details={
                        "preferred_path": preferred.relative_path,
                        "other_path": other.relative_path,
                    },
                    repairable=False,
                )
            )
            continue
        transcripts_by_id[location.session_id] = location

    rebuilt_entries: list[SessionIndexEntry] = []
    known_session_ids: set[str] = set()

    # 1) 以磁盘转录为权威来源重建可恢复条目。
    for session_id, location in sorted(transcripts_by_id.items()):
        known_session_ids.add(session_id)
        try:
            events = list(read_events(location.absolute_path, session_id))
        except SessionStoreError as exc:
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_UNREADABLE_TRANSCRIPT,
                    severity=SEVERITY_ERROR,
                    message=str(exc),
                    session_id=session_id,
                    path=location.relative_path,
                    repairable=False,
                )
            )
            # 读失败时若索引仍有条目，保留原索引，避免修复时误删。
            if session_id in index_by_id:
                rebuilt_entries.append(index_by_id[session_id])
            continue

        if not events:
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_EMPTY_TRANSCRIPT,
                    severity=SEVERITY_WARNING,
                    message=f"转录文件无可解析事件：{location.relative_path}",
                    session_id=session_id,
                    path=location.relative_path,
                    repairable=False,
                )
            )
            if session_id in index_by_id:
                rebuilt_entries.append(index_by_id[session_id])
            continue

        expected = build_index_entry_from_events(
            session_id=session_id,
            relative_path=location.relative_path,
            events=events,
        )
        if expected.workspace_root == "(unknown)":
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_MISSING_WORKSPACE,
                    severity=SEVERITY_WARNING,
                    message=f"转录缺少 session_started.workspace_root：{session_id}",
                    session_id=session_id,
                    path=location.relative_path,
                    repairable=False,
                )
            )

        current = index_by_id.get(session_id)
        if current is None:
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_ORPHAN_TRANSCRIPT,
                    severity=SEVERITY_ERROR,
                    message=f"发现未进入索引的转录：{location.relative_path}",
                    session_id=session_id,
                    path=location.relative_path,
                    details={"event_count": expected.event_count},
                    repairable=True,
                )
            )
        else:
            issues.extend(compare_index_entry(current, expected))
        rebuilt_entries.append(expected)

    # 2) 索引指向不存在或无法匹配磁盘转录的条目。
    for entry in entries:
        if entry.session_id in transcripts_by_id:
            continue
        known_session_ids.add(entry.session_id)
        absolute = (resolved_root / entry.path).resolve()
        if not is_relative_to(absolute, resolved_root):
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_ORPHAN_INDEX,
                    severity=SEVERITY_ERROR,
                    message=f"索引路径越界：{entry.path}",
                    session_id=entry.session_id,
                    path=entry.path,
                    repairable=False,
                )
            )
            continue
        if absolute.exists():
            # 文件存在但不在标准 sessions/archive 命名扫描结果中。
            issues.append(
                SessionConsistencyIssue(
                    code=ISSUE_PATH_MISMATCH,
                    severity=SEVERITY_ERROR,
                    message=f"索引路径不在标准会话目录扫描结果中：{entry.path}",
                    session_id=entry.session_id,
                    path=entry.path,
                    repairable=False,
                )
            )
            rebuilt_entries.append(entry)
            continue
        issues.append(
            SessionConsistencyIssue(
                code=ISSUE_MISSING_TRANSCRIPT,
                severity=SEVERITY_ERROR,
                message=f"索引指向的转录不存在：{entry.path}",
                session_id=entry.session_id,
                path=entry.path,
                repairable=True,
            )
        )
        # 缺失转录的索引条目默认不进入 proposed_entries，修复时会移除。
        # 这不会删除任何磁盘文件，只是让 index 与真实转录对齐。

    # 3) 孤立 artifact 目录只报告，不自动删除。
    artifact_ids = discover_artifact_session_ids(resolved_root / "artifacts")
    for artifact_session_id in artifact_ids:
        if artifact_session_id in known_session_ids or artifact_session_id in transcripts_by_id:
            continue
        if artifact_session_id in index_by_id:
            continue
        issues.append(
            SessionConsistencyIssue(
                code=ISSUE_ORPHAN_ARTIFACT,
                severity=SEVERITY_WARNING,
                message=f"发现无对应会话索引/转录的 artifact 目录：{artifact_session_id}",
                session_id=artifact_session_id,
                path=f"artifacts/{artifact_session_id}",
                repairable=False,
            )
        )

    # 保持与 list_sessions 相近的稳定顺序：按 updated_at 倒序。
    rebuilt_entries.sort(key=lambda item: item.updated_at, reverse=True)

    return SessionConsistencyReport(
        issues=tuple(issues),
        scanned_index_entries=len(entries),
        scanned_transcripts=len(transcripts),
        scanned_artifact_dirs=len(artifact_ids),
        proposed_entries=tuple(rebuilt_entries),
        applied=False,
        backup_path=None,
    )


def write_index_backup(index_path: Path, *, now: datetime | None = None) -> Path | None:
    """在覆盖 index.json 前生成同目录备份；索引不存在时返回 None。"""

    if not index_path.exists():
        return None
    timestamp = (now or utc_now()).strftime("%Y%m%d_%H%M%S")
    backup_path = index_path.with_name(f"index.json.bak.{timestamp}")
    # 极端并发下时间戳碰撞时追加后缀，避免覆盖上一份备份。
    suffix = 1
    while backup_path.exists():
        backup_path = index_path.with_name(f"index.json.bak.{timestamp}.{suffix}")
        suffix += 1
    try:
        shutil.copy2(index_path, backup_path)
    except OSError as exc:
        raise SessionStoreError(f"备份会话索引失败：{backup_path}，{exc}") from exc
    return backup_path


__all__ = [
    "ISSUE_ARCHIVED_MISMATCH",
    "ISSUE_EMPTY_TRANSCRIPT",
    "ISSUE_EVENT_COUNT_MISMATCH",
    "ISSUE_LAST_EVENT_MISMATCH",
    "ISSUE_MESSAGE_COUNT_MISMATCH",
    "ISSUE_MISSING_TRANSCRIPT",
    "ISSUE_MISSING_WORKSPACE",
    "ISSUE_ORPHAN_ARTIFACT",
    "ISSUE_ORPHAN_INDEX",
    "ISSUE_ORPHAN_TRANSCRIPT",
    "ISSUE_PATH_MISMATCH",
    "ISSUE_TITLE_MISMATCH",
    "ISSUE_UNREADABLE_TRANSCRIPT",
    "ISSUE_UPDATED_AT_MISMATCH",
    "ISSUE_WORKSPACE_MISMATCH",
    "SEVERITY_ERROR",
    "SEVERITY_INFO",
    "SEVERITY_WARNING",
    "SessionConsistencyIssue",
    "SessionConsistencyReport",
    "TranscriptLocation",
    "build_consistency_report",
    "build_index_entry_from_events",
    "compare_index_entry",
    "discover_artifact_session_ids",
    "discover_transcripts",
    "read_events_from_path",
    "write_index_backup",
]
