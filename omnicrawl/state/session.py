from __future__ import annotations

import hashlib
import json
import re
import secrets
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SESSION_EVENT_VERSION = 1
SESSION_ID_PATTERN = re.compile(r"^\d{8}-\d{6}-[a-f0-9]{6}$")
COMPACT_SUMMARY_PREFIX = "会话压缩摘要：\n"
TOOL_CALL_CONTEXT_PREFIX = "工具调用请求："
TOOL_RESULT_CONTEXT_PREFIX = "工具执行结果："
MESSAGE_EVENT_TYPES = {"user_message", "assistant_message"}
MODEL_CONTEXT_EVENT_TYPES = MESSAGE_EVENT_TYPES | {
    "compact_summary",
    "tool_call_requested",
    "tool_call_denied",
    "tool_result",
}
EMPTY_SESSION_EVENT_TYPES = {"session_started", "session_closed"}
MAX_PROMPT_HISTORY_DISPLAY_CHARS = 4000
TOOL_RESULT_INLINE_OUTPUT_CHARS = 8 * 1024
TOOL_RESULT_LARGE_OUTPUT_CHARS = 128 * 1024
TOOL_RESULT_PREVIEW_CHARS = 1200
_SENSITIVE_KEY_PARTS = (
    "api_key",
    "apikey",
    "authorization",
    "cookie",
    "key",
    "password",
    "secret",
    "token",
)
_SENSITIVE_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)((?:api[_-]?key|apikey|cookie|password|secret|token)"
    r"\s*[:=]\s*[\"']?)([^\"'\s,;]+)"
)
_BEARER_SECRET_PATTERN = re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}")
_PROVIDER_SECRET_PATTERN = re.compile(r"\b(?:sk|ak|ah)-[A-Za-z0-9_-]{24,}\b")


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

        event_count = _read_non_negative_int(data.get("event_count", 0), "event_count")
        message_count = _read_non_negative_int(data.get("message_count", 0), "message_count")
        last_event_type = data.get("last_event_type", "")
        if not isinstance(last_event_type, str):
            raise SessionStoreError("会话索引 last_event_type 必须是字符串。")
        raw_archived_at = data.get("archived_at")
        archived_at = _parse_datetime(raw_archived_at) if raw_archived_at is not None else None

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
            archived_at=archived_at,
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
            "archived_at": _format_datetime(self.archived_at) if self.archived_at is not None else None,
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
    archived_at: datetime | None = None


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
        self.artifacts_dir = self.root / "artifacts"
        self.summaries_dir = self.root / "summaries"
        self.exports_dir = self.root / "exports"
        self.archive_dir = self.root / "archive"
        self.prompt_history = PromptHistoryStore(self.history_path)

    def ensure(self) -> None:
        self.sessions_dir.mkdir(parents=True, exist_ok=True)
        self.artifacts_dir.mkdir(parents=True, exist_ok=True)
        self.summaries_dir.mkdir(parents=True, exist_ok=True)
        self.exports_dir.mkdir(parents=True, exist_ok=True)
        self.archive_dir.mkdir(parents=True, exist_ok=True)
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
            archived_at=None,
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
        safe_payload = self._prepare_event_payload(
            session_id=normalized_id,
            event_type=event_type,
            payload=payload,
        )
        event = SessionEvent.create(
            session_id=normalized_id,
            event_type=event_type,
            payload=safe_payload,
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

    def rename_session(
        self,
        session_id: str,
        title: str,
        *,
        now: datetime | None = None,
    ) -> SessionState:
        """更新会话标题，并把重命名动作写入转录。

        标题属于会话可恢复元数据，不能只改 `index.json`；追加
        `session_renamed` 事件后，即使索引未来需要重建，也能从 JSONL
        中还原用户最后一次命名。
        """

        cleaned_title = _clean_title(title)
        if not cleaned_title:
            raise SessionStoreError("会话标题不能为空。")
        self.append_event(
            session_id,
            "session_renamed",
            {"title": cleaned_title},
            now=now,
        )
        return self.load_session(session_id)

    def export_session_markdown(
        self,
        session_id: str,
        markdown_text: str,
        *,
        now: datetime | None = None,
    ) -> Path:
        """把用户主动导出的会话 Markdown 保存到正式会话导出目录。

        `.agent_tmp/` 仍用于一次性临时导出；这里写入 `.agent_sessions/exports/`
        是为了让恢复型会话拥有长期归档出口。导出事件只记录文件相对路径，
        不把整份 Markdown 再写回 JSONL，避免转录重复膨胀。
        """

        if not isinstance(markdown_text, str) or not markdown_text.strip():
            raise SessionStoreError("导出内容不能为空。")

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        self._entry_by_id(normalized_id)
        timestamp = (_utc_now() if now is None else _ensure_timezone(now)).astimezone(timezone.utc)
        filename = f"chat_export_{normalized_id}_{timestamp.strftime('%Y%m%d_%H%M%S')}.md"
        path = (self.exports_dir / filename).resolve()
        if not _is_relative_to(path, self.root):
            raise SessionStoreError(f"导出路径越界：{filename}")
        try:
            path.write_text(markdown_text, encoding="utf-8")
        except OSError as exc:
            raise SessionStoreError(f"写入会话导出失败：{path}，{exc}") from exc
        relative_path = path.relative_to(self.root).as_posix()
        self.append_event(
            normalized_id,
            "session_exported",
            {"path": relative_path, "format": "markdown"},
            now=timestamp,
        )
        return path

    def archive_session(
        self,
        session_id: str,
        *,
        now: datetime | None = None,
    ) -> SessionState:
        """把会话转录移入归档目录，并从默认会话列表中隐藏。

        归档只移动 JSONL 转录，不移动 artifact、summary 或 export。事件
        payload 保留旧路径和归档路径，后续如需重建索引也能看出用户做过
        归档操作；artifact 路径不变，避免已记录工具结果引用失效。
        """

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        if entry.archived_at is not None:
            raise SessionStoreError(f"会话已在归档中：{normalized_id}")

        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        archive_path = f"archive/{normalized_id}.jsonl"
        self.append_event(
            normalized_id,
            "session_archived",
            {
                "previous_path": entry.path,
                "archive_path": archive_path,
            },
            now=timestamp,
        )
        updated_entry = self._entry_by_id(normalized_id)
        self._move_session_file(updated_entry, archive_path)
        self._replace_entry(
            updated_entry,
            path=archive_path,
            archived_at=timestamp,
            updated_at=timestamp,
        )
        return self.load_session(normalized_id)

    def delete_session(
        self,
        session_id: str,
    ) -> None:
        """彻底删除会话：移除 JSONL 转录、关联 artifact 和索引条目。

        不可逆操作，调用方应自行确认。当前活跃会话不允许删除。
        """

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)

        # 删除 JSONL 转录文件
        session_file = self._session_path(entry)
        if session_file.exists():
            session_file.unlink()

        # 删除关联的 artifact 目录
        artifact_dir = self.artifacts_dir / normalized_id
        if artifact_dir.is_dir():
            import shutil
            shutil.rmtree(artifact_dir, ignore_errors=True)

        # 从索引中移除条目
        entries = self._load_entries()
        remaining = [e for e in entries if e.session_id != normalized_id]
        self._save_entries(remaining)

    def discard_empty_session(self, session_id: str) -> bool:
        """删除还没有真实聊天内容的空会话。

        GUI 启动会先准备一个当前会话，方便后续首条消息直接写入同一个
        `session_id`。如果用户只是打开又关闭窗口，这个占位会话只包含
        `session_started` 等生命周期事件，不应出现在历史列表里。这里由
        存储层统一读取事件流再判断，确保包含用户消息、助手回复、工具结果、
        重命名、导出或归档等任何业务事件的会话都不会被误删。
        """

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        if entry.archived_at is not None or entry.message_count > 0:
            return False

        events = self._read_events(entry)
        if any(event.type not in EMPTY_SESSION_EVENT_TYPES for event in events):
            return False

        session_file = self._session_path(entry)
        if session_file.exists():
            try:
                session_file.unlink()
            except OSError as exc:
                raise SessionStoreError(f"删除空会话转录失败：{session_file}，{exc}") from exc

        artifact_dir = self.artifacts_dir / normalized_id
        if artifact_dir.is_dir():
            import shutil

            shutil.rmtree(artifact_dir, ignore_errors=True)

        entries = self._load_entries()
        self._save_entries([item for item in entries if item.session_id != normalized_id])
        return True

    def unarchive_session(
        self,
        session_id: str,
        *,
        now: datetime | None = None,
    ) -> SessionState:
        """把归档会话恢复为活跃会话，供 `/resume` 继续写入。"""

        self.ensure()
        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        if entry.archived_at is None:
            return self.load_session(normalized_id)

        timestamp = _utc_now() if now is None else _ensure_timezone(now)
        active_path = f"sessions/{normalized_id}.jsonl"
        self.append_event(
            normalized_id,
            "session_unarchived",
            {
                "previous_path": entry.path,
                "active_path": active_path,
            },
            now=timestamp,
        )
        updated_entry = self._entry_by_id(normalized_id)
        self._move_session_file(updated_entry, active_path)
        self._replace_entry(
            updated_entry,
            path=active_path,
            archived_at=None,
            updated_at=timestamp,
        )
        return self.load_session(normalized_id)

    def load_session(self, session_id: str) -> SessionState:
        """读取 JSONL 并重建可恢复的模型历史。"""

        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        events = self._read_events(entry)
        messages: list[dict[str, str]] = []
        for event in events:
            if event.type == "compact_summary":
                # 压缩事件通过追加写落在被压缩历史之后，因此恢复时不能简单丢弃
                # 它之前的所有消息；需要保留压缩发生时仍留在窗口里的最近消息。
                summary_message = _event_to_model_message(event)
                remaining_count = _read_payload_non_negative_int(
                    event.payload.get("remaining_message_count", 0)
                )
                recent_messages = messages[-remaining_count:] if remaining_count else []
                messages = ([summary_message] if summary_message is not None else []) + recent_messages
                continue
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
            archived_at=entry.archived_at,
        )

    def read_session_events(self, session_id: str) -> list[SessionEvent]:
        """读取指定会话的完整事件流，供 UI 回放和正式导出使用。"""

        normalized_id = _normalize_session_id(session_id)
        entry = self._entry_by_id(normalized_id)
        return self._read_events(entry)

    def read_artifact_text(self, session_id: str, artifact_path: str) -> str:
        """读取 `.agent_sessions/artifacts/` 下的文本 artifact。

        API 客户端历史回放需要读取已持久化的 HTML UI artifact。
        这里统一做相对路径、会话归属和目录边界校验，调用方只拿到文本内容，
        不直接拼接本地路径，避免 UI 层绕过 SessionStore 的会话文件约束。
        """

        normalized_id = _normalize_session_id(session_id)
        normalized = _normalize_relative_artifact_path(artifact_path)
        relative_path = Path(normalized)
        if len(relative_path.parts) < 2 or relative_path.parts[1] != normalized_id:
            raise SessionStoreError(f"artifact 路径必须位于当前会话目录：{artifact_path}")
        path = (self.root / normalized).resolve()
        if not _is_relative_to(path, self.root.resolve()):
            raise SessionStoreError(f"artifact 路径越界：{artifact_path}")
        try:
            return path.read_text(encoding="utf-8")
        except FileNotFoundError as exc:
            raise SessionStoreError(f"artifact 不存在：{artifact_path}") from exc
        except OSError as exc:
            raise SessionStoreError(f"读取 artifact 失败：{artifact_path}，{exc}") from exc

    def list_sessions(
        self,
        *,
        workspace_root: Path | None = None,
        project_path: Path | str | None = None,
        limit: int = 10,
        include_archived: bool = False,
        archived_only: bool = False,
    ) -> list[SessionIndexEntry]:
        """按更新时间倒序列出会话，默认可限定在当前工作区或项目路径。"""

        self.ensure()
        entries = self._load_entries()
        filter_path = project_path if project_path is not None else workspace_root
        if filter_path is not None:
            workspace = str(Path(filter_path).expanduser().resolve())
            entries = [entry for entry in entries if entry.workspace_root == workspace]
        if archived_only:
            entries = [entry for entry in entries if entry.archived_at is not None]
        elif not include_archived:
            entries = [entry for entry in entries if entry.archived_at is None]
        entries.sort(key=lambda entry: entry.updated_at, reverse=True)
        return entries[: max(1, min(100, int(limit)))]

    def list_project_paths(self, *, include_archived: bool = True) -> list[str]:
        """列出会话索引中出现过的项目路径，供项目列表扫描使用。"""

        self.ensure()
        entries = self._load_entries()
        if not include_archived:
            entries = [entry for entry in entries if entry.archived_at is None]
        seen: dict[str, str] = {}
        for entry in entries:
            key = entry.workspace_root.casefold()
            if key not in seen:
                seen[key] = entry.workspace_root
        return sorted(seen.values(), key=lambda value: value.casefold())

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

    def _prepare_event_payload(
        self,
        *,
        session_id: str,
        event_type: str,
        payload: dict[str, Any] | None,
    ) -> dict[str, Any]:
        """写入 JSONL 前统一处理事件 payload。

        会话转录是长期可恢复状态，不能简单把工具参数和输出原样塞进去：
        常见密钥字段需要脱敏，大工具输出需要落到 artifact 文件并在 JSONL
        中保留摘要、哈希和相对路径。这样恢复时仍有足够上下文，同时避免
        单行 JSONL 被超大结果拖慢。
        """

        raw_payload = dict(payload or {})
        if event_type == "tool_result":
            raw_payload = self._prepare_tool_ui_artifact_payload(session_id, raw_payload)
        safe_payload = _redact_sensitive_values(raw_payload)
        if event_type == "tool_result":
            return self._prepare_tool_result_payload(session_id, safe_payload)
        return safe_payload

    def _prepare_tool_result_payload(self, session_id: str, payload: dict[str, Any]) -> dict[str, Any]:
        output = payload.get("output")
        if not isinstance(output, str):
            return payload

        output_hash = hashlib.sha256(output.encode("utf-8")).hexdigest()
        payload["output_sha256"] = output_hash
        payload["output_size_chars"] = len(output)
        if len(output) <= TOOL_RESULT_INLINE_OUTPUT_CHARS:
            payload["output"] = _redact_sensitive_text(output)
            payload["storage"] = "inline"
            return payload

        payload["output_preview"] = _redact_sensitive_text(_preview_text(output, TOOL_RESULT_PREVIEW_CHARS))
        payload["output"] = _tool_output_summary(output)
        payload["storage"] = "artifact"
        payload["artifact_truncated"] = len(output) > TOOL_RESULT_LARGE_OUTPUT_CHARS
        payload["artifact_path"] = self._write_tool_result_artifact(
            session_id=session_id,
            output=output,
            output_hash=output_hash,
            truncated=bool(payload["artifact_truncated"]),
        )
        return payload

    def _prepare_tool_ui_artifact_payload(self, session_id: str, payload: dict[str, Any]) -> dict[str, Any]:
        artifact = payload.get("ui_artifact")
        if not isinstance(artifact, dict) or artifact.get("type") != "html":
            return payload

        html = artifact.get("html")
        title = _clean_title(str(artifact.get("title") or "HTML 预览"))
        if not isinstance(html, str) or not html.strip():
            payload["ui_artifact"] = {
                "type": "html",
                "title": title,
                "path": str(artifact.get("path") or ""),
            }
            return payload

        html_hash = hashlib.sha256(html.encode("utf-8")).hexdigest()
        artifact_path = self._write_tool_html_artifact(
            session_id=session_id,
            html=html,
            output_hash=html_hash,
        )
        payload["ui_artifact"] = {
            "type": "html",
            "title": title,
            "path": str(artifact.get("path") or ""),
            "artifact_path": artifact_path,
            "html_size_chars": len(html),
            "html_sha256": html_hash,
        }
        return payload

    def _write_tool_html_artifact(
        self,
        *,
        session_id: str,
        html: str,
        output_hash: str,
    ) -> str:
        """保存 HTML UI artifact，并返回 `.agent_sessions/` 内相对路径。"""

        session_artifacts_dir = (self.artifacts_dir / session_id).resolve()
        if not _is_relative_to(session_artifacts_dir, self.root):
            raise SessionStoreError(f"artifact 目录越界：{session_id}")
        session_artifacts_dir.mkdir(parents=True, exist_ok=True)

        filename = f"html_preview_{output_hash[:16]}.html"
        path = (session_artifacts_dir / filename).resolve()
        if not _is_relative_to(path, self.root):
            raise SessionStoreError(f"HTML artifact 路径越界：{filename}")

        try:
            path.write_text(html, encoding="utf-8")
        except OSError as exc:
            raise SessionStoreError(f"写入 HTML artifact 失败：{path}，{exc}") from exc
        return path.relative_to(self.root).as_posix()

    def _write_tool_result_artifact(
        self,
        *,
        session_id: str,
        output: str,
        output_hash: str,
        truncated: bool,
    ) -> str:
        """保存大工具输出 artifact，并返回 `.agent_sessions/` 内相对路径。"""

        session_artifacts_dir = (self.artifacts_dir / session_id).resolve()
        if not _is_relative_to(session_artifacts_dir, self.root):
            raise SessionStoreError(f"artifact 目录越界：{session_id}")
        session_artifacts_dir.mkdir(parents=True, exist_ok=True)

        filename = f"tool_result_{output_hash[:16]}.txt"
        path = (session_artifacts_dir / filename).resolve()
        if not _is_relative_to(path, self.root):
            raise SessionStoreError(f"artifact 路径越界：{filename}")

        artifact_text = output[:TOOL_RESULT_LARGE_OUTPUT_CHARS] if truncated else output
        artifact_text = _redact_sensitive_text(artifact_text)
        if truncated:
            artifact_text += (
                "\n... artifact 已按 128KB 上限截断，"
                f"原始输出字符数：{len(output)}，sha256：{output_hash}。"
            )
        try:
            path.write_text(artifact_text, encoding="utf-8")
        except OSError as exc:
            raise SessionStoreError(f"写入工具输出 artifact 失败：{path}，{exc}") from exc
        return path.relative_to(self.root).as_posix()

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
            elif event.type == "session_renamed":
                renamed_title = event.payload.get("title", "")
                if isinstance(renamed_title, str) and renamed_title.strip():
                    title = _clean_title(renamed_title)
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
                    archived_at=item.archived_at,
                )
            )
        if not found:
            raise SessionStoreError(f"未找到会话：{entry.session_id}")
        self._save_entries(updated_entries)

    def _replace_entry(
        self,
        entry: SessionIndexEntry,
        *,
        path: str,
        archived_at: datetime | None,
        updated_at: datetime,
    ) -> None:
        """替换索引中的路径和归档状态，保留标题、计数等业务元数据。"""

        normalized_path = _normalize_relative_file_path(path)
        entries = self._load_entries()
        updated_entries: list[SessionIndexEntry] = []
        found = False
        for item in entries:
            if item.session_id != entry.session_id:
                updated_entries.append(item)
                continue
            found = True
            updated_entries.append(
                SessionIndexEntry(
                    session_id=item.session_id,
                    title=item.title,
                    workspace_root=item.workspace_root,
                    path=normalized_path,
                    created_at=item.created_at,
                    updated_at=updated_at,
                    event_count=item.event_count,
                    message_count=item.message_count,
                    last_event_type=item.last_event_type,
                    archived_at=archived_at,
                )
            )
        if not found:
            raise SessionStoreError(f"未找到会话：{entry.session_id}")
        self._save_entries(updated_entries)

    def _move_session_file(self, entry: SessionIndexEntry, destination: str) -> None:
        source_path = self._session_path(entry)
        destination_path = (self.root / _normalize_relative_file_path(destination)).resolve()
        if not _is_relative_to(destination_path, self.root):
            raise SessionStoreError(f"会话归档路径越界：{destination}")
        destination_path.parent.mkdir(parents=True, exist_ok=True)
        if destination_path.exists():
            raise SessionStoreError(f"会话归档目标已存在：{destination}")
        if not source_path.exists():
            raise SessionStoreError(f"会话转录不存在：{source_path}")
        try:
            source_path.replace(destination_path)
        except OSError as exc:
            raise SessionStoreError(f"移动会话转录失败：{source_path} -> {destination_path}，{exc}") from exc

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
            return {"role": "assistant", "content": f"{COMPACT_SUMMARY_PREFIX}{content}"}
    if event.type == "tool_call_requested":
        content = _tool_call_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_call_denied":
        content = _tool_denied_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    if event.type == "tool_result":
        content = _tool_result_context(event.payload)
        return {"role": "assistant", "content": content} if content else None
    return None


