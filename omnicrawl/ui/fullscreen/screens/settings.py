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

from ....agent import AgentError
from ....approval import (
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_REVIEW,
    approval_mode_label,
    save_approval_mode,
)
from ....config.core.settings import (
    SettingsConfigError,
    save_context_compaction_trigger_percent,
    save_context_window_tokens,
    save_feature_enabled,
    save_show_thinking,
    save_subagent_setting,
)
from ....config.features.subagents import (
    SUBAGENT_ADVANCED_SETTING_KEYS,
    SubAgentConfigError,
    validate_subagent_advanced_setting,
)
from ....llm import LLMError, save_reasoning_effort
from ..terminal.theme import terminal_css


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
_APPROVAL_OPTIONS = (APPROVAL_MODE_MANUAL, APPROVAL_MODE_REVIEW, APPROVAL_MODE_AUTO)
_CONTEXT_WINDOW_OPTIONS_K = (32, 64, 128, 256, 512, 1024, 2048)
# 上下文压缩阈值按当前上下文窗口的百分比设置，5% 为一个单位递进。
_CONTEXT_COMPACTION_PERCENT_OPTIONS = tuple(range(5, 100, 5))
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
    ("router", "任务思维路由", "router"),
)
_COLUMN_SLOTS = 16  # 每栏设置行数：左栏 15 项 + 1 空位，右栏真实设置项 + 空位。
# 普通模式左栏：先主设置，再“管理”入口，最后是开关项与压缩阈值项。
# 右栏固定为 show_thinking/router 两项，合计 16 项，右栏其余位置为空位。
_SETTING_ORDER = (
    "model",
    "context",
    "reasoning",
    "approval",
    "channels",
    "tools",
    "subagents_advanced",
    "mcp",
    "vision",
    "image_gen",
    "tts",
    "memory",
    "plugins",
    "subagents",
    "context_compaction",
    "context_compaction_threshold",
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
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #settings-dialog.advanced {
        width: 62;
    }
    #settings-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #settings-list {
        height: 1fr;
    }
    #settings-body {
        height: 100%;
        layout: horizontal;
    }
    .settings-column {
        width: 1fr;
        height: 100%;
    }
    #settings-divider {
        width: 1;
        height: 100%;
        color: $terminal-border-strong;
    }
    .settings-row {
        height: 2;
        padding: 0 1;
        color: $terminal-text-secondary;
    }
    .settings-row.selected {
        color: $terminal-amber;
        text-style: bold;
    }
    .settings-row.compact {
        height: 1;
    }
    #settings-status {
        height: 2;
        color: $terminal-white;
        margin-top: 1;
    }
    #settings-help {
        height: 1;
        color: $terminal-white;
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
            else _SETTING_ORDER
        )

    @property
    def _all_keys(self) -> tuple[str, ...]:
        """全部可设置键：左栏 + 右栏真实设置项（普通模式含思考显示与任务思维路由）。

        选中索引与行渲染基于该元组遍历；右栏其余位置仍为空位占位行。
        """

        if self._advanced:
            return self._row_keys
        return self._left_keys() + self._right_keys()

    def compose(self) -> ComposeResult:
        with Container(
            id="settings-dialog",
            classes="advanced" if self._advanced else "standard",
        ):
            yield Static(
                "子任务高级设置" if self._advanced else "运行设置",
                id="settings-title",
            )
            if self._advanced:
                # 子任务高级设置采用单列列表，参照“工具开关”页面的紧凑排布。
                with VerticalScroll(id="settings-list"):
                    for key in self._row_keys:
                        yield self._row_widget(key)
            else:
                with VerticalScroll(id="settings-list"):
                    with Container(id="settings-body"):
                        with VerticalScroll(id="settings-list-left", classes="settings-column"):
                            for key in self._left_keys():
                                yield self._row_widget(key)
                            for index in range(_COLUMN_SLOTS - len(self._left_keys())):
                                yield self._empty_row_widget(index, "left")
                        yield Static("│", id="settings-divider")
                        with VerticalScroll(id="settings-list-right", classes="settings-column"):
                            for key in self._right_keys():
                                yield self._row_widget(key)
                            for index in range(_COLUMN_SLOTS - len(self._right_keys())):
                                yield self._empty_row_widget(index, "right")
            yield Static(self._status, id="settings-status")
            yield Static("↑↓ 选择  ←→ 修改  Enter/空格确认  Esc 返回", id="settings-help")

    def on_mount(self) -> None:
        self.call_after_refresh(self._render_rows)

    def action_cancel(self) -> None:
        if not self._busy:
            self.dismiss(None)

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._all_keys)
            self._render_rows()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._all_keys)
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
        key = self._all_keys[self._selected]
        if not self._advanced and key in {"model", "channels", "vision", "image_gen", "tts"}:
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
            current = str(getattr(self._agent, "approval_mode", APPROVAL_MODE_REVIEW))
            try:
                index = _APPROVAL_OPTIONS.index(current)
            except ValueError:
                index = 0
            self._apply_setting(key, _APPROVAL_OPTIONS[(index + direction) % len(_APPROVAL_OPTIONS)])
            return
        if not self._advanced and key == "context_compaction_threshold":
            current = self._context_compaction_percent()
            options = _CONTEXT_COMPACTION_PERCENT_OPTIONS
            try:
                index = options.index(current)
            except ValueError:
                index = min(
                    range(len(options)),
                    key=lambda item: abs(options[item] - current),
                )
            self._apply_setting(
                key,
                options[(index + direction) % len(options)],
            )
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
        if key == "subagents":
            config = getattr(self._agent, "config", None)
            return bool(getattr(getattr(config, "subagents", None), "enabled", False))
        if key == "context_compaction":
            config = getattr(self._agent, "config", None)
            return bool(getattr(getattr(config, key, None), "enabled", False))
        if key == "router":
            config = getattr(self._agent, "config", None)
            return bool(getattr(config, "router_enabled", False))
        if key == "vision":
            config = getattr(self._agent, "config", None)
            vision = getattr(config, "vision", None)
            return bool(getattr(vision, "enabled", False))
        if key == "image_gen":
            config = getattr(self._agent, "config", None)
            image_gen = getattr(config, "image_gen", None)
            return bool(getattr(image_gen, "enabled", False))
        if key == "show_thinking":
            config = getattr(self._agent, "config", None)
            return bool(getattr(config, "show_thinking", True))
        return False

    def _subagent_config_value(self, key: str) -> int | float:
        config = getattr(getattr(self._agent, "config", None), "subagents", None)
        return getattr(config, key, 0)

    def _context_compaction_percent(self) -> int:
        """把当前触发阈值换算成最近的 5% 档位百分比。

        换算公式：``上下文 × 百分比 = trigger_context_tokens``；
        缺少阈值或上下文窗口信息时回退默认 75%。
        """

        config = getattr(getattr(self._agent, "config", None), "context_compaction", None)
        tokens = getattr(config, "trigger_context_tokens", None)
        context_window = int(getattr(self._agent, "context_window_tokens", 128_000))
        if not tokens or context_window <= 0:
            return 75
        percent = tokens * 100 / context_window
        return min(
            _CONTEXT_COMPACTION_PERCENT_OPTIONS,
            key=lambda option: abs(option - percent),
        )

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
            message = _apply_setting_value(self, key, value)
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
        for key in self._all_keys:
            text = f"{labels[key]}：{values[key]}"
            marker = "› " if key == self._all_keys[self._selected] else "  "
            row = self.query_one(f"#settings-row-{key}", Static)
            row.update(marker + text)
            selected = key == self._all_keys[self._selected]
            row.set_class(selected, "selected")
            if selected:
                row.scroll_visible(animate=False)
        self.query_one("#settings-status", Static).update(self._status)

    def _left_keys(self) -> tuple[str, ...]:
        """普通模式左栏设置项；高级模式单列时由 _all_keys 直接返回。"""

        return self._row_keys

    def _right_keys(self) -> tuple[str, ...]:
        """普通模式右栏真实设置项：当前为“思考显示”与“任务思维路由”。

        后续新增设置项时，保持 _SETTING_ORDER 为左栏项、右栏追加新 key 即可。
        """

        return () if self._advanced else ("show_thinking", "router")

    def _row_widget(self, key: str) -> Static:
        """生成单个设置行控件，含选中标记与当前状态值。"""
        values = self._current_row_values()
        labels = self._row_labels()
        marker = "› " if key == self._all_keys[self._selected] else "  "
        return Static(
            f"{marker}{labels[key]}：{values[key]}",
            id=f"settings-row-{key}",
            classes="settings-row compact",
        )

    def _empty_row_widget(self, index: int, column: str) -> Static:
        """生成空位设置行：右栏 16 个占位行，或左栏不足 16 行时的补位行。"""
        return Static(
            "",
            id=f"settings-slot-{column}-{index}",
            classes="settings-row compact",
        )

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
            "image_gen": "已开启" if self._feature_enabled("image_gen") else "已关闭",
            "tts": "管理",
            "reasoning": str(getattr(self._agent, "reasoning_effort", "none") or "none"),
            "context": f"{int(getattr(self._agent, 'context_window_tokens', 128_000)) // 1000}K",
            "approval": approval_mode_label(str(getattr(self._agent, "approval_mode", APPROVAL_MODE_REVIEW))),
            "tools": "管理",
            "subagents_advanced": "管理",
            "context_compaction_threshold": f"{self._context_compaction_percent()}%",
            "show_thinking": "已开启" if self._feature_enabled("show_thinking") else "已关闭",
        }
        for key, _label, _section in _FEATURES:
            values[key] = "管理" if key == "mcp" else ("已开启" if self._feature_enabled(key) else "已关闭")
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
            "image_gen": "图像生成",
            "tts": "TTS 语音合成",
            "reasoning": "推理强度",
            "context": "上下文长度（K）",
            "approval": "工具审批",
            "tools": "工具开关",
            "subagents_advanced": "子任务高级设置",
            "context_compaction_threshold": "上下文压缩阈值（%）",
            "show_thinking": "思考显示",
            **_SUBAGENT_ADVANCED_LABELS,
            **{key: label for key, label, _section in _FEATURES},
        }


