"""会话工具输出 artifact、脱敏和路径安全策略。"""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from typing import Any

# 路径判断与凭据脱敏的统一实现位于 omnicrawl.common；这里以 ``X as X`` 形式再导出，
# 既有调用方（agent / api / connectors / state 等 20+ 处）无需改导入路径。
from ..common.paths import is_relative_to
from ..common.redaction import redact_sensitive_text as redact_sensitive_text
from ..common.redaction import redact_sensitive_values as redact_sensitive_values
from .session_locking import atomic_write_text
from .session_models import SessionStoreError, clean_title


TOOL_RESULT_INLINE_OUTPUT_CHARS = 8 * 1024
TOOL_RESULT_LARGE_OUTPUT_CHARS = 128 * 1024
TOOL_RESULT_PREVIEW_CHARS = 1200
SUBAGENT_RESULT_LARGE_OUTPUT_CHARS = 128 * 1024
_SUBAGENT_TASK_ID_PATTERN = re.compile(r"^task-[a-f0-9]{12}$")
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

    def write_tool_result_artifact(self, session_id: str, output: str) -> str:
        """公开入口：把完整工具输出写入 artifact 文件，返回相对路径。

        用于批次输出预算机制：超限工具的完整内容先落盘，模型上下文只保留
        头尾预览与文件路径，模型可用 read_file 按路径读取完整内容。
        """

        output_hash = hashlib.sha256(output.encode("utf-8")).hexdigest()
        return self._write_tool_result_artifact(
            session_id=session_id,
            output=output,
            output_hash=output_hash,
            truncated=False,
        )

    def prepare_subagent_result(
        self,
        *,
        session_id: str,
        task_id: str,
        agent_type: str,
        description: str,
        result_text: str,
        summary_chars: int,
    ) -> dict[str, Any]:
        """生成可安全回传的摘要，并在结果较大时写入任务级 JSON artifact。

        该方法是 Session、API、TUI 和父模型共享的单一安全投影边界：完整结果先
        脱敏，再决定内联或 artifact；公开消费者永远不会接触原始结果文本。
        """

        if not _SUBAGENT_TASK_ID_PATTERN.fullmatch(task_id):
            raise SessionStoreError(f"SubAgent task_id 格式无效：{task_id}")
        if isinstance(summary_chars, bool) or not isinstance(summary_chars, int) or summary_chars <= 0:
            raise SessionStoreError("SubAgent summary_chars 必须是正整数。")

        safe_result = redact_sensitive_text(str(result_text or "").strip())
        summary = safe_result
        if len(summary) > summary_chars:
            summary = summary[:summary_chars] + "\n... 子任务结果已截断，完整结果见 artifact。"
        if len(safe_result) <= summary_chars:
            return {"summary": summary, "artifacts": []}

        output_hash = hashlib.sha256(safe_result.encode("utf-8")).hexdigest()
        truncated = False
        persisted_result = safe_result

        artifact_document = {
            "version": 1,
            "task_id": task_id,
            "agent_type": redact_sensitive_text(str(agent_type)),
            "description": redact_sensitive_text(str(description)),
            "result": persisted_result,
            "result_size_chars": len(safe_result),
            "result_sha256": output_hash,
            "truncated": truncated,
        }
        if truncated:
            artifact_document["truncation_note"] = (
                "artifact 已按 128KB 上限截断；sha256 描述脱敏后的完整结果。"
            )

        session_artifacts_dir = self._ensure_session_artifacts_dir(session_id)
        subagents_dir = (session_artifacts_dir / "subagents").resolve()
        if not is_relative_to(subagents_dir, self.root):
            raise SessionStoreError(f"SubAgent artifact 目录越界：{task_id}")
        path = (subagents_dir / f"{task_id}.json").resolve()
        if not is_relative_to(path, self.root):
            raise SessionStoreError(f"SubAgent artifact 路径越界：{task_id}")
        text = json.dumps(artifact_document, ensure_ascii=False, indent=2) + "\n"
        atomic_write_text(path, text)
        artifact_path = path.relative_to(self.root).as_posix()
        return {
            "summary": summary,
            "artifacts": [
                {
                    "type": "subagent_result",
                    "artifact_path": artifact_path,
                    "size_chars": len(safe_result),
                    "sha256": output_hash,
                    "truncated": truncated,
                }
            ],
        }

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
        # artifact 已取消 128KB 内容上限；保留字段供旧会话读取，但明确表示
        # 当前持久化文本始终完整，避免恢复逻辑误把完整结果当成丢失数据。
        payload["artifact_truncated"] = False
        payload["artifact_path"] = self._write_tool_result_artifact(
            session_id=session_id,
            output=output,
            output_hash=output_hash,
            truncated=False,
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
        artifact_text = redact_sensitive_text(output)
        if truncated:
            artifact_text += (
                "\n... artifact 已按安全上限截断，"
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
