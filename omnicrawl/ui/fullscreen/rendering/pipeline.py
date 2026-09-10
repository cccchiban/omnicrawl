"""全屏工作台的事件聚合与流式渲染管线。

P2 重构从 ``ui/fullscreen/__init__.py`` 的 ``OmniCrawlApp`` 拆出的独立模块
（2026-08-11）。这里集中：Agent 协议事件聚合（status/subagent/tool/token）、
流式 Markdown 渲染与回滚、运行时状态指示器、统计平均生成速率累计。

``RenderingMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；
跨领域方法（``_token_telemetry_text``、``_refresh_pending_queue_count``、
``_submit`` 等）仍通过 ``self`` 在 ``OmniCrawlApp`` 的 MRO 上解析。
类常量（``STREAM_RENDER_INTERVAL_SECONDS``、
``GENERATION_STANDBY_GAP_SECONDS``、``STATUS_SPINNER_FRAMES``）与实例
状态仍定义在 ``OmniCrawlApp``。
"""

from __future__ import annotations

import time
from datetime import datetime
from typing import Any

from rich.text import Text
from textual.containers import VerticalScroll
from textual.widgets import Static, TextArea

from ....agent.toolkit.tools import (
    ASK_USER_TOOL_NAME,
    TODO_TOOL_NAME,
    public_tool_arguments,
)
from ..terminal.theme import TOOL_TEXT
from .widgets import (
    AssistantMessage,
    ReasoningDisclosure,
    RuntimeStatus,
    SubAgentConversation,
    SubAgentProgressTree,
    ToolDisclosure,
)


