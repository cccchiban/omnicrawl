from __future__ import annotations

import json
import re
import secrets
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SESSION_EVENT_VERSION = 1
SESSION_ID_PATTERN = re.compile(r"^\d{8}-\d{6}-[a-f0-9]{6}$")
MODEL_CONTEXT_EVENT_TYPES = {"user_message", "assistant_message", "compact_summary"}
MESSAGE_EVENT_TYPES = {"user_message", "assistant_message"}
MAX_PROMPT_HISTORY_DISPLAY_CHARS = 4000


class SessionStoreError(RuntimeError):
    """会话索引、JSONL 转录或恢复数据不合法时抛出。"""


@dataclass(frozen=True)
class SessionEvent:
    """会话 JSONL 中的一条事件。

    所有持久化事件都使用同一个 envelope，方便后续增加 tool_result
    artifact、压缩摘要或分支会话时保持兼容。
    """

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
            session_id=_normalize_session_id(session_id),
            event_id=secrets.token_hex(12),
            parent_id=parent_id.strip() if isinstance(parent_id, str) and parent_id.strip() else None,
            type=_normalize_event_type(event_type),
            created_at=_utc_now() if now is None else _ensure_timezone(now),
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
            session_id=_normalize_session_id(raw_session_id),
            event_id=raw_event_id.strip(),
            parent_id=parent_id.strip() if isinstance(parent_id, str) and parent_id.strip() else None,
            type=_normalize_event_type(raw_type),
            created_at=_parse_datetime(raw_created_at),
            payload=payload,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "session_id": self.session_id,
            "event_id": self.event_id,
            "parent_id": self.parent_id,
            "type": self.type,
            "created_at": _format_datetime(self.created_at),
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

        event_count = _read_non_negative_int(data.get("event_count", 0), "event_count")
        message_count = _read_non_negative_int(data.get("message_count", 0), "message_count")
        last_event_type = data.get("last_event_type", "")
        if not isinstance(last_event_type, str):
            raise SessionStoreError("会话索引 last_event_type 必须是字符串。")

        return cls(
            session_id=_normalize_session_id(raw_session_id),
            title=raw_title.strip(),
            workspace_root=raw_workspace_root.strip(),
            path=_normalize_relative_file_path(raw_path),
            created_at=_parse_datetime(raw_created_at),
            updated_at=_parse_datetime(raw_updated_at),
            event_count=event_count,
            message_count=message_count,
            last_event_type=last_event_type.strip(),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "session_id": self.session_id,
            "title": self.title,
            "workspace_root": self.workspace_root,
            "path": self.path,
            "created_at": _format_datetime(self.created_at),
            "updated_at": _format_datetime(self.updated_at),
            "event_count": self.event_count,
            "message_count": self.message_count,
            "last_event_type": self.last_event_type,
        }


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


@dataclass(frozen=True)
class PromptHistoryEntry:
    """`.agent_sessions/history.jsonl` 中的一条用户提示历史。

    提示历史只服务于输入复用和检索，不参与会话恢复，也不会自动注入模型上下文。
    `display` 保存用户可复用的提示文本；如果后续需要支持大段粘贴的独立引用，
    可以把完整粘贴内容放进 `pasted_contents`，保持主记录轻量。
    """

    display: str
    timestamp: int
    project: str
    session_id: str
    pasted_contents: dict[str, Any]

    @classmethod
    def create(
        cls,
        *,
        display: str,
        project: Path,
        session_id: str,
        pasted_contents: dict[str, Any] | None = None,
        now: datetime | None = None,
    ) -> "PromptHistoryEntry":
        return cls(
            display=_clean_prompt_display(display),
            timestamp=_datetime_to_millis(_utc_now() if now is None else _ensure_timezone(now)),
            project=str(project.resolve()),
            session_id=_normalize_session_id(session_id),
            pasted_contents=dict(pasted_contents or {}),
        )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "PromptHistoryEntry":
        display = data.get("display", "")
        timestamp = data.get("timestamp")
        project = data.get("project", "")
        session_id = data.get("session_id", "")
        pasted_contents = data.get("pasted_contents", {})

        if not isinstance(display, str) or not display.strip():
            raise SessionStoreError("提示历史 display 必须是非空字符串。")
        if isinstance(timestamp, bool) or not isinstance(timestamp, int) or timestamp < 0:
            raise SessionStoreError("提示历史 timestamp 必须是非负整数。")
        if not isinstance(project, str) or not project.strip():
            raise SessionStoreError("提示历史 project 必须是非空字符串。")
        if not isinstance(pasted_contents, dict):
            raise SessionStoreError("提示历史 pasted_contents 必须是 JSON 对象。")

        return cls(
            display=_clean_prompt_display(display),
            timestamp=timestamp,
            project=project.strip(),
            session_id=_normalize_session_id(session_id),
            pasted_contents=pasted_contents,
        )

    @property
    def created_at(self) -> datetime:
        return datetime.fromtimestamp(self.timestamp / 1000, tz=timezone.utc)

    def to_dict(self) -> dict[str, Any]:
        return {
            "display": self.display,
            "timestamp": self.timestamp,
            "project": self.project,
            "session_id": self.session_id,
            "pasted_contents": self.pasted_contents,
        }