def _tool_call_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    arguments = payload.get("arguments", {})
    safe_arguments = arguments if isinstance(arguments, dict) else {}
    arguments_text = json.dumps(safe_arguments, ensure_ascii=False, sort_keys=True)
    return f"{TOOL_CALL_CONTEXT_PREFIX}{tool.strip()} 参数：{arguments_text}"


def _tool_denied_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    reason = payload.get("reason", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    reason_text = reason.strip() if isinstance(reason, str) and reason.strip() else "未批准。"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} 失败，原因：{reason_text}"


def _tool_result_context(payload: dict[str, Any]) -> str:
    tool = payload.get("tool", "")
    if not isinstance(tool, str) or not tool.strip():
        return ""
    ok = bool(payload.get("ok", False))
    status = "成功" if ok else "失败"
    output = payload.get("model_output")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output_preview")
    if not isinstance(output, str) or not output.strip():
        output = payload.get("output", "")
    if not isinstance(output, str):
        output = ""

    artifact_path = payload.get("artifact_path", "")
    artifact_hint = ""
    if isinstance(artifact_path, str) and artifact_path.strip():
        artifact_hint = f"\n完整输出 artifact：{artifact_path.strip()}"
    return f"{TOOL_RESULT_CONTEXT_PREFIX}{tool.strip()} {status}\n{output.strip()}{artifact_hint}".strip()


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


