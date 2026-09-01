"""视觉模型代理：把 Host 产生的图片交给独立 Runtime 并返回文本分析。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Callable

from ...config.models.llm import ActiveModelRef, LLMConfig
from ...config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ...config.models.vision import VisionConfiguration
from ...llm import ModelRuntimeManager, OpenAIResponseLLM
from .llm_protocol import AgentLLMProtocol
from ..types import AgentModelReply, ToolImageAttachment


class VisionProxyError(RuntimeError):
    """视觉模型代理未能得到可用的文本分析。"""


@dataclass(frozen=True)
class VisionAnalysis:
    """视觉代理成功返回的文本及实际使用的模型引用。"""

    text: str
    model: str


class VisionModelProxy:
    """使用独立模型 Runtime 顺序尝试视觉模型，避免污染主 Agent Runtime。"""

    def __init__(
        self,
        *,
        base_llm: LLMConfig,
        configuration: VisionConfiguration,
        workspace_root: Path,
        max_output_chars: int = 6000,
    ) -> None:
        self._base_llm = base_llm
        self._configuration = configuration
        self._workspace_root = Path(workspace_root).resolve()
        self._max_output_chars = max(1, int(max_output_chars))

    @property
    def enabled(self) -> bool:
        return self._configuration.enabled

    def analyze(
        self,
        images: tuple[ToolImageAttachment, ...],
        *,
        prompt: str,
        cancel_check: Callable[[], None] | None = None,
        on_token_usage: Callable[[int, int, int], None] | None = None,
    ) -> VisionAnalysis:
        """按配置顺序调用视觉模型；当前模型失败后继续尝试下一个。"""

        if not self._configuration.enabled:
            raise VisionProxyError("视觉模型代理未启用。")
        if not self._configuration.models:
            raise VisionProxyError("视觉模型代理已启用，但没有配置视觉模型。")
        if not images:
            raise VisionProxyError("视觉模型代理没有收到图片附件。")

        errors: list[str] = []
        for ref in self._configuration.models:
            if cancel_check is not None:
                cancel_check()
            label = _model_ref_label(ref)
            try:
                text = self._request_one(
                    ref,
                    images,
                    prompt=prompt,
                    cancel_check=cancel_check,
                    on_token_usage=on_token_usage,
                )
                return VisionAnalysis(text=text, model=label)
            except Exception as exc:
                if _looks_like_cancellation(exc):
                    raise
                message = str(exc).strip() or type(exc).__name__
                errors.append(f"{label}：{message[:240]}")

        detail = "；".join(errors) if errors else "没有可用的错误详情"
        raise VisionProxyError(f"视觉模型全部调用失败：{detail}")

    def _request_one(
        self,
        ref: ActiveModelRef,
        images: tuple[ToolImageAttachment, ...],
        *,
        prompt: str,
        cancel_check: Callable[[], None] | None,
        on_token_usage: Callable[[int, int, int], None] | None,
    ) -> str:
        selection = _model_ref_selection(ref)
        selected_llm = apply_model_selection(self._base_llm, selection)
        if ref.source == "detected" and ref.protocol:
            selected_llm = replace(selected_llm, protocol=ref.protocol)
        profile, descriptor = llm_config_to_profile_and_descriptor(selected_llm)
        manager = ModelRuntimeManager()
        runtime_snapshot: Any | None = None
        try:
            manager.bootstrap(profile, descriptor)
            runtime_snapshot = manager.acquire_turn()
            protocol = AgentLLMProtocol(
                client=None,
                model=selected_llm.model,
                request_timeout_seconds=int(selected_llm.request_timeout_seconds),
                # 故障转移不应被单个候选的长重试拖住；每个候选最多尝试两次。
                request_retry_count=max(1, min(int(selected_llm.request_retry_count), 2)),
                workspace_root=self._workspace_root,
                system_prompt_provider=lambda: "",
                prompt_cache_identity_provider=lambda: {
                    "scope": "omnicrawl-vision-proxy",
                    "profile": selected_llm.profile_id,
                    "model": selected_llm.model,
                },
                tools_provider=lambda: [],
                extra_body_provider=lambda: {},
                tool_name_from_function_name=lambda name: name,
                function_name_for_tool=lambda name: name,
                runtime_manager=manager,
                reasoning_effort_provider=lambda: "none",
            )
            reply: AgentModelReply = protocol.request_reply(
                _build_messages(images, prompt=prompt),
                lambda _text: None,
                on_token_usage or (lambda _input, _output, _cached: None),
                lambda: None,
                lambda _message: None,
                cancel_check,
                None,
                runtime_snapshot,
                lambda: None,
            )
            text = reply.content.strip()
            if not text:
                raise VisionProxyError("视觉模型返回了空文本。")
            return _bound_text(text, self._max_output_chars)
        except VisionProxyError:
            raise
        except Exception as exc:
            if _looks_like_cancellation(exc):
                raise
            formatted = OpenAIResponseLLM.format_request_error(exc)
            raise VisionProxyError(formatted or str(exc)) from exc
        finally:
            if runtime_snapshot is not None:
                manager.release_turn(runtime_snapshot)
            manager.close()


def _build_messages(
    images: tuple[ToolImageAttachment, ...],
    *,
    prompt: str,
) -> list[dict[str, Any]]:
    content: list[dict[str, Any]] = [{"type": "text", "text": prompt}]
    for image in images:
        content.append(
            {
                "type": "image_url",
                "image_url": {
                    "url": f"data:{image.media_type};base64,{image.data_base64}",
                    "detail": image.detail,
                },
            }
        )
    return [{"role": "user", "content": content}]


def _model_ref_selection(ref: ActiveModelRef) -> str:
    if ref.source == "custom" and ref.key:
        return ref.key
    if ref.source == "detected" and ref.profile and ref.model_id:
        return f"{ref.profile}/{ref.model_id}"
    raise VisionProxyError("视觉模型引用缺少可用的模型标识。")


def _model_ref_label(ref: ActiveModelRef) -> str:
    if ref.source == "custom":
        return ref.key or "custom/unknown"
    if ref.profile:
        return f"{ref.profile}/{ref.model_id}" if ref.model_id else ref.profile
    return ref.model_id or "unknown"


def _bound_text(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + "\n…视觉模型分析已截断。"


def _looks_like_cancellation(exc: Exception) -> bool:
    return (
        "cancel" in type(exc).__name__.casefold()
        or "cancel" in str(exc).casefold()
    )


__all__ = [
    "VisionAnalysis",
    "VisionModelProxy",
    "VisionProxyError",
]