def _apply_setting_value(screen: SettingsScreen, key: str, value: object) -> str:
    """按 key 应用单个设置并返回提示消息；失败抛异常由 _apply_setting 统一处理。

    P4 重构自 SettingsScreen._apply_setting 的 try 块；每个分支都遵循
    "记录旧值 → 应用 → 写盘 → 失败回滚 → 提示消息" 的模式。
    """

    if key == "reasoning":
        previous = str(getattr(screen._agent, "reasoning_effort", "none") or "none")
        normalized = screen._agent.set_reasoning_effort(str(value))
        try:
            path = save_reasoning_effort(normalized)
        except Exception:
            screen._agent.set_reasoning_effort(previous)
            raise
        return f"推理强度已设为 {_REASONING_LABELS[normalized]}，已保存到 {path}。"
    elif key == "context":
        previous = int(getattr(screen._agent, "context_window_tokens", 128_000))
        tokens = int(value)
        # 修改上下文长度时，压缩阈值按当前百分比自动跟随重算。
        percent = screen._context_compaction_percent()
        previous_trigger = getattr(
            getattr(getattr(screen._agent, "config", None), "context_compaction", None),
            "trigger_context_tokens",
            None,
        )
        screen._agent.set_context_window_tokens(tokens)
        try:
            path = save_context_window_tokens(
                tokens,
                model_source=str(getattr(getattr(screen._agent, "config", None), "llm", None) and getattr(screen._agent.config.llm, "model_source", "legacy") or "legacy"),
                catalog_key=str(getattr(getattr(screen._agent, "config", None), "llm", None) and getattr(screen._agent.config.llm, "catalog_key", "") or ""),
            )
            screen._agent.set_context_compaction_trigger_percent(percent)
            save_context_compaction_trigger_percent(
                percent,
                context_window_tokens=tokens,
            )
        except Exception:
            screen._agent.set_context_window_tokens(previous)
            if previous_trigger is not None:
                screen._agent.set_context_compaction_trigger_tokens(previous_trigger)
            raise
        threshold_tokens = tokens * percent // 100
        return (
            f"上下文长度已设为 {tokens // 1000}K，已保存到 {path}；"
            f"压缩阈值已同步为 {percent}%（{threshold_tokens} Token）。"
        )
    elif key == "context_compaction_threshold":
        percent = int(value)
        previous = screen._context_compaction_percent()
        context_window = int(getattr(screen._agent, "context_window_tokens", 128_000))
        screen._agent.set_context_compaction_trigger_percent(percent)
        try:
            path = save_context_compaction_trigger_percent(
                percent,
                context_window_tokens=context_window,
            )
        except Exception:
            screen._agent.set_context_compaction_trigger_percent(previous)
            raise
        tokens = context_window * percent // 100
        return (
            f"上下文压缩阈值已设为 {percent}%（{tokens} Token），已保存到 {path}。"
        )
    elif key == "approval":
        previous = str(getattr(screen._agent, "approval_mode", APPROVAL_MODE_REVIEW))
        mode = str(value)
        screen._agent.set_approval_mode(mode)
        try:
            path = save_approval_mode(mode)
        except Exception:
            screen._agent.set_approval_mode(previous)
            raise
        return f"审批模式已设为 {approval_mode_label(mode)}，已保存到 {path}。"
    elif key == "show_thinking":
        enabled = bool(value)
        previous = screen._feature_enabled("show_thinking")
        screen._agent.set_show_thinking(enabled)
        try:
            path = save_show_thinking(enabled)
        except Exception:
            screen._agent.set_show_thinking(previous)
            raise
        return f"思考显示已{'开启' if enabled else '关闭'}，已保存到 {path}。"
    elif screen._advanced:
        normalized = validate_subagent_advanced_setting(key, value)
        previous = screen._subagent_config_value(key)
        screen._agent.set_subagent_advanced_setting(key, normalized)
        try:
            path = save_subagent_setting(key, normalized)
        except Exception:
            screen._agent.set_subagent_advanced_setting(key, previous)
            raise
        return f"{_SUBAGENT_ADVANCED_LABELS[key]}已设为 {normalized:g}，已保存到 {path}。"
    else:
        enabled = bool(value)
        previous = screen._feature_enabled(key)
        path = save_feature_enabled(key, enabled)
        setter_name = {
            "memory": "set_memory_enabled",
            "mcp": "set_mcp_enabled",
            "plugins": "set_plugin_enabled",
            "subagents": "set_subagents_enabled",
            "context_compaction": "set_context_compaction_enabled",
            "router": "set_router_enabled",
        }[key]
        setter = getattr(screen._agent, setter_name)
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
        return f"{label}已{'开启' if enabled else '关闭'}，已保存到 {path}。"


__all__ = ["SettingsAction", "SettingsScreen"]
