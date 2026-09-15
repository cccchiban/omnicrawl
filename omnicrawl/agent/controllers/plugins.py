"""插件 Hook 分发与生命周期钩子。"""
from __future__ import annotations

from typing import Any
from ...extensions.plugin_manager import (
    PluginDispatchContext,
)
from ...extensions.plugin_models import HOOK_POLICIES

from .shared import (
    AgentError,
)


class PluginHooksMixin:
    """插件 Hook 分发与生命周期钩子。"""

    def _plugin_manager_or_none(self) -> Any | None:
        return getattr(self, "_plugin_manager", None)

    def _freeze_subagent_plugin_dispatch_context(self) -> PluginDispatchContext:
        """在父线程为子任务冻结只读 Plugin dispatch context。

        子任务不得调用 begin_turn/end_turn。无 PluginManager 时返回空 context，
        并在 worker 内激活，避免子任务回退到父 live plan。
        """

        manager = self._plugin_manager_or_none()
        if manager is None:
            return PluginDispatchContext(handlers=(), source="none")
        freeze = getattr(manager, "freeze_dispatch_context", None)
        if not callable(freeze):
            return PluginDispatchContext(handlers=(), source="none")
        context = freeze()
        if isinstance(context, PluginDispatchContext):
            return context
        handlers = tuple(getattr(context, "handlers", ()) or ())
        source = str(getattr(context, "source", "none") or "none")
        return PluginDispatchContext(handlers=handlers, source=source)

    def _plugin_begin_turn(self) -> None:
        manager = self._plugin_manager_or_none()
        if manager is None:
            return
        begin = getattr(manager, "begin_turn", None)
        if callable(begin):
            try:
                begin()
            except Exception:  # noqa: BLE001 - 插件回合开始钩子失败不阻断主流程
                pass

    def _plugin_end_turn(self) -> None:
        manager = self._plugin_manager_or_none()
        if manager is None:
            return
        end = getattr(manager, "end_turn", None)
        if callable(end):
            try:
                end()
            except Exception:  # noqa: BLE001 - 插件回合结束钩子失败不阻断主流程
                pass

    @staticmethod
    def _hook_requires_fail_closed(hook_name: str) -> bool:
        """判断该 Hook 在基础设施异常时是否应 fail-closed。

        与 HOOK_POLICIES 对齐：任一 on_* 策略为 reject-operation 时，
        Host 边界异常也必须拒绝操作，不能静默放行。
        """

        policy = HOOK_POLICIES.get(hook_name)
        if policy is None:
            return False
        return any(
            getattr(policy, field_name) == "reject-operation"
            for field_name in (
                "on_deny",
                "on_timeout",
                "on_protocol_error",
                "on_handler_error",
            )
        )

    # 插件故障导致的 Hook 拒绝：文案必须与插件显式 deny 区分（code 来自扩展层）。
    PLUGIN_DENY_LABELS = {
        "timeout": "超时",
        "protocol-error": "通信协议错误",
        "handler-error": "执行失败",
        "invalid-patch": "返回了非法改动",
        "dispatch-error": "分发异常",
    }

    @staticmethod
    def _plugin_denial_facts(hook_name: str, outcome: Any) -> dict[str, Any]:
        """提取拒绝事实：状态码、原因、出错 Handler 与耗时。"""

        detail: dict[str, Any] = {
            "hook": hook_name,
            "code": str(getattr(outcome, "deny_code", "") or ""),
            "reason": str(getattr(outcome, "deny_reason", "") or ""),
        }
        for result in getattr(outcome, "results", ()) or ():
            if str(getattr(result, "status", "") or "") not in {
                "timeout",
                "protocol-error",
                "handler-error",
            }:
                continue
            detail["handler"] = str(getattr(result, "handler_key", "") or "")
            elapsed_ms = getattr(result, "elapsed_ms", None)
            if isinstance(elapsed_ms, (int, float)):
                detail["elapsed_ms"] = float(elapsed_ms)
            break
        return detail

    def _plugin_denial_error(self, hook_name: str) -> AgentError:
        """生成 Hook 被拒的错误，区分插件故障与插件显式拒绝。

        插件故障（超时 / 协议错误 / Handler 异常）与插件显式 deny 都会让分发返回
        None，但只有后者是插件的意图；前者必须让用户看到真实原因与出错的 Handler，
        否则无从判断是哪一环挡下了本轮。
        """

        detail = getattr(self, "_plugin_denial_detail", None)
        if not isinstance(detail, dict) or detail.get("hook") != hook_name:
            return AgentError(f"{hook_name} 被插件拒绝。")
        reason = str(detail.get("reason") or "")
        label = self.PLUGIN_DENY_LABELS.get(str(detail.get("code") or ""))
        if label is None:
            suffix = f"：{reason}" if reason else "。"
            return AgentError(f"{hook_name} 被插件拒绝{suffix}")
        elapsed_ms = detail.get("elapsed_ms")
        facts = "，".join(
            item
            for item in (
                str(detail.get("handler") or ""),
                f"{elapsed_ms:.0f}ms" if isinstance(elapsed_ms, (int, float)) else "",
            )
            if item
        )
        detail_text = "；".join(item for item in (facts, reason) if item)
        if detail_text:
            return AgentError(f"{hook_name} 插件{label}（{detail_text}）")
        return AgentError(f"{hook_name} 插件{label}。")

    def _dispatch_plugin_hook(
        self,
        hook_name: str,
        payload: dict[str, Any] | None = None,
        *,
        turn_id: str | None = None,
        session_id: str | None = None,
    ) -> dict[str, Any] | None:
        """分发 Hook；返回最终 payload。若被 deny 则返回 None。

        无 PluginManager 或插件系统关闭时原样返回 payload，保持兼容路径。
        通知/观察类 Hook 失败 fail-open；守卫类 Hook 基础设施异常 fail-closed。
        """

        data = dict(payload or {})
        self._plugin_denial_detail = None
        manager = self._plugin_manager_or_none()
        if manager is None:
            return data
        dispatch = getattr(manager, "dispatch", None)
        if not callable(dispatch):
            return data
        try:
            resolved_session_id = session_id
            if resolved_session_id is None:
                try:
                    resolved_session_id = self.current_session_id or None
                except Exception:
                    resolved_session_id = None
            outcome = dispatch(
                hook_name,
                data,
                session_id=resolved_session_id,
                turn_id=turn_id,
            )
        except Exception as exc:
            # 守卫类 Hook 与 HOOK_POLICIES 保持一致：基础设施异常不可静默放行。
            if self._hook_requires_fail_closed(hook_name):
                self._plugin_denial_detail = {
                    "hook": hook_name,
                    "code": "dispatch-error",
                    "reason": str(exc),
                }
                return None
            return data
        denied = bool(getattr(outcome, "denied", False))
        if denied:
            self._plugin_denial_detail = self._plugin_denial_facts(hook_name, outcome)
            return None
        result_payload = getattr(outcome, "payload", data)
        return dict(result_payload) if isinstance(result_payload, dict) else data

    def _emit_session_lifecycle_hooks(self) -> None:
        """在会话创建/恢复完成后发布 session.* 通知/守卫结果后的 after Hook。"""

        state = getattr(self, "_session_state", None)
        if state is None:
            return
        session_id = getattr(state, "session_id", "") or self.current_session_id
        # 恢复路径：若启动参数指定了 resume_session_id，则发 resume.after；否则 start.after。
        if getattr(self.config, "resume_session_id", ""):
            denied = self._dispatch_plugin_hook(
                "session.resume.before",
                {"sessionId": session_id},
                session_id=session_id,
            )
            if denied is None:
                raise self._plugin_denial_error("session.resume.before")
            self._dispatch_plugin_hook(
                "session.resume.after",
                {"sessionId": session_id},
                session_id=session_id,
            )
        else:
            self._dispatch_plugin_hook(
                "session.start.after",
                {"sessionId": session_id},
                session_id=session_id,
            )

    def _append_prompt_history(self, text: str) -> None:
        """记录用户提交的真实提示，用于跨会话输入复用。

        这里和 `user_message` 转录分开写：转录负责恢复模型上下文，提示历史只用于
        UI 的上箭头/搜索复用。持久化失败直接中断本轮，避免用户以为历史已经可恢复。
        """

        self._session_facade().append_prompt_history(text)
