"""低成本模型结构化摘要调用、分块和 JSON 响应解析。"""

from __future__ import annotations

import json
import logging
from dataclasses import replace
from importlib import resources
from pathlib import Path
from typing import Any, Callable, Mapping, Sequence

from ..runtime.llm_protocol import AgentLLMProtocol, build_extra_body
from ...config.models.llm import LLMConfig, LLMError
from ...config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ...llm.runtime import ModelRuntimeManager
from .models import (
    CompactionBatch,
    ModelSummaryResult,
    SourceEvent,
    SummaryModelCall,
    SummaryModelResponse,
    TokenUsageSample,
)
from .policy import (
    estimate_json_tokens,
    estimate_messages_tokens,
    estimate_text_tokens,
)


# 摘要请求与主请求共享同一份工具声明，只有 tool_choice 不同：工具块在 prompt 里排在
# 前缀最前面，少带工具会让第一次摘要请求无法命中主请求已建立的前缀缓存。
_SUMMARY_TOOL_CHOICE = "none"
# 索引只是待压缩事件的目录（正文留在复用前缀里），预算越大分块越少、整段前缀重发
# 次数越少；这里给索引块设硬上限，运行态再按窗口余额放大（见 index_chunk_budget_tokens）。
_MAX_INDEX_CHUNK_TOKENS = 128_000
_SUMMARY_OUTPUT_RESERVE_TOKENS = 16_000

LOGGER = logging.getLogger(__name__)


class SummaryGenerationError(RuntimeError):
    """摘要模型调用或结构化响应解析失败。"""


