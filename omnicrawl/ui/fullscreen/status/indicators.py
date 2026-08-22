"""顶部 HUD 遥测与输入框上方的排队消息预览。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：
``StatusMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；
纯格式化函数保留在 ``ui/fullscreen/hud.py``（不依赖 Textual），本模块
只负责读取 Agent/App 状态并装配展示文本。
``ui/fullscreen/status/__init__.py`` 只做再导出。
"""

from __future__ import annotations

from rich.text import Text
from textual.widgets import Static

from .hud import (
    compact_token_count,
    context_summary_text,
    gradient_text,
    pending_queue_text,
    status_summary_text,
    token_telemetry_text,
)
from ..terminal.theme import ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY


class StatusMixin:
    """原 ``OmniCrawlApp`` 的 HUD 遥测与排队预览方法。"""

    def _pending_queue_text(self) -> Text:
        """生成右上角 FIFO 排队消息计数。"""

        return pending_queue_text(len(self._pending_inputs))

    def _refresh_pending_queue_count(self) -> None:
        """同步 HUD 排队计数与输入区上方的排队预览条。"""

        self._refresh_context_summary()
        self._render_pending_queue()

    def _pending_queue_rows(self) -> int:
        """排队预览条当前应占用的行数（无排队时为 0）。

        行数 = 1 行标题 + 每条摘要一行（最多 QUEUE_PREVIEW_MAX_ROWS）
        + 超出部分折叠提示行（仅当队列超过最大展示行数时）。
        """

        if not self._pending_inputs:
            return 0
        rows = 1 + min(len(self._pending_inputs), self.QUEUE_PREVIEW_MAX_ROWS)
        if len(self._pending_inputs) > self.QUEUE_PREVIEW_MAX_ROWS:
            rows += 1
        return rows

    def _render_pending_queue(self) -> None:
        """在 composer 上方渲染 FIFO 排队消息预览条；空队列时隐藏。

        标题行显示排队总数，随后按 FIFO 顺序展示每条消息的首行摘要
        （超出 QUEUE_PREVIEW_MAX_ROWS 的部分折叠为一行计数），并在每次
        队列变化（排队、逐条发送、清空）后同步高度。
        """

        queue = self.query_one("#pending-queue", Static)
        if not self._pending_inputs:
            queue.display = False
            queue.update("")
            return
        lines = Text(no_wrap=True, overflow="ellipsis")
        lines.append(
            f"⏳ {len(self._pending_inputs)} 条消息排队",
            style=f"bold {ACCENT_AMBER}",
        )
        for index, text in enumerate(
            list(self._pending_inputs)[: self.QUEUE_PREVIEW_MAX_ROWS], start=1
        ):
            first_line = text.splitlines()[0] if text else ""
            summary = " ".join(first_line.split())[: self.QUEUE_PREVIEW_SUMMARY_LIMIT]
            lines.append("\n")
            lines.append(f"  {index}. {summary}", style=TEXT_SECONDARY)
        if len(self._pending_inputs) > self.QUEUE_PREVIEW_MAX_ROWS:
            lines.append("\n")
            lines.append(
                f"  … 还有 {len(self._pending_inputs) - self.QUEUE_PREVIEW_MAX_ROWS} 条",
                style=TEXT_MUTED,
            )
        queue.update(lines)
        queue.display = True
        self._resize_composer_to_text()

    def _token_telemetry_text(self) -> Text:
        """生成第二行遥测：项目名、CTX 占用、IN/OUT/CA 与 tok/s。"""

        return token_telemetry_text(
            self._input_tokens,
            self._output_tokens,
            self._cached_input_tokens,
            getattr(self.agent, "context_window_tokens", 128_000),
            self._tokens_per_second,
        )

    @staticmethod
    def _compact_token_count(value: int) -> str:
        """兼容原有测试与调用入口。"""

        return compact_token_count(value)

    @staticmethod
    def _gradient_text(text: str) -> Text:
        """兼容原有测试与调用入口。"""

        return gradient_text(text)

    def _mcp_enabled_count(self) -> int:
        """返回当前全局启用的 MCP Server 数量，不触发 MCP 能力发现。"""

        manager = getattr(self.agent, "_mcp_manager", None)
        config = getattr(manager, "config", None)
        if not bool(getattr(config, "enabled", False)):
            return 0
        enabled_servers = getattr(config, "enabled_servers", ())
        try:
            return max(0, len(enabled_servers))
        except TypeError:
            return 0

    def _status_summary_text(self) -> Text:
        """生成第二行左段：模型、推理强度、审批模式、MCP 数量与排队数。

        行尾由 #token-telemetry 自带 “⁕ ” 前置分隔符衔接 CTX 段。
        """

        reasoning_effort = str(getattr(self.agent, "reasoning_effort", "") or "")
        if not reasoning_effort:
            reasoning_effort = self.startup.reasoning_effort or "DEFAULT"
        return status_summary_text(
            approval_mode=str(
                getattr(self.agent, "approval_mode", None) or self.startup.approval_label
            ),
            mcp_enabled_count=self._mcp_enabled_count(),
            pending_count=len(self._pending_inputs),
            model=str(getattr(self.agent, "current_model", "") or "NO MODEL"),
            reasoning_effort=reasoning_effort,
        )

    def _context_summary_text(self) -> Text:
        """渲染第一行左段：项目路径。"""

        return context_summary_text(
            workspace=str(
                getattr(self.agent, "workspace_root", "") or self.startup.workspace_label
            ),
        )

    def _refresh_context_summary(self) -> None:
        """刷新顶部两段卡片与随 MCP 设置变化的遥测字段。"""

        self.query_one("#context-summary", Static).update(self._context_summary_text())
        self.query_one("#status-summary", Static).update(self._status_summary_text())
        self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())


__all__ = ["StatusMixin"]
