"""上下文压缩与溢出恢复：手动/自动压缩、压缩记忆回写与会话归档。"""
from __future__ import annotations

import logging
from typing import Any, Callable, Mapping, Sequence
from ...context_compaction import (
    ContextCompactionService,
    ModelSummaryCompactor,
    RuntimeSummaryModelAdapter,
    SourceEvent,
    TokenUsageSample,
    estimate_json_tokens,
)
from ...session.history import compact_history
from ....llm import (
    LLMConfig,
)
from ....memory import (
    MemoryWriteRequest,
)
from ....session import (
    COMPACT_SUMMARY_PREFIX,
)
from ....state.session_projection import project_compaction_boundary_history

from ..shared import (
    AgentError,
)

LOGGER = logging.getLogger(__name__)

# 摘要请求沿用原请求前缀，索引块预算按剩余窗口收缩，避免「前缀 + 事件索引」超出摘要模型窗口。
_SUMMARY_INDEX_CHUNK_TOKENS = 12_000


class TurnCompactionMixin:
    """上下文压缩与溢出恢复：手动/自动压缩、压缩记忆回写与会话归档。"""

    def compact_conversation(self) -> str:
        """手动确定性压缩当前会话；该入口不产生模型调用。"""

        if len(self._history) < 4:
            raise AgentError("当前会话内容太少，暂不需要压缩。")
        summary = self._compact_history(force=True)
        if not summary:
            raise AgentError("当前会话内容太少，暂不需要压缩。")
        return summary

    def compact_conversation_model(self) -> str:
        """显式使用结构化摘要模型压缩。"""

        config = self.config.context_compaction
        source_events = self._context_compaction_source_events()
        service = self._context_compaction_service()
        outcome = service.manual_compact(
            source_events=source_events,
            target_summary_tokens=config.target_summary_tokens,
            reasoning_effort=config.reasoning_effort,
            preserve_exact_evidence=config.preserve_exact_evidence,
        )
        if outcome.compact_payload is None:
            if not outcome.fallback_required:
                raise AgentError(outcome.diagnostic or "当前会话内容太少，暂不需要模型压缩。")
            self._append_session_event(
                "context_compaction_failed",
                {"mode": "manual_model", "reason": outcome.diagnostic},
            )
            summary = self._compact_history(force=True)
            if not summary:
                raise AgentError("模型摘要失败，且当前会话无法建立确定性压缩边界。")
            return summary + "\n\n（模型摘要失败，已使用本地确定性降级。）"
        compact_payload = dict(outcome.compact_payload)
        archive_id = self._archive_compacted_events(compact_payload)
        if archive_id:
            compact_payload["archive_id"] = archive_id
        self._append_session_event("compact_summary", compact_payload)
        before_tokens = estimate_json_tokens(self._history)
        self._history = self._rebuild_history_after_compaction(compact_payload)
        self._write_compaction_memories(compact_payload)
        self._auto_recall_compaction_memory(compact_payload)
        after_tokens = estimate_json_tokens(self._history)
        notice = self._format_compaction_notice(before_tokens, after_tokens)
        self._last_compaction_notice = notice or ""
        return str(compact_payload["content"])

    def remember_review_report(self, report: str) -> None:
        """把评审报告注入父模型上下文，使下一轮模型请求能看到报告并继续处理。

        /review 斜杠命令在模型循环之外执行，报告默认只作界面状态消息展示；
        调用本方法后报告以 assistant 消息追加到进程内历史并写入会话事件，
        下一轮 working_messages 即包含完整报告，父模型可基于它修复问题、
        提交并推送变更等。空报告不注入。
        """

        text = str(report or "").strip()
        if not text:
            return
        self._history.append(
            {"role": "assistant", "content": f"[评审报告]\n{text}"}
        )
        self._append_session_event(
            "assistant_message",
            {"content": f"[评审报告]\n{text}"},
        )

    def _after_turn_measure_kwargs(
        self,
        *,
        context_messages: Sequence[Mapping[str, Any]],
        usage: TokenUsageSample,
    ) -> dict[str, Any]:
        """回合结束边界的测量输入：压缩判定与 Hook 载荷共用同一份口径。"""

        config = self.config.context_compaction
        return {
            "system_prompt": self._system_prompt(),
            "context_messages": context_messages,
            "history_messages": self._history,
            "tool_schemas": self._chat_completion_tools(),
            "recent_turns": config.recent_turns,
            "target_summary_tokens": config.target_summary_tokens,
            "next_user_reserve_tokens": config.next_user_reserve_tokens,
            "trigger_context_tokens": config.trigger_context_tokens,
            "context_window_tokens": int(
                getattr(
                    getattr(self.config, "llm", None),
                    "context_window_tokens",
                    128_000,
                )
            ),
            "emergency_context_ratio": config.emergency_context_ratio,
            "usage": usage,
        }

    def _trigger_context_compaction_after_turn(
        self,
        *,
        context_messages: Sequence[Mapping[str, Any]],
        usage: TokenUsageSample,
        status: Callable[[str], None] | None = None,
        turn_id: str | None = None,
    ) -> None:
        """回合结束边界的压缩触发点：实际上下文达到阈值时先发 Hook 再压缩。

        ``context.compaction.after_turn`` 是 notify Hook，只给插件观察机会；插件缺失、被拒绝或
        分发异常都不影响宿主压缩。无压缩配置时不做任何裁剪：按条数裁剪会改写
        已发送过的前缀，使同一会话内的前缀缓存周期性失效。
        """

        if getattr(self.config, "context_compaction", None) is None:
            return
        try:
            measurement = self._context_compaction_service().measure_after_complete_turn(
                **self._after_turn_measure_kwargs(
                    context_messages=context_messages,
                    usage=usage,
                )
            )
        except Exception:
            LOGGER.warning("上下文压缩测量失败，已跳过本回合压缩。", exc_info=True)
            return
        snapshot = measurement.snapshot
        if not snapshot.trigger_reached:
            # 未触发压缩的回合同样写入测量事件：会话转录与投影依赖逐回合的
            # 上下文计量，只在压缩时才记录会让计量停在旧值上。
            if measurement.event_payload:
                self._append_session_event(
                    "context_compaction_measurement",
                    dict(measurement.event_payload),
                )
            return
        try:
            self._dispatch_plugin_hook(
                "context.compaction.after_turn",
                {
                    "postTurnContextTokens": snapshot.post_turn_context_tokens,
                    "triggerContextTokens": snapshot.trigger_context_tokens,
                    "turnId": turn_id or "",
                },
                turn_id=turn_id,
            )
        except Exception:  # noqa: BLE001 - hook is observe-only
            LOGGER.warning(
                "context.compaction.after_turn hook dispatch failed; compaction continues.",
                exc_info=True,
            )
        self._run_context_compaction_after_turn(
            context_messages=context_messages,
            usage=usage,
            status=status,
        )

    def _run_context_compaction_after_turn(
        self,
        *,
        context_messages: Sequence[Mapping[str, Any]],
        usage: TokenUsageSample,
        status: Callable[[str], None] | None = None,
    ) -> None:
        config = self.config.context_compaction
        service = self._context_compaction_service()
        try:
            outcome = service.after_complete_turn(
                source_events=self._context_compaction_source_events(),
                minimum_turns_between_model_compactions=(
                    config.minimum_turns_between_model_compactions
                ),
                reasoning_effort=config.reasoning_effort,
                preserve_exact_evidence=config.preserve_exact_evidence,
                **self._after_turn_measure_kwargs(
                    context_messages=context_messages,
                    usage=usage,
                ),
            )
        except Exception:
            LOGGER.warning("上下文压缩自动流程失败，已跳过本回合。", exc_info=True)
            return

        measurement = dict(outcome.measurement_payload)
        compact_payload = None
        if outcome.compact_payload is not None:
            compact_payload = dict(outcome.compact_payload)
            # 归档被压缩窗口的原始事件（第二级存储），并把 archive_id 回写事件。
            archive_id = self._archive_compacted_events(compact_payload)
            if archive_id:
                compact_payload["archive_id"] = archive_id
                measurement["archive_id"] = archive_id
                measurement["archived_event_count"] = len(
                    compact_payload.get("compacted_event_ids", [])
                )
            coverage = compact_payload.get("coverage")
            if isinstance(coverage, Mapping):
                measurement["coverage"] = dict(coverage)
        if measurement:
            self._append_session_event("context_compaction_measurement", measurement)
        if compact_payload is not None:
            self._append_session_event(
                "compact_summary",
                compact_payload,
            )
            before_tokens = outcome.measurement_payload.get("estimated_next_input_tokens")
            if before_tokens is None:
                before_tokens = estimate_json_tokens(self._history)
            self._history = self._rebuild_history_after_compaction(compact_payload)
            self._write_compaction_memories(compact_payload)
            self._auto_recall_compaction_memory(compact_payload)
            # after 一律用替换后的真实 history 计算：无预算上限模式下
            # simulated_compacted_input_tokens 不承诺节省，直接用模拟值会误导显示。
            after_tokens = estimate_json_tokens(self._history)
            notice = self._format_compaction_notice(before_tokens, after_tokens)
            self._last_compaction_notice = notice or ""
            if notice and status is not None:
                status(notice)
            return
        if outcome.fallback_required:
            self._append_session_event(
                "context_compaction_failed",
                {"mode": "automatic_model", "reason": outcome.diagnostic},
            )
            fallback = self._compact_history(
                force=True,
                status=status,
                before_tokens=(
                    outcome.measurement_payload.get("estimated_next_input_tokens")
                    if isinstance(outcome.measurement_payload, Mapping)
                    else None
                ),
            )
            if not fallback:
                LOGGER.warning("模型摘要失败后无法建立确定性压缩边界：%s", outcome.diagnostic)

    def _context_compaction_service(self) -> ContextCompactionService:
        existing = getattr(self, "_context_compaction_service_instance", None)
        if existing is not None and hasattr(existing, "after_complete_turn"):
            return existing
        legacy = getattr(self, "_context_compaction_measurement", None)
        if legacy is not None and hasattr(legacy, "after_complete_turn"):
            return legacy
        llm_config = getattr(self.config, "llm", None)
        if not isinstance(llm_config, LLMConfig):
            return ContextCompactionService()
        model_adapter = RuntimeSummaryModelAdapter(
            parent_llm=llm_config,
            summary_profile=self.config.context_compaction.summary_profile,
            reasoning_effort=self.config.context_compaction.reasoning_effort,
            allow_cross_provider=self.config.context_compaction.allow_cross_provider,
            workspace_root=self.workspace_root,
            system_prompt_provider=self._system_prompt,
            context_prefix_provider=self._compaction_request_prefix,
            prompt_cache_identity_provider=self._prompt_cache_identity,
        )
        service = ContextCompactionService(
            compactor=ModelSummaryCompactor(
                model_adapter,
                max_input_tokens=_SUMMARY_INDEX_CHUNK_TOKENS,
            )
        )
        self._context_compaction_service_instance = service
        return service

    def _compaction_request_prefix(self) -> list[dict[str, Any]]:
        """复用原请求前缀：返回最近一次主请求的逐字消息。"""

        messages = getattr(self, "_last_request_messages", None)
        if not messages:
            return []
        return [dict(message) for message in messages]

    def _context_compaction_source_events(self) -> tuple[SourceEvent, ...]:
        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        if store is None or state is None:
            return ()
        return tuple(
            SourceEvent(event.event_id, event.type, dict(event.payload))
            for event in store.read_session_events(state.session_id)
        )

    @staticmethod
    def _format_compaction_notice(
        before_tokens: int | None,
        after_tokens: int | None,
    ) -> str | None:
        """生成“---已压缩 xxk~xxk ---”的分隔提示文本；数据缺失时返回 None。

        文本由 TUI 以灰色单独成行渲染（见 ``_handle_status`` 的 compact
        分支），作为“上下文已被摘要替换”的可见边界。
        """
        try:
            before = int(before_tokens) if before_tokens is not None else 0
            after = int(after_tokens) if after_tokens is not None else 0
        except (TypeError, ValueError):
            return None
        if before <= 0 or after <= 0:
            return None
        before_k = max(1, round(before / 1000))
        after_k = max(1, round(after / 1000))
        return f"---已压缩 {before_k}k~{after_k}k ---"

    def _compact_history(
        self,
        *,
        force: bool = False,
        status: Callable[[str], None] | None = None,
        before_tokens: int | None = None,
    ) -> str:
        """把早期历史压缩成单条摘要消息，避免长会话被硬裁剪。

        当前实现不调用模型，而是把被压缩的早期 user/assistant 轮次按顺序提炼成短摘要。
        这样摘要可预测、测试稳定，也不会在会话很长时额外消耗模型上下文或失败重试次数。
        """

        try:
            before = int(before_tokens) if before_tokens is not None else 0
        except (TypeError, ValueError):
            before = 0
        if before <= 0:
            before = estimate_json_tokens(self._history)
        result = compact_history(
            self._history,
            max_history_turns=self.config.max_history_turns,
            force=force,
        )
        if result is None:
            return ""

        self._append_session_event(
            "compact_summary",
            {
                "content": result.summary,
                "compacted_message_count": result.compacted_message_count,
                "remaining_message_count": len(result.recent_messages),
                "manual": force,
            },
        )
        summary_message = {"role": "assistant", "content": f"{COMPACT_SUMMARY_PREFIX}{result.summary}"}
        self._history = [summary_message, *result.recent_messages]
        self._write_compaction_memories({"content": result.summary})
        after = estimate_json_tokens(self._history)
        notice = self._format_compaction_notice(before, after)
        self._last_compaction_notice = notice or ""
        if notice and status is not None:
            status(notice)
        return result.summary

    def _rebuild_history_after_compaction(
        self,
        compact_payload: Mapping[str, Any],
    ) -> list[dict[str, Any]]:
        """用压缩边界重建运行期历史，并重置轨迹投影的累积基点。

        压缩后的历史必须等于「重启后由 ``project_session_history`` 重建」
        的结果，否则同一会话在压缩前后会出现两份不同上下文，既影响前缀缓存
        也影响模型看到的工具调用与结果。重建后轨迹投影清空，只累积压缩边界
        之后的新事件；已被摘要取代的窗口不再重复进入历史。
        """

        history = project_compaction_boundary_history(
            compact_payload,
            self._context_compaction_source_events(),
        )
        projector = self.__dict__.get("_turn_history_projector")
        if projector is not None:
            projector.reset()
        return history

    def _write_compaction_memories(self, compact_payload: Mapping[str, Any]) -> None:
        """把压缩结果写入当前会话级记忆；失败不影响压缩。"""

        store = getattr(self, "_session_memory_store", None)
        if store is None and not hasattr(self, "_session_memory_store"):
            # 兼容旧测试/嵌入调用方手工构造的 Agent；正式实例始终显式绑定
            # 会话级 Store，因此不会把项目级记忆当作压缩记忆目标。
            store = getattr(self, "_memory_store", None)
        if store is None:
            return

        structured = compact_payload.get("structured")
        project_sections: list[tuple[str, Any]] = []
        task_sections: list[tuple[str, Any]] = []
        if isinstance(structured, Mapping):
            project_sections = [
                ("项目目标", structured.get("objective")),
                ("项目约束", structured.get("constraints")),
                ("关键技术概念", structured.get("key_concepts")),
                ("关键决策", structured.get("decisions")),
                ("当前状态", structured.get("current_state")),
                ("文件与产物", structured.get("artifacts")),
                ("已读文件", structured.get("read_files")),
                ("修改文件", structured.get("modified_files")),
            ]
            task_sections = [
                ("完成状态", structured.get("completed")),
                ("失败尝试", structured.get("failed_attempts")),
                ("问题解决过程", structured.get("problem_solving_process")),
                ("已排除方案", structured.get("excluded_approaches")),
                ("后续事项", structured.get("open_issues")),
                ("可能的下一步", structured.get("next_steps")),
                ("用户消息原文", structured.get("user_messages")),
            ]
        else:
            summary = str(compact_payload.get("content") or "").strip()
            project_lines: list[str] = []
            task_lines: list[str] = []
            for line in summary.splitlines():
                stripped = line.strip()
                if stripped.startswith(
                    (
                        "- 既有摘要：",
                        "- 原始目标：",
                        "- 已压缩的用户后续要求：",
                    )
                ):
                    project_lines.append(stripped.removeprefix("- ").strip())
                elif stripped.startswith(
                    ("- 已完成/已回复要点：", "- 压缩前状态：", "- 下一步：")
                ):
                    task_lines.append(stripped.removeprefix("- ").strip())
            project_sections = [("项目目标与用户要求", project_lines)]
            task_sections = [("完成状态与后续事项", task_lines)]

        def render_memory(title: str, sections: Sequence[tuple[str, Any]]) -> str:
            lines = [f"## {title}"]
            for heading, raw_items in sections:
                if not isinstance(raw_items, Sequence) or isinstance(raw_items, (str, bytes)):
                    continue
                items: list[str] = []
                for raw_item in raw_items:
                    if isinstance(raw_item, Mapping):
                        # 文件类条目优先取 path，其余取 text。
                        text = str(raw_item.get("path") or raw_item.get("text") or "").strip()
                    else:
                        text = str(raw_item or "").strip()
                    if text:
                        items.append(text)
                if not items:
                    continue
                lines.append(f"### {heading}")
                lines.extend(f"- {item}" for item in items)
            return "\n".join(lines) if len(lines) > 1 else ""

        project_content = render_memory("压缩会话中的项目上下文", project_sections)
        task_content = render_memory("压缩会话中的任务状态", task_sections)
        requests: list[MemoryWriteRequest] = []
        if project_content:
            requests.append(
                MemoryWriteRequest(
                    content=project_content,
                    related_directories=[
                        "project-context/general",
                        "task-history/general",
                    ],
                    storage_directory="project-context/general",
                    source_event="context_compaction",
                )
            )
        if task_content:
            requests.append(
                MemoryWriteRequest(
                    content=task_content,
                    related_directories=[
                        "task-history/general",
                        "project-context/general",
                    ],
                    storage_directory="task-history/general",
                    source_event="context_compaction",
                )
            )
        if not requests:
            return
        try:
            store.write(requests)
        except Exception:
            LOGGER.warning(
                "会话压缩结果写入长期记忆失败，已保留压缩结果。",
                exc_info=True,
            )

    def _archive_compacted_events(self, compact_payload: Mapping[str, Any]) -> str:
        """把本次被压缩窗口的原始事件归档到二级存储；失败仅告警。

        归档目录由 SessionStore 管理（``archive/compacted/<session>/``），
        使摘要之外的任意被压缩事件都可按需精确恢复。返回 archive_id；
        未启用、无会话或无可归档事件时返回空字符串。
        """

        store = getattr(self, "_session_store", None)
        state = getattr(self, "_session_state", None)
        config = getattr(getattr(self, "config", None), "context_compaction", None)
        if store is None or state is None:
            return ""
        if config is not None and not config.archive_compacted_events:
            return ""
        compacted_ids = compact_payload.get("compacted_event_ids")
        if not isinstance(compacted_ids, list) or not compacted_ids:
            return ""
        wanted = set(compacted_ids)
        try:
            raw_events = [
                event.to_dict()
                for event in store.read_session_events(state.session_id)
                if event.event_id in wanted
            ]
            if not raw_events:
                return ""
            return store.archive_compacted_events(state.session_id, raw_events)
        except Exception:
            LOGGER.warning("压缩事件归档失败，已保留压缩结果。", exc_info=True)
            return ""

    def _auto_recall_compaction_memory(self, compact_payload: Mapping[str, Any]) -> None:
        """压缩完成后自动检索长期记忆，把命中结果注入投影并记录事件。

        用摘要目标/当前状态文本检索会话级记忆（缺省回退项目级），把命中
        摘要作为 assistant 消息紧跟压缩摘要注入，让模型恢复时立即看到
        “之前做过什么”；命中为空或检索失败时静默跳过。
        """

        config = getattr(getattr(self, "config", None), "context_compaction", None)
        if config is not None and not config.auto_memory_recall:
            return
        store = getattr(self, "_session_memory_store", None)
        if store is None:
            store = getattr(
                self,
                "_project_memory_store",
                getattr(self, "_memory_store", None),
            )
        if store is None or not isinstance(compact_payload.get("structured"), Mapping):
            return
        structured = compact_payload["structured"]
        parts: list[str] = []
        for values in (structured.get("objective"), structured.get("current_state")):
            if isinstance(values, list):
                parts.extend(str(item).strip() for item in values if str(item).strip())
        if not parts:
            return
        query = " ".join(parts)[:200]
        try:
            results = store.search(query, max_results=3)
        except Exception:
            LOGGER.warning("压缩后自动记忆检索失败，已跳过。", exc_info=True)
            return
        if not results:
            return
        hits = [
            {
                "id": result.id,
                "storage_directory": result.storage_directory,
                "summary": str(result.summary or "")[:200],
            }
            for result in results
        ]
        try:
            self._append_session_event(
                "compaction_memory_recall",
                {"query": query, "hits": hits},
            )
        except Exception:
            LOGGER.warning("压缩后记忆检索事件记录失败，已跳过。", exc_info=True)
        lines = ["记忆检索（压缩后自动补强）："]
        for index, result in enumerate(results, start=1):
            summary = str(result.summary or "").strip()
            if summary:
                lines.append(f"{index}. {summary}")
        recall_text = "\n".join(lines).strip()
        if not recall_text or len(self._history) < 1:
            return
        if len(recall_text) > 1_200:
            recall_text = recall_text[:1_200] + "\n..."
        self._history = [
            self._history[0],
            {"role": "assistant", "content": recall_text},
            *self._history[1:],
        ]
