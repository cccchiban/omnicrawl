"""会话工具输出 artifact、脱敏和路径安全策略。"""

from __future__ import annotations

import hashlib
import re
from pathlib import Path
from typing import Any


# 这些字段是凭据容器的稳定语义名，而不是任意包含 "key" 的业务字段。
# 保持集合精确可避免把 keyboard、monkey、key_count 等正常用户数据误删。
_SENSITIVE_FIELD_NAMES = frozenset(
    {
        "api_key",
        "apikey",
        "access_key",
        "secret_key",
        "authorization",
        "cookie",
        "password",
        "secret",
        "token",
        "access_token",
        "refresh_token",
        "id_token",
    }
)

from .session_models import SessionStoreError, clean_title, is_relative_to


TOOL_RESULT_INLINE_OUTPUT_CHARS = 8 * 1024
TOOL_RESULT_LARGE_OUTPUT_CHARS = 128 * 1024
TOOL_RESULT_PREVIEW_CHARS = 1200
_SENSITIVE_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)((?:api[_-]?key|apikey|access[_-]?key|secret[_-]?key|"
    r"access[_-]?token|refresh[_-]?token|id[_-]?token|cookie|password|secret|token)"
    r"\s*[:=]\s*[\"']?)([^\"'\s,;]+)"
)
_AUTHORIZATION_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)(\bauthorization\s*[:=]\s*[\"']?)(?:Bearer\s+)?([^\"'\s,;]+)"
)
_SENSITIVE_HTML_ATTRIBUTE_PATTERN = re.compile(
    r"(?i)(\b(?:api[_-]?key|apikey|access[_-]?key|secret[_-]?key|"
    r"access[_-]?token|refresh[_-]?token|id[_-]?token|authorization|cookie|"
    r"password|secret|token)\s*=\s*[\"'])(.*?)([\"'])"
)
_SENSITIVE_JSON_PROPERTY_PATTERN = re.compile(
    r"(?i)([\"'](?:api[_-]?key|apikey|access[_-]?key|secret[_-]?key|"
    r"access[_-]?token|refresh[_-]?token|id[_-]?token|authorization|cookie|"
    r"password|secret|token)[\"']\s*:\s*[\"'])(.*?)([\"'])"
)
_BEARER_SECRET_PATTERN = re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}")
_PROVIDER_SECRET_PATTERN = re.compile(r"\b(?:sk|ak|ah)-[A-Za-z0-9_-]{24,}\b")


class SessionArtifactStore:
    """管理会话工具输出文件，并生成可写入 JSONL 的安全元数据。"""

    def __init__(self, root: Path, artifacts_dir: Path) -> None:
        self.root = root.resolve()
        self.artifacts_dir = artifacts_dir.resolve()

    def prepare_event_payload(
        self,
        *,
        session_id: str,
        event_type: str,
        payload: dict[str, Any] | None,
    ) -> dict[str, Any]:
        raw_payload = dict(payload or {})
        if event_type == "tool_result":
            raw_payload = self._prepare_tool_ui_artifact_payload(session_id, raw_payload)
        safe_payload = redact_sensitive_values(raw_payload)
        if event_type == "tool_result":
            return self._prepare_tool_result_payload(session_id, safe_payload)
        return safe_payload

    def read_text(self, session_id: str, artifact_path: str) -> str:
        normalized = normalize_relative_artifact_path(artifact_path)
        relative_path = Path(normalized)
        if len(relative_path.parts) < 2 or relative_path.parts[1] != session_id:
            raise SessionStoreError(f"artifact 路径必须位于当前会话目录：{artifact_path}")
        path = (self.root / normalized).resolve()
        if not is_relative_to(path, self.root):
            raise SessionStoreError(f"artifact 路径越界：{artifact_path}")
        try:
            return path.read_text(encoding="utf-8")
        except FileNotFoundError as exc:
            raise SessionStoreError(f"artifact 不存在：{artifact_path}") from exc
        except OSError as exc:
            raise SessionStoreError(f"读取 artifact 失败：{artifact_path}，{exc}") from exc

    def _prepare_tool_result_payload(self, session_id: str, payload: dict[str, Any]) -> dict[str, Any]:
        output = payload.get("output")
        if not isinstance(output, str):
            return payload
        output_hash = hashlib.sha256(output.encode("utf-8")).hexdigest()
        payload["output_sha256"] = output_hash
        payload["output_size_chars"] = len(output)
        if len(output) <= TOOL_RESULT_INLINE_OUTPUT_CHARS:
            payload["output"] = redact_sensitive_text(output)
            payload["storage"] = "inline"
            return payload

        payload["output_preview"] = redact_sensitive_text(preview_text(output, TOOL_RESULT_PREVIEW_CHARS))
        payload["output"] = tool_output_summary(output)
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
        title = clean_title(str(artifact.get("title") or "HTML 预览"))
        if not isinstance(html, str) or not html.strip():
            payload["ui_artifact"] = {
                "type": "html",
                "title": title,
                "path": str(artifact.get("path") or ""),
            }
            return payload

        # HTML artifact 会被历史回放 API 读取，因此它与普通文本 artifact
        # 一样必须在写盘前脱敏。哈希与长度只描述实际可读取的持久化内容，
        # 不能继续暗示原始（可能含秘密）HTML 已被保留。
        persisted_html = redact_sensitive_html(html)
        html_hash = hashlib.sha256(persisted_html.encode("utf-8")).hexdigest()
        artifact_path = self._write_tool_html_artifact(
            session_id=session_id,
            html=persisted_html,
            output_hash=html_hash,
        )
        payload["ui_artifact"] = {
            "type": "html",
            "title": title,
            "path": str(artifact.get("path") or ""),
            "artifact_path": artifact_path,
            "html_size_chars": len(persisted_html),
            "html_sha256": html_hash,
            "redacted": True,
        }
        return payload

    def _write_tool_html_artifact(self, *, session_id: str, html: str, output_hash: str) -> str:
        session_artifacts_dir = self._ensure_session_artifacts_dir(session_id)
        filename = f"html_preview_{output_hash[:16]}.html"
        path = (session_artifacts_dir / filename).resolve()
        if not is_relative_to(path, self.root):
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
        session_artifacts_dir = self._ensure_session_artifacts_dir(session_id)
        filename = f"tool_result_{output_hash[:16]}.txt"
        path = (session_artifacts_dir / filename).resolve()
        if not is_relative_to(path, self.root):
            raise SessionStoreError(f"artifact 路径越界：{filename}")
        artifact_text = output[:TOOL_RESULT_LARGE_OUTPUT_CHARS] if truncated else output
        artifact_text = redact_sensitive_text(artifact_text)
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

    def _ensure_session_artifacts_dir(self, session_id: str) -> Path:
        session_artifacts_dir = (self.artifacts_dir / session_id).resolve()
        if not is_relative_to(session_artifacts_dir, self.root):
            raise SessionStoreError(f"artifact 目录越界：{session_id}")
        session_artifacts_dir.mkdir(parents=True, exist_ok=True)
        return session_artifacts_dir


