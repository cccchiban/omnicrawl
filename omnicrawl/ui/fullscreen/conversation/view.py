"""会话视图管理：欢迎页、清空视图、历史重放与后台任务日志刷新。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：
``ConversationViewMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字
一致；历史事件的重放渲染管线位于 ``ui/fullscreen/rendering.py``
（``RenderingMixin``），本模块只负责视图层的入口与聚合。
``ui/fullscreen/conversation/__init__.py`` 只做再导出。
"""

from __future__ import annotations

from textual.containers import VerticalScroll
from textual.widgets import Static

from ..support.monitor import format_monitor_display_batch


class ConversationViewMixin:
    """原 ``OmniCrawlApp`` 的会话视图方法。"""

    def _hide_welcome_logo(self) -> None:
        """隐藏启动欢迎 Logo 区域，让位给首条会话内容；幂等且容忍缺位。"""

        try:
            # 保持现有组件级语义，避免调用方/测试直接观察 Logo 时仍看到 display=True。
            self.query_one("#welcome-logo", Static).display = False
        except Exception:  # noqa: BLE001 - 组件尚未挂载时静默忽略
            pass
        try:
            self.query_one("#welcome-logo-area").display = False
        except Exception:  # noqa: BLE001 - 兼容无包装区域的旧布局
            pass

    def _clear_conversation_view(self) -> None:
        """移除对话消息并复位状态；空会话时保留欢迎 Logo。"""

        conversation = self.query_one("#conversation", VerticalScroll)
        # 会话恢复即使只有元数据事件，也会调用清空逻辑。不能用
        # ``remove_children``，否则启动后的空会话会连欢迎 Logo 一起删掉。
        for child in list(conversation.children):
            if child.id != "welcome-logo":
                child.remove()
        try:
            self.query_one("#welcome-logo", Static).display = True
        except Exception:  # noqa: BLE001 - 兼容无 Logo 的旧测试/布局
            pass
        self.conversation_text = ""
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        self._stream_start_text_len = None
        self._tool_messages.clear()
        self._subagent_trees.clear()
        self._subagent_conversations.clear()
        self._reasoning_message = None
        self._runtime_status_message = None
        self._clear_todo_plan()

    def _replay_session_conversation(self) -> None:
        """按会话事件恢复与实时页面一致的 TUI 组件（`--resume` 或 `/resume`）。

        `SessionState.messages` 是给模型用的投影，工具请求和结果在那里会变成
        assistant 文本；历史页面必须改用原始事件流，才能恢复工具卡、结果状态
        和 SubAgent 进度树。旧 Agent/测试替身没有事件接口时才回退到消息投影。
        读取失败或会话为空时静默跳过，不阻断界面启动与命令反馈。
        """

        events_getter = getattr(self.agent, "current_session_events", None)
        if callable(events_getter):
            try:
                events = list(events_getter() or [])
            except Exception:
                # 只有事件接口读取失败时才降级到消息投影；正常的空事件流
                # 也是权威结果，必须清空当前 TUI（例如 /undo 撤回会话中
                # 唯一一轮后），不能因为 ``if events`` 而保留旧消息。
                events = None
            if events is not None:
                self._replay_session_events(events)
                return

        getter = getattr(self.agent, "current_session_messages", None)
        if not callable(getter):
            return
        try:
            messages = getter()
        except Exception:
            return
        visible = [
            message
            for message in messages
            if isinstance(message, dict)
            and str(message.get("role", "")) in {"user", "assistant"}
            and str(message.get("content") or "").strip()
        ]
        if not visible:
            return
        self._clear_conversation_view()
        for message in visible:
            self._append_message(str(message["role"]), str(message["content"]))

    def action_clear_conversation(self) -> None:
        if self.is_generating:
            return
        self._clear_conversation_view()
        self._append_message("status", "已清空当前视图，不影响会话历史。")

    def _refresh_monitor_events(self) -> None:
        """渲染适配器返回的后台任务增量日志，不影响模型回合。"""

        for batch in self._monitor_state.refresh():
            self._append_message("tool", format_monitor_display_batch(batch))


__all__ = ["ConversationViewMixin"]
