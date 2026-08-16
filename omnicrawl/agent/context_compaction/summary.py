"""低成本模型结构化摘要调用、分块和 JSON 响应解析。"""

from __future__ import annotations

import json
from dataclasses import replace
from importlib import resources
from pathlib import Path
from typing import Any, Mapping, Sequence

from ..llm_protocol import AgentLLMProtocol, build_extra_body
from ...config.llm import LLMConfig, LLMError
from ...config.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ...llm.runtime import ModelRuntimeManager
from .models import (
    CompactionBatch,
    ModelSummaryResult,
    SourceEvent,
    SummaryModelCall,
    SummaryModelResponse,
    TokenUsageSample,
)
from .policy import estimate_json_tokens


class SummaryGenerationError(RuntimeError):
    """摘要模型调用或结构化响应解析失败。"""


class RuntimeSummaryModelAdapter:
    """通过现有统一 LLM 协议执行隔离的无工具摘要请求。"""

    def __init__(
        self,
        *,
        parent_llm: LLMConfig,
        summary_profile: str,
        reasoning_effort: str,
        allow_cross_provider: bool,
        workspace_root: Path,
    ) -> None:
        self._parent_llm = parent_llm
        self._summary_profile = summary_profile.strip()
        self._reasoning_effort = reasoning_effort
        self._allow_cross_provider = allow_cross_provider
        self._workspace_root = workspace_root

    def resolve_model_config(self) -> LLMConfig:
        try:
            selected = (
                apply_model_selection(self._parent_llm, self._summary_profile)
                if self._summary_profile
                else replace(
                    self._parent_llm,
                    provider_options=dict(self._parent_llm.provider_options),
                )
            )
        except LLMError as exc:
            raise SummaryGenerationError(f"摘要 Profile 无法解析：{exc}") from exc
        if (
            selected.provider != self._parent_llm.provider
            and not self._allow_cross_provider
        ):
            raise SummaryGenerationError(
                "摘要 Profile 属于其他供应商；请显式开启 allow_cross_provider。"
            )
        return replace(
            selected,
            reasoning_effort=self._reasoning_effort,
            provider_options=dict(selected.provider_options),
        )

    def __call__(
        self,
        messages: Sequence[Mapping[str, Any]],
    ) -> SummaryModelResponse:
        model_config = self.resolve_model_config()
        manager = ModelRuntimeManager()
        usage = TokenUsageSample()
        try:
            profile, descriptor = llm_config_to_profile_and_descriptor(model_config)
            snapshot = manager.bootstrap(profile, descriptor)
            protocol = AgentLLMProtocol(
                client=None,
                model=model_config.model,
                request_timeout_seconds=model_config.request_timeout_seconds,
                request_retry_count=model_config.request_retry_count,
                workspace_root=self._workspace_root,
                system_prompt_provider=lambda: "",
                prompt_cache_identity_provider=lambda: {
                    "workspace": str(self._workspace_root),
                    "context": "summary",
                    "model": model_config.model,
                },
                tools_provider=lambda: [],
                extra_body_provider=lambda: build_extra_body(model_config),
                tool_name_from_function_name=lambda name: name,
                function_name_for_tool=lambda name: name,
                runtime_manager=manager,
                reasoning_effort_provider=lambda: model_config.reasoning_effort,
            )

            def record_usage(input_tokens: int, output_tokens: int, cached: int) -> None:
                nonlocal usage
                usage = usage.add(input_tokens, output_tokens, cached)

            reply = protocol.request_reply(
                [dict(message) for message in messages],
                lambda _delta: None,
                record_usage,
                lambda: None,
                lambda _status: None,
                runtime_snapshot=snapshot,
                on_stream_rollback=lambda: None,
            )
            return SummaryModelResponse(
                content=reply.content,
                usage=usage,
                profile=model_config.catalog_key or model_config.model,
                provider=model_config.provider,
            )
        except SummaryGenerationError:
            raise
        except Exception as exc:
            raise SummaryGenerationError(f"摘要模型请求失败：{exc}") from exc
        finally:
            manager.close()