class PromptHistoryStore:
    """用户提示历史 JSONL 存储。

    和完整会话转录不同，提示历史只有用户提交的提示文本与归属信息。
    查询时默认按当前项目过滤、按时间倒序返回，并对相同文本去重，避免上箭头
    复用和 `/history` 列表里充满重复输入。
    """

    def __init__(self, path: Path) -> None:
        self.path = path.resolve()
        self.root = self.path.parent

    def ensure(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        if not self.path.exists():
            self.path.write_text("", encoding="utf-8")

    def append(
        self,
        *,
        display: str,
        project: Path,
        session_id: str,
        pasted_contents: dict[str, Any] | None = None,
        now: datetime | None = None,
    ) -> PromptHistoryEntry | None:
        """追加用户提示；空提示会被忽略并返回 None。"""

        cleaned = _clean_prompt_display(display)
        if not cleaned:
            return None

        self.ensure()
        entry = PromptHistoryEntry.create(
            display=cleaned,
            project=project,
            session_id=session_id,
            pasted_contents=pasted_contents,
            now=now,
        )
        try:
            with self.path.open("a", encoding="utf-8", newline="\n") as file:
                file.write(json.dumps(entry.to_dict(), ensure_ascii=False, separators=(",", ":")))
                file.write("\n")
        except OSError as exc:
            raise SessionStoreError(f"写入提示历史失败：{self.path}，{exc}") from exc
        return entry

    def search(
        self,
        *,
        project: Path | None = None,
        session_id: str | None = None,
        query: str = "",
        limit: int = 20,
    ) -> list[PromptHistoryEntry]:
        """查询提示历史，返回按时间倒序排列的去重结果。"""

        entries = self._read_entries()
        if project is not None:
            project_root = str(project.resolve())
            entries = [entry for entry in entries if entry.project == project_root]
        if session_id is not None and session_id.strip():
            normalized_session_id = _normalize_session_id(session_id)
            entries = [entry for entry in entries if entry.session_id == normalized_session_id]

        keyword = query.strip().casefold()
        if keyword:
            entries = [entry for entry in entries if keyword in entry.display.casefold()]

        seen_displays: set[str] = set()
        results: list[PromptHistoryEntry] = []
        ordered_entries = sorted(
            enumerate(entries),
            key=lambda item: (item[1].timestamp, item[0]),
            reverse=True,
        )
        for _line_index, entry in ordered_entries:
            dedupe_key = entry.display.casefold()
            if dedupe_key in seen_displays:
                continue
            seen_displays.add(dedupe_key)
            results.append(entry)
            if len(results) >= max(1, min(100, int(limit))):
                break
        return results

    def _read_entries(self) -> list[PromptHistoryEntry]:
        if not self.path.exists():
            return []
        try:
            lines = self.path.read_text(encoding="utf-8").splitlines()
        except UnicodeDecodeError as exc:
            raise SessionStoreError(f"提示历史不是 UTF-8 文本：{self.path}") from exc
        except OSError as exc:
            raise SessionStoreError(f"读取提示历史失败：{self.path}，{exc}") from exc

        entries: list[PromptHistoryEntry] = []
        for line in lines:
            if not line.strip():
                continue
            try:
                data = json.loads(line)
                if not isinstance(data, dict):
                    continue
                entries.append(PromptHistoryEntry.from_dict(data))
            except (json.JSONDecodeError, SessionStoreError):
                continue
        return entries


class SessionStore:
    """基于 JSONL 转录和 index.json 的会话存储。

    第一版保持线性会话链：每个会话一个 JSONL 文件，每条事件追加写入。
    恢复时只把语义上能进入模型上下文的事件还原成 `_history` 消息，UI
    通知和中断状态保留在索引与事件流中，不污染模型上下文。
    """

    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.index_path = self.root / "index.json"
        self.history_path = self.root / "history.jsonl"
        self.sessions_dir = self.root / "sessions"
        self.summaries_dir = self.root / "summaries"
        self.exports_dir = self.root / "exports"
        self.prompt_history = PromptHistoryStore(self.history_path)

    def ensure(self) -> None:
        self.sessions_dir.mkdir(parents=True, exist_ok=True)
        self.summaries_dir.mkdir(parents=True, exist_ok=True)
        self.exports_dir.mkdir(parents=True, exist_ok=True)
        if not self.index_path.exists():
            self._save_entries([])
        self.prompt_history.ensure()

    def start_session(
        self,
        workspace_root: Path,
        *,
        title: str = "",
        now: datetime | None = None,
    ) -> SessionState:
        """创建新会话，并立即写入 `session_started` 事件。"""

        self.ensure()
        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        session_id = self._make_session_id(timestamp)
        path = self.sessions_dir / f"{session_id}.jsonl"
        workspace = str(workspace_root.resolve())
        entry = SessionIndexEntry(
            session_id=session_id,
            title=_clean_title(title) or "新会话",
            workspace_root=workspace,
            path=f"sessions/{session_id}.jsonl",
            created_at=timestamp,
            updated_at=timestamp,
            event_count=0,
            message_count=0,
            last_event_type="",
        )
        entries = self._load_entries()
        entries.append(entry)
        self._save_entries(entries)
        self.append_event(
            session_id,
            "session_started",
            {
                "workspace_root": workspace,
                "title": entry.title,
            },
            now=timestamp,
        )
        return self.load_session(session_id)

    def append_event(
        self,
        session_id: str,
        event_type: str,
        payload: dict[str, Any] | None = None,
        *,
        parent_id: str | None = None,
        now: datetime | None = None,
    ) -> SessionEvent:
        """向指定会话追加一个事件并更新索引。"""

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        event = SessionEvent.create(
            session_id=normalized_id,
            event_type=event_type,
            payload=payload,
            parent_id=parent_id,
            now=now,
        )

        path = self._session_path(entry)
        try:
            with path.open("a", encoding="utf-8", newline="\n") as file:
                file.write(json.dumps(event.to_dict(), ensure_ascii=False, separators=(",", ":")))
                file.write("\n")
        except OSError as exc:
            raise SessionStoreError(f"写入会话转录失败：{path}，{exc}") from exc

        self._update_entry_after_event(entry, event)
        return event

    def load_session(self, session_id: str) -> SessionState:
        """读取 JSONL 并重建可恢复的模型历史。"""

        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        events = self._read_events(entry)
        messages: list[dict[str, str]] = []
        for event in events:
            message = _event_to_model_message(event)
            if message is not None:
                messages.append(message)
        last_event_type = events[-1].type if events else entry.last_event_type
        return SessionState(
            session_id=entry.session_id,
            title=entry.title,
            workspace_root=entry.workspace_root,
            path=self._session_path(entry),
            created_at=entry.created_at,
            updated_at=entry.updated_at,
            messages=messages,
            last_event_type=last_event_type,
            event_count=len(events),
        )

    def list_sessions(
        self,
        *,
        workspace_root: Path | None = None,
        limit: int = 10,
    ) -> list[SessionIndexEntry]:
        """按更新时间倒序列出会话，默认可限定在当前工作区。"""

        self.ensure()
        entries = self._load_entries()
        if workspace_root is not None:
            workspace = str(workspace_root.resolve())
            entries = [entry for entry in entries if entry.workspace_root == workspace]
        entries.sort(key=lambda entry: entry.updated_at, reverse=True)
        return entries[: max(1, min(100, int(limit)))]

    def append_prompt_history(
        self,
        *,
        display: str,
        workspace_root: Path,
        session_id: str,
        pasted_contents: dict[str, Any] | None = None,
        now: datetime | None = None,
    ) -> PromptHistoryEntry | None:
        """记录用户提示历史；该历史不进入模型上下文。"""

        self.ensure()
        return self.prompt_history.append(
            display=display,
            project=workspace_root,
            session_id=session_id,
            pasted_contents=pasted_contents,
            now=now,
        )

    def search_prompt_history(
        self,
        *,
        workspace_root: Path | None = None,
        session_id: str | None = None,
        query: str = "",
        limit: int = 20,
    ) -> list[PromptHistoryEntry]:
        """按当前项目、会话或关键词查询用户提示历史。"""

        self.ensure()
        return self.prompt_history.search(
            project=workspace_root,
            session_id=session_id,
            query=query,
            limit=limit,
        )

    def _make_session_id(self, timestamp: datetime) -> str:
        existing_ids = {entry.session_id for entry in self._load_entries()}
        while True:
            session_id = f"{timestamp.astimezone(timezone.utc).strftime('%Y%m%d-%H%M%S')}-{secrets.token_hex(3)}"
            if session_id not in existing_ids and not (self.sessions_dir / f"{session_id}.jsonl").exists():
                return session_id

    def _entry_by_id(self, session_id: str) -> SessionIndexEntry:
        entries = self._load_entries()
        for entry in entries:
            if entry.session_id == session_id:
                return entry
        raise SessionStoreError(f"未找到会话：{session_id}")

    def _update_entry_after_event(self, entry: SessionIndexEntry, event: SessionEvent) -> None:
        entries = self._load_entries()
        updated_entries: list[SessionIndexEntry] = []
        found = False
        for item in entries:
            if item.session_id != entry.session_id:
                updated_entries.append(item)
                continue
            found = True
            title = item.title
            if item.message_count == 0 and event.type == "user_message":
                content = event.payload.get("content", "")
                if isinstance(content, str) and content.strip():
                    title = _clean_title(content)
            updated_entries.append(
                SessionIndexEntry(
                    session_id=item.session_id,
                    title=title,
                    workspace_root=item.workspace_root,
                    path=item.path,
                    created_at=item.created_at,
                    updated_at=event.created_at,
                    event_count=item.event_count + 1,
                    message_count=item.message_count + (1 if event.type in MESSAGE_EVENT_TYPES else 0),
                    last_event_type=event.type,
                )
            )
        if not found:
            raise SessionStoreError(f"未找到会话：{entry.session_id}")
        self._save_entries(updated_entries)

    def _read_events(self, entry: SessionIndexEntry) -> list[SessionEvent]:
        path = self._session_path(entry)
        if not path.exists():
            return []
        events: list[SessionEvent] = []
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except UnicodeDecodeError as exc:
            raise SessionStoreError(f"会话转录不是 UTF-8 文本：{path}") from exc
        except OSError as exc:
            raise SessionStoreError(f"读取会话转录失败：{path}，{exc}") from exc
        for line in lines:
            if not line.strip():
                continue
            try:
                data = json.loads(line)
                if not isinstance(data, dict):
                    continue
                event = SessionEvent.from_dict(data)
            except (json.JSONDecodeError, SessionStoreError):
                continue
            if event.session_id == entry.session_id:
                events.append(event)
        return events

    def _session_path(self, entry: SessionIndexEntry) -> Path:
        path = (self.root / entry.path).resolve()
        if not _is_relative_to(path, self.root):
            raise SessionStoreError(f"会话路径越界：{entry.path}")
        return path

    def _load_entries(self) -> list[SessionIndexEntry]:
        if not self.index_path.exists():
            return []
        try:
            data = json.loads(self.index_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            raise SessionStoreError(f"会话索引不是合法 JSON：{self.index_path}") from exc
        except UnicodeDecodeError as exc:
            raise SessionStoreError(f"会话索引不是 UTF-8 文本：{self.index_path}") from exc
        except OSError as exc:
            raise SessionStoreError(f"读取会话索引失败：{self.index_path}，{exc}") from exc
        sessions = data.get("sessions", []) if isinstance(data, dict) else []
        if not isinstance(sessions, list):
            raise SessionStoreError("会话索引顶层字段 sessions 必须是列表。")
        return [SessionIndexEntry.from_dict(item) for item in sessions if isinstance(item, dict)]

    def _save_entries(self, entries: list[SessionIndexEntry]) -> None:
        self.root.mkdir(parents=True, exist_ok=True)
        data = {"sessions": [entry.to_dict() for entry in entries]}
        temp_path = self.index_path.with_suffix(".json.tmp")
        try:
            temp_path.write_text(
                json.dumps(data, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            temp_path.replace(self.index_path)
        except OSError as exc:
            raise SessionStoreError(f"写入会话索引失败：{self.index_path}，{exc}") from exc


def _event_to_model_message(event: SessionEvent) -> dict[str, str] | None:
    if event.type == "user_message":
        content = event.payload.get("content", "")
        return {"role": "user", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "assistant_message":
        content = event.payload.get("content", "")
        return {"role": "assistant", "content": content} if isinstance(content, str) and content.strip() else None
    if event.type == "compact_summary":
        content = event.payload.get("content", "")
        if isinstance(content, str) and content.strip():
            return {"role": "assistant", "content": f"会话压缩摘要：\n{content}"}
    return None


def _normalize_session_id(raw_session_id: Any) -> str:
    if not isinstance(raw_session_id, str) or not raw_session_id.strip():
        raise SessionStoreError("session_id 必须是非空字符串。")
    session_id = raw_session_id.strip()
    if not SESSION_ID_PATTERN.match(session_id):
        raise SessionStoreError(f"session_id 格式无效：{raw_session_id}")
    return session_id


def _normalize_event_type(raw_event_type: Any) -> str:
    if not isinstance(raw_event_type, str) or not raw_event_type.strip():
        raise SessionStoreError("会话事件 type 必须是非空字符串。")
    event_type = raw_event_type.strip()
    if not re.match(r"^[a-z][a-z0-9_]{0,63}$", event_type):
        raise SessionStoreError(f"会话事件 type 格式无效：{raw_event_type}")
    return event_type


def _normalize_relative_file_path(raw_path: Any) -> str:
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise SessionStoreError("会话路径必须是非空字符串。")
    normalized = raw_path.strip().replace("\\", "/")
    path = Path(normalized)
    if path.is_absolute() or ".." in path.parts:
        raise SessionStoreError(f"会话路径必须是安全相对路径：{raw_path}")
    if path.suffix.lower() != ".jsonl":
        raise SessionStoreError(f"会话转录文件必须是 JSONL：{raw_path}")
    return normalized


def _parse_datetime(raw_value: Any) -> datetime:
    if not isinstance(raw_value, str) or not raw_value.strip():
        raise SessionStoreError("时间戳必须是非空字符串。")
    value = raw_value.strip()
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise SessionStoreError(f"时间戳格式无效：{raw_value}") from exc
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


def _read_non_negative_int(value: Any, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise SessionStoreError(f"会话索引 {name} 必须是非负整数。")
    return value


def _clean_title(value: str) -> str:
    title = " ".join(value.strip().split())
    if len(title) > 60:
        return title[:57] + "..."
    return title


def _clean_prompt_display(value: str) -> str:
    prompt = value.replace("\r\n", "\n").replace("\r", "\n").strip()
    if len(prompt) > MAX_PROMPT_HISTORY_DISPLAY_CHARS:
        return prompt[:MAX_PROMPT_HISTORY_DISPLAY_CHARS] + "\n... 提示历史已截断。"
    return prompt


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
