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
    decrypt_frame,
    gradient_text,
    load_carousel_message_lines,
    pending_queue_text,
    status_summary_text,
    token_telemetry_text,
)
from ..terminal.theme import ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY


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
            except Exception:
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
        except Exception:
            pass

    def _carousel_animation_finish(self) -> None:
        """动画收口：停止帧定时器、渲染目标页稳态文本并安排下一次停留。"""

        interval = getattr(self, "_carousel_anim_interval", None)
        if interval is not None:
            try:
                interval.stop()
            except Exception:
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
        except Exception:
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


__all__ = ["StatusMixin"]
