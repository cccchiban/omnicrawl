"""全屏 TUI 的中文运行设置面板。"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional

from textual import work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, VerticalScroll
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
    save_subagent_setting,
)
from ...config.subagents import (
    SUBAGENT_ADVANCED_SETTING_KEYS,
    SubAgentConfigError,
    validate_subagent_advanced_setting,
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
_SUBAGENT_ADVANCED_LABELS = {
    "max_concurrency": "最大并发数",
    "max_tasks_per_batch": "每批最大任务数",
    "default_timeout_seconds": "子任务超时（秒）",
    "model_request_concurrency": "模型请求并发数",
    "verify_command_timeout_seconds": "验证检查超时（秒）",
    "task_retention_minutes": "任务保留时间（分钟）",
}
_SUBAGENT_ADVANCED_OPTIONS: dict[str, tuple[int | float, ...]] = {
    "max_concurrency": (1, 2, 3, 4),
    "max_tasks_per_batch": (1, 2, 3, 4),
    "default_timeout_seconds": (30, 60, 120, 300, 600, 1200, 3600),
    "model_request_concurrency": (1, 2, 3, 4),
    "verify_command_timeout_seconds": (30, 60, 120, 180, 240, 360),
    "task_retention_minutes": (15, 30, 60, 120, 360, 1440, 10080),
}
_FEATURES = (
    ("memory", "记忆功能", "memory"),
    ("mcp", "MCP 工具", "mcp"),
    ("plugins", "插件功能", "plugins"),
    ("subagents", "子任务功能", "subagents"),
    ("context_compaction", "上下文压缩", "context_compaction"),
    ("file_name_index", "文件名快速索引", "file_name_index"),
    ("content_index", "内容关键词索引", "content_index"),
)


class SettingsScreen(ModalScreen[Optional[SettingsAction]]):
    """用方向键和 Enter 操作的紧凑中文设置面板。"""

    BINDINGS = [
        ("escape", "cancel", "取消"),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
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
        height: 29;
        max-height: 90%;
        padding: 1 2;
        border: solid $terminal-border-strong;
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
    .settings-row.compact {
        height: 1;
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

    def __init__(self, agent: Any, *, advanced: bool = False) -> None:
        super().__init__()
        self._agent = agent
        self._advanced = advanced
        self._selected = 0
        self._busy = False
        self._status = "选择设置项目后按 Enter 修改；模型会打开模型选择器。"
        self._row_keys = (
            tuple(SUBAGENT_ADVANCED_SETTING_KEYS)
            if advanced
            else ("model", "channels", "vision", "reasoning", "context", "approval", "tools", "subagents_advanced")
            + tuple(item[0] for item in _FEATURES)
        )

    def compose(self) -> ComposeResult:
        with Container(id="settings-dialog"):
            yield Static(
                "子任务高级设置" if self._advanced else "运行设置",
                id="settings-title",
            )
            with VerticalScroll(id="settings-list"):
                values = self._current_row_values()
                labels = self._row_labels()
                for key in self._row_keys:
                    marker = "› " if key == self._row_keys[self._selected] else "  "
                    yield Static(
                        f"{marker}{labels[key]}：{values[key]}",
                        id=f"settings-row-{key}",
                        classes="settings-row compact" if not self._advanced else "settings-row",
                    )
            yield Static(self._status, id="settings-status")
            yield Static("↑↓ 选择  ←→ 修改  Enter/空格确认  Esc 返回", id="settings-help")

    def on_mount(self) -> None:
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
        if not self._advanced and key in {"model", "channels", "vision"}:
            self.dismiss(SettingsAction(key))
            return
        if not self._advanced and key == "subagents_advanced":
            self.dismiss(SettingsAction("subagents_advanced"))
            return
        if not self._advanced and key == "tools":
            self.dismiss(SettingsAction("tools_settings"))
            return
        if not self._advanced and key == "mcp":
            self.dismiss(SettingsAction("mcp_settings"))
            return
        if not self._advanced and key == "reasoning":
            current = str(getattr(self._agent, "reasoning_effort", "none") or "none")
            try:
                index = _REASONING_OPTIONS.index(current)
            except ValueError:
                index = 0
            self._apply_setting(key, _REASONING_OPTIONS[(index + direction) % len(_REASONING_OPTIONS)])
            return
        if not self._advanced and key == "context":
            current_k = int(getattr(self._agent, "context_window_tokens", 128_000)) // 1000
            try:
                index = _CONTEXT_WINDOW_OPTIONS_K.index(current_k)
            except ValueError:
                index = min(range(len(_CONTEXT_WINDOW_OPTIONS_K)), key=lambda item: abs(_CONTEXT_WINDOW_OPTIONS_K[item] - current_k))
            self._apply_setting(key, _CONTEXT_WINDOW_OPTIONS_K[(index + direction) % len(_CONTEXT_WINDOW_OPTIONS_K)] * 1000)
            return
        if not self._advanced and key == "approval":
            current = str(getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL))
            try:
                index = _APPROVAL_OPTIONS.index(current)
            except ValueError:
                index = 0
            self._apply_setting(key, _APPROVAL_OPTIONS[(index + direction) % len(_APPROVAL_OPTIONS)])
            return
        if self._advanced:
            options = _SUBAGENT_ADVANCED_OPTIONS[key]
            current = self._subagent_config_value(key)
            try:
                index = options.index(current)
            except ValueError:
                index = min(range(len(options)), key=lambda item: abs(float(options[item]) - float(current)))
            self._apply_setting(key, options[(index + direction) % len(options)])
            return
        current = self._feature_enabled(key)
        self._apply_setting(key, not current)

    def _feature_enabled(self, key: str) -> bool:
        if key == "memory":
            return getattr(self._agent, "_memory_store", None) is not None
        if key == "mcp":
            return bool(getattr(getattr(self._agent, "_mcp_manager", None), "enabled", False))
        if key == "plugins":
            return bool(getattr(getattr(self._agent, "_plugin_manager", None), "enabled", False))
        if key in {"subagents", "context_compaction"}:
            config = getattr(self._agent, "config", None)
            return bool(getattr(getattr(config, key, None), "enabled", False))
        if key == "file_name_index":
            config = getattr(self._agent, "config", None)
            return bool(getattr(config, "file_name_index_enabled", False))
        if key == "content_index":
            config = getattr(self._agent, "config", None)
            return bool(getattr(config, "content_index_enabled", False))
        if key == "vision":
            config = getattr(self._agent, "config", None)
            vision = getattr(config, "vision", None)
            return bool(getattr(vision, "enabled", False))
        return False

    def _subagent_config_value(self, key: str) -> int | float:
        config = getattr(getattr(self._agent, "config", None), "subagents", None)
        return getattr(config, key, 0)

    @staticmethod
    def _subagent_advanced_keys() -> tuple[str, ...]:
        return SUBAGENT_ADVANCED_SETTING_KEYS

    @staticmethod
    def _subagent_advanced_labels() -> dict[str, str]:
        return dict(_SUBAGENT_ADVANCED_LABELS)

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
                        model_source=str(getattr(getattr(self._agent, "config", None), "llm", None) and getattr(self._agent.config.llm, "model_source", "legacy") or "legacy"),
                        catalog_key=str(getattr(getattr(self._agent, "config", None), "llm", None) and getattr(self._agent.config.llm, "catalog_key", "") or ""),
                    )
                except Exception:
                    self._agent.set_context_window_tokens(previous)
                    raise
                message = f"上下文长度已设为 {tokens // 1000}K，已保存到 {path}。"
            elif key == "approval":
                previous = str(getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL))
                mode = str(value)
                self._agent.set_approval_mode(mode)
                try:
                    path = save_approval_mode(mode)
                except Exception:
                    self._agent.set_approval_mode(previous)
                    raise
                message = f"审批模式已设为 {approval_mode_label(mode)}，已保存到 {path}。"
            elif self._advanced:
                normalized = validate_subagent_advanced_setting(key, value)
                previous = self._subagent_config_value(key)
                self._agent.set_subagent_advanced_setting(key, normalized)
                try:
                    path = save_subagent_setting(key, normalized)
                except Exception:
                    self._agent.set_subagent_advanced_setting(key, previous)
                    raise
                message = f"{_SUBAGENT_ADVANCED_LABELS[key]}已设为 {normalized:g}，已保存到 {path}。"
            else:
                enabled = bool(value)
                previous = self._feature_enabled(key)
                path = save_feature_enabled(key, enabled)
                setter_name = {
                    "memory": "set_memory_enabled",
                    "mcp": "set_mcp_enabled",
                    "plugins": "set_plugin_enabled",
                    "subagents": "set_subagents_enabled",
                    "context_compaction": "set_context_compaction_enabled",
                    "file_name_index": "set_file_name_index_enabled",
                    "content_index": "set_content_index_enabled",
                }[key]
                setter = getattr(self._agent, setter_name)
                try:
                    setter(enabled)
                except Exception as setter_error:
                    try:
                        save_feature_enabled(key, previous)
                    except Exception as rollback_error:
                        raise SettingsConfigError(
                            "运行时设置应用失败，且配置回滚失败；"
                            f"当前配置与运行态可能不一致：{setter_error}；{rollback_error}"
                        ) from rollback_error
                    raise
                label = dict((item[0], item[1]) for item in _FEATURES)[key]
                message = f"{label}已{'开启' if enabled else '关闭'}，已保存到 {path}。"
        except (AgentError, LLMError, SettingsConfigError, SubAgentConfigError, OSError) as exc:
            message = f"设置未完成：{exc}"
        except Exception as exc:
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
            selected = key == self._row_keys[self._selected]
            row.set_class(selected, "selected")
            if selected:
                row.scroll_visible(animate=False)
        self.query_one("#settings-status", Static).update(self._status)

    def _current_row_values(self) -> dict[str, str]:
        if self._advanced:
            return {
                key: self._format_subagent_value(self._subagent_config_value(key))
                for key in self._row_keys
            }
        values = {
            "model": str(getattr(self._agent, "current_model", "未设置") or "未设置"),
            "channels": "管理",
            "vision": "已开启" if self._feature_enabled("vision") else "已关闭",
            "reasoning": _REASONING_LABELS.get(str(getattr(self._agent, "reasoning_effort", "none") or "none"), "默认"),
            "context": f"{int(getattr(self._agent, 'context_window_tokens', 128_000)) // 1000}K",
            "approval": approval_mode_label(str(getattr(self._agent, "approval_mode", APPROVAL_MODE_MANUAL))),
            "tools": "进入",
            "subagents_advanced": "进入",
        }
        for key, _label, _section in _FEATURES:
            values[key] = "进入" if key == "mcp" else ("已开启" if self._feature_enabled(key) else "已关闭")
        return values

    @staticmethod
    def _format_subagent_value(value: int | float) -> str:
        return f"{value:g}"

    @staticmethod
    def _row_labels() -> dict[str, str]:
        return {
            "model": "模型",
            "channels": "模型渠道",
            "vision": "视觉",
            "reasoning": "推理强度",
            "context": "上下文长度（K）",
            "approval": "工具审批",
            "tools": "工具开关",
            "subagents_advanced": "子任务高级设置",
            **_SUBAGENT_ADVANCED_LABELS,
            **{key: label for key, label, _section in _FEATURES},
        }


__all__ = ["SettingsAction", "SettingsScreen"]
