"""跨子系统共用的凭据脱敏。

同一份脱敏策略曾由 ``state/session_artifacts.py``（7 条正则）与
``mcp/security.py``（4 条正则）各维护一份：MCP 那份漏掉了 PEM 私钥、
GitHub token、AWS access key，审计日志因此少脱敏这几类凭据。实现统一到
本模块，调用方（含 MCP 审计）都从包根按依赖方向导入，不引入子系统之间
的相互依赖。
"""

from __future__ import annotations

import re
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
_SENSITIVE_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)((?:api[_-]?key|apikey|access[_-]?key|secret[_-]?key|"
    r"access[_-]?token|refresh[_-]?token|id[_-]?token|cookie|password|secret|token)"
    r"\s*[:=]\s*[\"']?)([^\"'\s,;]+)"
)
_AUTHORIZATION_ASSIGNMENT_PATTERN = re.compile(
    r"(?i)(\bauthorization\s*[:=]\s*[\"']?)(?:Bearer\s+)?([^\"'\s,;]+)"
)
_BEARER_SECRET_PATTERN = re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{8,}")
_PROVIDER_SECRET_PATTERN = re.compile(r"\b(?:sk|ak|ah)-[A-Za-z0-9_-]{24,}\b")
_GITHUB_SECRET_PATTERN = re.compile(
    r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b"
)
_AWS_ACCESS_KEY_PATTERN = re.compile(
    r"\b(?:AKIA|ASIA|AIDA|AROA|AIPA|ANPA|ANVA|ASCA)[A-Z0-9]{16}\b"
)
_PEM_PRIVATE_KEY_PATTERN = re.compile(
    r"-----BEGIN(?: [A-Z0-9]+)? PRIVATE KEY-----.*?"
    r"-----END(?: [A-Z0-9]+)? PRIVATE KEY-----",
    re.DOTALL,
)


def redact_sensitive_values(value: Any) -> Any:
    """递归脱敏常见密钥字段，供审计与事件载荷使用。"""

    if isinstance(value, dict):
        redacted: dict[str, Any] = {}
        for key, item in value.items():
            if not isinstance(key, str):
                continue
            normalized_key = key.strip().lower().replace("-", "_")
            is_header_secret = (
                normalized_key.startswith("x_")
                and normalized_key[2:] in _SENSITIVE_FIELD_NAMES
            )
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
    """清理文本中的常见明文凭据（赋值、Bearer、厂商密钥、私钥、云凭据）。"""

    redacted = _PEM_PRIVATE_KEY_PATTERN.sub("*** PRIVATE KEY REDACTED ***", text)
    redacted = _SENSITIVE_ASSIGNMENT_PATTERN.sub(r"\1***", redacted)
    redacted = _AUTHORIZATION_ASSIGNMENT_PATTERN.sub(r"\1Bearer ***", redacted)
    redacted = _BEARER_SECRET_PATTERN.sub("Bearer ***", redacted)
    redacted = _PROVIDER_SECRET_PATTERN.sub("***", redacted)
    redacted = _GITHUB_SECRET_PATTERN.sub("***", redacted)
    return _AWS_ACCESS_KEY_PATTERN.sub("***", redacted)


__all__ = [
    "redact_sensitive_text",
    "redact_sensitive_values",
]
