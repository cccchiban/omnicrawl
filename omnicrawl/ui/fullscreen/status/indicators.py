"""顶部 HUD 遥测与输入框上方的排队消息预览。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：
``StatusMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；
纯格式化函数保留在 ``ui/fullscreen/hud.py``（不依赖 Textual），本模块
只负责读取 Agent/App 状态并装配展示文本。
``ui/fullscreen/status/__init__.py`` 只做再导出。
"""

from __future__ import annotations

from collections import deque

from rich.text import Text
from textual import events
from textual.containers import Horizontal, Vertical
from textual.widgets import Static, TextArea

from .hud import (
    context_summary_text,
    decrypt_frame,
    load_carousel_message_lines,
    pending_queue_text,
    status_summary_text,
    token_telemetry_text,
)
from ..terminal.theme import ACCENT_AMBER, TEXT_MUTED


class QueueDelete(Static):
    """排队消息行尾的 [ DELETE ] 热区。

    悬停由 CSS 点亮为红色并显示手型光标，点击把该条从队列撤回并
    回填到输入框（动作由宿主 App 的 ``_withdraw_pending_input`` 完成），
    与 RuntimeStatus 的 [ ESC ] 采用同样的宿主回调模式。
    """

    can_focus = False

    def __init__(self, index: int) -> None:
        self._queue_index = index
        super().__init__(" [ DELETE ]", markup=False)

    def on_click(self, event: events.Click) -> None:
        if event.chain != 1:
            return
        action = getattr(self.app, "_withdraw_pending_input", None)
        if action is not None:
            event.stop()
            action(self._queue_index)


class QueueToggle(Static):
    """排队折叠提示行：队列超过可见上限时显示「… 还有 N 条 ›」。

    点击在展开全部与折叠回前几条之间切换（宿主
    ``_toggle_pending_queue_expanded``）；悬停高亮为青色。
    """

    can_focus = False

    def __init__(self, label: str) -> None:
        super().__init__(label, markup=False)

    def on_click(self, event: events.Click) -> None:
        if event.chain != 1:
            return
        action = getattr(self.app, "_toggle_pending_queue_expanded", None)
        if action is not None:
            event.stop()
            action()


class PendingQueue(Vertical):
    """生成期间 FIFO 排队消息的可交互预览条（composer 上方）。

    首行标题固定显示排队总数，之后按 FIFO 顺序每行一条消息摘要，
    行尾是独立的 [ DELETE ] 热区：悬停变红、点击撤回该条并把内容回填
    输入框。队列超过可见上限（构造参数）时默认折叠为前若干条 + 一行
    「展开」提示；点击提示行可展开/收起全部条目，行数随内容伸缩。
    """

    DEFAULT_CSS = """
    PendingQueue {
        height: auto;
    }
    PendingQueue > .queue-row {
        height: 1;
        width: 1fr;
    }
    .queue-summary {
        height: 1;
        width: 1fr;
        overflow: hidden;
        text-overflow: ellipsis;
        text-wrap: nowrap;
    }
    QueueDelete {
        height: 1;
        width: auto;
        text-style: bold dim;
        pointer: pointer;
    }
    QueueDelete:hover {
        color: ansi_red;
        text-style: bold;
    }
    QueueToggle {
        height: 1;
        width: auto;
        text-style: bold dim;
        pointer: pointer;
    }
    QueueToggle:hover {
        color: ansi_bright_cyan;
        text-style: bold;
    }
    """

    def __init__(self, *, max_visible: int = 3, id: str | None = None) -> None:
        super().__init__(id=id)
        self._max_visible = max_visible

    def update_items(
        self,
        items: list[str],
        expanded: bool,
        *,
        summary_limit: int = 40,
    ) -> None:
        """按当前队列内容重建预览行；空队列时只清空不显示。

        ``expanded`` 为 True 且队列超过可见上限时展示全部条目并附加
        「收起」行；否则折叠为前 ``max_visible`` 条 + 「展开」行。
        """

        self.remove_children()
        count = len(items)
        if not count:
            return
        title = Text(f"⏳ {count} 条消息排队", style=f"bold {ACCENT_AMBER}")
        self.mount(Static(title))
        show_all = expanded and count > self._max_visible
        visible = items if show_all else items[: self._max_visible]
        for index, text in enumerate(visible):
            first_line = text.splitlines()[0] if text else ""
            summary = " ".join(first_line.split())[:summary_limit]
            row = Horizontal(
                Static(
                    f"  {index + 1}. {summary}",
                    classes="queue-summary",
                    markup=False,
                ),
                QueueDelete(index),
                classes="queue-row",
            )
            self.mount(row)
        if count > self._max_visible:
            if show_all:
                toggle_label = "  « 收起"
            else:
                toggle_label = f"  … 还有 {count - self._max_visible} 条 ›"
            self.mount(QueueToggle(toggle_label))


