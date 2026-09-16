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


def find_conversation_scroll(host: object) -> VerticalScroll | None:
    """快速定位 ``#conversation`` 消息区；未挂载时返回 None。

    通用 ``query("#conversation")`` 每次都要对整棵组件树做 CSS 选择器匹配
    （长会话下每条历史消息都参与），在流式高频路径上是主要开销；ID 选择器
    改走 Textual 的 ``query_one`` 索引查询（O(1) 且有缓存）。没有
    ``query_one`` 的测试替身仍退回通用查询。
    """

    query_one = getattr(host, "query_one", None)
    if callable(query_one):
        try:
            return query_one("#conversation", VerticalScroll)
        except Exception:  # noqa: BLE001 - 尚未挂载/已卸载时与空查询同义
            return None
    conversations = host.query("#conversation")
    if not conversations:
        return None
    return conversations.first(VerticalScroll)


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

    def _refresh_trailing_message_marker(self) -> None:
        """把末条消息标记移动到消息序列的最后一项。

        末条消息需要去掉底部间隔（``margin-bottom: 0``），否则会话底部会多出
        一行空白。原实现用 CSS ``:last-child`` 表达，但顺序伪类会让每条消息都
        被 Textual 视为「顺序样式」节点：每次挂载新消息都要为全部历史消息重算
        样式，长会话中每挂载一条的成本随消息数线性增长。这里改为显式的
        ``trailing`` 类，只在末项变化时更新旧、新两个组件。
        """

        conversation = find_conversation_scroll(self)
        if conversation is None:
            return
        # ``Widget.remove()`` 只是把组件标记为待移除（``Prune`` 消息异步完成
        # 真正的 DOM 摘除），因此在本次刷新早于移除完成时，末项仍是正在退场
        # 的组件。``_pruning`` 由 Textual 在发起移除时同步置位，这里把它视为
        # 已移除，保证标记与移除后的最终序列一致。
        displayed = [
            child
            for child in conversation.displayed_children
            if not getattr(child, "_pruning", False)
        ]
        last = displayed[-1] if displayed else None
        if last is not None and not last.has_class("message"):
            last = None
        previous = getattr(self, "_trailing_message_widget", None)
        if previous is last:
            return
        if previous is not None and previous.parent is not None:
            previous.set_class(False, "trailing")
        if last is not None:
            last.set_class(True, "trailing")
        self._trailing_message_widget = last

    def _hide_welcome_logo(self) -> None:
        """隐藏启动欢迎 Logo 区域，让位给首条会话内容；幂等且容忍缺位。

        流式路径每个分片都会调用本方法；Logo 已隐藏时直接返回，避免重复
        停表、重复落定静态文本以及随之而来的组件重绘。
        """

        if getattr(self, "_welcome_logo_hidden", False):
            return
        # 首条消息即停掉入场动画，避免隐藏后仍有回调刷新不可见组件。
        stop = getattr(self, "_stop_welcome_logo_animation", None)
        if callable(stop):
            stop()
        try:
            # 保持现有组件级语义，避免调用方/测试直接观察 Logo 时仍看到 display=True。
            self.query_one("#welcome-logo", Static).display = False
            self._welcome_logo_hidden = True
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
        # Logo 重新可见：允许下一条消息再次执行一次隐藏收口。
        self._welcome_logo_hidden = False
        self.conversation_text = ""
        self._stream_message = None
        self._reset_stream_state()
        self._stream_start_text_len = None
        self._trailing_message_widget = None
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