class RenderingMixin:
    """原 ``OmniCrawlApp`` 的事件聚合与流式渲染方法。"""

    def _active_announcer(self):
        """返回当前可用的回合朗读器（无实例/未启用时返回 None）。"""
        get_instance = getattr(self, "_speech_announcer_instance", None)
        if not callable(get_instance):
            return None
        try:
            return get_instance()
        except Exception:  # noqa: BLE001 - 朗读器异常不拖垮渲染
            return None

    @property
    def _stream_markdown(self) -> str:
        """当前流式消息的完整 Markdown（含 ``◇ `` 前缀，惰性拼接）。

        流式分片先累积到 ``_stream_chunks``，仅在停顿/收口全量重绘时
        join，避免 ``str +=`` 的 O(n²) 累积。
        """

        return "◇ " + "".join(self._stream_chunks)

    @_stream_markdown.setter
    def _stream_markdown(self, value: str) -> None:
        if value:
            if value.startswith("◇ "):
                value = value[2:]
            self._stream_chunks = [value]
        else:
            self._stream_chunks = []

    @staticmethod
    def _is_conversation_at_end(conversation: VerticalScroll) -> bool:
        """判断用户是否仍在消息流底部，避免后台更新抢回滚动位置。"""

        return conversation.is_vertical_scroll_end


    def _stream_logical_lines(self) -> int:
        """当前流式消息的逻辑行数（增量累计，避免逐分片 splitlines）。"""

        return self._stream_nl_count + (0 if self._stream_ends_newline else 1)

    def _reset_stream_state(self) -> None:
        """复位流式渲染的状态字段（收口/封口/清空后必须保持一致）。

        仅复位渲染管线自身维护的字段；``_stream_message``（消息组件引用）
        与 ``_stream_start_text_len``（回滚起点）由调用方按场景单独处理。
        """

        self._stream_markdown = ""
        self._stream_render_buffer = ""
        self._stream_nl_count = 0
        self._stream_ends_newline = True
        self._stream_last_delta_at = 0.0
        self._stream_render_pending = False

    def _stream_is_settled(self) -> bool:
        """距最后一个流式分片是否已超过停顿阈值。"""

        return (time.monotonic() - self._stream_last_delta_at) >= self.STREAM_SETTLE_SECONDS

    def _flush_stream_buffer(self, message: AssistantMessage) -> None:
        """把已就绪的流式缓冲按块增量渲染（保留跨行完整性）。

        缓冲只在遇到换行（或超过 STREAM_CHUNK_LIMIT）时落盘，且只落盘到
        最后一个换行为止的完整行，未换行的尾部留在缓冲中等待后续分片，
        避免同一行文本被切成多段分片渲染而错行。行数/可见性与滚动由
        调用方统一处理。
        """

        if not self._stream_render_buffer:
            return
        buffer = self._stream_render_buffer
        self._stream_render_buffer = ""
        head, sep, tail = buffer.rpartition("\n")
        if sep:
            self._stream_render_buffer = tail
            chunk = head + sep
        else:
            chunk = buffer
        if chunk:
            message.append_stream_chunk(chunk)

    @staticmethod
    def _scroll_conversation_if_following(
        conversation: VerticalScroll,
        follow_latest: bool,
    ) -> None:
        """仅在用户原本位于底部时跟随新增内容。"""

        if follow_latest:
            if conversation.max_scroll_y > 0:
                # Textual 的锚定语义会在内容重新布局后持续跟随底部，并在用户
                # 手动滚动时自动释放；这比跨刷新排队 scroll_end 更能避免流式
                # 更新竞态。
                # 已锚定时不再重复调用 anchor()（其内部会立即 scroll_end，
                # 触发整条会话同步布局）：流式/动画高频路径只需维持锚定状态，
                # 布局期 compositor 会自动跟随底部。
                if not conversation.is_anchored:
                    conversation.anchor()
                # 不得再排队 scroll_end：锚定后新增内容由布局期 compositor
                # 自动跟随底部；若跨刷新排队 scroll_end，用户上滑释放锚定后
                # 迟到的回调仍会执行 Textual 的 scroll_end（先清除
                # _anchor_released 再贴底），把用户拉回底部。
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


    @staticmethod
    def _replay_event_value(event: Any, key: str, default: Any = None) -> Any:
        """从 SessionEvent 或测试替身中读取统一字段。"""

        if isinstance(event, dict):
            return event.get(key, default)
        return getattr(event, key, default)

    def _replay_event_timestamp(self, event: Any, fallback: float) -> float:
        """把持久化事件时间转换为工具卡可用的单调无关时间戳。"""

        value = self._replay_event_value(event, "created_at")
        if isinstance(value, datetime):
            return value.timestamp()
        if isinstance(value, (int, float)):
            return float(value)
        if isinstance(value, str) and value.strip():
            try:
                return datetime.fromisoformat(value.strip().replace("Z", "+00:00")).timestamp()
            except ValueError:
                pass
        return fallback

    def _replay_tool_output(self, payload: dict[str, Any]) -> str:
        """优先读取工具结果 artifact，使历史工具正文与实时页面一致。"""

        artifact_path = payload.get("artifact_path")
        reader = getattr(self.agent, "read_session_artifact_text", None)
        session_id = getattr(self.agent, "current_session_id", "")
        if isinstance(artifact_path, str) and artifact_path.strip() and callable(reader):
            try:
                artifact = reader(str(session_id), artifact_path)
            except Exception:
                artifact = ""
            if isinstance(artifact, str) and artifact:
                return artifact

        for key in ("full_output", "output", "model_output", "output_preview"):
            value = payload.get(key)
            if isinstance(value, str) and value:
                return value
        # 与实时 `_handle_tool_result` 保持一致：非隐藏工具的空结果也有
        # 一行“无输出”正文，避免恢复页比实时页少一行。
        return "无输出"

    def _replay_session_events(self, events: list[Any]) -> None:
        """按持久化事件重建历史页面，而不是把模型消息投影逐条塞进 TUI。

        模型上下文投影会把工具请求/结果转换为 assistant 文本；那条投影适合
        继续请求模型，却会丢失实时页面中的工具卡、结果状态和 SubAgent 进度
        树。因此恢复展示必须消费事件流，并复用实时渲染组件的同一套入口。
        """

        self._clear_conversation_view()
        pending_by_id: dict[str, ToolDisclosure] = {}
        pending_by_tool: dict[str, list[ToolDisclosure]] = {}
        denied_by_id: dict[str, str] = {}
        replay_now = time.time()

        subagent_event_names = {
            "subagent_task_queued": "subagent.task.queued",
            "subagent_task_started": "subagent.task.started",
            "subagent_task_running": "subagent.task.running",
            "subagent_task_waiting_approval": "subagent.task.waiting_approval",
            "subagent_task_completed": "subagent.task.completed",
            "subagent_task_failed": "subagent.task.failed",
            "subagent_task_cancelled": "subagent.task.cancelled",
            "subagent_task_approval_cancelled": "subagent.task.approval_cancelled",
        }

        def remove_pending(widget: ToolDisclosure) -> None:
            for call_id, candidate in list(pending_by_id.items()):
                if candidate is widget:
                    pending_by_id.pop(call_id, None)
            for tool_name, candidates in list(pending_by_tool.items()):
                pending_by_tool[tool_name] = [
                    candidate for candidate in candidates if candidate is not widget
                ]
                if not pending_by_tool[tool_name]:
                    pending_by_tool.pop(tool_name, None)

        def find_pending(payload: dict[str, Any]) -> ToolDisclosure | None:
            call_id = str(payload.get("tool_call_id") or "").strip()
            if call_id and call_id in pending_by_id:
                return pending_by_id[call_id]
            tool_name = str(payload.get("tool") or "").strip()
            candidates = pending_by_tool.get(tool_name, [])
            return candidates[0] if candidates else None

        for index, event in enumerate(events):
            event_type = str(self._replay_event_value(event, "type", "") or "")
            payload = self._replay_event_value(event, "payload", {})
            if not isinstance(payload, dict):
                payload = {}
            event_time = self._replay_event_timestamp(event, replay_now + index)

            if event_type == "user_message":
                content = payload.get("content")
                if isinstance(content, str) and content.strip():
                    self._append_message("user", content)
                continue

            if event_type == "assistant_message":
                content = payload.get("session_content", payload.get("content"))
                if isinstance(content, str) and content.strip():
                    self._append_message("assistant", content)
                continue

            if event_type == "tool_call_requested":
                tool_name = str(payload.get("tool") or "").strip()
                if not tool_name:
                    continue
                arguments = payload.get("arguments")
                if not isinstance(arguments, dict):
                    arguments = {}
                if tool_name == TODO_TOOL_NAME:
                    # Todo 工具是展示层状态更新，不在会话区生成工具卡；
                    # 恢复时使用最后一次提交的清单重建输入框上方计划区。
                    self._handle_todo_update({"todos": arguments.get("todos", [])})
                    continue
                if tool_name == ASK_USER_TOOL_NAME:
                    # ask_user 的问题属于入口控制面板，不把控制参数当成
                    # 普通会话消息或工具卡正文重放。
                    continue
                widget = ToolDisclosure(tool_name, arguments, event_time)
                conversation = self.query_one("#conversation", VerticalScroll)
                conversation.mount(widget)
                self._register_conversation_widget(widget, tool_name)
                self._append_conversation_text(f"{tool_name}\n")
                call_id = str(payload.get("tool_call_id") or "").strip()
                if call_id:
                    pending_by_id[call_id] = widget
                pending_by_tool.setdefault(tool_name, []).append(widget)
                continue

            if event_type == "tool_call_denied":
                tool_name = str(payload.get("tool") or "").strip()
                reason = str(payload.get("reason") or "工具调用未获批准。")
                call_id = str(payload.get("tool_call_id") or "").strip()
                if call_id:
                    denied_by_id[call_id] = reason
                elif tool_name:
                    # 没有 call id 的旧事件按工具名延后匹配，避免在随后
                    # 的 tool_result 到达前把实时页面错误地提前封口。
                    denied_by_id[f"tool:{tool_name}"] = reason
                continue

            if event_type == "tool_result":
                if str(payload.get("tool") or "").strip() in {
                    TODO_TOOL_NAME,
                    ASK_USER_TOOL_NAME,
                }:
                    continue
                widget = find_pending(payload)
                if widget is None:
                    tool_name = str(payload.get("tool") or "未知工具")
                    widget = ToolDisclosure(tool_name, {}, event_time)
                    self.query_one("#conversation", VerticalScroll).mount(widget)
                    self._register_conversation_widget(widget, tool_name)
                output = self._replay_tool_output(payload)
                widget.finish(
                    ok=bool(payload.get("ok", False)),
                    output=output,
                    finished_at=max(event_time, widget.started_at),
                )
                self._register_conversation_widget(widget)
                remove_pending(widget)
                continue

            replay_subagent_type = subagent_event_names.get(event_type)
            if replay_subagent_type is not None:
                self._handle_subagent_event(replay_subagent_type, payload)
                continue

            if event_type == "turn_cancelled":
                summary = payload.get("summary")
                if not isinstance(summary, str) or not summary.strip():
                    summary = "（上一回合被取消，未生成最终回复）"
                self._append_message("assistant", summary)
                continue

            if event_type == "session_interrupted":
                self._append_message("status", "上一回合在会话恢复前中断。")
                continue

            if event_type == "compact_summary":
                content = payload.get("content")
                if isinstance(content, str) and content.strip():
                    self._append_message("assistant", f"会话压缩摘要：\n{content}")

        for widget in list(pending_by_id.values()) + [
            widget
            for candidates in pending_by_tool.values()
            for widget in candidates
            if widget not in pending_by_id.values()
        ]:
            call_id = next(
                (
                    value
                    for value, candidate in pending_by_id.items()
                    if candidate is widget
                ),
                "",
            )
            reason = denied_by_id.get(call_id) or denied_by_id.get(
                f"tool:{widget.tool_name}",
                "工具调用在会话结束前未收到结果。",
            )
            widget.finish(
                ok=False,
                output=reason,
                finished_at=max(replay_now, widget.started_at),
            )
            remove_pending(widget)

        self._conversation_visibility_batching = False
        self._request_conversation_visibility_refresh()


    def _handle_status(self, message: str) -> None:
        if not message:
            return
        if message.startswith("压缩完成"):
            self._append_message("status", message)
            return
        if message.startswith("正在重试"):
            # 重试仍处于等待模型回复阶段：用 working 状态显示
            # “⠸ 正在重试(第N次)”，与“正在思考”同帧节奏，不落入静态等待。
            self._set_runtime_status(message, "working")
            return
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
        # /review 等派生评审流程：子代理对话面板（│ 包裹）替代进度树。
        if getattr(self, "_conversation_stream_active", False):
            self._handle_subagent_conversation_event(event_name, payload)
            return

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
            self._register_conversation_widget(tree, "◇ 子任务进度")
        tree.update_task(
            task_id=task_id,
            agent_type=str(payload.get("agent_type") or "subagent"),
            description=str(payload.get("description") or task_id),
            status=status,
        )
        self._register_conversation_widget(tree, tree.render_text().plain)
        # 运行状态始终保持为消息流末项；树新增或增高后需恢复这一顺序。
        if self._runtime_status_message is not None:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )


    def _handle_subagent_conversation_event(
        self,
        event_name: str,
        payload: dict[str, Any],
    ) -> None:
        """把子代理对话/工具事件渲染到 │ 包裹的会话面板。"""

        task_id = str(payload.get("task_id") or "task")
        batch_id = str(payload.get("batch_id") or f"batch-{task_id}")
        agent_type = str(payload.get("agent_type") or "subagent")
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        panel = self._subagent_conversations.get(batch_id)
        if panel is None or panel.parent is None:
            panel = SubAgentConversation(batch_id, agent_type)
            self._subagent_conversations[batch_id] = panel
            conversation.mount(panel)
            self._register_conversation_widget(panel, panel.logical_text)

        if event_name == "subagent.tool.started":
            panel.append(
                "⌁ " + self._subagent_tool_brief(
                    str(payload.get("tool") or ""),
                    payload.get("arguments"),
                )
            )
        elif event_name == "subagent.tool.completed":
            ok = bool(payload.get("ok"))
            duration = payload.get("duration_seconds")
            suffix = (
                f" · {float(duration):.2f}s"
                if isinstance(duration, (int, float))
                else ""
            )
            panel.append(
                f"● {'成功' if ok else '失败'}{suffix}",
                "green" if ok else "red",
            )
            output = str(payload.get("output") or "")
            for line in self._sample_output_lines(output):
                panel.append(line, "dim")
        elif event_name == "subagent.turn.text":
            for line in str(payload.get("text") or "").splitlines():
                if line.strip():
                    panel.append(line)
        elif event_name in {
            "subagent.task.completed",
            "subagent.task.failed",
            "subagent.task.cancelled",
            "subagent.task.approval_cancelled",
        }:
            if event_name == "subagent.task.completed":
                panel.finish("✓ 子代理评审完成")
            elif event_name == "subagent.task.cancelled" or event_name == "subagent.task.approval_cancelled":
                panel.finish("– 子代理评审已取消")
            else:
                failure_line = "× 子代理评审失败"
                error = payload.get("error")
                if isinstance(error, dict):
                    reason = str(error.get("message") or "").strip()
                    code = str(error.get("code") or "").strip()
                    diagnostic = error.get("diagnostic")
                    category = ""
                    if isinstance(diagnostic, dict):
                        category = str(diagnostic.get("category") or "").strip()
                    labels = [part for part in (code, category) if part]
                    if reason:
                        failure_line += f"：{reason}"
                    if labels:
                        failure_line += "（" + "，".join(labels) + "）"
                panel.finish(failure_line)
            self._set_runtime_status("完成", "complete")

        self._set_conversation_widget_line_count(panel, panel.logical_text)
        self._request_conversation_visibility_refresh()
        if self._runtime_status_message is not None:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )

    @staticmethod
    def _subagent_tool_brief(tool_name: str, arguments: Any) -> str:
        """把工具调用压缩为一行摘要：git 显示 action+参数，其余显示首个路径字段。"""

        parts: list[str] = []
        if tool_name == "git" and isinstance(arguments, dict):
            action = arguments.get("action")
            if action:
                parts.append(str(action))
            raw_args = arguments.get("args")
            if isinstance(raw_args, list):
                parts.extend(str(item) for item in raw_args)
        elif isinstance(arguments, dict):
            for key in ("path", "paths", "pattern", "query", "text", "scope"):
                value = arguments.get(key)
                if isinstance(value, str) and value.strip():
                    parts.append(value.strip())
                    break
                if isinstance(value, list) and value:
                    parts.append(str(value[0]))
                    break
        brief = " ".join(parts).strip()
        if len(brief) > 60:
            brief = brief[:57] + "..."
        return f"{tool_name} · {brief}" if brief else tool_name

    @staticmethod
    def _sample_output_lines(output: str, *, max_lines: int = 5) -> list[str]:
        """工具输出采样：最多五行，超出时保留首尾各两行。"""

        lines = [line.rstrip() for line in output.splitlines() if line.strip()]
        if len(lines) <= max_lines:
            return lines
        return lines[:2] + ["…"] + lines[-2:]


    def _handle_tool_start(self, step: int, tool_call: Any) -> None:
        del step  # Agent 仍按步骤回调，但极简 HUD 不展示内部步骤编号。
        self._hide_welcome_logo()
        # 工具调用是模型 pass 的明确边界：把此前流式分片全量收口为精确
        # Markdown，再封口回复组件。
        self._render_stream_markdown(force=True)
        # 工具调用是模型 pass 的明确边界。必须封口此前的回复组件，否则工具
        # 返回后的最终回答会继续写入旧组件，在视觉上倒插到工具记录之前。
        if self._reasoning_message is not None:
            # 先补齐未完成行，再封口思考组件。
            self._reasoning_message.flush_tail()
        self._stream_message = None
        self._reset_stream_state()
        self._stream_start_text_len = None
        self._reasoning_message = None
        tool_name = str(getattr(tool_call, "name", ""))
        if tool_name == TODO_TOOL_NAME:
            self._set_runtime_status("正在更新计划", "working")
            return
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        tool_message = ToolDisclosure(
            tool_name,
            self._public_tool_arguments(tool_call),
            time.perf_counter(),
        )
        self._tool_messages[self._tool_call_key(tool_call)] = tool_message
        conversation.mount(tool_message)
        self._register_conversation_widget(tool_message, tool_name)
        self._append_conversation_text(f"{tool_name}\n")
        # ask_user 提问期间底部状态同样显示「等待回复」，与工具卡上的
        # 「↘ 等待回复...」一致；回答后由 _handle_tool_result 恢复。
        self._set_runtime_status(
            "等待回复" if tool_name == ASK_USER_TOOL_NAME else "正在调用",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _handle_tool_result(self, tool_call: Any, result: Any) -> None:
        tool_name = str(getattr(tool_call, "name", ""))
        if tool_name == TODO_TOOL_NAME:
            self._tool_messages.pop(self._tool_call_key(tool_call), None)
            self._set_runtime_status("正在思考", "working")
            return
        if tool_name == ASK_USER_TOOL_NAME:
            # 用户回答后：工具卡由「↘ 等待回复...」收口为「↗ 已收到回复」
            # 并冻结实际等待耗时，随后退出实时计时集合。
            key = self._tool_call_key(tool_call)
            tool_message = self._tool_messages.pop(key, None)
            if tool_message is None:
                # 兼容缺失 start 事件的协议实现，同时保持既有交互。
                tool_message = ToolDisclosure(
                    tool_name,
                    self._public_tool_arguments(tool_call),
                    time.perf_counter(),
                )
                self.query_one("#conversation", VerticalScroll).mount(tool_message)
                self._register_conversation_widget(tool_message, tool_name)
            completed_at = getattr(result, "completed_at", None)
            output = str(
                getattr(result, "full_output", "") or result.output or "无输出"
            )
            tool_message.finish(
                ok=bool(result.ok),
                output=output,
                finished_at=(
                    completed_at if completed_at is not None else time.perf_counter()
                ),
            )
            self._register_conversation_widget(tool_message)
            self._set_runtime_status("正在思考", "working")
            return
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
            self._register_conversation_widget(tool_message, str(tool_call.name))
        # 优先用 Agent 层记录的真实完成时刻（快工具提前完成、整批等待慢工具
        # 时也能显示各自真实耗时）；缺省时退回到当前时刻。
        completed_at = getattr(result, "completed_at", None)
        tool_message.finish(
            ok=bool(result.ok),
            output=output,
            finished_at=completed_at if completed_at is not None else time.perf_counter(),
        )
        self._register_conversation_widget(tool_message)
        self._append_conversation_text(f"结果  {tool_message.status}\n{output}\n")
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
        self._carousel_refresh()


    @staticmethod
    def _estimate_generation_tokens(text: str) -> float:
        """把流式文本增量粗略估算为 token 数。

        CJK 字符按 1 token、其他字符按 4 字符 1 token 估算。供应商不提供
        流中逐片 token 计数，该估算只用于实时速率展示，不做精确计量。
        """

        cjk = sum(1 for ch in text if ord(ch) > 0x2E7F)
        return cjk + (len(text) - cjk) / 4.0


    def _record_generation_delta(self, delta: str) -> None:
        """累计思考/正文流增量的估算 token 与连续输出时长。

        每次增量把估算 token 累入总输出 token；与上一次增量的间隔不超过
        ``GENERATION_STANDBY_GAP_SECONDS`` 的视为连续输出，该间隔计入总
        输出时长；超过阈值的间隔为待机（工具执行、模型停顿、回合间隙），
        不计时。这样 t/s = 总输出 token ÷ 总输出时长 只反映实际输出速度，
        不被空闲等待稀释，且输出停止后统计平均值保留不归零。
        """

        if not delta:
            return
        tokens = self._estimate_generation_tokens(delta)
        self._generation_total_tokens += tokens
        now = time.monotonic()
        if self._last_generation_at is not None:
            gap = now - self._last_generation_at
            if 0 < gap <= self.GENERATION_STANDBY_GAP_SECONDS:
                self._generation_total_seconds += gap
        self._last_generation_at = now


    def _update_token_telemetry(self) -> None:
        """刷新底部轮播 HUD 中的遥测页；widget 不在活动查询树中时静默跳过。

        定时器回调可能在模态屏打开或应用关闭过程中触发，此时主工作台
        组件已不在活动 Screen 的 DOM 中，query_one 会抛 NoMatches。
        """

        self._carousel_refresh()


    def _refresh_token_rate(self) -> None:
        """按累计统计平均重算 t/s 并刷新顶部遥测；无输出记录时保持 --。

        速率 = 累计输出 token ÷ 累计输出时长（会话级平均值），输出停止或
        回合结束后保留不归零；尚无输出或输出时长不可测时保持 0，显示 --。
        """

        if len(self.screen_stack) > 1:
            # 模态审批屏成为活动 Screen 后主工作台不在查询树中，此时跳过。
            return
        if self._generation_total_seconds > 0:
            rate = self._generation_total_tokens / self._generation_total_seconds
        else:
            # 尚无输出记录（或单分片回复输出时长不可测）：不展示虚假速率。
            rate = 0.0
        if rate != self._tokens_per_second:
            self._tokens_per_second = rate
            self._update_token_telemetry()


    def _append_reasoning_delta(self, delta: str) -> None:
        if not delta:
            return
        self._hide_welcome_logo()
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        show_thinking = bool(
            getattr(getattr(self.agent, "config", None), "show_thinking", True)
        )
        # 思考显示关闭时仍正常累计文本状态与 token 统计，只是不创建/挂载
        # 思考块组件（Markdown 渲染）；推理链路本身不受影响。
        if show_thinking and self._reasoning_message is None:
            self._reasoning_message = ReasoningDisclosure()
            conversation.mount(self._reasoning_message)
        if self._reasoning_message is not None:
            self._reasoning_message.append_delta(delta)
            # 行数由组件增量累计，避免逐分片对完整思考文本 splitlines。
            self._set_conversation_widget_lines(
                self._reasoning_message,
                self._reasoning_message._line_count,
            )
            self._request_conversation_visibility_refresh()
        self._record_generation_delta(delta)
        self._set_runtime_status("正在思考", "working")
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )


    def _append_delta(self, delta: str) -> None:
        if not delta:
            return
        self._hide_welcome_logo()
        if self._reasoning_message is not None:
            # 思考阶段结束，同步补齐未完成行，保证推理内容展示完整。
            self._reasoning_message.flush_tail()
            self._set_conversation_widget_lines(
                self._reasoning_message,
                self._reasoning_message._line_count,
            )
            self._request_conversation_visibility_refresh()
        self._reasoning_message = None
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        if self._stream_message is None:
            # 记录本轮流式输出起点，供模型流中断自动重试前回滚已显示内容。
            self._stream_start_text_len = self._conversation_text_len
            self._stream_message = AssistantMessage()
            conversation.mount(self._stream_message)
            self._register_conversation_widget(self._stream_message, "")
        self._stream_chunks.append(delta)
        self._stream_nl_count += delta.count("\n")
        self._stream_ends_newline = delta.endswith("\n")
        self._stream_last_delta_at = time.monotonic()
        # 换行边界缓冲：只增量渲染已就绪的完整行，避免逐分片全量重解析
        # 整条消息的 Markdown（长消息数百毫秒/次）。
        self._stream_render_buffer += delta
        if "\n" in self._stream_render_buffer or len(
            self._stream_render_buffer
        ) >= self.STREAM_CHUNK_LIMIT:
            self._flush_stream_buffer(self._stream_message)
        self._set_conversation_widget_lines(self._stream_message, self._stream_logical_lines())
        self._request_conversation_visibility_refresh()
        self._append_conversation_text(f"{delta}\n")
        self._record_generation_delta(delta)
        if not self._stream_render_pending:
            self._stream_render_pending = True
            self.set_timer(self.STREAM_RENDER_INTERVAL_SECONDS, self._render_stream_markdown)
        self._set_runtime_status(
            "正在回复",
            "working",
            follow_latest=follow_latest,
        )
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )
        announcer = self._active_announcer()
        if announcer is not None:
            announcer.feed_text(delta)


    def _render_stream_markdown(self, *, force: bool = False) -> None:
        """合并短时间内的流式分片，避免逐片重解析完整 Markdown。

        流式期间正文已由 ``_append_delta`` 按换行边界增量渲染；本方法只
        在流式停顿（``STREAM_SETTLE_SECONDS``）或显式收口（``force``，
        工具边界/回合结束）时做一次全量精确重绘，修正跨分块 Markdown 的
        近似结果。超过 ``STREAM_FULL_RENDER_LIMIT`` 的长消息在停顿时不再
        全量重绘（保留增量渲染结果），避免一次停顿触发数百毫秒的同步
        重解析。
        """

        self._stream_render_pending = False
        message = self._stream_message
        if message is None or message.parent is None:
            return
        conversations = self.query("#conversation")
        if not conversations:
            return
        conversation = conversations.first(VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        settled = force or self._stream_is_settled()
        if settled:
            markdown = self._stream_markdown
            if force or len(markdown) <= self.STREAM_FULL_RENDER_LIMIT:
                self._stream_render_buffer = ""
                message.update(markdown)
            elif self._stream_render_buffer:
                # 长消息停顿：不做全量重绘，但要把残留的未换行缓冲落盘，
                # 避免尾部文本停在不可见状态。
                self._flush_stream_buffer(message)
        self._set_conversation_widget_lines(message, self._stream_logical_lines())
        self._request_conversation_visibility_refresh()
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )
        if not settled and not self._stream_render_pending:
            # 定时器早于停顿阈值触发（流式分片间隔 < STREAM_SETTLE_SECONDS
            # 时经常发生）：若不再续排，后续停顿将永远没有收口回调，残留的
            # 未换行缓冲会一直停留在不可见状态。此处重排一次定时器，让它在
            # 距最后分片满阈值后再做全量收口。
            self._stream_render_pending = True
            self.set_timer(self.STREAM_RENDER_INTERVAL_SECONDS, self._render_stream_markdown)


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
        self._reset_stream_state()
        self._mark_conversation_visibility_dirty()
        self._request_conversation_visibility_refresh()
        if self._stream_start_text_len is not None:
            self._truncate_conversation_text(self._stream_start_text_len)
            self._stream_start_text_len = None
        if self._reasoning_message is not None:
            try:
                if self._reasoning_message.parent is not None:
                    self._reasoning_message.remove()
            except Exception:
                pass
            self._reasoning_message = None
        # 撤销本次回合已累计的 token 与输出时长：重试生成的完整回复不会与
        # 半截输出重复计数；统计平均值本身保留（不归零）。
        snapshot = self._generation_stats_snapshot
        if snapshot is not None:
            (
                self._generation_total_tokens,
                self._generation_total_seconds,
                self._last_generation_at,
            ) = snapshot
        self._refresh_token_rate()
        announcer = self._active_announcer()
        if announcer is not None:
            announcer.rollback_turn()


    def _append_message(
        self,
        kind: str,
        text: str,
        *,
        merge_with_previous: bool = False,
        track_tool: bool = False,
    ) -> None:
        self._hide_welcome_logo()
        conversation = self.query_one("#conversation", VerticalScroll)
        follow_latest = self._is_conversation_at_end(conversation)
        follow_with_runtime_status = self._runtime_status_message is not None
        if merge_with_previous and self._stream_message is not None:
            # 合并路径不常用：直接全量收口，保证内容与行数一致。
            self._stream_markdown += text
            self._stream_render_buffer = ""
            self._stream_nl_count = self._stream_markdown.count("\n")
            self._stream_ends_newline = self._stream_markdown.endswith("\n")
            self._stream_message.update(self._stream_markdown)
            self._set_conversation_widget_line_count(self._stream_message, self._stream_markdown)
            self._request_conversation_visibility_refresh()
        else:
            if kind == "user":
                # 用户消息：去掉 $ 前缀，改为顶部灰色斜体 user： 标签行
                # （与正文同左缘对齐）；正文为显式白色（.user-message
                # color: $terminal-white），青色细竖条由 border-left 提供。
                # 注意 Text 构造器的 style 会成为后续 append 的默认样式，
                # 因此从空 Text 开始逐段追加，保证正文不继承标签的斜体。
                labeled = Text()
                labeled.append("user：", style=f"{TOOL_TEXT} italic")
                labeled.append("\n")
                labeled.append(text)
                widget = Static(labeled, classes="message user-message")
            else:
                prefixes = {
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
                # 若仍有未收口的流式消息（如流式期间插入状态消息），先全量
                # 收口为精确 Markdown，避免其停留在分块渲染的近似状态。
                if self._stream_message is not None:
                    self._render_stream_markdown(force=True)
                self._stream_message = None
                self._reset_stream_state()
                self._stream_start_text_len = None
            conversation.mount(widget)
            self._register_conversation_widget(widget, text)
            if track_tool:
                self._tool_messages[f"legacy:{id(widget)}"] = widget
        self._append_conversation_text(f"{text}\n")
        if follow_with_runtime_status:
            self._render_status_indicator(follow_latest=follow_latest)
        self._scroll_conversation_if_following(conversation, follow_latest)


    def _finish_turn(self) -> None:
        # 回合结束必须收口：把流式分块渲染的近似结果重绘为精确 Markdown。
        self._render_stream_markdown(force=True)
        was_cancelled = self._cancel_requested.is_set()
        self.is_generating = False
        # 派生评审流程结束：退出子代理对话流模式，后续子代理事件恢复进度树。
        self._conversation_stream_active = False
        announcer = self._active_announcer()
        if announcer is not None:
            announcer.flush_turn(cancelled=was_cancelled)
        if self._reasoning_message is not None:
            # 推理后直接结束回合（无回复/无工具）时，同样补齐未完成行。
            self._reasoning_message.flush_tail()
        self._reasoning_message = None
        # 回合结束刷新遥测：统计平均速率保留，不再归零为 --。
        self._refresh_token_rate()
        # 取消回合的终态不能被 finally 中的通用完成逻辑覆盖为“完成”：
        # 状态文本保持与 cancel_pending_turn 展示的“已取消”一致。
        self._set_runtime_status("已取消" if was_cancelled else "完成", "complete")
        # 回合结束兜底关闭仍显示的提问面板：ask_user 工具批次超时后后台
        # 等待线程可能尚未清理（或用户始终未答），不能把提问残留到下一回合。
        if self._ask_user_request is not None:
            self._set_ask_user_request(None)
            # 唤醒仍阻塞在 _ask_user 等待循环中的后台线程，让其读到空答案
            # 并退出，避免旧请求与新回合的提问复用同一 Event 造成错乱。
            self._ask_user_event.set()
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
            status = RuntimeStatus()
            self._runtime_status_message = status
            conversation.mount(status)
        spinner_frame = self.STATUS_SPINNER_FRAMES[self._status_spinner_index]
        # 所有活动中的回合状态都支持 Esc 取消；在状态行尾固定显示提示，
        # 让“正在思考/回复/调用”等同类状态的中断入口清晰可见。
        # [ ESC ] 是 RuntimeStatus 的恒定子组件：状态文本高频重绘不触碰
        # 它，鼠标悬停由 CSS :hover 单独点亮为淡蓝色。
        status_text = Text(f"{spinner_frame} {self._runtime_status_text}")
        status.update_status(status_text)
        if (
            status.parent is conversation
            and conversation.children
            and conversation.children[-1] is not status
        ):
            conversation.move_child(status, after=conversation.children[-1])
        self._scroll_conversation_if_following(
            conversation,
            follow_latest,
        )

