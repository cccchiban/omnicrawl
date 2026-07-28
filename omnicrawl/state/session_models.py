"""会话事件、索引和恢复状态的数据模型与格式校验。"""

from __future__ import annotations

import re
import secrets
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SESSION_EVENT_VERSION = 1
SESSION_ID_PATTERN = re.compile(r"^\d{8}-\d{6}-[a-f0-9]{6}$")
COMPACT_SUMMARY_PREFIX = "会话压缩摘要：\n"
MESSAGE_EVENT_TYPES = {"user_message", "assistant_message"}
MODEL_CONTEXT_EVENT_TYPES = MESSAGE_EVENT_TYPES | {
    "compact_summary",
    "tool_call_requested",
    "tool_call_denied",
    "tool_result",
}
EMPTY_SESSION_EVENT_TYPES = {"session_started", "session_closed"}
SUBAGENT_EVENT_TYPES = {
    "subagent_batch_created",
    "subagent_task_queued",
    "subagent_task_started",
    "subagent_task_waiting_approval",
    "subagent_task_completed",
    "subagent_task_failed",
    "subagent_task_cancelled",
}


class SessionStoreError(RuntimeError):
    """会话索引、JSONL 转录或恢复数据不合法时抛出。"""


@dataclass(frozen=True)
class SessionEvent:
    """会话 JSONL 中的一条版本化事件。"""

    version: int
    session_id: str
    event_id: str
    parent_id: str | None
    type: str
    created_at: datetime
    payload: dict[str, Any]

    @classmethod
    def create(
        cls,
        *,
        session_id: str,
        event_type: str,
        payload: dict[str, Any] | None = None,
        parent_id: str | None = None,
        now: datetime | None = None,
    ) -> "SessionEvent":
        return cls(
            version=SESSION_EVENT_VERSION,
            session_id=normalize_session_id(session_id),
            event_id=secrets.token_hex(12),
            parent_id=parent_id.strip() if isinstance(parent_id, str) and parent_id.strip() else None,
            type=normalize_event_type(event_type),
            created_at=utc_now() if now is None else ensure_timezone(now),
            payload=dict(payload or {}),
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "SessionEvent":
        try:
            raw_version = data["version"]
            raw_session_id = data["session_id"]
            raw_event_id = data["event_id"]
            raw_type = data["type"]
            raw_created_at = data["created_at"]
        except KeyError as exc:
            raise SessionStoreError(f"会话事件缺少字段：{exc.args[0]}。") from exc

        if raw_version != SESSION_EVENT_VERSION:
            raise SessionStoreError(f"暂不支持的会话事件版本：{raw_version}。")
        if not isinstance(raw_event_id, str) or not raw_event_id.strip():
            raise SessionStoreError("会话事件 event_id 必须是非空字符串。")

        parent_id = data.get("parent_id")
        if parent_id is not None and not isinstance(parent_id, str):
            raise SessionStoreError("会话事件 parent_id 必须是字符串或 null。")
        payload = data.get("payload", {})
        if not isinstance(payload, dict):
            raise SessionStoreError("会话事件 payload 必须是 JSON 对象。")

        return cls(
            version=raw_version,
            session_id=normalize_session_id(raw_session_id),
            event_id=raw_event_id.strip(),
            parent_id=parent_id.strip() if isinstance(parent_id, str) and parent_id.strip() else None,
            type=normalize_event_type(raw_type),
            created_at=parse_datetime(raw_created_at),
            payload=payload,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "session_id": self.session_id,
            "event_id": self.event_id,
            "parent_id": self.parent_id,
            "type": self.type,
            "created_at": format_datetime(self.created_at),
            "payload": self.payload,
        }


@dataclass(frozen=True)
class SessionIndexEntry:
    """`.agent_sessions/index.json` 中的会话索引条目。"""

    session_id: str
    title: str
    workspace_root: str
    path: str
    created_at: datetime
    updated_at: datetime
    event_count: int
    message_count: int
    last_event_type: str
    archived_at: datetime | None = None

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "SessionIndexEntry":
        try:
            raw_session_id = data["session_id"]
            raw_title = data["title"]
            raw_workspace_root = data["workspace_root"]
            raw_path = data["path"]
            raw_created_at = data["created_at"]
            raw_updated_at = data["updated_at"]
        except KeyError as exc:
            raise SessionStoreError(f"会话索引缺少字段：{exc.args[0]}。") from exc

        if not isinstance(raw_title, str):
            raise SessionStoreError("会话索引 title 必须是字符串。")
        if not isinstance(raw_workspace_root, str) or not raw_workspace_root.strip():
            raise SessionStoreError("会话索引 workspace_root 必须是非空字符串。")
        event_count = read_non_negative_int(data.get("event_count", 0), "event_count")
        message_count = read_non_negative_int(data.get("message_count", 0), "message_count")
        last_event_type = data.get("last_event_type", "")
        if not isinstance(last_event_type, str):
            raise SessionStoreError("会话索引 last_event_type 必须是字符串。")
        raw_archived_at = data.get("archived_at")

        return cls(
            session_id=normalize_session_id(raw_session_id),
            title=raw_title.strip(),
            workspace_root=raw_workspace_root.strip(),
            path=normalize_relative_file_path(raw_path),
            created_at=parse_datetime(raw_created_at),
            updated_at=parse_datetime(raw_updated_at),
            event_count=event_count,
            message_count=message_count,
            last_event_type=last_event_type.strip(),
            archived_at=parse_datetime(raw_archived_at) if raw_archived_at is not None else None,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "title": self.title,
            "workspace_root": self.workspace_root,
            "path": self.path,
            "created_at": format_datetime(self.created_at),
            "updated_at": format_datetime(self.updated_at),
            "event_count": self.event_count,
            "message_count": self.message_count,
            "last_event_type": self.last_event_type,
            "archived_at": format_datetime(self.archived_at) if self.archived_at is not None else None,
        }


@dataclass(frozen=True)
class SessionUndoPlan:
    """最近一轮的稳定事件集合，供副作用预检后原子提交回退。"""

    session_id: str
    event_ids: tuple[str, ...]
    events: tuple[SessionEvent, ...]
    user_event_id: str
    assistant_event_id: str | None
    kind: str


@dataclass(frozen=True)
class SessionState:
    """从 JSONL 转录恢复出的会话状态。"""

    session_id: str
    title: str
    workspace_root: str
    path: Path
    created_at: datetime
    updated_at: datetime
    messages: list[dict[str, str]]
    last_event_type: str
    event_count: int
    archived_at: datetime | None = None


def normalize_session_id(raw_session_id: Any) -> str:
    if not isinstance(raw_session_id, str) or not raw_session_id.strip():
        raise SessionStoreError("session_id 必须是非空字符串。")
    session_id = raw_session_id.strip()
    if not SESSION_ID_PATTERN.match(session_id):
        raise SessionStoreError(f"session_id 格式无效：{raw_session_id}")
    return session_id


def normalize_event_type(raw_event_type: Any) -> str:
    if not isinstance(raw_event_type, str) or not raw_event_type.strip():
        raise SessionStoreError("会话事件 type 必须是非空字符串。")
    event_type = raw_event_type.strip()
    if not re.match(r"^[a-z][a-z0-9_]{0,63}$", event_type):
        raise SessionStoreError(f"会话事件 type 格式无效：{raw_event_type}")
    return event_type


def normalize_relative_file_path(raw_path: Any) -> str:
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise SessionStoreError("会话路径必须是非空字符串。")
    normalized = raw_path.strip().replace("\\", "/")
    path = Path(normalized)
    if path.is_absolute() or ".." in path.parts:
        raise SessionStoreError(f"会话路径必须是安全相对路径：{raw_path}")
    if path.suffix.lower() != ".jsonl":
        raise SessionStoreError(f"会话转录文件必须是 JSONL：{raw_path}")
    return normalized


def parse_datetime(raw_value: Any) -> datetime:
    if not isinstance(raw_value, str) or not raw_value.strip():
        raise SessionStoreError("时间戳必须是非空字符串。")
    try:
        parsed = datetime.fromisoformat(raw_value.strip().replace("Z", "+00:00"))
    except ValueError as exc:
        raise SessionStoreError(f"时间戳格式无效：{raw_value}") from exc
    return ensure_timezone(parsed)


def format_datetime(value: datetime) -> str:
    return ensure_timezone(value).isoformat()


def datetime_to_millis(value: datetime) -> int:
    return int(ensure_timezone(value).timestamp() * 1000)


def ensure_timezone(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def read_non_negative_int(value: Any, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise SessionStoreError(f"会话索引 {name} 必须是非负整数。")
    return value


def read_payload_non_negative_int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return 0
    return value


def clean_title(value: str) -> str:
    title = " ".join(value.strip().split())
    if len(title) > 60:
        return title[:57] + "..."
    return title


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