class ModelSummaryCompactor:
    """只负责生成结构化摘要；来源与事实校验由 validator 执行。"""

    def __init__(
        self,
        call_model: SummaryModelCall,
        *,
        max_input_tokens: int = 64_000,
    ) -> None:
        if max_input_tokens <= 0:
            raise ValueError("max_input_tokens 必须是正整数。")
        self._call_model = call_model
        self._max_input_tokens = max_input_tokens

    def compact(
        self,
        batch: CompactionBatch,
        *,
        target_summary_tokens: int,
        validation_feedback: Sequence[str] = (),
    ) -> ModelSummaryResult:
        previous = _previous_structured(batch.previous_summary)
        chunks = _chunk_events(batch.events, self._max_input_tokens)
        if not chunks:
            raise SummaryGenerationError("没有可供模型摘要的事件。")

        aggregate_usage = TokenUsageSample()
        attempts = 0
        partials: list[Mapping[str, Any]] = []
        profile = ""
        provider = ""
        for chunk_index, chunk in enumerate(chunks, start=1):
            result = self._request_structured(
                previous_summary=previous if chunk_index == 1 else None,
                source_events=chunk,
                target_summary_tokens=target_summary_tokens,
                validation_feedback=validation_feedback,
                operation="extract_chunk" if len(chunks) > 1 else "merge_summary",
                chunk_index=chunk_index,
                chunk_count=len(chunks),
            )
            aggregate_usage = aggregate_usage.add(
                result.usage.input_tokens,
                result.usage.output_tokens,
                result.usage.cached_input_tokens,
            )
            attempts += result.attempts
            profile = result.profile or profile
            provider = result.provider or provider
            partials.append(result.structured)

        if len(partials) == 1:
            return ModelSummaryResult(
                structured=partials[0],
                usage=aggregate_usage,
                profile=profile,
                provider=provider,
                attempts=attempts,
            )

        merged = self._request_structured(
            previous_summary=previous,
            source_events=(),
            partial_summaries=partials,
            target_summary_tokens=target_summary_tokens,
            validation_feedback=validation_feedback,
            operation="merge_chunks",
            chunk_index=1,
            chunk_count=1,
        )
        aggregate_usage = aggregate_usage.add(
            merged.usage.input_tokens,
            merged.usage.output_tokens,
            merged.usage.cached_input_tokens,
        )
        return ModelSummaryResult(
            structured=merged.structured,
            usage=aggregate_usage,
            profile=merged.profile or profile,
            provider=merged.provider or provider,
            attempts=attempts + merged.attempts,
        )

    def _request_structured(
        self,
        *,
        previous_summary: Mapping[str, Any] | None,
        source_events: Sequence[SourceEvent],
        target_summary_tokens: int,
        validation_feedback: Sequence[str],
        operation: str,
        chunk_index: int,
        chunk_count: int,
        partial_summaries: Sequence[Mapping[str, Any]] = (),
    ) -> ModelSummaryResult:
        base_payload = {
            "operation": operation,
            # 0 表示无摘要预算上限：向模型传 null + budget_limited=false，
            # 由 summary_prompt.md 规则 9 引导完整性优先。
            "target_summary_tokens": (
                target_summary_tokens if target_summary_tokens > 0 else None
            ),
            "budget_limited": target_summary_tokens > 0,
            "chunk_index": chunk_index,
            "chunk_count": chunk_count,
            "previous_summary": previous_summary,
            "source_events": [event.to_prompt_dict() for event in source_events],
            "partial_summaries": list(partial_summaries),
            "validation_feedback": list(validation_feedback),
        }
        usage = TokenUsageSample()
        latest_profile = ""
        latest_provider = ""
        parse_error = ""
        for attempt in range(1, 3):
            payload = dict(base_payload)
            if parse_error:
                payload["response_error"] = parse_error
                payload["instruction"] = "上次响应不是合法 JSON 对象，请严格按 Schema 重试。"
            prompt = load_summary_prompt() + "\n\n输入：\n" + json.dumps(
                payload,
                ensure_ascii=False,
                separators=(",", ":"),
            )
            response = self._call_model(({"role": "user", "content": prompt},))
            usage = usage.add(
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cached_input_tokens,
            )
            latest_profile = response.profile or latest_profile
            latest_provider = response.provider or latest_provider
            try:
                structured = parse_structured_summary(response.content)
            except SummaryGenerationError as exc:
                parse_error = str(exc)
                continue
            return ModelSummaryResult(
                structured=structured,
                usage=usage,
                profile=latest_profile,
                provider=latest_provider,
                attempts=attempt,
            )
        raise SummaryGenerationError(f"摘要模型连续返回无效结构：{parse_error}")


def load_summary_prompt() -> str:
    return (
        resources.files("omnicrawl.agent.context_compaction")
        .joinpath("summary_prompt.md")
        .read_text(encoding="utf-8")
        .strip()
    )


def parse_structured_summary(content: str) -> Mapping[str, Any]:
    text = content.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        if lines and lines[0].strip().startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    try:
        value = json.loads(text)
    except json.JSONDecodeError as exc:
        raise SummaryGenerationError(f"摘要响应不是合法 JSON：{exc.msg}") from exc
    if not isinstance(value, dict):
        raise SummaryGenerationError("摘要响应顶层必须是 JSON 对象。")
    return value


def _previous_structured(payload: Mapping[str, Any] | None) -> Mapping[str, Any] | None:
    if payload is None:
        return None
    structured = payload.get("structured")
    if isinstance(structured, dict):
        return structured
    content = payload.get("content")
    if isinstance(content, str) and content.strip():
        return {"legacy_content": content.strip()}
    return None


def _chunk_events(
    events: Sequence[SourceEvent],
    max_input_tokens: int,
) -> list[tuple[SourceEvent, ...]]:
    chunks: list[list[SourceEvent]] = []
    current: list[SourceEvent] = []
    current_tokens = 0
    for event in events:
        event_tokens = estimate_json_tokens(event.to_prompt_dict())
        if event_tokens > max_input_tokens:
            if current:
                chunks.append(current)
                current = []
                current_tokens = 0
            chunks.extend([[part] for part in _split_large_event(event, max_input_tokens)])
            continue
        if current and current_tokens + event_tokens > max_input_tokens:
            chunks.append(current)
            current = []
            current_tokens = 0
        current.append(event)
        current_tokens += event_tokens
    if current:
        chunks.append(current)
    return [tuple(chunk) for chunk in chunks]


def _split_large_event(event: SourceEvent, max_input_tokens: int) -> list[SourceEvent]:
    serialized = json.dumps(dict(event.payload), ensure_ascii=False, separators=(",", ":"))
    # 启发式估算按最坏的 CJK 1 字符≈1 Token 切分，给提示词和 Schema 留 20% 余量。
    chunk_chars = max(1_000, int(max_input_tokens * 0.8))
    parts: list[SourceEvent] = []
    total = max(1, math_ceil_div(len(serialized), chunk_chars))
    for index in range(total):
        text = serialized[index * chunk_chars : (index + 1) * chunk_chars]
        parts.append(
            SourceEvent(
                event_id=event.event_id,
                type=event.type,
                payload={
                    "chunk_text": text,
                    "chunk_index": index + 1,
                    "chunk_count": total,
                },
            )
        )
    return parts


def math_ceil_div(value: int, divisor: int) -> int:
    return (value + divisor - 1) // divisor
