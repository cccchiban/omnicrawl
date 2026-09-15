"""工具输出压缩代理：把工具结果交给独立小模型压成精简观察。

与视觉模型代理同构：使用独立 Runtime 与独立协议，不污染主 Agent Runtime，
也不会把工具输出塞进主对话历史。调用方（工具批次收口）负责决定「压缩结果
是否被采纳」，这里只负责发出请求并做最小必要清洗。
"""

from __future__ import annotations

from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Callable

from ...config.features.tool_output_compression import ToolOutputCompressionConfig
from ...config.models.llm import LLMConfig
from ...config.models.llm_multi import (
    apply_model_selection,
    llm_config_to_profile_and_descriptor,
)
from ...llm import ModelRuntimeManager, OpenAIResponseLLM
from .llm_protocol import AgentLLMProtocol, build_extra_body
from ..types import AgentModelReply

SYSTEM_TEMPLATE_NAME = "tool_output_compression_system.md"
_OUTPUT_OPEN = "<<<TOOL_OUTPUT_START>>>"
_OUTPUT_CLOSE = "<<<TOOL_OUTPUT_END>>>"
_OMITTED_NOTE = "…（原始输出过长，中间部分已省略）…"


class ToolOutputCompressionError(RuntimeError):
    """工具输出压缩未能得到可用的精简文本。"""


@dataclass(frozen=True)
class ToolOutputCompressionResult:
    """压缩成功返回的精简文本及实际使用的模型引用。"""

    text: str
    model: str


class ToolOutputCompressor:
    """使用独立模型 Runtime 把工具输出压缩为精简观察。"""

    def __init__(
        self,
        *,
        base_llm: LLMConfig,
        configuration: ToolOutputCompressionConfig,
        workspace_root: Path,
    ) -> None:
        self._base_llm = base_llm
        self._configuration = configuration
        self._workspace_root = Path(workspace_root).resolve()

    @property
    def active(self) -> bool:
        return self._configuration.active

    def compress(
        self,
        *,
        tool_name: str,
        arguments_summary: str,
        task_hint: str,
        output: str,
        cancel_check: Callable[[], None] | None = None,
    ) -> ToolOutputCompressionResult:
        """把一次工具调用的输出压缩为精简文本。

        模型不可用、返回工具调用或返回空文本时抛
        ``ToolOutputCompressionError``，由调用方回退为原始输出。
        """

        if not self.active:
            raise ToolOutputCompressionError("工具输出压缩未启用或未选择模型。")
        if not output.strip():
            raise ToolOutputCompressionError("工具输出为空，无需压缩。")

        selection = self._configuration.model_key.strip()
        sampled = _sample_output(output, self._configuration.max_input_chars)
        system_prompt = system_prompt_text()
        messages = _build_messages(
            tool_name=tool_name,
            arguments_summary=arguments_summary,
            task_hint=task_hint,
            output=sampled,
        )

        selected_llm = apply_model_selection(self._base_llm, selection)
        request_llm = _request_model_config(selected_llm, self._configuration)
        profile, descriptor = llm_config_to_profile_and_descriptor(request_llm)
        manager = ModelRuntimeManager()
        runtime_snapshot: Any | None = None
        try:
            manager.bootstrap(profile, descriptor)
            runtime_snapshot = manager.acquire_turn()
            protocol = AgentLLMProtocol(
                client=None,  # 统一 Runtime 路径，不创建父 client
                model=request_llm.model,
                request_timeout_seconds=int(self._configuration.timeout_seconds),
                # 压缩是旁路调用：失败即回退原文，不做长重试。
                request_retry_count=1,
                workspace_root=self._workspace_root,
                system_prompt_provider=lambda: system_prompt,
                prompt_cache_identity_provider=lambda: {
                    "scope": "omnicrawl-tool-output-compression",
                    "profile": request_llm.profile_id,
                    "model": request_llm.model,
                },
                tools_provider=lambda: [],
                extra_body_provider=lambda: build_extra_body(request_llm),
                tool_name_from_function_name=lambda name: name,
                function_name_for_tool=lambda name: name,
                runtime_manager=manager,
                reasoning_effort_provider=lambda: effective_reasoning_effort(
                    self._configuration
                ),
            )
            reply: AgentModelReply = protocol.request_reply(
                messages,
                lambda _text: None,
                lambda _input, _output, _cached: None,
                lambda: None,
                lambda _message: None,
                cancel_check,
                None,
                runtime_snapshot,
                lambda: None,
            )
            if reply.tool_calls:
                raise ToolOutputCompressionError("压缩模型返回了工具调用。")
            text = _clean_reply_text(reply.content or "")
            if not text:
                raise ToolOutputCompressionError("压缩模型返回了空文本。")
            return ToolOutputCompressionResult(
                text=_bound_text(text, self._configuration.max_output_chars),
                model=selection,
            )
        except ToolOutputCompressionError:
            raise
        except Exception as exc:  # noqa: BLE001 - 统一转成压缩失败信封
            if looks_like_cancellation(exc):
                raise
            formatted = OpenAIResponseLLM.format_request_error(exc)
            raise ToolOutputCompressionError(formatted or str(exc)) from exc
        finally:
            if runtime_snapshot is not None:
                manager.release_turn(runtime_snapshot)
            manager.close()


