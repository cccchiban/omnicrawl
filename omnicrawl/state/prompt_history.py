"""用户提示历史的数据模型与 JSONL 存储。"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .session_artifacts import redact_sensitive_text, redact_sensitive_values
from .session_models import (
    SessionStoreError,
    datetime_to_millis,
    ensure_timezone,
    normalize_session_id,
    utc_now,
)


MAX_PROMPT_HISTORY_DISPLAY_CHARS = 4000


@dataclass(frozen=True)
class PromptHistoryEntry:
    """`.agent_sessions/history.jsonl` 中的一条用户提示历史。"""

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
            display=redact_sensitive_text(clean_prompt_display(display)),
            timestamp=datetime_to_millis(utc_now() if now is None else ensure_timezone(now)),
            project=str(project.resolve()),
            session_id=normalize_session_id(session_id),
            pasted_contents=redact_sensitive_values(dict(pasted_contents or {})),
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
            # 历史文件可能来自旧版本；读取时也返回安全的展示数据，
            # 但不改写用户已有 JSONL，避免隐式迁移或扩大数据变更范围。
            display=redact_sensitive_text(clean_prompt_display(display)),
            timestamp=timestamp,
            project=project.strip(),
            session_id=normalize_session_id(session_id),
            pasted_contents=redact_sensitive_values(pasted_contents),
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
    """用户提示历史 JSONL 存储。"""

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

        cleaned = clean_prompt_display(display)
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
            normalized_session_id = normalize_session_id(session_id)
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


def clean_prompt_display(value: str) -> str:
    prompt = value.replace("\r\n", "\n").replace("\r", "\n").strip()
    if len(prompt) > MAX_PROMPT_HISTORY_DISPLAY_CHARS:
        return prompt[:MAX_PROMPT_HISTORY_DISPLAY_CHARS] + "\n... 提示历史已截断。"
    return prompt