class StatusMixin:
    """原 ``OmniCrawlApp`` 的 HUD 遥测与排队预览方法。"""

    # 底部单行轮播 HUD：遥测页 10s → 工作区路径页 10s → 留言页 10s 循环。
    CAROUSEL_TELEMETRY_SECONDS = 10
    CAROUSEL_WORKSPACE_SECONDS = 10
    CAROUSEL_MESSAGE_SECONDS = 10
    # 留言页无内容时的兜底占位文本。
    CAROUSEL_MESSAGE_FALLBACK = "🎲 留言本空空如也，去写一条吧～"
    # 切换时的解密扫描特效时长与帧间隔。
    CAROUSEL_ANIMATION_SECONDS = 1.5
    CAROUSEL_ANIMATION_FRAME_SECONDS = 0.05

    def _carousel_page_duration(self) -> float:
        """当前页的停留时长：遥测/工作区路径/留言各 10s。"""

        if self._carousel_page == "telemetry":
            return self.CAROUSEL_TELEMETRY_SECONDS
        if self._carousel_page == "workspace":
            return self.CAROUSEL_WORKSPACE_SECONDS
        return self.CAROUSEL_MESSAGE_SECONDS

    def _carousel_next_page(self) -> str:
        """返回下一页类型：telemetry → workspace → message 循环。"""

        return {
            "telemetry": "workspace",
            "workspace": "message",
            "message": "telemetry",
        }[self._carousel_page]

    def _carousel_ensure_message_line(self) -> str | None:
        """在载入页面时固定当前留言：首次切入（或已有留言失效）时随机抽取。"""

        try:
            lines = load_carousel_message_lines()
        except Exception:
            lines = []
        if not lines:
            return None
        line = getattr(self, "_carousel_message_line", None)
        if line not in lines:
            line = self._carousel_rand.choice(lines)
            self._carousel_message_line = line
        return line

    def _carousel_message_text(self) -> Text:
        """按已固定留言渲染留言页；无候选时显示占位文本。

        每次切入留言页时 ``_carousel_switch_to`` 已清空缓存，
        保证每轮停留展示一条新抽取的留言。
        """

        line = self._carousel_ensure_message_line()
        if line is None:
            return Text(self.CAROUSEL_MESSAGE_FALLBACK, style=TEXT_MUTED)
        return Text(line, style=TEXT_MUTED)

    def _carousel_build_page_text(self, page: str) -> Text:
        """按页类型装配完整内容：工作区路径页 / 遥测+模型状态页 / 留言页。

        留言页文案仅在当轮停留期间固定：切入该页时重新随机抽取，
        停留中遥测刷新等重绘不换句子；下一轮切回时再抽新句。
        """

        if page == "workspace":
            return self._context_summary_text()
        if page == "message":
            return self._carousel_message_text()
        rendered = self._token_telemetry_text()
        rendered.append_text(self._status_summary_text())
        return rendered

    def _carousel_display_text(self) -> Text:
        """当前页的稳态展示文本（compose 初始渲染用）。"""

        if self._carousel_page == "message":
            # 预抽取一次，保证首帧与其他路径渲染的留言一致。
            self._carousel_ensure_message_line()
        return self._carousel_build_page_text(self._carousel_page)

    def _carousel_refresh(self) -> None:
        """内容变化时刷新轮播显示；动画期间只更新目标页文本。

        widget 不在活动查询树中（模态屏打开/应用关闭）时静默跳过。
        """

        try:
            widget = self.query_one("#carousel-display", Static)
        except Exception:
            return
        if self._carousel_animating:
            self._carousel_anim_target = self._carousel_build_page_text(
                self._carousel_page
            )
            return
        self._carousel_settled_text = self._carousel_build_page_text(
            self._carousel_page
        )
        widget.update(self._carousel_settled_text)

    def _carousel_start(self) -> None:
        """启动底部轮播：先渲染当前页并安排第一次切换。"""

        self._carousel_settled_text = self._carousel_build_page_text(
            self._carousel_page
        )
        self._carousel_schedule_hold()

    def _carousel_schedule_hold(self) -> None:
        """按当前页时长安排下一次切换；取消可能残留的旧定时器。"""

        timer = getattr(self, "_carousel_hold_timer", None)
        if timer is not None:
            try:
                timer.stop()
            except Exception:  # noqa: BLE001 - 轮播停留定时器已停止时重复 stop 无害
                pass
        self._carousel_hold_timer = self.set_timer(
            self._carousel_page_duration(),
            self._carousel_hold_expired,
        )

    def _carousel_hold_expired(self) -> None:
        """停留结束：模态屏打开期间暂缓切换，否则执行解密扫描切换。"""

        if len(self.screen_stack) > 1:
            self.set_timer(1.0, self._carousel_hold_expired)
            return
        self._carousel_switch_to(self._carousel_next_page())

    def _carousel_switch_to(self, page: str, *, animate: bool = True) -> None:
        """切换到指定页；``animate=False`` 时直接落定（测试/即时路径）。

        切入留言页时清空上一条已固定留言，使该轮重新随机抽取；
        避免整轮停留结束后再次切回时永远复用同一句。
        """

        old = self._carousel_settled_text or self._carousel_build_page_text(
            self._carousel_page
        )
        if page == "message":
            self._carousel_message_line = None
        target = self._carousel_build_page_text(page)
        self._carousel_page = page
        self._carousel_anim_target = target
        self._carousel_anim_target_old = old
        self._carousel_animating = animate
        if not animate:
            self._carousel_animation_finish()
            return
        self._carousel_anim_frame = 0
        self._carousel_anim_total_frames = max(
            1,
            round(
                self.CAROUSEL_ANIMATION_SECONDS
                / self.CAROUSEL_ANIMATION_FRAME_SECONDS
            ),
        )
        if self._carousel_anim_interval is None:
            self._carousel_anim_interval = self.set_interval(
                self.CAROUSEL_ANIMATION_FRAME_SECONDS,
                self._carousel_animation_tick,
            )
        self._carousel_animation_tick()

    def _carousel_animation_tick(self) -> None:
        """推进一帧解密扫描动画；结束后落定并安排下一次停留。"""

        self._carousel_anim_frame += 1
        if self._carousel_anim_frame >= self._carousel_anim_total_frames:
            self._carousel_animation_finish()
            return
        progress = self._carousel_anim_frame / self._carousel_anim_total_frames
        frame = decrypt_frame(
            self._carousel_anim_target_old,
            self._carousel_anim_target,
            progress,
            rand_source=self._carousel_rand,
        )
        try:
            self.query_one("#carousel-display", Static).update(frame)
        except Exception:  # noqa: BLE001 - 轮播组件可能尚未挂载，动画帧渲染失败不中断
            pass

    def _carousel_animation_finish(self) -> None:
        """动画收口：停止帧定时器、渲染目标页稳态文本并安排下一次停留。"""

        interval = getattr(self, "_carousel_anim_interval", None)
        if interval is not None:
            try:
                interval.stop()
            except Exception:  # noqa: BLE001 - 轮播动画定时器已停止时重复 stop 无害
                pass
        self._carousel_anim_interval = None
        self._carousel_animating = False
        self._carousel_settled_text = self._carousel_build_page_text(
            self._carousel_page
        )
        try:
            self.query_one("#carousel-display", Static).update(
                self._carousel_settled_text
            )
        except Exception:  # noqa: BLE001 - 轮播组件可能尚未挂载，失败不中断动画收口
            pass
        self._carousel_schedule_hold()

    def _pending_queue_text(self) -> Text:
        """生成右上角 FIFO 排队消息计数。"""

        return pending_queue_text(len(self._pending_inputs))

    def _refresh_pending_queue_count(self) -> None:
        """同步 HUD 排队计数与输入区上方的排队预览条。"""

        self._refresh_context_summary()
        self._render_pending_queue()

    def _pending_queue_rows(self) -> int:
        """排队预览条当前应占用的行数（无排队时为 0）。

        行数 = 1 行标题 + 可见消息行数 + （超出可见上限时）1 行
        展开/收起提示行；展开态下可见消息行数为全部，折叠态为前
        ``QUEUE_PREVIEW_MAX_ROWS`` 条。
        """

        count = len(self._pending_inputs)
        if not count:
            return 0
        visible = (
            count
            if self._pending_queue_expanded and count > self.QUEUE_PREVIEW_MAX_ROWS
            else min(count, self.QUEUE_PREVIEW_MAX_ROWS)
        )
        rows = 1 + visible
        if count > self.QUEUE_PREVIEW_MAX_ROWS:
            rows += 1
        return rows

    def _render_pending_queue(self) -> None:
        """在 composer 上方渲染 FIFO 排队消息预览条；空队列时隐藏。

        标题行显示排队总数，随后按 FIFO 顺序每行展示一条消息摘要，
        行尾 [ DELETE ] 可撤回对应消息并回填输入框；队列超过可见上限
        时默认折叠为前几条 + 展开提示行，点击提示行可展开/收起全部，
        并在每次队列变化（排队、撤回、逐条发送、清空）后同步高度。
        """

        try:
            queue = self.query_one("#pending-queue", PendingQueue)
        except Exception:  # noqa: BLE001 - 组件尚未挂载的测试替身
            return
        if not self._pending_inputs:
            queue.display = False
            queue.remove_children()
            self._pending_queue_expanded = False
            self._resize_composer_to_text()
            return
        if len(self._pending_inputs) <= self.QUEUE_PREVIEW_MAX_ROWS:
            # 队列缩回可见上限内：退出展开态，避免残留无效的展开/收起行。
            self._pending_queue_expanded = False
        queue.update_items(
            list(self._pending_inputs),
            self._pending_queue_expanded,
            summary_limit=self.QUEUE_PREVIEW_SUMMARY_LIMIT,
        )
        queue.display = True
        self._resize_composer_to_text()

    def _withdraw_pending_input(self, index: int) -> None:
        """撤回第 ``index`` 条排队消息：移出队列并回填输入框。

        仅在生成期间队列非空且索引有效时执行；直接替换输入框当前
        内容（不自动发送），让用户修改后再次 Enter 提交。正在提问
        （ask_user 面板激活）时不执行，避免回填内容被提问模式吞掉。
        """

        if not self.is_generating or not 0 <= index < len(self._pending_inputs):
            return
        if self._ask_user_request is not None:
            return
        items = list(self._pending_inputs)
        text = items.pop(index)
        self._pending_inputs = deque(items)
        composer = self.query_one("#composer", TextArea)
        composer.text = text
        composer.cursor_location = (0, len(text))
        composer.focus()
        self._refresh_pending_queue_count()

    def _toggle_pending_queue_expanded(self) -> None:
        """展开/收起排队预览中被折叠的条目。"""

        if len(self._pending_inputs) <= self.QUEUE_PREVIEW_MAX_ROWS:
            return
        self._pending_queue_expanded = not self._pending_queue_expanded
        self._render_pending_queue()

    def _token_telemetry_text(self) -> Text:
        """生成第二行遥测：项目名、CTX 占用、IN/OUT/CA 与 tok/s。"""

        return token_telemetry_text(
            self._input_tokens,
            self._output_tokens,
            self._cached_input_tokens,
            getattr(self.agent, "context_window_tokens", 128_000),
            self._tokens_per_second,
        )

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
        """生成遥测页右段：模型、推理强度、审批模式、MCP 数量与排队数。

        行首自带 “⁕ ” 前置分隔符衔接遥测段（CTX/t/s）段尾。
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
        """刷新底部轮播 HUD：上下文/遥测随 MCP 设置与计数变化。"""

        self._carousel_refresh()


__all__ = ["PendingQueue", "QueueDelete", "QueueToggle", "StatusMixin"]
