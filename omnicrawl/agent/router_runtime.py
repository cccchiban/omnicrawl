"""任务路由运行时状态（dsh-routing-suite / dsh-router-standard 的 OmniCrawl 适配层）。

职责：
- 会话级 override / 首条真实用户消息 / 晋升标记；
- system prompt 的 persona 注入；
- 首轮核心工具面过滤（Host 目录可见性）；
- weak 模式近距离引导文本；
- dev_router_status / dev_router_mode 的文本逻辑。
"""

from __future__ import annotations

from typing import Any, Iterable

from .router import (
    GUIDE_DEEP,
    GUIDE_WEAK,
    MODE_WEAK,
    band_for,
    band_of,
    core_for,
    fmt_mode,
    is_complex_task,
    mode_from_session_events,
    parse_mode,
    persona_for,
    testiness_for,
)

# DSH 工具名 → OmniCrawl Host 工具名（若名称不同）。
ROUTER_TOOL_ALIASES = {
    "read": "read",
    "edit": "replace_text",
    "glob": "find",
    "grep": "grep",
    "write": "write_file",
    "str_replace_editor": "replace_text",
}

# DSH 首轮工具面中始终由插件追加的平台 shell。
SHELL_TOOLS = ("bash", "powershell")

# standard 路由模式使用的 RL 接口还原 persona（minimal 的精确 RL 训练句）。
RL_PERSONA = "You are a helpful software engineer assistant."

# 首轮工具面受限说明（未晋升时追加到 system prompt 末尾）。
# 让模型知道：首轮 search_tools 只会返回核心工具子集；调用任一可用工具后
# 晋升，全部工具解锁；dev_router_status 可查看路由状态。
_ROUTER_FIRST_TURN_GUIDANCE = (
    "\n\nTask routing is active on this first turn: search_tools returns only a "
    "core tool subset (read / replace_text / find / grep / shell). After you "
    "successfully call any available tool, the full tool surface unlocks; "
    "matched tools are loaded into the conversation and can then be called "
    "natively by their real names. If a tool is missing from search results, "
    "call an available core tool first, then search again. After the surface "
    "unlocks, dev_router_status inspects routing state and dev_router_mode "
    "overrides the session mode (spec / weak / mixed / react)."
)


def _map_core_tool(name: str) -> str | None:
    """把 DSH 核心工具名映射到 OmniCrawl Host 工具名。"""

    return ROUTER_TOOL_ALIASES.get(name)