def system_prompt_text() -> str:
    """读取内置工具输出压缩系统提示模板。"""

    template_path = Path(__file__).resolve().parents[2] / "templates" / SYSTEM_TEMPLATE_NAME
    try:
        text = template_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ToolOutputCompressionError(f"读取压缩系统提示模板失败：{exc}") from exc
    return text.strip()


def effective_reasoning_effort(configuration: ToolOutputCompressionConfig) -> str:
    """本次压缩请求的思考深度：关闭思考时显式下发 ``none``。"""

    if not configuration.thinking_enabled:
        return "none"
    return configuration.reasoning_effort


def _request_model_config(
    selected_llm: LLMConfig,
    configuration: ToolOutputCompressionConfig,
) -> LLMConfig:
    """把思考开关与深度写进本次请求的模型配置。

    选中的模型配置默认沿用主模型的思考设置，不覆盖的话压缩请求会跟着主模型
    一起思考（或一起不思考），与压缩模型自己的设置矛盾。
    """

    effort = effective_reasoning_effort(configuration)
    return replace(
        selected_llm,
        thinking_type="disabled" if effort == "none" else "enabled",
        reasoning_effort=effort,
    )


def _build_messages(
    *,
    tool_name: str,
    arguments_summary: str,
    task_hint: str,
    output: str,
) -> list[dict[str, Any]]:
    """构造压缩请求：任务背景 + 工具调用摘要 + 被包裹的原始输出。"""

    task_text = task_hint.strip() or "（未提供）"
    arguments_text = arguments_summary.strip() or "（无参数）"
    content = (
        "请压缩下面这次工具调用的原始输出。\n\n"
        "## 当前任务\n"
        f"{task_text}\n\n"
        "## 工具调用\n"
        f"工具：{tool_name}\n"
        f"参数摘要：{arguments_text}\n\n"
        f"## 原始输出（{len(output)} 字符）\n"
        f"{_OUTPUT_OPEN}\n{output}\n{_OUTPUT_CLOSE}\n"
    )
    return [{"role": "user", "content": content}]


def _sample_output(output: str, max_chars: int) -> str:
    """按头尾采样把超长输出压到输入预算内，保留「已省略」说明。"""

    budget = max(1, int(max_chars))
    if len(output) <= budget:
        return output
    note_cost = len(_OMITTED_NOTE)
    usable = max(2, budget - note_cost)
    head_chars = int(usable * 0.6)
    tail_chars = usable - head_chars
    head = output[:head_chars]
    tail = output[-tail_chars:] if tail_chars else ""
    return f"{head}{_OMITTED_NOTE}{tail}"


def _clean_reply_text(text: str) -> str:
    """去掉模型常见的包装：代码围栏与「压缩结果：」这类标签行。"""

    cleaned = text.strip()
    if cleaned.startswith("```"):
        lines = cleaned.splitlines()
        lines = lines[1:]
        if lines and lines[-1].strip().startswith("```"):
            lines = lines[:-1]
        cleaned = "\n".join(lines).strip()
    lines = cleaned.splitlines()
    if len(lines) > 1 and _is_label_line(lines[0]):
        cleaned = "\n".join(lines[1:]).strip()
    return cleaned


def _is_label_line(line: str) -> bool:
    compact = line.strip().rstrip("：:").strip()
    return len(compact) <= 8 and compact in {
        "压缩",
        "摘要",
        "结果",
        "输出",
        "压缩结果",
        "压缩后",
        "压缩输出",
        "摘要结果",
    }


def _bound_text(text: str, max_chars: int) -> str:
    limit = max(1, int(max_chars))
    if len(text) <= limit:
        return text
    return text[:limit] + "\n…压缩结果已截断。"


def looks_like_cancellation(exc: Exception) -> bool:
    return (
        "cancel" in type(exc).__name__.casefold()
        or "cancel" in str(exc).casefold()
    )


__all__ = [
    "ToolOutputCompressionError",
    "ToolOutputCompressionResult",
    "ToolOutputCompressor",
    "effective_reasoning_effort",
    "looks_like_cancellation",
    "system_prompt_text",
]
