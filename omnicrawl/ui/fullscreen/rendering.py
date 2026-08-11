"""全屏工作台的事件聚合与流式渲染管线。

P2 重构从 ``ui/fullscreen/__init__.py`` 的 ``OmniCrawlApp`` 拆出的独立模块
（2026-08-11）。这里集中：Agent 协议事件聚合（status/subagent/tool/token）、
流式 Markdown 渲染与回滚、运行时状态指示器、生成速率滑动窗口估算。

``RenderingMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；
跨领域方法（``_token_telemetry_text``、``_refresh_pending_queue_count``、
``_submit`` 等）仍通过 ``self`` 在 ``OmniCrawlApp`` 的 MRO 上解析。
类常量（``STREAM_RENDER_INTERVAL_SECONDS``、``TOKEN_RATE_WINDOW_SECONDS``、
``STATUS_SPINNER_FRAMES``）与实例状态仍定义在 ``OmniCrawlApp``。
"""

from __future__ import annotations

import time
from typing import Any

from rich.text import Text
from textual.containers import VerticalScroll
from textual.widgets import Static, TextArea

from ...agent.tools import public_tool_arguments
from .widgets import (
    AssistantMessage,
    ReasoningDisclosure,
    SubAgentProgressTree,
    ToolDisclosure,
)


