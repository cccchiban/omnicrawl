"""全屏 TUI 的中文运行设置面板（全屏三区：顶部标题 + 左侧列表 + 右侧二级面板）。

布局与交互：
- 顶部：与窗口标题一致的“运行设置”标题，贴近左侧栏对齐；
- 左侧：圆角框内从上到下排列所有设置项，只显示设置项名称，不显示状态；
- 右侧：比左侧更宽的圆角框，实时展示左侧选中项的二级菜单内容：
  - 简单开关/枚举项 → 单个下拉选项框（选择即应用保存）；
  - 工具设置（审批模式 + MCP 策略/Server + 内置工具开关）→ 分节列表，
    Enter/←→/空格 修改选中行并即时保存；
  - 子任务设置（功能开关 + 高级参数）→ 同一分节列表；
  - 模型 / 模型渠道 → 内嵌选择/管理面板（切换/保存后留在右侧）；
  - 其余复杂设置项 → 对应 *SettingsPane 完整管理界面。
- 键盘：左侧行获得焦点时 ↑↓ 移动并实时刷新右侧；Enter/→ 把焦点移入
  右侧操作；右侧内 Esc 先回左侧，再 Esc 关闭整个设置面板。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional

from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Vertical, VerticalScroll
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
from ....config.core.runtime import resolve_config_path, resolve_models_path
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
from ....config.features.tools import (
    TOOL_SWITCH_KEYS,
    TOOL_SWITCH_LABELS,
    ToolSwitchConfigError,
    save_tool_switch,
)
from ....llm import LLMError, save_reasoning_effort
from ..terminal.theme import terminal_css
from .panes import SelectPane, SettingsPane

SETTINGS_TITLE = "运行设置"


@dataclass(frozen=True)
class SettingsAction:
    """设置面板关闭时返回的 UI 动作（导航层协议：name 标识要打开的整屏页）。"""

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
_APPROVAL_LABELS = {mode: approval_mode_label(mode) for mode in _APPROVAL_OPTIONS}
_CONTEXT_WINDOW_OPTIONS_K = (32, 64, 128, 256, 512, 1024, 2048)
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
    ("plugins", "插件功能", "plugins"),
)
# 一级设置项（左侧列表，自上而下）。工具审批/MCP/工具开关合并为 tools，
# 子任务功能/高级合并为 subagents。
_SETTING_ORDER = (
    "model",
    "context",
    "reasoning",
    "channels",
    "tools",
    "vision",
    "image_gen",
    "tts",
    "run_guard",
    "agent_workspace",
    "memory",
    "plugins",
    "subagents",
    "context_compaction_threshold",
    "show_thinking",
)


class SettingsScreen(ModalScreen[Any]):
    """全屏三区设置面板。关闭时 dismiss(None)。"""

    BINDINGS = [
        Binding("escape", "exit_settings", "退出设置", priority=True),
        Binding("up", "move_up", "上一项"),
        Binding("down", "move_down", "下一项"),
        ("left", "back_to_list", "返回左侧"),
        ("enter", "enter_active", "进入"),
        ("right", "enter_active", "进入"),
    ]

    CSS = terminal_css("""
    SettingsScreen {
        background: $terminal-canvas;
    }
    #settings-title {
        height: 3;
        align: left middle;
        margin-left: 2;
        color: $terminal-white;
        text-style: bold;
    }
    #settings-main {
        height: 1fr;
        layout: horizontal;
    }
    #settings-left-col {
        width: 30;
        min-width: 24;
        height: 100%;
        padding: 0 1 0 2;
    }
    #settings-left-box {
        height: 100%;
        border: round $terminal-border-strong;
        background: $terminal-surface;
        padding: 1;
    }
    #settings-right-col {
        width: 1fr;
        height: 100%;
        padding: 0 2 0 1;
    }
    #settings-right-box {
        height: 100%;
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #settings-right-title {
        height: 1;
        margin: 1 2 0 2;
        color: $terminal-white;
        text-style: bold;
    }
    #settings-right-area {
        height: 1fr;
    }
    .settings-row {
        height: 1;
        padding: 0 1;
        color: $terminal-text-secondary;
    }
    .settings-row:focus {
        color: $terminal-amber;
        text-style: bold;
    }
    #settings-guide {
        height: 100%;
        content-align: center middle;
        color: $terminal-text-secondary;
    }
    #settings-help {
        height: 1;
        padding: 0 2;
        color: $terminal-text-muted;
    }
    """)

    def __init__(self, agent: Any, *, advanced: bool = False) -> None:
        super().__init__()
        self._agent = agent
        self._advanced = advanced
        self._row_keys = (
            tuple(SUBAGENT_ADVANCED_SETTING_KEYS) if advanced else _SETTING_ORDER
        )
        self._rows: list[_SettingsRow] = []
        self._pane: Optional[SettingsPane] = None
        self._pane_key: Optional[str] = None
        self._last_status = ""
        self._mount_seq = 0

    # ---------- compose ----------

    def compose(self) -> ComposeResult:
        title = "子任务高级设置" if self._advanced else SETTINGS_TITLE
        yield Static(title, id="settings-title")
        with Container(id="settings-main"):
            with Vertical(id="settings-left-col"):
                with Container(id="settings-left-box"):
                    with VerticalScroll(id="settings-left-list"):
                        for key in self._row_keys:
                            row = _SettingsRow(key, self._row_label(key))
                            self._rows.append(row)
                            yield row
            with Vertical(id="settings-right-col"):
                with Container(id="settings-right-box"):
                    yield Static(self._right_title(), id="settings-right-title")
                    yield Container(id="settings-right-area")
        yield Static(self._help_text(), id="settings-help")

    def on_mount(self) -> None:
        self.call_after_refresh(self._focus_row)

    def _help_text(self) -> str:
        if self._advanced:
            return "↑↓ 选择参数  ←→/Enter 修改  Esc 返回"
        return "↑↓ 选择设置项（右侧实时预览）  Enter/→ 进入右侧  ←/Esc 返回  Esc 在左侧退出"

    # ---------- 标签 / 状态查询 ----------

    @staticmethod
    def _row_labels() -> dict[str, str]:
        labels = {
            "model": "模型",
            "context": "上下文长度",
            "reasoning": "推理强度",
            "channels": "模型渠道",
            "tools": "工具设置",
            "vision": "视觉",
            "image_gen": "图像生成",
            "tts": "TTS 语音合成",
            "run_guard": "持续运转",
            "agent_workspace": "隔离工作区",
            "memory": "记忆功能",
            "plugins": "插件功能",
            "subagents": "子任务设置",
            "context_compaction_threshold": "上下文压缩阈值",
            "show_thinking": "思考显示",
        }
        labels.update(_SUBAGENT_ADVANCED_LABELS)
        return labels

    def _row_label(self, key: str) -> str:
        return self._row_labels().get(key, key)

    def _right_title(self) -> str:
        key = self._pane_key or (self._rows[0].key if self._rows else "")
        return self._row_label(key)

    # ---------- 键盘：移动 / 进入 / 返回 / 退出 ----------

    def action_move_up(self) -> None:
        self._move_selection(-1)

    def action_move_down(self) -> None:
        self._move_selection(1)

    def _move_selection(self, delta: int) -> None:
        if not self._rows:
            return
        focused = self.screen.focused
        if not isinstance(focused, _SettingsRow):
            self._focus_row()
            return
        index = self._rows.index(focused)
        target = self._rows[(index + delta) % len(self._rows)]
        target.focus()
        target.scroll_visible(animate=False)

    def _focus_row(self, key: Optional[str] = None) -> None:
        if not self._rows:
            return
        target = self._rows[0]
        if key is not None:
            for row in self._rows:
                if row.key == key:
                    target = row
                    break
        target.focus()
        target.scroll_visible(animate=False)

    def action_enter_active(self) -> None:
        """左侧当前行 Enter/→：焦点进入右侧面板。"""
        if self._pane is not None and self._pane.is_attached:
            self._pane.activate()

    def action_back_to_list(self) -> None:
        """右侧 → 左侧：聚焦左侧当前行。"""
        self._focus_row(self._pane_key)

    def action_exit_settings(self) -> None:
        if isinstance(self.screen.focused, _SettingsRow):
            self.dismiss(None)
            return
        # 焦点在右侧 pane：先回左侧列表。
        self._focus_row(self._pane_key)

    # ---------- 右侧面板管理 ----------

    def _on_row_focused(self, row: "_SettingsRow") -> None:
        """左侧行获得焦点：记录选中项，异步刷新右侧（预览）。"""
        key = row.key
        if key == self._pane_key and self._pane is not None and self._pane.is_attached:
            return
        self._pane_key = key
        self._refresh_right_area()

    def _refresh_right_area(self) -> None:
        """移除右侧旧内容并用唯一 id 挂载新内容。"""
        if not self.is_mounted:
            return
        self._mount_seq += 1
        area = self.query_one("#settings-right-area", Container)
        # remove_children 是异步移除：旧节点延迟销毁，因此新节点必须用
        # 递增唯一 id，避免与尚未销毁的旧 id 冲突。
        area.remove_children()
        key = self._pane_key or (self._rows[0].key if self._rows else "")
        title = self.query_one("#settings-right-title", Static)
        title.update(self._row_label(key))
        self.call_after_refresh(self._mount_current_pane, key)

    def _mount_current_pane(self, key: str) -> None:
        """刷新回调中真正挂载右侧内容（此时旧节点已移除）。"""
        if not self.is_mounted or key != self._pane_key:
            return
        area = self.query_one("#settings-right-area", Container)
        pane = self._build_pane(key)
        if pane is None:
            self._pane = None
            area.mount(
                Static(
                    "",
                    id=f"settings-guide-{self._mount_seq}",
                    classes="settings-guide",
                )
            )
            return
        self._pane = pane
        pane.bind_pane_events(
            on_back=self.action_back_to_list,
            on_commit=self._commit_pane,
            on_navigate=self._navigate_pane,
            on_modal=self._open_modal,
        )
        pane.id = f"settings-pane-{self._mount_seq}"
        area.mount(pane)
        # 预览：焦点留在左侧行，不自动进入右侧；pane 已挂载可接收按键。
        self._pane.refresh_pane()

    def _commit_pane(self, result: Any = None) -> None:
        """面板保存完成：SelectPane 已就地刷新；模型/渠道提交后保留在
        右侧（切换/保存后不打断浏览），其余表单类保存后返回左侧。"""
        if isinstance(self._pane, SelectPane):
            return
        if self._pane_key in ("model", "channels"):
            return
        if result is not None and isinstance(result, str):
            self._last_status = result
        self._focus_row(self._pane_key)

    def _navigate_pane(self, target: str, payload: Any) -> None:
        """面板请求切换到其它二级面板或弹层。

        - mcp_servers：以整屏弹层打开原 MCPServerListScreen（保留完整
          三级 Server 编辑协议），关闭后回左侧 MCP 行。
        """
        if target == "mcp_servers":
            from .mcp_server_list_screen import MCPServerListScreen

            self.app.push_screen(
                MCPServerListScreen(self._agent),
                lambda _result: self._focus_row("mcp"),
            )

    def _open_modal(self, factory: Any, on_result: Any) -> None:
        self.app.push_screen(factory(), on_result)

    # ---------- 构建右侧面板 ----------

    def _build_pane(self, key: str) -> Optional[SettingsPane]:
        """根据左侧项构造右侧 pane；引导项返回 None。"""
        if self._advanced:
            current = self._subagent_value(key)
            options = _SUBAGENT_ADVANCED_OPTIONS[key]
            return SelectPane(
                [(f"{value:g}", value) for value in options],
                current,
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        if key == "model":
            from .model_picker import ModelPickerPane

            return ModelPickerPane(self._agent, refresh_on_open=True)
        if key == "channels":
            return self._build_channel_pane()
        if key == "context":
            current_k = int(getattr(self._agent, "context_window_tokens", 128_000)) // 1000
            return SelectPane(
                [(f"{k}K", k * 1000) for k in _CONTEXT_WINDOW_OPTIONS_K],
                current_k * 1000,
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        if key == "reasoning":
            current = str(getattr(self._agent, "reasoning_effort", "none") or "none")
            return SelectPane(
                [(label, opt) for opt, label in _REASONING_LABELS.items()],
                current,
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        if key == "tools":
            return self._build_tools_pane()
        if key == "subagents":
            return self._build_subagents_pane()
        if key == "context_compaction_threshold":
            current = self._context_compaction_percent()
            return SelectPane(
                [(f"{p}%", p) for p in _CONTEXT_COMPACTION_PERCENT_OPTIONS],
                current,
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        if key == "show_thinking":
            return SelectPane(
                [("开启", True), ("关闭", False)],
                self._feature_enabled("show_thinking"),
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        if key in {item[0] for item in _FEATURES}:
            return SelectPane(
                [("开启", True), ("关闭", False)],
                self._feature_enabled(key),
                lambda value: self._apply_simple(key, value),
                agent=self._agent,
            )
        return self._build_complex_pane(key)

    def _build_channel_pane(self) -> Any:
        """构造渠道管理 pane，apply 后把默认渠道应用到 agent。"""

        from .channel_manager import ChannelManagerPane

        def apply_channels(configuration) -> None:
            self._agent.set_model(configuration.default_key)

        return ChannelManagerPane(
            resolve_config_path(),
            resolve_models_path(),
            apply_configuration=apply_channels,
            agent=self._agent,
        )

    def _build_tools_pane(self) -> "_ToolsPane":
        """构造合并“工具设置”分节面板（审批模式 + MCP + 内置工具开关）。"""
        return _ToolsPane(self._agent, self._apply_tools_row, self._navigate_pane)

    def _build_subagents_pane(self) -> "_SubagentsPane":
        """构造合并“子任务设置”分节面板（功能开关 + 高级参数）。"""
        return _SubagentsPane(self._agent, applier=self._apply_subagents_row)

    # ---------- 合并项行处理（_ToolsPane/_SubagentsPane 回调） ----------

    def _apply_tools_row(self, row_key: str, direction: int = 1) -> str:
        """处理工具设置分节面板中审批/MCP/工具开关行的修改。

        row_key 取值：
        - ``approval``           审批模式（按 direction 循环）
        - ``mcp-enabled``        MCP 总开关（切换）
        - ``mcp-network``        外部网络工具（切换）
        - ``mcp-write``          写入操作确认（切换）
        - ``mcp-command``        命令操作确认（切换）
        - ``mcp-audit``          审计日志（切换）
        - ``mcp-timeout``        默认超时秒数（按 direction 循环）
        - ``tool:<工具名>``      内置工具开关（切换）
        """
        if row_key == "approval":
            previous = str(getattr(self._agent, "approval_mode", APPROVAL_MODE_REVIEW))
            try:
                index = _APPROVAL_OPTIONS.index(previous)  # type: ignore[arg-type]
            except ValueError:
                index = 0
            mode = _APPROVAL_OPTIONS[(index + direction) % len(_APPROVAL_OPTIONS)]
            self._agent.set_approval_mode(mode)
            try:
                path = save_approval_mode(mode)
            except Exception:
                self._agent.set_approval_mode(previous)
                raise
            return f"审批模式已设为 {approval_mode_label(mode)}，已保存到 {path}。"
        if row_key.startswith("tool:"):
            tool_name = row_key[5:]
            enabled = not self._tool_enabled(tool_name)
            previous = self._tool_enabled(tool_name)
            try:
                self._agent.set_tool_enabled(tool_name, enabled)
                try:
                    path = save_tool_switch(tool_name, enabled)
                except Exception:
                    self._agent.set_tool_enabled(tool_name, previous)
                    raise
            except (AgentError, ToolSwitchConfigError, OSError) as exc:
                return f"设置未完成：{exc}"
            label = TOOL_SWITCH_LABELS.get(tool_name, tool_name)
            return f"{label}已{'启用' if enabled else '关闭'}，已保存到 {path}。"
        # MCP 行：开关切换；timeout 为档位循环。
        from dataclasses import replace

        from ....mcp.config import MCPConfig, load_mcp_config
        from .mcp_settings import _apply_and_save, _current_config

        config = _current_config(self._agent)
        if not isinstance(config, MCPConfig):
            config = load_mcp_config()
        if row_key == "mcp-enabled":
            candidate = replace(config, enabled=not config.enabled)
        elif row_key == "mcp-network":
            candidate = replace(
                config,
                policy=replace(
                    config.policy,
                    allow_external_network_tools=not config.policy.allow_external_network_tools,
                ),
            )
        elif row_key == "mcp-write":
            candidate = replace(
                config,
                policy=replace(
                    config.policy,
                    require_confirmation_for_write=not config.policy.require_confirmation_for_write,
                ),
            )
        elif row_key == "mcp-command":
            candidate = replace(
                config,
                policy=replace(
                    config.policy,
                    require_confirmation_for_command=not config.policy.require_confirmation_for_command,
                ),
            )
        elif row_key == "mcp-audit":
            candidate = replace(
                config,
                policy=replace(
                    config.policy,
                    audit_log_enabled=not config.policy.audit_log_enabled,
                ),
            )
        elif row_key == "mcp-timeout":
            options = (10, 30, 60, 120, 300)
            current_value = getattr(config, "default_timeout_seconds", 60)
            index = min(range(len(options)), key=lambda i: abs(options[i] - current_value))
            new_value = options[(index + direction) % len(options)]
            candidate = replace(config, default_timeout_seconds=new_value)
        else:
            return "设置未完成：未知的设置项。"
        try:
            path = _apply_and_save(self, candidate)
        except Exception as exc:  # noqa: BLE001 - 写盘/校验失败统一转为状态文本
            return f"设置未完成：{exc}"
        return f"MCP 设置已保存：{path}"

    def _apply_subagents_row(self, row_key: str, direction: int = 1) -> str:
        """处理子任务设置分节面板中的行修改。

        row_key 取值：
        - ``enabled``              子任务功能总开关（切换）
        - ``<advanced-key>``       高级参数（按 direction 循环）
        """
        if row_key == "enabled":
            enabled = not self._feature_enabled("subagents")
            previous = not enabled
            try:
                path = save_feature_enabled("subagents", enabled)
                try:
                    self._agent.set_subagents_enabled(enabled)
                except Exception:
                    save_feature_enabled("subagents", previous)
                    raise
            except (AgentError, SettingsConfigError, OSError) as exc:
                return f"设置未完成：{exc}"
            return f"子任务功能已{'开启' if enabled else '关闭'}，已保存到 {path}。"
        # 高级参数行
        key = row_key
        options = _SUBAGENT_ADVANCED_OPTIONS[key]
        current = self._subagent_value(key)
        try:
            index = options.index(current)  # type: ignore[arg-type]
        except ValueError:
            index = min(range(len(options)), key=lambda i: abs(float(options[i]) - float(current)))
        value = options[(index + direction) % len(options)]
        try:
            normalized = validate_subagent_advanced_setting(key, value)
            previous = self._subagent_value(key)
            self._agent.set_subagent_advanced_setting(key, normalized)
            try:
                path = save_subagent_setting(key, normalized)
            except Exception:
                self._agent.set_subagent_advanced_setting(key, previous)
                raise
        except (AgentError, LLMError, SettingsConfigError, SubAgentConfigError, OSError) as exc:
            return f"设置未完成：{exc}"
        return f"{_SUBAGENT_ADVANCED_LABELS[key]}已设为 {normalized:g}，已保存到 {path}。"

    def _tool_enabled(self, tool_name: str) -> bool:
        config = getattr(self._agent, "config", None)
        disabled = frozenset(getattr(config, "disabled_tools", ()))
        return tool_name not in disabled

    def _build_complex_pane(self, key: str) -> Optional[SettingsPane]:
        """延迟构造复杂项面板，避免导入环。"""
        if key == "tools":
            from .tool_settings import ToolSettingsPane

            return ToolSettingsPane(self._agent)
        if key == "mcp":
            from .mcp_settings_screen import MCPSettingsPane

            return MCPSettingsPane(self._agent)
        if key == "vision":
            from .vision_settings import VisionSettingsPane

            return VisionSettingsPane(
                self._agent,
                resolve_config_path(),
                apply_configuration=getattr(self._agent, "set_vision_configuration", None),
            )
        if key == "image_gen":
            from .image_gen_settings import ImageGenSettingsPane

            return ImageGenSettingsPane(
                resolve_config_path(),
                agent=self._agent,
                apply_configuration=getattr(self._agent, "set_image_gen_configuration", None),
            )
        if key == "tts":
            from .tts_settings import TTSSettingsPane

            return TTSSettingsPane(
                resolve_config_path(),
                agent=self._agent,
                apply_configuration=getattr(self._agent, "set_tts_configuration", None),
            )
        if key == "run_guard":
            from .run_guard_settings import RunGuardSettingsPane

            return RunGuardSettingsPane(
                resolve_config_path(),
                agent=self._agent,
                apply_configuration=getattr(self._agent, "set_run_guard_configuration", None),
            )
        if key == "agent_workspace":
            from .agent_workspace_settings import AgentWorkspaceSettingsPane

            return AgentWorkspaceSettingsPane(
                resolve_config_path(),
                agent=self._agent,
                apply_configuration=getattr(self._agent, "set_agent_workspace_configuration", None),
            )
        return None

    # ---------- 简单项保存（SelectPane applier） ----------

    def _apply_simple(self, key: str, value: Any) -> str:
        if self._advanced:
            try:
                normalized = validate_subagent_advanced_setting(key, value)
                previous = self._subagent_value(key)
                self._agent.set_subagent_advanced_setting(key, normalized)
                try:
                    path = save_subagent_setting(key, normalized)
                except Exception:
                    self._agent.set_subagent_advanced_setting(key, previous)
                    raise
                return f"{_SUBAGENT_ADVANCED_LABELS[key]}已设为 {normalized:g}，已保存到 {path}。"
            except (AgentError, LLMError, SettingsConfigError, SubAgentConfigError, OSError) as exc:
                return f"设置未完成：{exc}"
            except Exception as exc:
                return f"设置未完成：{exc}"
        try:
            return _apply_setting_value(self, key, value)
        except (AgentError, LLMError, SettingsConfigError, OSError) as exc:
            return f"设置未完成：{exc}"
        except Exception as exc:
            return f"设置未完成：{exc}"

    # ---------- 查询（供保存函数使用） ----------

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
        if key == "show_thinking":
            config = getattr(self._agent, "config", None)
            return bool(getattr(config, "show_thinking", True))
        return False

    def _context_compaction_percent(self) -> int:
        config = getattr(getattr(self._agent, "config", None), "context_compaction", None)
        tokens = getattr(config, "trigger_context_tokens", None)
        context_window = int(getattr(self._agent, "context_window_tokens", 128_000))
        if not tokens or context_window <= 0:
            return 80
        percent = tokens * 100 / context_window
        return min(
            _CONTEXT_COMPACTION_PERCENT_OPTIONS,
            key=lambda option: abs(option - percent),
        )

    def _subagent_value(self, key: str) -> int | float:
        config = getattr(getattr(self._agent, "config", None), "subagents", None)
        return getattr(config, key, 0)


class _GroupedRowsPane(SettingsPane):
    """右侧“分节列表”面板基类：按分区平铺多行设置项，即时修改。

    Enter/←→/空格 对选中行执行修改（切换或档位循环），Esc 返回左侧。
    状态文本来自 applier；子类通过 ``row_value`` 提供每行当前值的展示。
    """

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("up", "move_up", "上一项", priority=True),
        Binding("down", "move_down", "下一项", priority=True),
        ("left", "change", "修改"),
        ("right", "change", "修改"),
        ("enter", "change", "修改"),
        ("space", "change", "修改"),
    ]

    DEFAULT_CSS = terminal_css("""
    #grouped-pane-list { height: 1fr; }
    .grouped-pane-head { height: 1; padding: 0 1; color: $terminal-text-muted; text-style: bold; }
    .grouped-pane-row { height: 1; padding: 0 1; color: $terminal-text-secondary; }
    .grouped-pane-row.selected { color: $terminal-amber; text-style: bold; }
    #grouped-pane-status { height: 2; color: $terminal-white; margin-top: 1; }
    """)

    #: 分节定义：(分节标题, ((row_key, 行标签), ...))；子类覆盖。
    sections: tuple[tuple[str, tuple[tuple[str, str], ...]], ...] = ()

    def __init__(
        self,
        agent: Any,
        *,
        applier: Any,
    ) -> None:
        super().__init__(agent=agent)
        self._applier = applier
        self._rows: list[str] = []
        self._labels: dict[str, str] = {}
        for _head, items in self.sections:
            for key, label in items:
                self._rows.append(key)
                self._labels[key] = label
        self._selected = 0
        self._busy = False
        self._status = ""

    def compose_pane(self) -> ComposeResult:
        with VerticalScroll(id="grouped-pane-list"):
            index = 0
            for head, items in self.sections:
                if head:
                    yield Static(head, classes="grouped-pane-head")
                for _key, _label in items:
                    yield Static(
                        "",
                        id=f"grouped-pane-row-{index}",
                        classes="grouped-pane-row",
                    )
                    index += 1
        yield Static(self._status, id="grouped-pane-status")

    # ---------- 子类实现 ----------

    def row_value(self, key: str) -> str:
        """返回某行的当前值文本；子类实现。"""
        raise NotImplementedError

    def activate_row(self, key: str, direction: int) -> str:
        """执行某行修改；默认交给 applier。返回状态文本。"""
        return self._applier(key, direction)

    # ---------- 渲染 ----------

    def _row_text(self, index: int) -> str:
        key = self._rows[index]
        marker = "› " if index == self._selected else "  "
        return f"{marker}{self._labels.get(key, key)}：{self.row_value(key)}"

    def refresh_pane(self) -> None:
        if not self.is_mounted:
            return
        for index, key in enumerate(self._rows):
            row = self.query_one(f"#grouped-pane-row-{index}", Static)
            row.update(self._row_text(index))
            row.set_class(index == self._selected, "selected")
            if index == self._selected:
                row.scroll_visible(animate=False)
        self.query_one("#grouped-pane-status", Static).update(self._status)

    # ---------- 键盘 ----------

    def action_cancel(self) -> None:
        if not self._busy:
            self.request_back()

    def action_move_up(self) -> None:
        if not self._busy:
            self._selected = (self._selected - 1) % len(self._rows)
            self.refresh_pane()

    def action_move_down(self) -> None:
        if not self._busy:
            self._selected = (self._selected + 1) % len(self._rows)
            self.refresh_pane()

    def action_change(self) -> None:
        self._change(1)

    def _change(self, direction: int) -> None:
        if self._busy:
            return
        key = self._rows[self._selected]
        try:
            status = self.activate_row(key, direction)
        except Exception as exc:  # noqa: BLE001 - 保存/应用失败统一转为状态文本
            status = f"设置未完成：{exc}"
        self._status = status
        self.refresh_pane()


class _ToolsPane(_GroupedRowsPane):
    """“工具设置”分节面板：审批模式 + MCP 策略/Server + 内置工具开关。"""

    def __init__(self, agent: Any, applier: Any, navigator: Any) -> None:
        self._navigator = navigator
        super().__init__(agent, applier=applier)
        self._registered = frozenset(
            getattr(getattr(agent, "_tools", None), "keys", lambda: ())()
        )

    sections = (
        (
            "审批模式",
            (("approval", "审批模式"),),
        ),
        (
            "MCP 工具",
            (
                ("mcp-enabled", "MCP 总开关"),
                ("mcp-network", "外部网络工具"),
                ("mcp-write", "写入操作确认"),
                ("mcp-command", "命令操作确认"),
                ("mcp-audit", "审计日志"),
                ("mcp-timeout", "默认超时"),
                ("mcp-servers", "Server 管理"),
            ),
        ),
        (
            "内置工具开关",
            tuple((f"tool:{name}", TOOL_SWITCH_LABELS.get(name, name)) for name in TOOL_SWITCH_KEYS),
        ),
    )

    def _mcp_config(self) -> Any:
        from ....mcp.config import MCPConfig, load_mcp_config
        from .mcp_settings import _current_config

        config = _current_config(self._agent)
        return config if isinstance(config, MCPConfig) else load_mcp_config()

    def row_value(self, key: str) -> str:
        if key == "approval":
            return approval_mode_label(
                str(getattr(self._agent, "approval_mode", APPROVAL_MODE_REVIEW))
            )
        if key.startswith("tool:"):
            tool_name = key[5:]
            config = getattr(self._agent, "config", None)
            disabled = frozenset(getattr(config, "disabled_tools", ()))
            enabled = tool_name not in disabled
            state = "已启用" if enabled else "已关闭"
            if tool_name not in self._registered:
                return f"{state}（未注册）"
            return state
        config = self._mcp_config()
        policy = config.policy
        values = {
            "mcp-enabled": "已开启" if config.enabled else "已关闭",
            "mcp-network": "已允许" if policy.allow_external_network_tools else "已禁止",
            "mcp-write": "需要确认" if policy.require_confirmation_for_write else "免确认",
            "mcp-command": "需要确认" if policy.require_confirmation_for_command else "免确认",
            "mcp-audit": "已开启" if policy.audit_log_enabled else "已关闭",
            "mcp-timeout": f"{config.default_timeout_seconds} 秒",
            "mcp-servers": f"管理（{len(config.servers)} 个）",
        }
        return values.get(key, "")

    def activate_row(self, key: str, direction: int) -> str:
        if key == "mcp-servers":
            if self._navigator is not None:
                self._navigator("mcp_servers", None)
            return ""
        return super().activate_row(key, direction)


class _SubagentsPane(_GroupedRowsPane):
    """“子任务设置”分节面板：功能总开关 + 高级资源参数。"""

    sections = (
        (
            "子任务功能",
            (("enabled", "功能总开关"),),
        ),
        (
            "高级参数",
            tuple((key, _SUBAGENT_ADVANCED_LABELS[key]) for key in SUBAGENT_ADVANCED_SETTING_KEYS),
        ),
    )

    def row_value(self, key: str) -> str:
        if key == "enabled":
            config = getattr(getattr(self._agent, "config", None), "subagents", None)
            return "已开启" if bool(getattr(config, "enabled", False)) else "已关闭"
        config = getattr(getattr(self._agent, "config", None), "subagents", None)
        return f"{getattr(config, key, 0):g}"


class _SettingsRow(Static, can_focus=True):
    """左侧设置项行：可聚焦，焦点变化实时刷新右侧。"""

    BINDINGS = [
        Binding("up", "row_up", "上一项", priority=True),
        Binding("down", "row_down", "下一项", priority=True),
    ]

    def __init__(self, key: str, label: str) -> None:
        super().__init__(label, id=f"settings-row-{key}", classes="settings-row")
        self.key = key

    def on_focus(self) -> None:
        if isinstance(self.screen, SettingsScreen):
            self.screen._on_row_focused(self)

    def action_row_up(self) -> None:
        if isinstance(self.screen, SettingsScreen):
            self.screen._move_selection(-1)

    def action_row_down(self) -> None:
        if isinstance(self.screen, SettingsScreen):
            self.screen._move_selection(1)


def _apply_setting_value(screen: SettingsScreen, key: str, value: object) -> str:
    """按 key 应用单个设置并返回提示消息；失败抛异常由调用方统一处理。"""

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
                model_source=str(
                    getattr(getattr(screen._agent, "config", None), "llm", None)
                    and getattr(screen._agent.config.llm, "model_source", "legacy")
                    or "legacy"
                ),
                catalog_key=str(
                    getattr(getattr(screen._agent, "config", None), "llm", None)
                    and getattr(screen._agent.config.llm, "catalog_key", "")
                    or ""
                ),
            )
            screen._agent.set_context_compaction_trigger_percent(percent)
            save_context_compaction_trigger_percent(percent, context_window_tokens=tokens)
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
                percent, context_window_tokens=context_window
            )
        except Exception:
            screen._agent.set_context_compaction_trigger_percent(previous)
            raise
        tokens = context_window * percent // 100
        return f"上下文压缩阈值已设为 {percent}%（{tokens} Token），已保存到 {path}。"
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
    else:
        enabled = bool(value)
        previous = screen._feature_enabled(key)
        path = save_feature_enabled(key, enabled)
        setter_name = {
            "memory": "set_memory_enabled",
            "plugins": "set_plugin_enabled",
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
