"""工具调用审批与自动审查：批内审批、删除意图识别、review 决策解析。"""
from __future__ import annotations

import json
from typing import Any, Callable, Mapping, Sequence
from ...toolkit.approval_policy import (
    GIT_TIER_HIGH,
    GIT_TIER_READONLY,
    TOOL_REVIEW_SYSTEM_PROMPT,
    arguments_have_delete_intent,
    classify_shell_command,
    command_has_delete_intent,
    description_has_delete_intent,
    git_action_tier,
    is_delete_behavior_tool_call,
    is_git_mutation_tool_call,
    is_git_tool_call,
    is_shell_command_tool_call,
    parse_tool_review_response,
    text_has_delete_intent,
    tool_accepts_shell_command,
)
from ...toolkit.host_tools import (
    HostToolCatalog,
    INVOKE_TOOL_NAME,
    build_provider_tools,
    public_invoke_arguments,
    tool_validation_error_result,
    validate_tool_arguments,
)
from ...toolkit.tools import (
    build_agent_tools,
    build_mcp_tools,
    mcp_prompt_result,
    mcp_resource_result,
    mcp_tool_result,
    normalize_tool_call,
    public_tool_arguments,
    TODO_TOOL_NAME,
    workspace_command_tool_result,
    workspace_tool_result,
)
from ...subagents.verify import VERIFY_COMMAND_TOOL_NAME, build_verify_command_tool
from ...types import AgentModelReply, ToolCall, ToolDefinition, ToolResult
from ....approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
    load_approval_mode,
    load_approval_review_model,
    normalize_approval_mode,
)
from ....llm import (
    LLMConfig,
    LLMError,
    ModelError,
    ModelErrorCode,
    ModelRuntimeManager,
    OpenAIResponseLLM,
    load_llm_config,
    normalize_reasoning_effort,
)