def _normalize_relative_artifact_path(raw_path: Any) -> str:
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise SessionStoreError("artifact 路径必须是非空字符串。")
    normalized = raw_path.strip().replace("\\", "/")
    path = Path(normalized)
    if path.is_absolute() or ".." in path.parts:
        raise SessionStoreError(f"artifact 路径必须是安全相对路径：{raw_path}")
    if not path.parts or path.parts[0].lower() != "artifacts":
        raise SessionStoreError(f"artifact 路径必须位于 artifacts 目录：{raw_path}")
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


def _read_payload_non_negative_int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return 0
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


def _redact_sensitive_values(value: Any) -> Any:
    """递归脱敏会话转录中的常见密钥字段和值。"""

    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                continue
            lowered = key.lower()
            if any(part in lowered for part in _SENSITIVE_KEY_PARTS):
                redacted[key] = "***"
            else:
                redacted[key] = _redact_sensitive_values(item)
        return redacted
    if isinstance(value, list):
        return [_redact_sensitive_values(item) for item in value[:100]]
    if isinstance(value, str):
        return _redact_sensitive_text(value)
    return value


def _redact_sensitive_text(text: str) -> str:
    redacted = _SENSITIVE_ASSIGNMENT_PATTERN.sub(r"\1***", text)
    redacted = _BEARER_SECRET_PATTERN.sub("Bearer ***", redacted)
    redacted = _PROVIDER_SECRET_PATTERN.sub("***", redacted)
    return redacted


def _preview_text(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    head_chars = max_chars // 2
    tail_chars = max_chars - head_chars
    return text[:head_chars] + "\n... 中间内容已省略 ...\n" + text[-tail_chars:]


def _tool_output_summary(output: str) -> str:
    return (
        "工具输出较大，已写入 artifact；"
        f"字符数：{len(output)}，sha256：{hashlib.sha256(output.encode('utf-8')).hexdigest()}。"
    )


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