def normalize_relative_artifact_path(raw_path: Any) -> str:
    if not isinstance(raw_path, str) or not raw_path.strip():
        raise SessionStoreError("artifact 路径必须是非空字符串。")
    normalized = raw_path.strip().replace("\\", "/")
    path = Path(normalized)
    if path.is_absolute() or ".." in path.parts:
        raise SessionStoreError(f"artifact 路径必须是安全相对路径：{raw_path}")
    if not path.parts or path.parts[0].lower() != "artifacts":
        raise SessionStoreError(f"artifact 路径必须位于 artifacts 目录：{raw_path}")
    return normalized


def redact_sensitive_values(value: Any) -> Any:
    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                continue
            normalized_key = key.strip().lower().replace("-", "_")
            is_header_secret = normalized_key.startswith("x_") and normalized_key[2:] in _SENSITIVE_FIELD_NAMES
            redacted[key] = (
                "***"
                if normalized_key in _SENSITIVE_FIELD_NAMES or is_header_secret
                else redact_sensitive_values(item)
            )
        return redacted
    if isinstance(value, list):
        return [redact_sensitive_values(item) for item in value[:100]]
    if isinstance(value, str):
        return redact_sensitive_text(value)
    return value


def redact_sensitive_text(text: str) -> str:
    redacted = _SENSITIVE_ASSIGNMENT_PATTERN.sub(r"\1***", text)
    redacted = _AUTHORIZATION_ASSIGNMENT_PATTERN.sub(r"\1Bearer ***", redacted)
    redacted = _BEARER_SECRET_PATTERN.sub("Bearer ***", redacted)
    return _PROVIDER_SECRET_PATTERN.sub("***", redacted)


def redact_sensitive_html(html: str) -> str:
    """清理会被直接落盘和回放的 HTML 中常见凭据表示。

    HTML 可把同一份秘密放在普通文本、属性、脚本或内嵌 JSON 中；这里不尝试
    解析或重写 DOM，避免改变结构和回放行为，只替换凭据值本身。
    """

    redacted = _SENSITIVE_HTML_ATTRIBUTE_PATTERN.sub(r"\1***\3", html)
    redacted = _SENSITIVE_JSON_PROPERTY_PATTERN.sub(r"\1***\3", redacted)
    return redact_sensitive_text(redacted)


def preview_text(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    head_chars = max_chars // 2
    tail_chars = max_chars - head_chars
    return text[:head_chars] + "\n... 中间内容已省略 ...\n" + text[-tail_chars:]


def tool_output_summary(output: str) -> str:
    return (
        "工具输出较大，已写入 artifact；"
        f"字符数：{len(output)}，sha256：{hashlib.sha256(output.encode('utf-8')).hexdigest()}。"
    )