class RuntimeSummaryModelAdapter:
    """通过复用原请求前缀（含系统提示词与工具声明），且用 tool_choice=none 禁止调用工具。"""

    def __init__(
        self,
        *,
        parent_llm: LLMConfig,
        summary_profile: str,
        reasoning_effort: str,
        allow_cross_provider: bool,
        workspace_root: Path,
        system_prompt_provider: Callable[[], str] | None = None,
        context_prefix_provider: Callable[[], Sequence[Mapping[str, Any]]] | None = None,
        prompt_cache_identity_provider: Callable[[], Mapping[str, str]] | None = None,
        tools_provider: Callable[[], Sequence[Mapping[str, Any]]] | None = None,
    ) -> None:
        self._parent_llm = parent_llm
        self._summary_profile = summary_profile.strip()
        self._reasoning_effort = reasoning_effort
        self._allow_cross_provider = allow_cross_provider
        self._workspace_root = workspace_root
        self._system_prompt_provider = system_prompt_provider
        self._context_prefix_provider = context_prefix_provider
        self._prompt_cache_identity_provider = prompt_cache_identity_provider
        self._tools_provider = tools_provider

    def _context_prefix(self) -> list[dict[str, Any]]:
        provider = self._context_prefix_provider
        if provider is None:
            return []
        return [dict(message) for message in provider() or ()]

    def _tools_payload(self) -> list[dict[str, Any]]:
        """与主请求一致的工具面：逐字相同才能命中同一前缀缓存。

        摘要走其他模型时前缀缓存本就不共享，此时不声明工具，避免落到不支持工具的模型上。
        """

        provider = self._tools_provider
        if provider is None or not self._shares_parent_model():
            return []
        return [dict(tool) for tool in provider() or ()]

    def _shares_parent_model(self) -> bool:
        try:
            return self.resolve_model_config().model == self._parent_llm.model
        except SummaryGenerationError:
            return False

    def index_chunk_budget_tokens(self) -> int:
        """按「摘要模型窗口 − 复用前缀 − 输出预留」估算单块事件索引预算。

        返回 0 表示窗口余额不足：调用方回退到默认预算，继续分块。
        """

        try:
            window = int(
                getattr(self.resolve_model_config(), "context_window_tokens", 0) or 0
            )
        except SummaryGenerationError:
            return 0
        if window <= 0:
            return 0
        return max(
            0,
            window // 2 - self._prefix_token_estimate() - _SUMMARY_OUTPUT_RESERVE_TOKENS,
        )

    def _prefix_token_estimate(self) -> int:
        """复用前缀的 token 估算：系统提示词 + 逐字复用的主请求消息。"""

        total = estimate_text_tokens(self._system_prompt())
        return total + estimate_messages_tokens(self._context_prefix())

    def _system_prompt(self) -> str:
        provider = self._system_prompt_provider
        return str(provider() or "") if provider is not None else ""

    def _prompt_cache_identity(self, model: str) -> dict[str, str]:
        """与主请求同一缓存身份：前缀一致时摘要调用才命中同一前缀缓存。"""

        provider = self._prompt_cache_identity_provider
        if provider is None:
            return {
                "workspace": str(self._workspace_root),
                "context": "summary",
                "model": model,
            }
        return {str(key): str(value) for key, value in (provider() or {}).items()}

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
            # tool_choice 经 extra_body 提升为顶层请求参数：工具面照带（命中前缀缓存），
            # 但模型不允许调用工具。
            provider_options={
                **dict(selected.provider_options),
                "tool_choice": _SUMMARY_TOOL_CHOICE,
            },
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
            prefix = self._context_prefix()
            tools_payload = self._tools_payload()
            prefix_tokens = self._prefix_token_estimate()
            protocol = AgentLLMProtocol(
                client=None,
                model=model_config.model,
                request_timeout_seconds=model_config.request_timeout_seconds,
                request_retry_count=model_config.request_retry_count,
                workspace_root=self._workspace_root,
                system_prompt_provider=self._system_prompt,
                prompt_cache_identity_provider=lambda: self._prompt_cache_identity(
                    model_config.model
                ),
                tools_provider=lambda: tools_payload,
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
                [*prefix, *[dict(message) for message in messages]],
                lambda _delta: None,
                record_usage,
                lambda: None,
                lambda _status: None,
                runtime_snapshot=snapshot,
                on_stream_rollback=lambda: None,
            )
            # 与网关侧请求日志对齐的画像：前缀逐字一致时，第一次摘要请求也应命中主请求缓存。
            LOGGER.info(
                "上下文摘要请求：model=%s prefix_messages=%d prefix_tokens≈%d tools=%d "
                "tool_choice=%s input=%d cached=%d output=%d",
                model_config.catalog_key or model_config.model,
                len(prefix),
                prefix_tokens,
                len(tools_payload),
                _SUMMARY_TOOL_CHOICE if tools_payload else "-",
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
            )
            return SummaryModelResponse(
                content=reply.content,
                usage=usage,
                profile=model_config.catalog_key or model_config.model,
                provider=model_config.provider,
                tool_calls=len(reply.tool_calls),
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
        budget_provider: Callable[[], int] | None = None,
    ) -> None:
        if max_input_tokens <= 0:
            raise ValueError("max_input_tokens 必须是正整数。")
        self._call_model = call_model
        self._max_input_tokens = max_input_tokens
        self._budget_provider = budget_provider

    def _chunk_budget(self) -> int:
        """单块索引预算：默认值与窗口余额取大者，并受硬上限约束。"""

        budget = self._max_input_tokens
        if self._budget_provider is not None:
            try:
                budget = max(budget, int(self._budget_provider() or 0))
            except Exception:  # noqa: BLE001 - 预算解析失败时退回默认预算
                pass
        return min(budget, _MAX_INDEX_CHUNK_TOKENS)

    def compact(
        self,
        batch: CompactionBatch,
        *,
        target_summary_tokens: int,
        validation_feedback: Sequence[str] = (),
    ) -> ModelSummaryResult:
        previous = _previous_structured(batch.previous_summary)
        chunks = _chunk_events(batch.events, self._chunk_budget())
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
            # 事件正文随复用的原请求前缀一起发送，这里只给模型 ID、类型与预览索引。
            "events_index": [event.to_index_dict() for event in source_events],
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
            if response.tool_calls:
                parse_error = "摘要响应里出现了工具调用；本请求禁止调用工具，只允许输出 JSON 对象。"
                continue
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
        event_tokens = estimate_json_tokens(event.to_index_dict())
        if current and current_tokens + event_tokens > max_input_tokens:
            chunks.append(current)
            current = []
            current_tokens = 0
        current.append(event)
        current_tokens += event_tokens
    if current:
        chunks.append(current)
    return [tuple(chunk) for chunk in chunks]

