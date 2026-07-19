"""全屏 TUI 的中文运行设置面板。"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Callable, Optional

from textual import work
from textual.app import ComposeResult
from textual.containers import Container, Vertical
from textual.screen import ModalScreen
from textual.widgets import Static

from ...agent import AgentError
from ...approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    approval_mode_label,
    save_approval_mode,
)
from ...config.settings import (
    SettingsConfigError,
    save_context_window_tokens,
    save_feature_enabled,
)
from ...llm import LLMError, save_reasoning_effort
from .theme import terminal_css


@dataclass(frozen=True)
class SettingsAction:
    """设置面板关闭时返回的 UI 动作。"""

    name: str


_REASONING_OPTIONS = ("none", "low", "medium", "high", "xhigh", "max")
_REASONING_LABELS = {
    "none": "关闭",
    "low": "低",
    "medium": "中",
    "high": "高",
    "xhigh": "超高",
    "max": "最大",
}
_APPROVAL_OPTIONS = (APPROVAL_MODE_MANUAL, APPROVAL_MODE_AUTO, APPROVAL_MODE_REVIEW)
_CONTEXT_WINDOW_OPTIONS_K = (32, 64, 128, 256, 512, 1024, 2048)

_FEATURES = (
    ("memory", "记忆功能", "memory"),
    ("mcp", "MCP 工具", "mcp"),
    ("plugins", "插件功能", "plugins"),
    ("subagents", "子任务功能", "subagents"),
)


class SettingsScreen(ModalScreen[Optional[SettingsAction]]):
    """用方向键和 Enter 操作的紧凑中文设置面板。"""

    BINDINGS = [
        ("escape", "cancel", "取消"),
        ("up", "move_up", "上一项"),
        ("down", "move_down", "下一项"),
        ("left", "previous_value", "上一个"),
        ("right", "next_value", "下一个"),
        ("enter", "confirm", "选择"),
        ("space", "confirm", "切换"),
    ]

    CSS = terminal_css("""
    SettingsScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #settings-dialog {
        width: 78;
        max-width: 94%;
        height: 25;
        max-height: 90%;
        padding: 1 2;
        border: solid $terminal-green;
        background: $terminal-surface;
    }
    #settings-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-green;
        text-style: bold;
    }
    #settings-list {
        height: 1fr;
    }
    .settings-row {
        height: 2;
        padding: 0 1;
        color: $terminal-text-secondary;
    }
    .settings-row.selected {
        color: $terminal-text;
        background: $terminal-blue-soft;
        text-style: bold;
    }
    #settings-status {
        height: 2;
        color: $terminal-blue;
        margin-top: 1;
    }
    #settings-help {
        height: 1;
        color: $terminal-text-muted;
        margin-top: 1;
    }
    """)

    def __init__(self, agent: Any) -> None:
        super().__init__()
        self._agent = agent
        self._selected = 0
        self._busy = False
        self._status = "选择设置项目后按 Enter 修改；模型会打开模型选择器。"
        self._row_keys = ("model", "reasoning", "context", "approval") + tuple(
            item[0] for item in _FEATURES
        )

    def compose(self) -> ComposeResult:
        with Container(id="settings-dialog"):
            yield Static("运行设置", id="settings-title")
            with Vertical(id="settings-list"):
                values = self._current_row_values()
                labels = self._row_labels()
                for key in self._row_keys:
                    marker = "› " if key == self._row_keys[self._selected] else "  "
                    yield Static(
                        f"{marker}{labels[key]}：{values[key]}",
                        id=f"settings-row-{key}",
                        classes="settings-row",
                    )
            yield Static(self._status, id="settings-status")
            yield Static("↑↓ 选择  ←→ 修改  Enter/空格确认  Esc 关闭", id="settings-help")

    def on_mount(self) -> None:
        # Screen 挂载时子组件可能仍在完成初始化，延后一次避免行内容被覆盖。
        self.call_after_refresh(self._render_rows)

    def action_cancel(self) -> None:
        if not self._busy:
            self.dismiss(None)

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._row_keys)
            self._render_rows()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._row_keys)
            self._render_rows()

    def action_previous_value(self) -> None:
        self._change_selected(-1)

    def action_next_value(self) -> None:
        self._change_selected(1)

    def action_confirm(self) -> None:
        self._change_selected(1)

    def _change_selected(self, direction: int) -> None:
        if self._busy:
            return
        key = self._row_keys[self._selected]
        if key == "model":
            self.dismiss(SettingsAction("model"))
            return
        if key == "reasoning":
            current = str(getattr(self._agent, "reasoning_effort", "none") or "none")
            try:
                index = _REASONING_OPTIONS.index(current)
            except ValueError:
                index = 0
            value = _REASONING_OPTIONS[(index + direction) % len(_REASONING_OPTIONS)]
            self._apply_setting(key, value)
            return
        if key == "context":
            current_tokens = int(getattr(self._agent, "context_window_tokens", 128_000))
            current_k = current_tokens // 1000
            try:
                index = _CONTEXT_WINDOW_OPTIONS_K.index(current_k)
            except ValueError:
                index = min(
                    range(len(_CONTEXT_WINDOW_OPTIONS_K)),
                    key=lambda item: abs(_CONTEXT_WINDOW_OPTIONS_K[item] - current_k),
                )
            next_k = _CONTEXT_WINDOW_OPTIONS_K[
                (index + direction) % len(_CONTEXT_WINDOW_OPTIONS_K)
            ]
            self._apply_setting(key, next_k * 1000)
            return
        if key == "approval":
            current = str(getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL))
            try:
                index = _APPROVAL_OPTIONS.index(current)
            except ValueError:
                index = 0
            value = _APPROVAL_OPTIONS[(index + direction) % len(_APPROVAL_OPTIONS)]
            self._apply_setting(key, value)
            return
        current = self._feature_enabled(key)
        self._apply_setting(key, not current)

    def _feature_enabled(self, key: str) -> bool:
        if key == "memory":
            return getattr(self._agent, "_memory_store", None) is not None
        if key == "mcp":
            manager = getattr(self._agent, "_mcp_manager", None)
            return bool(getattr(manager, "enabled", False))
        if key == "plugins":
            manager = getattr(self._agent, "_plugin_manager", None)
            return bool(getattr(manager, "enabled", False))
        if key == "subagents":
            config = getattr(self._agent, "config", None)
            return bool(getattr(getattr(config, "subagents", None), "enabled", False))
        return False

    @work(thread=True, exclusive=True, group="settings-apply", exit_on_error=False)
    def _apply_setting(self, key: str, value: object) -> None:
        self.app.call_from_thread(self._set_busy, True, "正在应用设置…")
        try:
            if key == "reasoning":
                previous = str(getattr(self._agent, "reasoning_effort", "none") or "none")
                normalized = self._agent.set_reasoning_effort(str(value))
                try:
                    path = save_reasoning_effort(normalized)
                except Exception:
                    self._agent.set_reasoning_effort(previous)
                    raise
                message = f"推理强度已设为 {_REASONING_LABELS[normalized]}，已保存到 {path}。"
            elif key == "context":
                previous = int(getattr(self._agent, "context_window_tokens", 128_000))
                tokens = int(value)
                self._agent.set_context_window_tokens(tokens)
                try:
                    path = save_context_window_tokens(
                        tokens,
                        model_source=str(
                            getattr(getattr(self._agent, "config", None), "llm", None)
                            and getattr(self._agent.config.llm, "model_source", "legacy")
                            or "legacy"
                        ),
                        catalog_key=str(
                            getattr(getattr(self._agent, "config", None), "llm", None)
                            and getattr(self._agent.config.llm, "catalog_key", "")
                            or ""
                        ),
                    )
                except Exception:
                    self._agent.set_context_window_tokens(previous)
                    raise
                message = f"上下文长度已设为 {tokens // 1000}K，已保存到 {path}。"
            elif key == "approval":
                previous = str(
                    getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL)
                )
                mode = str(value)
                self._agent.set_approval_mode(mode)
                try:
                    path = save_approval_mode(mode)
                except Exception:
                    self._agent.set_approval_mode(previous)
                    raise
                message = f"审批模式已设为 {approval_mode_label(mode)}，已保存到 {path}。"
            else:
                enabled = bool(value)
                previous = self._feature_enabled(key)
                # 先写入配置，运行时重建失败时恢复旧值，避免磁盘和当前 Agent
                # 一边开启、一边关闭的半生效状态。
                path = save_feature_enabled(key, enabled)
                setter = {
                    "memory": self._agent.set_memory_enabled,
                    "mcp": self._agent.set_mcp_enabled,
                    "plugins": self._agent.set_plugin_enabled,
                    "subagents": self._agent.set_subagents_enabled,
                }[key]
                try:
                    setter(enabled)
                except Exception as setter_error:
                    try:
                        save_feature_enabled(key, previous)
                    except Exception as rollback_error:
                        # 原子写回虽然不会留下半个文件，但在磁盘满、权限变化等
                        # 情况下第二次写回仍可能失败。此时无法安全地宣称配置已
                        # 回滚，必须把配置与运行态可能不一致的风险呈现给用户。
                        raise SettingsConfigError(
                            "运行时设置应用失败，且配置回滚失败；"
                            f"当前配置与运行态可能不一致：{setter_error}；{rollback_error}"
                        ) from rollback_error
                    raise
                label = dict((item[0], item[1]) for item in _FEATURES)[key]
                message = f"{label}已{'开启' if enabled else '关闭'}，已保存到 {path}。"
        except (AgentError, LLMError, SettingsConfigError, OSError) as exc:
            message = f"设置未完成：{exc}"
        except Exception as exc:  # noqa: BLE001 - 面板必须把失败安全返回 UI
            message = f"设置未完成：{exc}"
        self.app.call_from_thread(self._set_busy, False, message)

    def _set_busy(self, busy: bool, status: str) -> None:
        self._busy = busy
        self._status = status
        self._render_rows()

    def _render_rows(self) -> None:
        if not self.is_mounted:
            return
        values = self._current_row_values()
        labels = self._row_labels()
        for key in self._row_keys:
            text = f"{labels[key]}：{values[key]}"
            marker = "› " if key == self._row_keys[self._selected] else "  "
            row = self.query_one(f"#settings-row-{key}", Static)
            row.update(marker + text)
            row.set_class(key == self._row_keys[self._selected], "selected")
        self.query_one("#settings-status", Static).update(self._status)

    def _current_row_values(self) -> dict[str, str]:
        values = {
            "model": str(getattr(self._agent, "current_model", "未设置") or "未设置"),
            "reasoning": _REASONING_LABELS.get(
                str(getattr(self._agent, "reasoning_effort", "none") or "none"),
                "默认",
            ),
            "context": f"{int(getattr(self._agent, 'context_window_tokens', 128_000)) // 1000}K",
            "approval": approval_mode_label(
                str(getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL))
            ),
        }
        for key, _label, _section in _FEATURES:
            values[key] = "已开启" if self._feature_enabled(key) else "已关闭"
        return values

    @staticmethod
    def _row_labels() -> dict[str, str]:
        return {
            "model": "模型",
            "reasoning": "推理强度",
            "context": "上下文长度（K）",
            "approval": "工具审批",
            **{key: label for key, label, _section in _FEATURES},
        }


__all__ = ["SettingsAction", "SettingsScreen"]