class RouterRuntime:
    """进程内路由状态；会话持久事件仍作为模式推导的兜底来源。"""

    def __init__(self, *, router_mode: str = "standard") -> None:
        self.router_mode = "standard" if router_mode != "spec" else "spec"
        # session_id -> 显式模式（number 0..1 或 "weak"）
        self.overrides: dict[str, Any] = {}
        # session_id -> 首条真实用户消息文本（首次 assemble 早于事件落盘的修复）
        self.first_user_text: dict[str, str] = {}
        # 已晋升（出现持久 tool_call_requested / tool_result）的 session_id
        self.promoted: set[str] = set()

    # ── 状态写入 ──────────────────────────────────────────────────────────

    def capture_user_text(self, session_id: str, text: str) -> None:
        """捕获首条真实用户消息（issue #3 修复的本地等价物）。"""

        stripped = (text or "").strip()
        if stripped and session_id not in self.first_user_text:
            self.first_user_text[session_id] = stripped

    def mark_promoted(self, session_id: str) -> None:
        """首次持久工具调用后标记晋升，之后放开完整工具面。"""

        self.promoted.add(session_id)

    def restore_from_events(self, session_id: str, events: Iterable[Any]) -> None:
        """从持久会话事件恢复路由状态（resume-safe）。

        只恢复晋升标记与显式 override：
        - ``router_promoted`` 事件 → 标记已晋升；
        - ``router_override`` 事件 → 恢复会话级模式 override。
        被 /undo 回退的事件已被 ``_active_session_events`` 过滤，因此这里
        不会把已回退轮次的晋升/override 重新引入。
        """

        for event in events or ():
            event_type = getattr(event, "type", None)
            payload = getattr(event, "payload", None) or {}
            if event_type == "router_promoted":
                self.promoted.add(session_id)
            elif event_type == "router_override":
                token = payload.get("mode")
                parsed = parse_mode(token)
                if parsed == "auto":
                    self.overrides.pop(session_id, None)
                elif parsed is not None:
                    self.overrides[session_id] = parsed

    def set_mode(self, session_id: str, token: Any) -> str:
        """设置会话 override；auto 清除。返回人读状态。"""

        parsed = parse_mode(token)
        if parsed is None:
            return (
                f'invalid mode "{token}": use spec/weak/mixed/react, 0-100, '
                "0.0-1.0, or auto"
            )
        if parsed == "auto":
            self.overrides.pop(session_id, None)
        else:
            self.overrides[session_id] = parsed
        current = self.mode_for_session(session_id)
        return f"mode={fmt_mode(current)} (band={band_for(current)}) — next request applies"

    # ── 状态读取 ──────────────────────────────────────────────────────────

    def mode_for_session(
        self,
        session_id: str,
        events: Iterable[Any] | None = None,
    ) -> Any:
        """会话模式优先级：override > 首条真实消息 > 持久事件兜底。"""

        if session_id in self.overrides:
            return self.overrides[session_id]
        if session_id in self.first_user_text:
            return self._classify(self.first_user_text[session_id])
        if events is not None:
            return mode_from_session_events(events)
        return MODE_WEAK

    def is_promoted(self, session_id: str) -> bool:
        return session_id in self.promoted

    def _classify(self, text: str) -> Any:
        # 延迟导入避免循环；router 模块是纯逻辑。
        from .router import classify_task

        return classify_task(text)

    # ── prompt / 工具面 / 引导 ────────────────────────────────────────────

    def apply_system_prompt(
        self,
        template: str,
        *,
        session_id: str,
        model_id: Any,
        events: Iterable[Any] | None = None,
    ) -> str:
        """按路由模式改写 system prompt。

        - standard：RL 训练句置顶，同时保留原始模板（OmniCrawl 的安全/权限/协作
          协议必须保留；DSH minimal 的 46 字符纯净面在本 harness 上会丢失这些
          硬约束，因此采用保守增强而不是完全替换）。
        - spec：分类 persona 置顶，其余模板原样保留。
        - 首轮工具面受限说明在 persona 之后追加，让模型知道首轮只能通过
          search_tools 找到核心子集、调用任一可用工具后晋升放开全部工具。
        """

        base = (template or "").strip()
        mode = self.mode_for_session(session_id, events=events)
        if self.router_mode == "standard":
            persona = RL_PERSONA
        else:
            persona = persona_for(mode, model_id)
        if not base:
            return persona
        head = f"{persona}\n\n{base}"
        if self.is_promoted(session_id):
            return head
        return head + _ROUTER_FIRST_TURN_GUIDANCE

    def visible_tools(
        self,
        session_id: str,
        *,
        events: Iterable[Any] | None = None,
    ) -> set[str] | None:
        """返回首轮允许暴露的 Host 工具名集合；已晋升或未启用时返回 None（全部）。

        weak 带的 RL-shape 面包含 shell + str_replace_editor（等价 replace_text）。
        """

        if self.is_promoted(session_id):
            return None
        mode = self.mode_for_session(session_id, events=events)
        mapped: set[str] = set()
        for name in core_for(mode):
            real = _map_core_tool(name)
            if real:
                mapped.add(real)
        mapped.update(SHELL_TOOLS)
        return mapped

    def guide_for(
        self,
        text: str,
        *,
        session_id: str,
        events: Iterable[Any] | None = None,
    ) -> str | None:
        """weak 模式下为真实用户消息生成近距离引导；强模式/已晋升返回 None。"""

        stripped = (text or "").strip()
        if not stripped or self.is_promoted(session_id):
            return None
        mode = self.mode_for_session(session_id, events=events)
        if band_of(mode) != "weak":
            return None
        return GUIDE_DEEP if is_complex_task(stripped) else GUIDE_WEAK

    # ── 自优化工具文本 ────────────────────────────────────────────────────

    def status_text(
        self,
        session_id: str,
        *,
        model_id: Any,
        events: Iterable[Any] | None = None,
    ) -> str:
        """dev_router_status 输出。"""

        mode = self.mode_for_session(session_id, events=events)
        persona = persona_for(mode, model_id).replace("\n", " / ")
        core = core_for(mode)
        return "\n".join(
            [
                f"router-mode={self.router_mode} (standard=RL接口还原 / spec=深度思考优先)",
                f"mode={fmt_mode(mode)} (band={band_for(mode)})",
                f"persona={persona}",
                f"core=[{', '.join(core)}]",
                f"testiness={testiness_for(mode)}",
                f"override={'yes' if session_id in self.overrides else 'no'}",
                f"promoted={'yes' if self.is_promoted(session_id) else 'no'}",
            ]
        )


__all__ = [
    "RL_PERSONA",
    "ROUTER_TOOL_ALIASES",
    "SHELL_TOOLS",
    "RouterRuntime",
]
