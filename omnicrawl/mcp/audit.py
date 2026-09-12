"""MCP 子系统内部模块。

本文件由原合并入口按既有模块边界恢复，职责说明见模块内公开对象。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .security import redact_sensitive_text, redact_sensitive_values


DEFAULT_MCP_AUDIT_LOG_PATH = ".omnicrawl/logs/mcp-audit.jsonl"

# 审计只保留输出预览的前 1000 字符；脱敏窗口略大于预览长度，让跨截断边界的
# 密钥在截断前就被正则吞掉，同时对超长输出保持常数级开销。
_PREVIEW_MAX_CHARS = 1000
_PREVIEW_REDACTION_WINDOW = 2000


@dataclass(frozen=True)
class MCPAuditEvent:
    """一条 MCP 调用审计记录。"""

    timestamp: str
    session_id: str
    audit_id: str
    server_name: str
    tool_name: str
    arguments_redacted: dict[str, Any]
    approval_mode: str
    approval_result: str
    duration_ms: int
    ok: bool
    error_code: str | None
    output_preview: str


class MCPAuditLogger:
    """把 MCP 工具调用写入本地 JSONL 审计日志。"""

    def __init__(
        self,
        workspace_root: Path,
        *,
        enabled: bool,
        relative_path: str = DEFAULT_MCP_AUDIT_LOG_PATH,
    ) -> None:
        self.enabled = enabled
        self.workspace_root = workspace_root.resolve()
        self.path = self._resolve_log_path(relative_path)
        # 目录只需创建一次；每次调用都 mkdir 会给工具调用的返回路径多塞一次
        # 系统调用。目录被外部删除时由下次写入重新创建。
        self._directory_ready = False

    def record_tool_call(
        self,
        *,
        session_id: str,
        audit_id: str,
        server_name: str,
        tool_name: str,
        arguments: dict[str, Any],
        approval_mode: str,
        approval_result: str,
        duration_ms: int,
        ok: bool,
        error_code: str | None,
        output: str,
    ) -> None:
        """记录一次 MCP Tool 调用。

        审计只保存参数脱敏版和输出预览，避免把密钥或大文件内容写入日志。
        """

        if not self.enabled:
            return

        event = MCPAuditEvent(
            timestamp=datetime.now().astimezone().isoformat(timespec="seconds"),
            session_id=session_id,
            audit_id=audit_id,
            server_name=server_name,
            tool_name=tool_name,
            arguments_redacted=redact_sensitive_values(arguments),
            approval_mode=approval_mode,
            approval_result=approval_result,
            duration_ms=duration_ms,
            ok=ok,
            error_code=error_code,
            # 先清理再截断，避免位于预览末尾的秘密部分残留在审计日志。
            # 只脱敏有界前缀：输出可能上千倍于预览长度，全文跑 4 轮正则会让
            # 审计成为工具调用路径上的开销大头（实测 5MB 输出约 0.9 秒）。
            output_preview=_preview(
                redact_sensitive_text(
                    _head(output, max_chars=_PREVIEW_REDACTION_WINDOW)
                )
            ),
        )
        try:
            self._ensure_directory()
            with self.path.open("a", encoding="utf-8") as file:
                file.write(json.dumps(event.__dict__, ensure_ascii=False) + "\n")
        except OSError:
            # 审计日志不能影响主业务。调用方仍会在工具结果中看到真实执行状态。
            self._directory_ready = False
            return

    def _ensure_directory(self) -> None:
        if self._directory_ready:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._directory_ready = True

    def _resolve_log_path(self, relative_path: str) -> Path:
        cleaned = relative_path.strip().replace("\\", "/") or DEFAULT_MCP_AUDIT_LOG_PATH
        candidate = Path(cleaned)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        try:
            resolved.relative_to(self.workspace_root)
        except ValueError:
            return self.workspace_root / DEFAULT_MCP_AUDIT_LOG_PATH
        return resolved


def _head(output: str, *, max_chars: int) -> str:
    """取输出头部有界片段，用于在脱敏前把正则开销限制为常数。"""

    text = output.strip()
    return text if len(text) <= max_chars else text[:max_chars]


def _preview(output: str, max_chars: int = _PREVIEW_MAX_CHARS) -> str:
    text = output.strip()
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + "\n... 审计输出预览已截断。"