class ToolApprovalMixin:
    """工具调用审批与自动审查：批内审批、删除意图识别、review 决策解析。"""

    def _run_tool(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
        *,
        on_start: Callable[[], None] | None = None,
    ) -> ToolResult:
        """兼容单工具调用：先审批，再执行已批准工具。"""

        denied_result = self._approve_tool_for_batch(tool, arguments)
        if denied_result is not None:
            return denied_result
        return self._execute_approved_tool(tool, arguments, on_start=on_start)

    def _approve_tool_for_batch(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
        *,
        persist_session_events: bool = True,
    ) -> ToolResult | None:
        """在启动批量执行前按调用顺序审批；返回值非空表示拒绝结果。

        子代理与父 Agent 共用同一审批模式：auto 全部放行；review 模式下
        bash/powershell 的危险命令和高风险 Git 操作由独立审查模型综合判断，
        其余工具自动放行；manual 模式下 bash/powershell 人工确认，其余工具自动放行。
        """

        # tool.call.before：可改参数或拒绝；修改后仍走后续 schema/审批。
        call_payload = self._dispatch_plugin_hook(
            "tool.call.before",
            {"tool": tool.name, "arguments": dict(arguments)},
        )
        if call_payload is None:
            reason = f"插件拒绝工具调用：{tool.name}。"
            if persist_session_events:
                self._append_session_event(
                    "tool_call_denied",
                    {
                        "tool": tool.name,
                        "arguments": public_tool_arguments(tool.name, arguments),
                        "reason": reason,
                    },
                )
            return ToolResult(ok=False, output=reason)
        if isinstance(call_payload.get("arguments"), dict):
            arguments.clear()
            arguments.update(call_payload["arguments"])

        validation_issues = validate_tool_arguments(tool, arguments)
        if validation_issues:
            result = tool_validation_error_result(tool, validation_issues)
            if persist_session_events:
                self._append_session_event(
                    "tool_call_denied",
                    {
                        "tool": tool.name,
                        "arguments": public_tool_arguments(tool.name, arguments),
                        "reason": "工具参数未通过 Host Schema 校验。",
                    },
                )
            return result

        approval_mode = self._effective_approval_mode()
        requires_confirmation = tool.requires_confirmation

        # tool.approval.before：只能拒绝，不能代表用户批准。
        approval_guard = self._dispatch_plugin_hook(
            "tool.approval.before",
            {
                "tool": tool.name,
                "arguments": dict(arguments),
                "requiresConfirmation": requires_confirmation,
                "mode": approval_mode,
            },
        )
        if approval_guard is None:
            reason = f"插件在审批前拒绝：{tool.name}。"
            if persist_session_events:
                self._append_session_event(
                    "tool_call_denied",
                    {
                        "tool": tool.name,
                        "arguments": public_tool_arguments(tool.name, arguments),
                        "reason": reason,
                    },
                )
            return ToolResult(ok=False, output=reason)

        if not requires_confirmation:
            self._dispatch_plugin_hook(
                "tool.approval.after",
                {
                    "tool": tool.name,
                    "approved": True,
                    "mode": approval_mode,
                },
            )
            return None

        approved, denial_reason = self._approve_tool_call(tool, arguments)

        if not approved:
            reason = denial_reason or f"未批准执行：{tool.name}。"
            mcp_manager = getattr(self, "_mcp_manager", None)
            if mcp_manager is not None and tool.name in mcp_manager.registry.tools:
                mcp_manager.record_denied_tool_call(tool.name, arguments, reason)
            if persist_session_events:
                self._append_session_event(
                    "tool_call_denied",
                    {
                        "tool": tool.name,
                        "arguments": public_tool_arguments(tool.name, arguments),
                        "reason": reason,
                    },
                )
            self._dispatch_plugin_hook(
                "tool.approval.after",
                {
                    "tool": tool.name,
                    "approved": False,
                    "reason": reason,
                    "mode": approval_mode,
                },
            )
            return ToolResult(ok=False, output=reason)
        self._dispatch_plugin_hook(
            "tool.approval.after",
            {"tool": tool.name, "approved": True, "mode": approval_mode},
        )
        if persist_session_events:
            self._append_session_event(
                "tool_call_approved",
                {
                    "tool": tool.name,
                    "arguments": public_tool_arguments(tool.name, arguments),
                    "mode": approval_mode,
                },
            )
        return None

    def _execute_approved_tool(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
        *,
        on_start: Callable[[], None] | None = None,
        runner: Callable[
            [Callable[[dict[str, Any]], ToolResult], dict[str, Any]], ToolResult
        ] | None = None,
    ) -> ToolResult:
        """执行已完成审批的工具，供同批任务安全并发调用。"""

        before = self._dispatch_plugin_hook(
            "tool.execute.before",
            {"tool": tool.name, "arguments": dict(arguments)},
        )
        if before is None:
            return ToolResult(ok=False, output=f"插件在执行前拒绝：{tool.name}。")

        try:
            if on_start is not None:
                on_start()
            result = runner(tool.run, arguments) if runner is not None else tool.run(arguments)
        except Exception as exc:
            if self._is_turn_cancel_exception(exc):
                # 取消属于父 turn 控制流，不能降级成普通 ToolResult 让模型继续执行。
                raise
            self._dispatch_plugin_hook(
                "tool.execute.error",
                {"tool": tool.name, "error": str(exc)},
            )
            return ToolResult(ok=False, output=str(exc))

        display_text = result.full_output or result.output
        after_payload = self._dispatch_plugin_hook(
            "tool.execute.after",
            {
                "tool": tool.name,
                "ok": result.ok,
                "displayText": display_text,
                "annotations": {},
            },
        ) or {}
        if isinstance(after_payload.get("displayText"), str):
            display_text = after_payload["displayText"]

        model_output = result.output
        return ToolResult(
            ok=result.ok,
            output=model_output,
            full_output=display_text,
            ui_artifact=result.ui_artifact,
            model_images=result.model_images,
            error_code=result.error_code,
            retryable=result.retryable,
        )

    @classmethod
    def _tool_call_requires_serial_execution(
        cls,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> bool:
        """文件写入和具备显式删除行为的调用是批次屏障，其余调用允许并行。"""

        return (
            tool.name
            in {
                "Edit_file",
                "write_file",
                "subagent",
                TODO_TOOL_NAME,
                "ask_user",
                VERIFY_COMMAND_TOOL_NAME,
                # 同一模型回复中的桌面调用必须保持顺序，例如先激活窗口再输入文本。
                "windows_window",
                "windows_control",
                "windows_input",
                "windows_clipboard",
                "windows_screenshot",
            }
            or cls._is_delete_behavior_tool_call(tool, arguments)
            or is_git_mutation_tool_call(tool, arguments)
        )

    def _effective_approval_mode(self) -> str:
        """返回当前线程实际生效的审批模式。

        子任务 worker 可在线程内覆盖为 AUTO（如 gitMode=full 的评审角色强制
        自动批准），使完整 git 权限在收集 diff 时不被主对话的 manual/review
        模式逐条打断；覆盖只对当前线程可见，不改变父 Agent 的全局模式。
        """

        override = getattr(getattr(self, "_approval_mode_local", None), "mode", None)
        if override:
            return override
        return getattr(self.config, "approval_mode", APPROVAL_MODE_REVIEW)

    def _approve_tool_call(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> tuple[bool, str]:
        """根据审批模式处理工具许可，返回 (是否批准, 拒绝原因)。"""

        mode = self._effective_approval_mode()
        if mode == APPROVAL_MODE_AUTO:
            return True, ""
        if is_git_tool_call(tool):
            # 结构化 git 工具按 action 风险分级：只读档直接放行；本地变更档
            # review 模式放行（与文件写入同档）、manual 模式人工确认；所有高风险档
            # review 模式统一交给模型按任务目标/影响范围综合审查，manual 模式人工确认。
            tier = git_action_tier(arguments)
            if tier == GIT_TIER_READONLY:
                return True, ""
            if mode == APPROVAL_MODE_REVIEW:
                if tier == GIT_TIER_HIGH:
                    return self._review_tool_call(tool, arguments)
                return True, ""
            return (
                self._confirm(
                    tool.name,
                    public_tool_arguments(tool.name, arguments),
                ),
                f"用户取消执行：{tool.name}。",
            )
        if mode == APPROVAL_MODE_REVIEW:
            # 3A 静态规则前置分流：shell 命令先按内容分类——只有删除类与
            # 下载并执行不明脚本类才进入模型审查，其余命令（含访问项目目录外
            # 文件）直接放行；非 shell 工具的删除/清空类调用（含 MCP）也进入
            # 模型审查。这样审查资源只用于真正需要守住的破坏性操作。
            if is_shell_command_tool_call(tool, arguments):
                command = str(arguments.get("command") or "")
                if classify_shell_command(command) == "review":
                    return self._review_tool_call(tool, arguments)
                return True, ""
            if is_delete_behavior_tool_call(tool, arguments):
                return self._review_tool_call(tool, arguments)
            return True, ""
        # manual 模式只对 bash/powershell 人工确认，其余工具自动放行。
        if not is_shell_command_tool_call(tool, arguments):
            return True, ""
        return self._confirm(
            tool.name,
            public_tool_arguments(tool.name, arguments),
        ), f"用户取消执行：{tool.name}。"

    @classmethod
    def _is_delete_behavior_tool_call(
        cls,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> bool:
        return is_delete_behavior_tool_call(tool, arguments)

    @staticmethod
    def _tool_accepts_shell_command(tool: ToolDefinition) -> bool:
        return tool_accepts_shell_command(tool)

    @staticmethod
    def _arguments_have_delete_intent(
        value: Any,
        *,
        intent_keys: set[str] | None = None,
    ) -> bool:
        if intent_keys is None:
            return arguments_have_delete_intent(value)
        return arguments_have_delete_intent(value, intent_keys=intent_keys)

    @staticmethod
    def _command_has_delete_intent(command: str) -> bool:
        return command_has_delete_intent(command)

    @staticmethod
    def _text_has_delete_intent(text: str) -> bool:
        return text_has_delete_intent(text)

    @staticmethod
    def _description_has_delete_intent(text: str) -> bool:
        return description_has_delete_intent(text)

    def _review_tool_call(
        self,
        tool: ToolDefinition,
        arguments: dict[str, Any],
    ) -> tuple[bool, str]:
        """用最小上下文审查工具调用是否可自动批准，与主对话完全隔离。

        审查请求不再复用主对话 system prompt 与完整消息历史，杜绝四类上下文
        污染：身份继承（审查模型被当作主代理）、行为示范（历史中的工具调用/
        Markdown/XML 诱导模仿）、注入通道（用户原文与工具输出全文进审查上下文）、
        注意力稀释（全量历史逐次重发）。

        现在的审查请求 = 固定审查者身份（instructions）+ 待审查工具调用 JSON +
        最近一条用户消息截断摘要（仅供理解意图，prompt 已声明可能含注入，只作
        参考）。审查前缀固定不变，仍可命中输入前缀缓存降低审查成本。
        子代理线程的审查使用各自线程保存的消息快照提取用户摘要。
        """

        context_messages = getattr(
            getattr(self, "_review_context_local", None),
            "messages",
            None,
        )
        user_summary = self._extract_user_intent_summary(context_messages)
        review_payload = {
            "tool": tool.name,
            "description": tool.description,
            "arguments": arguments,
            "workspace_root": str(self.workspace_root),
            # 最近一条用户消息的截断摘要（2B）：帮助审查者理解任务意图，
            # 同时不暴露完整对话历史；可能含提示词注入，提示词已声明仅作参考。
            "user_intent_summary": user_summary,
        }
        review_instruction = (
            "待审查的工具调用（JSON）：\n"
            + json.dumps(review_payload, ensure_ascii=False, indent=2)
        )
        input_messages = [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": review_instruction}
                ],
            }
        ]
        # 独立审查者身份提示词，不继承主对话 system prompt。
        instructions = TOOL_REVIEW_SYSTEM_PROMPT
        # 独立审查模型（1A）：approval.review_model 非空时使用，否则沿用主模型。
        review_model = (
            getattr(getattr(self, "config", None), "approval_review_model", "")
            or ""
        ).strip()
        model = review_model or self.config.llm.model
        try:
            response = self._llm_client().responses.create(
                model=model,
                instructions=instructions,
                input=input_messages,
                # 审查请求只输出文本结论：禁用工具调用，避免审查模型输出
                # 工具调用块（tool_calls）而破坏严格 JSON 约定。
                tools=[],
                # 与主对话 Runtime 一致使用标准 Responses 思考参数：旧 chat 风格
                # 字段 thinking/reasoning_effort 在 Responses 网关不被识别，会让
                # 审查模型按默认思考强度运行并只返回 reasoning item，导致下文
                # 提取不到审查结论。none 档位实测能真正关闭思考。
                extra_body={"reasoning": {"effort": "none"}},
                timeout=min(self.config.request_timeout_seconds, 60),
            )
        except Exception as exc:
            return False, f"自动审查请求失败：{OpenAIResponseLLM.format_request_error(exc)}"

        try:
            review_text = OpenAIResponseLLM._extract_text(
                response, include_reasoning=True
            )
            approved, reason = self._parse_tool_review_response(review_text)
        except Exception as exc:
            # 网关可能返回非标准 Responses 结构（如 reasoning item 的 content 为
            # null、output_text 为空等），解析失败时按拒绝处理并给出原因，不能把
            # 异常抛到回合层导致整轮任务中断并显示“界面任务异常”。
            return False, f"自动审查响应解析失败：{OpenAIResponseLLM.format_request_error(exc)}"
        if approved:
            return True, ""
        if not review_text:
            # 区分“真·空响应”与“思考-only 响应”，让拒绝原因可操作：
            # 前者通常表示请求/网关异常，后者表示思考控制未生效。
            if OpenAIResponseLLM._has_reasoning_output(response):
                detail = "审查模型进入思考模式且未返回可解析文本"
            else:
                detail = "审查模型未返回任何文本"
            return False, f"自动审查拒绝执行：{detail}。"
        # review_text 非空但未批准：附模型原始返回（截断）便于区分“明确拒绝”
        # 与“输出格式不符合约定”，避免只看报错无法判断是模型决策还是格式问题。
        snippet = review_text if len(review_text) <= 120 else review_text[:120] + "…"
        return False, (
            f"自动审查拒绝执行：{reason or '模型未给出批准结论。'}"
            f"（审查模型返回：{snippet}）"
        )

    # 审查上下文允许提取的用户消息摘要最大长度（字符）。
    _REVIEW_USER_SUMMARY_MAX_CHARS = 600

    @classmethod
    def _extract_user_intent_summary(
        cls,
        messages: Sequence[Any] | None,
        max_chars: int = _REVIEW_USER_SUMMARY_MAX_CHARS,
    ) -> str:
        """从消息快照提取最近一条非空用户消息文本（截断），供审查理解任务意图。

        只提取纯文本内容，忽略工具调用块与图片块，避免把工具输出或注入载荷
        大段带进审查上下文；无快照时返回空串，审查仍可用。
        """

        for message in reversed(messages or ()):
            if not isinstance(message, dict) or message.get("role") != "user":
                continue
            content = message.get("content")
            if isinstance(content, str):
                text = content
            elif isinstance(content, list):
                parts: list[str] = []
                for part in content:
                    if not isinstance(part, dict):
                        continue
                    part_text = part.get("text")
                    if isinstance(part_text, str) and part_text:
                        parts.append(part_text)
                text = "".join(parts)
            else:
                continue
            text = (text or "").strip()
            if not text:
                continue
            return text[:max_chars]
        return ""

    @staticmethod
    def _parse_tool_review_response(review_text: str) -> tuple[bool, str]:
        return parse_tool_review_response(review_text)
