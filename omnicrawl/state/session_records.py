"""会话 JSONL 与提示历史的解码、版本迁移和损坏诊断。

SESSION-DEBT-005 / 006：
- 按版本分发解码，把可识别的旧格式纯函数迁移到当前模型；
- 未知版本、JSON 损坏、字段错误产生结构化诊断，不再静默吞掉；
- 默认不改写磁盘上的原始转录或 history.jsonl。
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .session_artifacts import redact_sensitive_text
from .session_models import (
    SESSION_EVENT_VERSION,
    SessionEvent,
    SessionStoreError,
)


LOGGER = logging.getLogger(__name__)

# 诊断码保持稳定，便于 API/TUI 和测试断言。
DIAG_INVALID_JSON = "invalid_json"
DIAG_NOT_OBJECT = "not_object"
DIAG_INVALID_FIELDS = "invalid_fields"
DIAG_UNSUPPORTED_VERSION = "unsupported_event_version"
DIAG_SESSION_ID_MISMATCH = "session_id_mismatch"
DIAG_TRAILING_INCOMPLETE = "trailing_incomplete"
DIAG_LEGACY_MIGRATED = "legacy_event_migrated"
DIAG_PROMPT_INVALID = "invalid_prompt_history"

SEVERITY_ERROR = "error"
SEVERITY_WARNING = "warning"
SEVERITY_INFO = "info"

# 索引 schema 版本与事件版本独立演进。
SESSION_INDEX_SCHEMA_VERSION = 1
SUPPORTED_EVENT_VERSIONS = frozenset({0, 1})
# version 缺失但其余核心字段齐全时，视为正式 version 字段落地前的遗留格式。
_LEGACY_UNVERSIONED = "unversioned"


@dataclass(frozen=True)
class SessionRecordDiagnostic:
    """单条 JSONL 记录的结构化诊断。"""

    code: str
    severity: str
    message: str
    path: str | None = None
    line_no: int | None = None
    session_id: str | None = None
    recoverable: bool = True
    details: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "code": self.code,
            "severity": self.severity,
            "message": self.message,
            "path": self.path,
            "line_no": self.line_no,
            "session_id": self.session_id,
            "recoverable": self.recoverable,
            "details": dict(self.details),
        }


@dataclass(frozen=True)
class SessionEventReadResult:
    """一次转录读取结果：有效事件 + 诊断。"""

    events: tuple[SessionEvent, ...]
    diagnostics: tuple[SessionRecordDiagnostic, ...]

    @property
    def has_errors(self) -> bool:
        return any(item.severity == SEVERITY_ERROR for item in self.diagnostics)

    def to_dict(self) -> dict[str, Any]:
        return {
            "event_count": len(self.events),
            "diagnostic_count": len(self.diagnostics),
            "has_errors": self.has_errors,
            "diagnostics": [item.to_dict() for item in self.diagnostics],
            "events": [event.to_dict() for event in self.events],
        }


def migrate_event_dict(data: dict[str, Any]) -> tuple[dict[str, Any], str | None]:
    """把可识别的旧事件字典迁移为当前版本字段。

    返回 `(migrated_dict, migration_tag)`。`migration_tag` 为 None 表示
    已是当前版本；非 None 表示发生了纯内存迁移，磁盘文件保持不变。

    支持范围：
    - version == 1：当前格式；
    - version == 0：原型格式，字段与 v1 相同，仅版本号不同；
    - 缺失 version 但具备 v1 核心字段：正式 version 字段落地前的遗留格式。
    """

    if not isinstance(data, dict):
        raise SessionStoreError("会话事件必须是 JSON 对象。")

    raw_version = data.get("version", _LEGACY_UNVERSIONED)
    if raw_version == SESSION_EVENT_VERSION:
        return dict(data), None

    if raw_version == 0:
        migrated = dict(data)
        migrated["version"] = SESSION_EVENT_VERSION
        # 原型格式曾用 id 作为事件主键；兼容读入后统一为 event_id。
        if "event_id" not in migrated and isinstance(migrated.get("id"), str):
            migrated["event_id"] = migrated.pop("id")
        return migrated, "v0"

    if raw_version is _LEGACY_UNVERSIONED:
        required = ("session_id", "event_id", "type", "created_at")
        if all(key in data for key in required):
            migrated = dict(data)
            migrated["version"] = SESSION_EVENT_VERSION
            return migrated, "unversioned"

    if raw_version is _LEGACY_UNVERSIONED:
        raise SessionStoreError("会话事件缺少 version，且不具备可迁移的遗留字段。")
    raise SessionStoreError(f"暂不支持的会话事件版本：{raw_version}。")


def decode_session_event_dict(
    data: Any,
    *,
    path: str | None = None,
    line_no: int | None = None,
    expected_session_id: str | None = None,
) -> tuple[SessionEvent | None, list[SessionRecordDiagnostic]]:
    """解码单个事件对象，返回事件或诊断。"""

    diagnostics: list[SessionRecordDiagnostic] = []
    if not isinstance(data, dict):
        diagnostics.append(
            SessionRecordDiagnostic(
                code=DIAG_NOT_OBJECT,
                severity=SEVERITY_ERROR,
                message="会话事件 JSON 顶层必须是对象。",
                path=path,
                line_no=line_no,
                recoverable=True,
            )
        )
        return None, diagnostics

    try:
        migrated, migration_tag = migrate_event_dict(data)
        event = SessionEvent.from_dict(migrated)
    except SessionStoreError as exc:
        message = str(exc)
        code = (
            DIAG_UNSUPPORTED_VERSION
            if "暂不支持的会话事件版本" in message
            else DIAG_INVALID_FIELDS
        )
        diagnostics.append(
            SessionRecordDiagnostic(
                code=code,
                severity=SEVERITY_ERROR,
                message=message,
                path=path,
                line_no=line_no,
                session_id=_safe_session_id(data),
                recoverable=True,
                details={"raw_version": data.get("version")},
            )
        )
        return None, diagnostics

    if migration_tag is not None:
        diagnostics.append(
            SessionRecordDiagnostic(
                code=DIAG_LEGACY_MIGRATED,
                severity=SEVERITY_INFO,
                message=f"已将遗留事件格式迁移到 version={SESSION_EVENT_VERSION}（仅内存，未改写磁盘）。",
                path=path,
                line_no=line_no,
                session_id=event.session_id,
                recoverable=True,
                details={"migration": migration_tag, "from_version": data.get("version")},
            )
        )

    if expected_session_id is not None and event.session_id != expected_session_id:
        diagnostics.append(
            SessionRecordDiagnostic(
                code=DIAG_SESSION_ID_MISMATCH,
                severity=SEVERITY_WARNING,
                message=(
                    f"事件 session_id 与转录归属不一致：事件为 {event.session_id}，"
                    f"期望为 {expected_session_id}。"
                ),
                path=path,
                line_no=line_no,
                session_id=event.session_id,
                recoverable=True,
                details={
                    "event_session_id": event.session_id,
                    "expected_session_id": expected_session_id,
                },
            )
        )
        return None, diagnostics

    return event, diagnostics


def decode_session_event_line(
    line: str,
    *,
    path: str | None = None,
    line_no: int | None = None,
    expected_session_id: str | None = None,
    is_last_nonempty_line: bool = False,
    file_ends_with_newline: bool = True,
) -> tuple[SessionEvent | None, list[SessionRecordDiagnostic]]:
    """解码 JSONL 单行。"""

    stripped = line.strip()
    if not stripped:
        return None, []

    try:
        data = json.loads(line)
    except json.JSONDecodeError as exc:
        # 尾部半行通常来自崩溃中断写入；中间损坏更可能是真实损坏。
        trailing = is_last_nonempty_line and not file_ends_with_newline
        code = DIAG_TRAILING_INCOMPLETE if trailing else DIAG_INVALID_JSON
        severity = SEVERITY_WARNING if trailing else SEVERITY_ERROR
        snippet = redact_sensitive_text(stripped[:120])
        return None, [
            SessionRecordDiagnostic(
                code=code,
                severity=severity,
                message=(
                    "转录末尾存在未完成的 JSON 行，可能由写入中断导致。"
                    if trailing
                    else f"会话转录 JSON 损坏：第 {line_no} 行。"
                ),
                path=path,
                line_no=line_no,
                session_id=expected_session_id,
                recoverable=True,
                details={
                    "json_error": str(exc),
                    "snippet": snippet,
                    "trailing": trailing,
                },
            )
        ]

    return decode_session_event_dict(
        data,
        path=path,
        line_no=line_no,
        expected_session_id=expected_session_id,
    )


def read_session_events_with_diagnostics(
    path: Path,
    *,
    session_id: str | None = None,
    relative_path: str | None = None,
) -> SessionEventReadResult:
    """读取转录文件并收集诊断；坏行不阻断其余有效事件。"""

    if not path.exists():
        return SessionEventReadResult(events=(), diagnostics=())

    try:
        raw_text = path.read_text(encoding="utf-8")
    except UnicodeDecodeError as exc:
        raise SessionStoreError(f"会话转录不是 UTF-8 文本：{path}") from exc
    except OSError as exc:
        raise SessionStoreError(f"读取会话转录失败：{path}，{exc}") from exc

    lines = raw_text.splitlines()
    file_ends_with_newline = raw_text.endswith("\n") or raw_text == ""
    display_path = relative_path or str(path)
    nonempty_indexes = [index for index, line in enumerate(lines) if line.strip()]
    last_nonempty = nonempty_indexes[-1] if nonempty_indexes else None

    events: list[SessionEvent] = []
    diagnostics: list[SessionRecordDiagnostic] = []
    for index, line in enumerate(lines):
        line_no = index + 1
        event, line_diagnostics = decode_session_event_line(
            line,
            path=display_path,
            line_no=line_no,
            expected_session_id=session_id,
            is_last_nonempty_line=index == last_nonempty,
            file_ends_with_newline=file_ends_with_newline,
        )
        diagnostics.extend(line_diagnostics)
        if event is not None:
            events.append(event)

    log_record_diagnostics(diagnostics)
    return SessionEventReadResult(events=tuple(events), diagnostics=tuple(diagnostics))


def parse_index_document(data: Any) -> tuple[list[dict[str, Any]], int]:
    """解析 index.json 顶层文档，返回 sessions 列表与 schema 版本。

    缺失 schema_version 时按 1 处理，兼容既有磁盘数据。
    """

    if not isinstance(data, dict):
        raise SessionStoreError("会话索引顶层必须是 JSON 对象。")
    raw_version = data.get("schema_version", SESSION_INDEX_SCHEMA_VERSION)
    if isinstance(raw_version, bool) or not isinstance(raw_version, int):
        raise SessionStoreError("会话索引 schema_version 必须是整数。")
    if raw_version != SESSION_INDEX_SCHEMA_VERSION:
        raise SessionStoreError(
            f"暂不支持的会话索引 schema 版本：{raw_version}。"
            f"当前支持版本：{SESSION_INDEX_SCHEMA_VERSION}。"
        )
    sessions = data.get("sessions", [])
    if not isinstance(sessions, list):
        raise SessionStoreError("会话索引顶层字段 sessions 必须是列表。")
    return [item for item in sessions if isinstance(item, dict)], raw_version


def build_index_document(entries: list[Any]) -> dict[str, Any]:
    """构造带 schema_version 的索引文档。"""

    return {
        "schema_version": SESSION_INDEX_SCHEMA_VERSION,
        "sessions": [
            entry.to_dict() if hasattr(entry, "to_dict") else entry
            for entry in entries
        ],
    }


def _safe_session_id(data: dict[str, Any]) -> str | None:
    value = data.get("session_id")
    return value if isinstance(value, str) and value.strip() else None


def log_record_diagnostics(diagnostics: list[SessionRecordDiagnostic] | tuple[SessionRecordDiagnostic, ...]) -> None:
    """把诊断写入日志；消息本身已避免嵌入原始秘密字段，snippet 也已脱敏。"""

    for item in diagnostics:
        if item.severity == SEVERITY_INFO:
            LOGGER.info(
                "session record diagnostic [%s] %s (path=%s line=%s)",
                item.code,
                item.message,
                item.path,
                item.line_no,
            )
        elif item.severity == SEVERITY_WARNING:
            LOGGER.warning(
                "session record diagnostic [%s] %s (path=%s line=%s)",
                item.code,
                item.message,
                item.path,
                item.line_no,
            )
        else:
            LOGGER.error(
                "session record diagnostic [%s] %s (path=%s line=%s)",
                item.code,
                item.message,
                item.path,
                item.line_no,
            )


__all__ = [
    "DIAG_INVALID_FIELDS",
    "DIAG_INVALID_JSON",
    "DIAG_LEGACY_MIGRATED",
    "DIAG_NOT_OBJECT",
    "DIAG_PROMPT_INVALID",
    "DIAG_SESSION_ID_MISMATCH",
    "DIAG_TRAILING_INCOMPLETE",
    "DIAG_UNSUPPORTED_VERSION",
    "SESSION_INDEX_SCHEMA_VERSION",
    "SEVERITY_ERROR",
    "SEVERITY_INFO",
    "SEVERITY_WARNING",
    "SUPPORTED_EVENT_VERSIONS",
    "SessionEventReadResult",
    "SessionRecordDiagnostic",
    "build_index_document",
    "decode_session_event_dict",
    "decode_session_event_line",
    "migrate_event_dict",
    "parse_index_document",
    "read_session_events_with_diagnostics",
    "log_record_diagnostics",
]
