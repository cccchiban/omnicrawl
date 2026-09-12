"""Advisor（顾问策略）Mixin：给执行者一个更强的“第二意见”旁路单轮补全。

对应 rpiv-advisor：新增零参数 ``advisor`` 工具，执行者在回合中调用时，把当前
工作消息（存活于压缩的投影）转发给独立的顾问模型，拿回纯文本 plan/correction/
stop 三类指导后作为工具结果继续执行。

设计要点（详见 omnicrawl/docs/advisor_design.md）：
- 顾问是一次旁路单轮补全，不是 SubAgent：不产生转录、无工具、无审批。
- 顾问模型 = 独立冻结的 LLMConfig（复用 apply_model_selection + Runtime）。
- 未配置/黑名单命中时 advisor 不进工具表（零成本）。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Mapping

from ...config.features.advisor import AdvisorConfig, load_advisor_config
from ...config.models.llm import LLMError
from ...config.models.llm_multi import apply_model_selection, llm_config_to_profile_and_descriptor
from ...llm import LLMConfig, ModelRuntimeManager
from .shared import AgentError
from ..runtime.llm_protocol import (
    AgentLLMProtocol,
    build_extra_body,
    function_name_for_tool,
    tool_name_from_function_name,
)
from ..types import ToolDefinition, ToolResult

ADVISOR_TOOL_NAME = "advisor"
ADVISOR_SYSTEM_TEMPLATE_NAME = "advisor_system.md"
_ADVISOR_NUDGE_TEXT = "请基于以上执行者工作情况给出 plan/correction/stop 指导。"
_ADVISOR_EMPTY_ERROR = "顾问连续两次返回空响应，请稍后重试。"


def advisor_system_prompt() -> str:
    """读取内置顾问系统提示模板。"""

    template_path = Path(__file__).resolve().parents[2] / "templates" / ADVISOR_SYSTEM_TEMPLATE_NAME
    try:
        text = template_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise AgentError(f"读取顾问系统提示模板失败：{exc}") from exc
    return text.strip()


def strip_inflight_advisor_call(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """剥掉尾部 assistant 消息中 name=advisor 的孤儿 toolCall。

    正在执行的 ``advisor()`` 调用在消息尾部还没有对应 tool 结果；OpenAI/
    Anthropic/GLM 会拒收这种孤儿 toolCall，转发给顾问前必须剥掉。
    非尾部消息里的 advisor 调用（已配对）保持不变。
    """

    if not messages:
        return list(messages)
    cleaned = list(messages)
    for index in range(len(cleaned) - 1, -1, -1):
        message = cleaned[index]
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        tool_calls = message.get("tool_calls")
        if not isinstance(tool_calls, list) or not tool_calls:
            break
        remaining: list[dict[str, Any]] = []
        stripped = False
        for call in tool_calls:
            if (
                isinstance(call, dict)
                and call.get("function")
                and str(call.get("function", {}).get("name") or "").strip().casefold()
                == ADVISOR_TOOL_NAME
            ):
                stripped = True
                continue
            remaining.append(call)
        if stripped:
            message = dict(message)
            if remaining:
                message["tool_calls"] = remaining
            else:
                message.pop("tool_calls", None)
            cleaned[index] = message
        break
    return cleaned


def ensure_user_tail(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """保证消息尾部是 user 角色：部分模型拒绝 assistant 结尾的请求。

    尾为 assistant（含只有思考文本、无 tool_calls 的情况）时，追加一条极简
    user 消息让顾问“请基于以上工作情况给出指导”。
    """

    if not messages:
        return [{"role": "user", "content": _ADVISOR_NUDGE_TEXT}]
    cleaned = list(messages)
    if cleaned[-1].get("role") == "user":
        return cleaned
    cleaned.append({"role": "user", "content": _ADVISOR_NUDGE_TEXT})
    return cleaned


def build_advisor_branch(messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """构建转发给顾问的消息分支：剥孤儿调用 + 保证 user 尾。"""

    return ensure_user_tail(strip_inflight_advisor_call(messages))


def executor_tool_inventory(tools: Mapping[str, ToolDefinition]) -> str:
    """从工具表生成 `## Available Executor Tools` 清单（按键排序、稳定序列化）。

    顾问不持有工具，但需要知道执行者能调用什么来判断“工具选择是否恰当”。
    稳定序列化（键排序）供 Provider prompt 缓存对齐。
    """

    lines = ["## Available Executor Tools"]
    for name in sorted(tools):
        tool = tools[name]
        description = " ".join(str(getattr(tool, "description", "") or "").split())
        lines.append(f"- {name}: {description}")
    return "\n".join(lines)


class AdvisorMixin:
    """Advisor 工具实现：读取配置、冻结模型、构造独立协议并完成单轮补全。"""

    # -- 配置读写 ---------------------------------------------------------

    def _advisor_config(self) -> AdvisorConfig:
        """返回当前 [advisor] 配置；Agent 内存优先，未初始化时读取磁盘。"""

        config = getattr(self.config, "advisor", None)
        if isinstance(config, AdvisorConfig):
            return config
        return load_advisor_config()

    def _advisor_is_active(self) -> bool:
        """advisor 是否真正可用（显式启用且已选模型，且当前模型不在黑名单）。"""

        advisor = self._advisor_config()
        if not advisor.active:
            return False
        return not self._advisor_blacklisted_for_current_model(advisor)

    def _advisor_blacklisted_for_current_model(self, advisor: AdvisorConfig) -> bool:
        """判断当前执行者模型是否命中 disabled_for_models 黑名单。

        黑名单项支持 models.toml key/alias、profile/model_id 片段或裸 model_id
        子串匹配（大小写不敏感），便于配置“弱模型不能咨询强顾问”。
        """

        if not advisor.disabled_for_models:
            return False
        llm = getattr(self.config, "llm", None)
        if llm is None:
            return False
        haystack = " ".join(
            str(value)
            for value in (
                getattr(llm, "catalog_key", ""),
                getattr(llm, "profile_id", ""),
                getattr(llm, "model", ""),
            )
            if value
        ).casefold()
        return any(str(item).strip().casefold() in haystack for item in advisor.disabled_for_models)

    def _tool_advisor(self, arguments: dict[str, Any]) -> ToolResult:
        """零参数 advisor 工具：把当前工作分支转发给顾问模型并返回指导。"""

        _ = arguments
        advisor = self._advisor_config()
        if not advisor.active:
            return _advisor_error_result("顾问未启用：请先通过 /advisor 选择顾问模型。")
        if self._advisor_blacklisted_for_current_model(advisor):
            return _advisor_error_result("当前执行者模型在顾问黑名单中，advisor 不可用。")

        # 从当前回合工作消息构造分支。loop 执行期间通过 turn 级上下文注入
        # 当前消息，避免工具线程读到旧状态。
        working_messages = self._current_working_messages()
        if not working_messages:
            return _advisor_error_result("当前没有可评审的工作上下文。")
        branch = build_advisor_branch(working_messages)

        self._advisor_report_status(
            f"正在咨询顾问（{advisor.model_key}，effort={advisor.display_effort}）…"
        )
        try:
            return self._call_advisor(branch, advisor)
        except AgentError:
            raise
        except Exception as exc:  # noqa: BLE001 - 统一转换为工具错误信封
            return _advisor_error_result(f"顾问调用失败：{exc}")
        finally:
            self._advisor_report_status("")

    def _advisor_report_status(self, message: str) -> None:
        """把状态上报到当前工具批次的 status 通道（没有则静默忽略）。"""

        reporter = getattr(self, "_advisor_status_reporter", None)
        if reporter is None:
            return
        try:
            reporter(message)
        except Exception:  # noqa: BLE001 - 状态展示不得中断顾问调用
            pass

    def _current_working_messages(self) -> list[dict[str, Any]]:
        """返回当前回合的工作消息（存活于压缩的投影）。

        工具执行在线程池；回合把最新工作消息保存在 turn-local 槽，保证 advisor
        转发的是“当前模型正在看的分支”，而不是旧 _history。
        """

        messages = getattr(self, "_advisor_turn_messages", None)
        if isinstance(messages, list) and messages:
            return list(messages)
        history = getattr(self, "_history", None)
        if isinstance(history, list) and history:
            return list(history)
        return []

    def _call_advisor(
        self,
        branch: list[dict[str, Any]],
        advisor: AdvisorConfig,
    ) -> ToolResult:
        """冻结顾问模型 → 独立 Runtime/协议 → 单轮无工具补全 → 结果信封。

        空响应做一次“相同输入重试”（与 rpiv 一致的有界重试）；aborted/error
        短路不重试。整个过程响应取消，避免 ESC 后顾问请求仍在后台烧钱。
        """

        parent_llm = getattr(self.config, "llm", None)
        if not isinstance(parent_llm, LLMConfig):
            return _advisor_error_result("当前运行态不支持顾问模型（缺少完整 LLM 配置）。")

        selection = advisor.model_key.strip()
        try:
            selected_llm = apply_model_selection(parent_llm, selection)
            frozen_llm = _freeze_llm_config(selected_llm)
            profile, descriptor = llm_config_to_profile_and_descriptor(frozen_llm)
            profile = _freeze_profile(profile)
            descriptor = _freeze_descriptor(descriptor)
        except LLMError as exc:
            return _advisor_error_result(f"顾问模型无法解析：{exc}")
        except Exception as exc:  # noqa: BLE001 - Runtime 前配置错误统一为错误信封
            return _advisor_error_result(f"顾问模型配置无效：{exc}")

        # 独立 Runtime：不能借用父 Agent 的 runtime_snapshot（不同模型/凭据）。
        manager = ModelRuntimeManager()
        try:
            manager.bootstrap(profile, descriptor)
        except Exception as exc:  # noqa: BLE001 - bootstrap 失败需清理半初始化 Runtime
            try:
                manager.close()
            except Exception:  # noqa: BLE001 - 关闭失败不掩盖原始错误
                pass
            return _advisor_error_result(f"顾问模型 Runtime 初始化失败：{exc}")

        cancel_check = getattr(self, "_cancel_check", None) or (lambda: None)
        system_prompt = advisor_system_prompt()
        # 工具清单前置：顾问不持有工具，但需要知道执行者可用工具面。
        inventory = executor_tool_inventory(getattr(self, "_tools", {}))
        prompt_messages = [
            {"role": "user", "content": inventory},
            *branch,
        ]

        protocol = AgentLLMProtocol(
            client=None,  # 统一 Runtime 路径，不创建父 client
            model=frozen_llm.model,
            request_timeout_seconds=min(
                int(getattr(self.config, "request_timeout_seconds", 180)),
                int(getattr(frozen_llm, "request_timeout_seconds", 180)),
            ),
            request_retry_count=1,  # 空响应由 request_reply 有界重试一次；其余错误短路
            workspace_root=self.workspace_root,
            system_prompt_provider=lambda: system_prompt,
            prompt_cache_identity_provider=lambda: {
                "workspace": str(self.workspace_root),
                "advisor": "system",
                "model": selection,
            },
            tools_provider=lambda: [],
            extra_body_provider=lambda: build_extra_body(frozen_llm),
            tool_name_from_function_name=lambda function_name: tool_name_from_function_name(
                function_name,
                {},
            ),
            function_name_for_tool=lambda tool_name: function_name_for_tool(tool_name),
            runtime_manager=manager,
            reasoning_effort_provider=lambda: advisor.display_effort,
        )

        try:
            # request_reply 在空响应时按 request_retry_count 有界重试一次；
            # aborted/error/截断由协议层分类为可重试或直接短路。
            reply = self._request_advisor_once(
                protocol,
                prompt_messages,
                cancel_check,
            )
            text = (reply.content or "").strip()
            if not text:
                return _advisor_error_result(_ADVISOR_EMPTY_ERROR)
            return _advisor_success_result(
                text,
                advisor=advisor,
                selection=selection,
                usage=getattr(reply, "_advisor_usage", None),
            )
        except AgentError:
            raise
        except Exception as exc:  # noqa: BLE001 - 协议错误统一转错误信封
            return _advisor_error_result(f"顾问请求失败：{exc}")
        finally:
            try:
                manager.close()
            except Exception:  # noqa: BLE001 - 关闭失败不掩盖主结果
                pass

    def _request_advisor_once(
        self,
        protocol: AgentLLMProtocol,
        messages: list[dict[str, Any]],
        cancel_check: Callable[[], None],
    ) -> Any:
        """执行一次顾问单轮补全；响应取消。"""

        usage_holder: dict[str, tuple[int, int, int] | None] = {"usage": None}

        def on_token_usage(input_tokens: int, output_tokens: int, cached_input_tokens: int) -> None:
            usage_holder["usage"] = (input_tokens, output_tokens, cached_input_tokens)

        def on_delta(_delta: str) -> None:
            return None

        def on_wait() -> None:
            return None

        def on_retry_status(_message: str) -> None:
            return None

        reply = protocol.request_reply(
            messages,
            on_delta,
            on_token_usage,
            on_wait,
            on_retry_status,
            cancel_check=cancel_check,
        )
        usage = usage_holder["usage"]
        if usage is not None:
            self._record_advisor_usage(usage)
        try:
            object.__setattr__(reply, "_advisor_usage", usage)
        except Exception:  # noqa: BLE001 - 只读/冻结回复对象可能拒绝附加属性
            pass
        return reply

    def _record_advisor_usage(self, usage: tuple[int, int, int]) -> None:
        """记录顾问 token 用量（供 UI/API 统计；不进 Session 正文）。"""

        recorder = getattr(self, "_record_token_usage", None)
        if recorder is None:
            return
        try:
            recorder(*usage)
        except Exception:  # noqa: BLE001 - 用量记录失败不影响结果
            pass


def _freeze_llm_config(config: LLMConfig) -> LLMConfig:
    """复制 LLMConfig 的可变映射，避免配置后续更新影响已冻结请求参数。"""

    from dataclasses import replace

    return replace(config, provider_options=dict(config.provider_options))


def _freeze_profile(profile: Any) -> Any:
    from dataclasses import replace

    return replace(profile, provider_options=dict(getattr(profile, "provider_options", {})))


def _freeze_descriptor(descriptor: Any) -> Any:
    from dataclasses import replace

    return replace(descriptor, provider_options=dict(getattr(descriptor, "provider_options", {})))


def _advisor_success_result(
    text: str,
    *,
    advisor: AdvisorConfig,
    selection: str,
    usage: tuple[int, int, int] | None,
) -> ToolResult:
    """构造 advisor 成功结果信封（纯文本 + details 元数据）。"""

    details: dict[str, Any] = {
        "advisor_model": selection,
        "effort": advisor.display_effort,
    }
    if usage is not None:
        details["usage"] = {
            "input_tokens": usage[0],
            "output_tokens": usage[1],
            "cached_input_tokens": usage[2],
        }
    return ToolResult(ok=True, output=text, full_output=text, ui_artifact={"advisor": details})


def _advisor_error_result(message: str) -> ToolResult:
    return ToolResult(ok=False, output=message, full_output=message)


__all__ = [
    "ADVISOR_TOOL_NAME",
    "AdvisorMixin",
    "advisor_system_prompt",
    "build_advisor_branch",
    "ensure_user_tail",
    "executor_tool_inventory",
    "strip_inflight_advisor_call",
]
