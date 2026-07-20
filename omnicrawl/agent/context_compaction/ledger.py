"""上下文压缩测量事件的 Token 账本序列化。"""

from __future__ import annotations

from typing import Any

from .models import ContextBudgetSnapshot, TokenUsageSample


class UsageLedger:
    """把预算快照转为不含会话正文的可持久化诊断载荷。"""

    schema_version = 1

    def measurement_payload(
        self,
        snapshot: ContextBudgetSnapshot,
        usage: TokenUsageSample,
    ) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "mode": "measurement_only",
            **snapshot.to_dict(),
            "usage": usage.to_dict(),
        }
