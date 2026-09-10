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


CONVERSATION_DISPLAY_MAX_LOGICAL_LINES = 2000


def logical_text_line_count(text: object) -> int:
    """计算对话正文的逻辑行数，不把终端宽度导致的软折行算入。"""

    value = str(text or "")
    return len(value.splitlines())


def conversation_widget_line_count(widget: object) -> int:
    """读取消息组件的逻辑行数；兼容旧组件与测试替身。"""

    count = getattr(widget, "_conversation_logical_line_count", None)
    if isinstance(count, int) and count > 0:
        return count
    content = getattr(widget, "content", None)
    plain = getattr(content, "plain", None)
    if isinstance(plain, str):
        return logical_text_line_count(plain)
    return 1


class ConversationViewMixin:
    """原 ``OmniCrawlApp`` 的会话视图方法。"""

    @property
    def conversation_text(self) -> str:
        """会话文本的惰性拼接视图（仅 /undo 回滚截断时物化）。

        流式分片逐条追加到 ``_conversation_chunks``，避免 ``str +=`` 的
        O(n²) 累积；只有读取（测试/调试）与回滚时才 join。
        """

        return "".join(self._conversation_chunks)

    @conversation_text.setter
    def conversation_text(self, value: str) -> None:
        if value:
            self._conversation_chunks = [value]
        else:
            self._conversation_chunks = []
        self._conversation_text_len = len(value)

    def _append_conversation_text(self, text: str) -> None:
        """以 O(1) 追加一段会话文本并累计总长度。"""

        self._conversation_chunks.append(text)
        self._conversation_text_len += len(text)

    def _truncate_conversation_text(self, target_len: int) -> None:
        """把会话文本截断到指定长度（供流中断回滚）。"""

        chunks = self._conversation_chunks
        total = self._conversation_text_len
        while chunks and total > target_len:
            chunk = chunks.pop()
            excess = total - target_len
            if len(chunk) > excess:
                chunks.append(chunk[: len(chunk) - excess])
                total = target_len
            else:
                total -= len(chunk)
        self._conversation_text_len = total

    def _set_conversation_widget_lines(self, widget: object, line_count: int) -> None:
        """给消息组件记录逻辑行数（增量计数版本，供流式路径使用）。"""

        line_count = max(0, int(line_count))
        previous = getattr(widget, "_conversation_logical_line_count", None)
        setattr(widget, "_conversation_logical_line_count", line_count)
        if previous != line_count:
            self._mark_conversation_visibility_dirty()

    def _mark_conversation_visibility_dirty(self) -> None:
        """标记消息数量或顺序变化，延后到安全时机重算显示窗口。"""

        self._conversation_visibility_dirty = True

    def _refresh_conversation_visibility(self) -> None:
        """只保留最近 2000 个逻辑行的消息显示，旧组件仍留在 DOM 中。

        这里按消息组件边界隐藏最早的一条或多条消息，而不是删除组件。
        这样会话数据、工具卡状态和流式对象仍可继续更新；``/undo`` 重放
        会话后，若被撤回的回合释放出空间，之前隐藏的旧消息会自动重新显示。
        ``runtime-status-message`` 与欢迎 Logo 是界面装饰，不占用消息行预算。
        """

        if not getattr(self, "_conversation_visibility_dirty", True):
            return
        conversations = self.query("#conversation")
        if not conversations:
            self._conversation_visibility_refresh_pending = False
            return
        conversation = conversations.first(VerticalScroll)
        max_lines = max(
            1,
            int(
                getattr(
                    self,
                    "CONVERSATION_DISPLAY_MAX_LOGICAL_LINES",
                    CONVERSATION_DISPLAY_MAX_LOGICAL_LINES,
                )
            ),
        )
        message_widgets = [
            child
            for child in conversation.children
            if child.id != "welcome-logo"
            and not child.has_class("runtime-status-message")
        ]

        # 可见窗口是消息序列的后缀。使用索引而不是集合，既不依赖 Widget
        # 是否可哈希，也能让同一组件的显示状态在每次重算时保持稳定。
        # 最新一条消息即使超出整个行预算也必须显示（只截断它前面的旧
        # 消息），否则超长输出（超过 2000 逻辑行）会让整个会话区空白。
        first_visible = len(message_widgets) - 1
        remaining = max_lines
        for index in range(len(message_widgets) - 1, -1, -1):
            line_count = conversation_widget_line_count(message_widgets[index])
            if line_count > remaining:
                if index == len(message_widgets) - 1:
                    first_visible = index
                    remaining = 0
                break
            first_visible = index
            remaining -= line_count

        visible_lines = 0
        for index, child in enumerate(message_widgets):
            should_display = index >= first_visible
            # 只在状态变化时写 display：流式高频重算中绝大多数组件
            # 状态不变，避免反复触发 styles 更新与布局失效。
            if child.display != should_display:
                child.display = should_display
            if should_display:
                visible_lines += conversation_widget_line_count(child)
        self._conversation_visible_logical_lines = visible_lines
        self._conversation_visibility_dirty = False

    def _request_conversation_visibility_refresh(self) -> None:
        """合并同一批事件的窗口重算，避免流式输出逐片扫描全部消息。"""

        self._mark_conversation_visibility_dirty()
        if getattr(self, "_conversation_visibility_refresh_pending", False):
            return
        self._conversation_visibility_refresh_pending = True
        try:
            self.call_after_refresh(self._run_conversation_visibility_refresh)
        except Exception:
            # 兼容组件尚未挂载的测试替身；正式挂载后下一次请求会重算。
            self._conversation_visibility_refresh_pending = False

    def _run_conversation_visibility_refresh(self) -> None:
        self._conversation_visibility_refresh_pending = False
        self._refresh_conversation_visibility()

    def _set_conversation_widget_line_count(self, widget: object, text: object) -> None:
        """给消息组件记录逻辑行数，避免从 RichLog 的软折行反推。"""

        self._set_conversation_widget_lines(widget, logical_text_line_count(text))

    def _register_conversation_widget(
        self,
        widget: object,
        logical_text: object | None = None,
    ) -> None:
        """登记一个已挂载消息并合并触发显示窗口重算。"""

        if logical_text is None:
            content = getattr(widget, "content", None)
            logical_text = getattr(content, "plain", None)
        self._set_conversation_widget_line_count(
            widget,
            "" if logical_text is None else logical_text,
        )
        self._request_conversation_visibility_refresh()

    def _conversation_logical_line_count(self, widget: object) -> int:
        """返回单个消息组件的逻辑行数，供渲染层更新后复核。"""

        return conversation_widget_line_count(widget)

    def _hide_welcome_logo(self) -> None:
        """隐藏启动欢迎 Logo 区域，让位给首条会话内容；幂等且容忍缺位。"""

        # 首条消息即停掉入场动画，避免隐藏后仍有回调刷新不可见组件。
        stop = getattr(self, "_stop_welcome_logo_animation", None)
        if callable(stop):
            stop()
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
        self._reset_stream_state()
        self._stream_start_text_len = None
        self._conversation_visibility_dirty = True
        self._conversation_visibility_refresh_pending = False
        self._conversation_visible_logical_lines = 0
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
