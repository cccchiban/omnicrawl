"""工具表构建与 system prompt 渲染：内置与 MCP 工具面。"""
from __future__ import annotations

import re
from pathlib import Path
from ...toolkit.tools import (
    build_agent_tools,
    build_mcp_tools,
)
from ....knowledge import KnowledgeBase
from ...context.prompt_context import (
    build_system_prompt,
)
from ...types import ToolDefinition

from ..shared import (
    AgentError,
    SYSTEM_PROMPT_FILE,
)


_MODE_NAME_PATTERN = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")


class ToolBuildingMixin:
    """工具表构建与 system prompt 渲染：内置与 MCP 工具面。"""

    def _build_tools(self) -> dict[str, ToolDefinition]:
        windows_desktop = self._windows_desktop_toolbox()
        run_guard = getattr(getattr(self, "config", None), "run_guard", None)
        disabled_tools = frozenset(
            getattr(getattr(self, "config", None), "disabled_tools", ())
        )
        advisor = getattr(getattr(self, "config", None), "advisor", None)
        advisor_active = bool(
            advisor is not None
            and getattr(advisor, "active", False)
        )
        # 未配置/黑名单命中时 advisor runner 传 None，工具表根本不出现 advisor
        # （与 rpiv issue #72 一致：不要在 active set 里留一个永远失败的 stub）。
        advisor_runner = None
        if advisor_active:
            blacklist_check = getattr(self, "_advisor_blacklisted_for_current_model", None)
            blacklisted = bool(
                blacklist_check is not None and blacklist_check(advisor)
            )
            if not blacklisted and "advisor" not in disabled_tools:
                advisor_runner = getattr(self, "_tool_advisor", None)
        tools = build_agent_tools(
            mcp_manager=self._mcp_manager,
            memory_enabled=getattr(
                self,
                "_project_memory_store",
                getattr(self, "_memory_store", None),
            ) is not None,
            list=self._tool_list,
            find=self._tool_find,
            read=self._tool_read,
            read_image=self._tool_read_image,
            grep=self._tool_grep,
            web_search=self._tool_web_search,
            fetcher=self._tool_fetcher,
            image_gen=self._tool_image_gen,
            tts=self._tool_tts_synthesize,
            edit_file=self._tool_edit_file,
            write_file=self._tool_write_file,
            bash=self._tool_bash,
            powershell=self._tool_powershell,
            monitor=self._tool_monitor,
            git=self._tool_git,
            memory_search=self._tool_memory_search,
            memory_read=self._tool_memory_read,
            memory_expand_related=self._tool_memory_expand_related,
            memory_write=self._tool_memory_write,
            kb_search=self._tool_kb_search,
            kb_read=self._tool_kb_read,
            kb_write=self._tool_kb_write,
            kb_append=self._tool_kb_append,
            kb_list=self._tool_kb_list,
            mcp_call=self._tool_mcp_call,
            mcp_read_resource=self._tool_mcp_read_resource,
            mcp_get_prompt=self._tool_mcp_get_prompt,
            evidence_recall=(
                self._tool_recall_session_evidence
                if getattr(self, "_session_store", None) is not None
                else None
            ),
            subagent=(
                self._tool_subagent
                if getattr(self, "_subagent_coordinator", None) is not None
                else None
            ),
            subagent_types=(
                self._subagent_coordinator.available_agent_types()
                if getattr(self, "_subagent_coordinator", None) is not None
                else ()
            ),
            update_todos=self._tool_update_todos,
            ask_user=self._tool_ask_user,
            pause_work=(
                self._tool_pause_work
                if bool(getattr(run_guard, "enabled", False))
                else None
            ),
            advisor=advisor_runner,
            windows_window=(windows_desktop.run_window if windows_desktop is not None else None),
            windows_control=(windows_desktop.run_control if windows_desktop is not None else None),
            windows_input=(windows_desktop.run_input if windows_desktop is not None else None),
            windows_clipboard=(
                windows_desktop.run_clipboard if windows_desktop is not None else None
            ),
            windows_screenshot=(
                windows_desktop.run_screenshot if windows_desktop is not None else None
            ),
            disabled_tools=disabled_tools,
        )
        return tools

    def _build_mcp_tools(self) -> list[ToolDefinition]:
        return build_mcp_tools(
            mcp_manager=self._mcp_manager,
            mcp_call=self._tool_mcp_call,
            mcp_read_resource=self._tool_mcp_read_resource,
            mcp_get_prompt=self._tool_mcp_get_prompt,
        )

    @property
    def active_mode(self) -> str:
        """返回当前 Agent 的活动模式；未启用模式时为空字符串。"""

        return str(getattr(self, "_active_mode_name", "") or "")

    def activate_mode(self, mode: str) -> str:
        """加载并启用包内模式模板，模式切换成功后才更新 Agent 状态。"""

        if not isinstance(mode, str):
            raise AgentError("模式名称必须是字符串。")
        normalized = mode.strip().casefold()
        if not _MODE_NAME_PATTERN.fullmatch(normalized):
            raise AgentError("模式名称只能使用小写字母、数字和单连字符。")

        prompt_path = Path(__file__).resolve().parents[3] / "templates" / f"{normalized}.md"
        try:
            prompt = prompt_path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"模式模板 {normalized}.md 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取模式模板 {normalized}.md 失败：{exc}") from exc
        if not prompt:
            raise AgentError(f"模式模板 {normalized}.md 不能为空。")

        self._active_mode_name = normalized
        self._active_mode_prompt = prompt
        return normalized

    def _system_prompt(self) -> str:
        """返回基础 system prompt，并在末尾追加当前活动模式提示词。"""

        template = build_system_prompt(self._system_prompt_template)
        # 仅当 advisor 真正启用（配置 + 未命中黑名单）时追加使用准则区块，
        # 与工具表剥离保持一致：未启用时不向模型暴露 advisor 概念。
        advisor_block = self._advisor_guidelines_block()
        if advisor_block:
            template = f"{template}\n\n{advisor_block}"
        mode_prompt = str(getattr(self, "_active_mode_prompt", "") or "").strip()
        if mode_prompt:
            mode_name = self.active_mode or "active"
            template = (
                f'{template}\n\n<active_mode_prompt name="{mode_name}">\n'
                f"{mode_prompt}\n"
                "</active_mode_prompt>"
            )
        return template

    def _advisor_guidelines_block(self) -> str:
        """返回 advisor 使用准则；未启用或命中黑名单时返回空串（零成本）。"""

        advisor = getattr(getattr(self, "config", None), "advisor", None)
        if advisor is None or not getattr(advisor, "active", False):
            return ""
        blacklist_check = getattr(self, "_advisor_blacklisted_for_current_model", None)
        if blacklist_check is not None and blacklist_check(advisor):
            return ""
        return (
            "顾问策略（advisor）使用准则：\n"
            "- 你可以在关键时刻调用零参数 `advisor` 工具，把当前整段工作上下文交给"
            "已配置的更强顾问模型，获得 plan / correction / stop 三类指导。\n"
            "- 适合调用：重大实质工作（写代码、下结论）之前；反复失败或方案不收敛（卡住）时；"
            "换方向之前；长任务承诺方案前至少一次、声明完成前至少一次（先落盘再调用）。\n"
            "- 不适合调用：短任务且下一步由刚读到的工具输出直接决定时；只做定向探索时。\n"
            "- 向用户提问超时或用户未作答（`ask_user` 超时/返回失败）时，可调用一次 `advisor` "
            "代替用户评估并给出合理决策方向，避免任务空等；顾问决策不得代替高危或需审批操作的明确用户授权。\n"
            "- 收到指导后给其实质权重；若与你自己观察到的证据冲突，用一次 `advisor`"
            " 把冲突摆给顾问做 reconcile，不盲从也不盲弃。\n"
            "- 你必须在调用后下一条对用户可见的回复中转述关键指导（用户看不到折叠的工具卡片）。"
        )

    def _render_system_prompt_template(self, tool_lines: str) -> str:
        """兼容旧测试入口；新链路不再向 system prompt 注入动态工具清单。"""

        _ = tool_lines
        return self._system_prompt()

    def _agent_temp_dir_display(self) -> str:
        temp_workspace = getattr(self, "_temp_workspace", None)
        return temp_workspace.display_path if temp_workspace is not None else ".omnicrawl/.agent_tmp"

    def _load_system_prompt_template(self) -> str:
        """读取独立系统提示词模板，避免把长规范硬编码在 Python 代码里。"""

        # 本文件位于 controllers/tools/ 下三层，提示词模板统一放在包内 templates/。
        prompt_path = Path(__file__).resolve().parents[3] / "templates" / SYSTEM_PROMPT_FILE
        try:
            template = prompt_path.read_text(encoding="utf-8").strip()
        except UnicodeDecodeError as exc:
            raise AgentError(f"{SYSTEM_PROMPT_FILE} 必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise AgentError(f"读取 {SYSTEM_PROMPT_FILE} 失败：{exc}") from exc

        try:
            return build_system_prompt(template)
        except ValueError as exc:
            raise AgentError(str(exc)) from exc

    def _get_knowledge_base(self) -> KnowledgeBase:
        """返回跨项目工作知识库实例（惰性创建）。"""

        if self._knowledge_base is None:
            self._knowledge_base = KnowledgeBase()
        return self._knowledge_base