class RenderingMixin:
    """原 ``OmniCrawlApp`` 的事件聚合与流式渲染方法。"""

    @staticmethod
    def _is_conversation_at_end(conversation: VerticalScroll) -> bool:
        """判断用户是否仍在消息流底部，避免后台更新抢回滚动位置。"""

        return conversation.is_vertical_scroll_end


    @staticmethod
    def _scroll_conversation_if_following(
        conversation: VerticalScroll,
        follow_latest: bool,
        *,
        defer_until_refresh: bool = False,
    ) -> None:
        """仅在用户原本位于底部时跟随新增内容。"""

        if follow_latest:
            if conversation.max_scroll_y > 0:
                # Textual 的锚定语义会在内容重新布局后持续跟随底部，并在用户
                # 手动滚动时自动释放；这比跨刷新排队 scroll_end 更能避免流式
                # 更新竞态。
                conversation.anchor()
                if defer_until_refresh:
                    conversation.call_after_refresh(conversation.scroll_end, animate=False)
            else:
                # 内容不足一屏时，Textual 8.2.7 的 anchor 会让 compositor 在
                # 布局时把 scroll_y 设为「内容高度 - 容器高度」的负值（该路径
                # 不经过 validate/clamp），消息被推到视口底部、顶部出现大片
                # 空白。此时无需滚动，取消锚定并保持顶部对齐；布局完成后若
                # 内容已超出一屏（跨屏边界），再恢复底部跟随。
                conversation.anchor(False)
                conversation.scroll_y = 0
                conversation.call_after_refresh(
                    lambda: (
                        conversation.anchor()
                        if conversation.max_scroll_y > 0
                        else None
                    )
                )


    def _handle_status(self, message: str) -> None:
        if message:
            self._set_runtime_status("等待", "waiting")


    def _handle_subagent_event(self, event_name: str, payload: dict[str, Any]) -> None:
        """按批次原地更新子任务树，不暴露 prompt、结果或原始异常。"""

        status_by_event = {
            "subagent.task.queued": "queued",
            "subagent.task.started": "running",
            "subagent.task.running": "running",
            "subagent.task.waiting_approval": "waiting_approval",
            "subagent.task.completed": "completed",
            "subagent.task.failed": "failed",
            "subagent.task.cancelled": "cancelled",
            "subagent.task.approval_cancelled": "cancelled",
        }
        status = status_by_event.get(event_name)
        if status is None:
            return

        task_id = str(payload.get("task_id") or "task")
        batch_id = str(payload.get("batch_id") or f"batch-{task_id}")
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        tree = self._subagent_trees.get(batch_id)
        if tree is None or tree.parent is None:
            tree = SubAgentProgressTree(batch_id)
            self._subagent_trees[batch_id] = tree
            conversation.mount(tree)
        tree.update_task(
            task_id=task_id,
            agent_type=str(payload.get("agent_type") or "subagent"),
            description=str(payload.get("description") or task_id),
            status=status,
        )
        # 运行状态始终保持为消息流末项；树新增或增高后需恢复这一顺序。
        if self._runtime_status_message is not None:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
            defer_until_refresh=True,
        )


    def _handle_tool_start(self, step: int, tool_call: Any) -> None:
        del step  # Agent 仍按步骤回调，但极简 HUD 不展示内部步骤编号。
        self._render_stream_markdown()
        # 工具调用是模型 pass 的明确边界。必须封口此前的回复组件，否则工具
        # 返回后的最终回答会继续写入旧组件，在视觉上倒插到工具记录之前。
        if self._reasoning_message is not None:
            # 先补齐未完成行，再封口思考组件。
            self._reasoning_message.flush_tail()
        self._stream_message = None
        self._stream_markdown = ""
        self._stream_start_text_len = None
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        tool_message = ToolDisclosure(
            str(tool_call.name),
            self._public_tool_arguments(tool_call),
            time.perf_counter(),
        )
        self._tool_messages[self._tool_call_key(tool_call)] = tool_message
        conversation.mount(tool_message)
        self.conversation_text += f"{tool_call.name}\n"
        self._set_runtime_status(
            "正在调用",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _handle_tool_result(self, tool_call: Any, result: Any) -> None:
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        output = str(getattr(result, "full_output", "") or result.output or "无输出")
        key = self._tool_call_key(tool_call)
        tool_message = self._tool_messages.pop(key, None)
        if tool_message is None:
            # 兼容缺失 start 事件的协议实现，同时仍保持默认折叠交互。
            tool_message = ToolDisclosure(
                str(tool_call.name),
                self._public_tool_arguments(tool_call),
                time.perf_counter(),
            )
            self.query_one("#conversation", VerticalScroll).mount(tool_message)
        tool_message.finish(
            ok=bool(result.ok),
            output=output,
            finished_at=time.perf_counter(),
        )
        self.conversation_text += f"结果  {tool_message.status}\n{output}\n"
        self._scroll_conversation_if_following(conversation, follow_latest)
        self._set_runtime_status("正在思考", "working")


    @staticmethod
    def _public_tool_arguments(tool_call: Any) -> Any:
        """隐藏 SubAgent 完整 prompt，其余工具保持既有参数展示。"""

        arguments = getattr(tool_call, "arguments", None)
        if not isinstance(arguments, dict):
            return arguments
        return public_tool_arguments(str(getattr(tool_call, "name", "")), arguments)


    @staticmethod
    def _tool_call_key(tool_call: Any) -> str:
        """优先以协议 ID 关联并发工具记录；缺失 ID 时退化为对象身份。"""

        tool_call_id = str(getattr(tool_call, "id", "") or "")
        return tool_call_id or f"object:{id(tool_call)}"


    def _handle_token_usage(self, incoming: int, outgoing: int, cached: int) -> None:
        """刷新最近一次模型请求的 Token 遥测与上下文占用进度。"""

        self._input_tokens = max(0, int(incoming))
        self._output_tokens = max(0, int(outgoing))
        self._cached_input_tokens = max(0, int(cached))
        self.query_one("#token-telemetry", Static).update(self._token_telemetry_text())


    @staticmethod
    def _estimate_generation_tokens(text: str) -> float:
        """把流式文本增量粗略估算为 token 数。

        CJK 字符按 1 token、其他字符按 4 字符 1 token 估算。供应商不提供
        流中逐片 token 计数，该估算只用于实时速率展示，不做精确计量。
        """

        cjk = sum(1 for ch in text if ord(ch) > 0x2E7F)
        return cjk + (len(text) - cjk) / 4.0


    def _prune_generation_samples(self) -> None:
        """丢弃窗口之外的采样点，保持滑动窗口有界。"""

        cutoff = time.monotonic() - self.TOKEN_RATE_WINDOW_SECONDS
        while self._generation_samples and self._generation_samples[0][0] < cutoff:
            self._generation_samples.popleft()


    def _record_generation_delta(self, delta: str) -> None:
        """记录思考/正文流增量的时间与估算 token，供 tok/s 实时计算。"""

        if not delta:
            return
        self._generation_samples.append(
            (time.monotonic(), self._estimate_generation_tokens(delta))
        )
        self._prune_generation_samples()


    def _update_token_telemetry(self) -> None:
        """刷新顶部遥测行；widget 不在活动查询树中时静默跳过。

        定时器回调可能在模态屏打开或应用关闭过程中触发，此时主工作台
        组件已不在活动 Screen 的 DOM 中，直接 query_one 会抛 NoMatches。
        """

        try:
            self.query_one("#token-telemetry", Static).update(
                self._token_telemetry_text()
            )
        except Exception:
            pass


    def _refresh_token_rate(self) -> None:
        """重算最近窗口内的生成速率并刷新顶部遥测；无采样时跳过。"""

        if len(self.screen_stack) > 1:
            # 模态审批屏成为活动 Screen 后主工作台不在查询树中，此时跳过。
            return
        if not self._generation_samples:
            if self._tokens_per_second:
                self._tokens_per_second = 0.0
                self._update_token_telemetry()
            return
        self._prune_generation_samples()
        if not self._generation_samples:
            # 窗口过期：速率归零并刷新，避免残留旧速率。
            self._tokens_per_second = 0.0
            self._update_token_telemetry()
            return
        now = time.monotonic()
        span = min(
            now - self._generation_samples[0][0],
            self.TOKEN_RATE_WINDOW_SECONDS,
        )
        # 防御刚收到大量增量立即刷新导致的瞬时尖峰。
        span = max(span, 0.25)
        total = sum(tokens for _, tokens in self._generation_samples)
        rate = total / span if span > 0 else 0.0
        if rate != self._tokens_per_second:
            self._tokens_per_second = rate
            self._update_token_telemetry()


    def _reset_token_rate(self) -> None:
        """回合结束或流回滚时归零速率并刷新遥测，避免残留旧速率。"""

        self._generation_samples.clear()
        self._tokens_per_second = 0.0
        self._update_token_telemetry()


    def _append_reasoning_delta(self, delta: str) -> None:
        if not delta:
            return
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._reasoning_message is None:
            self._reasoning_message = ReasoningDisclosure()
            conversation.mount(self._reasoning_message)
        self._reasoning_message.append_delta(delta)
        self._record_generation_delta(delta)
        self._set_runtime_status("正在思考", "working")
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        if self._reasoning_message is not None:
            # 思考阶段结束，同步补齐未完成行，保证推理内容展示完整。
            self._reasoning_message.flush_tail()
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._stream_message is None:
            # 记录本轮流式输出起点，供模型流中断自动重试前回滚已显示内容。
            self._stream_start_text_len = len(self.conversation_text)
            self._stream_message = AssistantMessage(self._stream_markdown)
            self._stream_markdown = "◇ "
            conversation.mount(self._stream_message)
        self._stream_markdown += delta
        self.conversation_text += f"{delta}\n"
        self._record_generation_delta(delta)
        if not self._stream_render_pending:
            self._stream_render_pending = True
            self.set_timer(self.STREAM_RENDER_INTERVAL_SECONDS, self._render_stream_markdown)
        self._set_runtime_status(
            "正在回复",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _render_stream_markdown(self) -> None:
        """合并短时间内的流式分片，避免逐片重解析完整 Markdown。"""

        self._stream_render_pending = False
        if self._stream_message is not None:
            conversations = self.query("#conversation")
            if not conversations or self._stream_message.parent is None:
                self._stream_render_pending = False
                return
            conversation = conversations.first(VerticalScroll)
            follow_latest = self._is_conversation_at_end(conversation)
            self._stream_message.update(self._stream_markdown)
            self._scroll_conversation_if_following(
                conversation,
                follow_latest,
                defer_until_refresh=True,
            )


    def _rollback_stream(self) -> None:
        """撤销本轮流式输出已展示的内容，供模型流中断自动重试前调用。

        重试成功后模型会重新生成完整回复；若不清除半截内容，新旧文本会
        在同一消息组件内拼接错乱。推理组件同样移除，因其内容也来自旧请求。
        """

        if self._stream_message is not None:
            try:
                if self._stream_message.parent is not None:
                    self._stream_message.remove()
            except Exception:
                pass
            self._stream_message = None
        self._stream_markdown = ""
        self._stream_render_pending = False
        if self._stream_start_text_len is not None:
            self.conversation_text = self.conversation_text[: self._stream_start_text_len]
            self._stream_start_text_len = None
        if self._reasoning_message is not None:
            try:
                if self._reasoning_message.parent is not None:
                    self._reasoning_message.remove()
            except Exception:
                pass
            self._reasoning_message = None
        self._reset_token_rate()


    def _append_message(
        self,
        kind: str,
        text: str,
        *,
        merge_with_previous: bool = False,
        track_tool: bool = False,
    ) -> None:
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        follow_with_runtime_status = self._runtime_status_message is not None
        if merge_with_previous and self._stream_message is not None:
            self._stream_markdown += text
            self._stream_message.update(self._stream_markdown)
        else:
            prefixes = {
                "user": "$ ",
                "assistant": "◇ ",
                "status": "· ",
                "tool": "⌁ ",
                "error": "△ ",
            }
            prefixed_text = f"{prefixes.get(kind, '· ')}{text}"
            if kind == "assistant":
                widget = AssistantMessage(prefixed_text)
            else:
                widget = Static(Text(prefixed_text), classes=f"message {kind}-message")
            if merge_with_previous:
                self._stream_message = widget
                self._stream_markdown = text
            else:
                self._stream_message = None
                self._stream_markdown = ""
                self._stream_start_text_len = None
            conversation.mount(widget)
            if track_tool:
                self._tool_messages[f"legacy:{id(widget)}"] = widget
        self.conversation_text += f"{text}\n"
        if follow_with_runtime_status:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _finish_turn(self) -> None:
        self._render_stream_markdown()
        was_cancelled = self._cancel_requested.is_set()
        self.is_generating = False
        if self._reasoning_message is not None:
            # 推理后直接结束回合（无回复/无工具）时，同样补齐未完成行。
            self._reasoning_message.flush_tail()
        self._reasoning_message = None
        self._reset_token_rate()
        # 取消回合的终态不能被 finally 中的通用完成逻辑覆盖为“完成”：
        # 状态文本保持与 cancel_pending_turn 展示的“已取消”一致。
        self._set_runtime_status("已取消" if was_cancelled else "完成", "complete")
        self.query_one("#composer", TextArea).focus()
        self._drain_pending_inputs()


    def _drain_pending_inputs(self) -> None:
        """在当前回合完成后按 FIFO 处理提交内容，避免 worker 重叠。"""

        while (
            self._pending_inputs
            and not self.is_generating
            and len(self.screen_stack) == 1
        ):
            text = self._pending_inputs.popleft()
            self._refresh_pending_queue_count()
            self._submit(text)


    def _set_runtime_status(
        self,
        text: str,
        state: str,
        *,
        follow_latest: bool | None = None,
    ) -> None:
        status_changed = (
            text != self._runtime_status_text or state != self._runtime_status_state
        )
        self._runtime_status_text = text
        self._runtime_status_state = state
        # 流式思考和回复分片会重复上报同一状态；仅在阶段切换时重置，
        # 否则高频事件会把动画持续钉在首帧。
        if status_changed:
            self._status_spinner_index = 0
        self._render_status_indicator(follow_latest=follow_latest)


    def _remove_runtime_status_message(self) -> None:
        status = self._runtime_status_message
        self._runtime_status_message = None
        if status is not None:
            status.remove()


    def _tick_status_indicator(self) -> None:
        """任务运行期间轮换固定宽度的 Braille 状态帧。"""

        # 模态审批成为当前 Screen 后，主工作台组件不在活动查询树中。此时暂停
        # 动画，既避免计时器访问隐藏状态，也不干扰 Esc 的审批取消绑定。
        if len(self.screen_stack) > 1:
            return
        for tree in self._subagent_trees.values():
            if tree.parent is not None:
                tree.refresh_elapsed()
        for tool_message in self._tool_messages.values():
            # 工具行与子代理树同节奏实时跳动；已完成的记录不在 dict 中，
            # 历史回放写入的 legacy 记录由 refresh_elapsed 的状态判断跳过。
            if tool_message.parent is not None:
                tool_message.refresh_elapsed()
        if self._runtime_status_state not in {"working", "waiting"}:
            return
        self._status_spinner_index = (
            self._status_spinner_index + 1
        ) % len(self.STATUS_SPINNER_FRAMES)
        self._render_status_indicator()


    def _render_status_indicator(self, *, follow_latest: bool | None = None) -> None:
        is_active = self._runtime_status_state in {"working", "waiting"}
        if not is_active:
            self._remove_runtime_status_message()
            return

        conversations = self.query("#conversation")
        if not conversations:
            return
        conversation = conversations.first(VerticalScroll)
        if follow_latest is None:
            follow_latest = self._is_conversation_at_end(conversation)
        status = self._runtime_status_message
        if status is None:
            status = Static("", classes="message runtime-status-message")
            self._runtime_status_message = status
            conversation.mount(status)
        spinner_frame = self.STATUS_SPINNER_FRAMES[self._status_spinner_index]
        status.update(f"{spinner_frame} {self._runtime_status_text}")
        if (
            status.parent is conversation
            and conversation.children
            and conversation.children[-1] is not status
        ):
            conversation.move_child(status, after=conversation.children[-1])
        self._scroll_conversation_if_following(conversation, follow_latest)

