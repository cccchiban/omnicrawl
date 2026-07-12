from __future__ import annotations

import json
import secrets
import tempfile
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


# 锁以规范化后的会话根目录为粒度共享，覆盖同一进程中不同
# SessionStore 实例的 JSONL 追加和 index 读改写。跨进程协议仍不在本次范围。
_STORE_LOCKS: dict[Path, threading.RLock] = {}
_STORE_LOCKS_GUARD = threading.Lock()

from .prompt_history import (
    MAX_PROMPT_HISTORY_DISPLAY_CHARS,
    PromptHistoryEntry,
    PromptHistoryStore,
)
from .session_artifacts import (
    TOOL_RESULT_INLINE_OUTPUT_CHARS,
    TOOL_RESULT_LARGE_OUTPUT_CHARS,
    TOOL_RESULT_PREVIEW_CHARS,
    SessionArtifactStore,
    normalize_relative_artifact_path as _normalize_relative_artifact_path,
    redact_sensitive_values as _redact_sensitive_values,
)
from .session_models import (
    COMPACT_SUMMARY_PREFIX,
    EMPTY_SESSION_EVENT_TYPES,
    MESSAGE_EVENT_TYPES,
    MODEL_CONTEXT_EVENT_TYPES,
    SESSION_EVENT_VERSION,
    SESSION_ID_PATTERN,
    SessionEvent,
    SessionIndexEntry,
    SessionState,
    SessionStoreError,
    clean_title as _clean_title,
    ensure_timezone as _ensure_timezone,
    format_datetime as _format_datetime,
    is_relative_to as _is_relative_to,
    normalize_relative_file_path as _normalize_relative_file_path,
    normalize_session_id as _normalize_session_id,
    read_payload_non_negative_int as _read_payload_non_negative_int,
    utc_now as _utc_now,
)
from .session_projection import (
    TOOL_CALL_CONTEXT_PREFIX,
    TOOL_RESULT_CONTEXT_PREFIX,
    event_to_model_message as _event_to_model_message,
)


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
        self._write_lock = _lock_for_root(self.root)
        self.prompt_history = PromptHistoryStore(self.history_path)
        self.artifacts = SessionArtifactStore(self.root, self.artifacts_dir)

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

        with self._write_lock:
            self.ensure()
            timestamp = _utc_now() if now is None else _ensure_timezone(now)
            session_id = self._make_session_id(timestamp)
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

        with self._write_lock:
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

        with self._write_lock:
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

        with self._write_lock:
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

        with self._write_lock:
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

        with self._write_lock:
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

        with self._write_lock:
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
        """兼容原扩展点，并将 artifact 策略委托给独立存储对象。"""

        raw_payload = dict(payload or {})
        if event_type == "tool_result":
            raw_payload = self._prepare_tool_ui_artifact_payload(session_id, raw_payload)
        safe_payload = _redact_sensitive_values(raw_payload)
        if event_type == "tool_result":
            return self._prepare_tool_result_payload(session_id, safe_payload)
        return safe_payload

    def _prepare_tool_result_payload(
        self,
        session_id: str,
        payload: dict[str, Any],
    ) -> dict[str, Any]:
        """兼容旧私有调用；新代码应通过 `_prepare_event_payload` 编排。"""

        return self.artifacts._prepare_tool_result_payload(session_id, payload)

    def _prepare_tool_ui_artifact_payload(
        self,
        session_id: str,
        payload: dict[str, Any],
    ) -> dict[str, Any]:
        """兼容旧私有调用；实际 HTML 持久化由 artifact 子域负责。"""

        return self.artifacts._prepare_tool_ui_artifact_payload(session_id, payload)

    def _write_tool_html_artifact(
        self,
        *,
        session_id: str,
        html: str,
        output_hash: str,
    ) -> str:
        """兼容旧私有调用。"""

        return self.artifacts._write_tool_html_artifact(
            session_id=session_id,
            html=html,
            output_hash=output_hash,
        )

    def _write_tool_result_artifact(
        self,
        *,
        session_id: str,
        output: str,
        output_hash: str,
        truncated: bool,
    ) -> str:
        """兼容旧私有调用。"""

        return self.artifacts._write_tool_result_artifact(
            session_id=session_id,
            output=output,
            output_hash=output_hash,
            truncated=truncated,
        )

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
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                newline="\n",
                dir=self.root,
                prefix=f"{self.index_path.stem}.",
                suffix=".tmp",
                delete=False,
            ) as file:
                temp_path = Path(file.name)
                file.write(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
            temp_path.replace(self.index_path)
        except OSError as exc:
            raise SessionStoreError(f"写入会话索引失败：{self.index_path}，{exc}") from exc


def _lock_for_root(root: Path) -> threading.RLock:
    """返回同一会话根目录共享的可重入进程内写锁。"""

    resolved_root = root.resolve()
    with _STORE_LOCKS_GUARD:
        lock = _STORE_LOCKS.get(resolved_root)
        if lock is None:
            lock = threading.RLock()
            _STORE_LOCKS[resolved_root] = lock
        return lock


__all__ = [
    "COMPACT_SUMMARY_PREFIX",
    "EMPTY_SESSION_EVENT_TYPES",
    "MAX_PROMPT_HISTORY_DISPLAY_CHARS",
    "MESSAGE_EVENT_TYPES",
    "MODEL_CONTEXT_EVENT_TYPES",
    "PromptHistoryEntry",
    "PromptHistoryStore",
    "SESSION_EVENT_VERSION",
    "SESSION_ID_PATTERN",
    "SessionEvent",
    "SessionIndexEntry",
    "SessionState",
    "SessionStore",
    "SessionStoreError",
    "TOOL_CALL_CONTEXT_PREFIX",
    "TOOL_RESULT_CONTEXT_PREFIX",
    "TOOL_RESULT_INLINE_OUTPUT_CHARS",
    "TOOL_RESULT_LARGE_OUTPUT_CHARS",
    "TOOL_RESULT_PREVIEW_CHARS",
]
