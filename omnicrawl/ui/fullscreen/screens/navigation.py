"""设置面板导航：主菜单、模型选择器、MCP 与各子设置页的打开与回退。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：
``SettingsNavigationMixin`` 的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字
一致；各设置页 Screen 仍是 ``fullscreen`` 包内的独立模块
（``settings.py``、``model_picker.py``、``channel_manager.py``、
``mcp_settings.py``、``tool_settings.py``、``vision_settings.py``、
``image_gen_settings.py``）。``ui/fullscreen/screens/__init__.py`` 只做再导出。
"""

from __future__ import annotations

from ....config.core.runtime import resolve_config_path, resolve_models_path
from .run_guard_settings import RunGuardSettingsScreen
from .agent_workspace_settings import AgentWorkspaceSettingsScreen
from .channel_manager import ChannelManagerResult, ChannelManagerScreen
from .image_gen_settings import ImageGenSettingsResult, ImageGenSettingsScreen
from .mcp_settings import MCPServerListScreen, MCPSettingsAction, MCPSettingsScreen
from .model_picker import ModelPickerResult, ModelPickerScreen
from .settings import SettingsAction, SettingsScreen
from .tool_settings import ToolSettingsScreen
from .tts_settings import TTSSettingsResult, TTSSettingsScreen
from .vision_settings import VisionSettingsResult, VisionSettingsScreen


class SettingsNavigationMixin:
    """原 ``OmniCrawlApp`` 的设置面板导航方法。"""

    def _open_settings(self) -> None:
        """打开中文设置面板；模型项关闭后复用现有模型选择器。"""

        self.push_screen(SettingsScreen(self.agent), self._receive_settings_action)

    def _receive_settings_action(self, action: SettingsAction | None) -> None:
        """设置面板关闭回调：按返回动作路由到对应子页；未知动作直接收尾。

        各子页的打开逻辑独立在 ``_open_settings_*`` 方法中，本方法只做
        薄路由，避免 10 层闭包嵌套。
        """

        if action is not None:
            if action.name == "model":
                self._open_model_picker(refresh=False)
            elif action.name == "channels":
                self._open_settings_channels()
            elif action.name == "subagents_advanced":
                self._open_settings_subagents_advanced()
            elif action.name == "mcp_settings":
                self._open_settings_mcp()
            elif action.name == "tools_settings":
                self._open_settings_tools()
            elif action.name == "vision":
                self._open_settings_vision()
            elif action.name == "image_gen":
                self._open_settings_image_gen()
            elif action.name == "run_guard":
                self._open_settings_run_guard()
            elif action.name == "agent_workspace":
                self._open_settings_agent_workspace()
            elif action.name == "tts":
                self._open_settings_tts()
            else:
                self._drain_pending_inputs()
        else:
            self._drain_pending_inputs()
        self._refresh_context_summary()

    def _open_settings_channels(self) -> None:
        """打开模型渠道管理；保存成功后清遥测并回设置面板。"""

        def apply_channels(configuration) -> None:
            self.agent.set_model(configuration.default_key)

        def receive_channels(result: ChannelManagerResult | None) -> None:
            if result is not None:
                self._input_tokens = 0
                self._output_tokens = 0
                self._cached_input_tokens = 0
                self._append_message(
                    "status",
                    f"模型渠道已保存，当前模型：{self.agent.current_model}",
                )
            self._refresh_context_summary()
            self._open_settings()

        self.push_screen(
            ChannelManagerScreen(
                resolve_config_path(),
                resolve_models_path(),
                apply_configuration=apply_channels,
            ),
            receive_channels,
        )

    def _open_settings_subagents_advanced(self) -> None:
        """打开子任务高级参数设置；关闭后回设置面板。"""

        self.push_screen(
            SettingsScreen(self.agent, advanced=True),
            lambda _action: self._open_settings(),
        )

    def _open_settings_mcp(self) -> None:
        """打开 MCP 全局设置；servers 项由 _receive_mcp_settings 继续路由。"""

        self.push_screen(
            MCPSettingsScreen(self.agent),
            self._receive_mcp_settings,
        )

    def _open_settings_tools(self) -> None:
        """打开工具开关设置；关闭后回设置面板。"""

        self.push_screen(
            ToolSettingsScreen(self.agent),
            lambda _action: self._open_settings(),
        )

    def _open_settings_vision(self) -> None:
        """打开视觉模型代理设置；保存成功提示后回设置面板。"""

        def apply_vision(configuration) -> None:
            self.agent.set_vision_configuration(configuration)

        def receive_vision(result: VisionSettingsResult | None) -> None:
            if result is not None:
                state = "已启用" if result.configuration.enabled else "已停用"
                self._append_message(
                    "status",
                    f"视觉模型代理{state}，已配置 {len(result.configuration.models)} 个故障转移模型。",
                )
            self._open_settings()

        self.push_screen(
            VisionSettingsScreen(
                self.agent,
                resolve_config_path(),
                apply_configuration=apply_vision,
            ),
            receive_vision,
        )

    def _open_settings_image_gen(self) -> None:
        """打开图像生成设置；保存成功提示后回设置面板。"""

        def apply_image_gen(configuration) -> None:
            self.agent.set_image_gen_configuration(configuration)

        def receive_image_gen(result: ImageGenSettingsResult | None) -> None:
            if result is not None:
                state = "已启用" if result.configuration.enabled else "已停用"
                self._append_message(
                    "status",
                    f"图像生成{state}（{result.configuration.model}，"
                    f"{result.configuration.base_url}）。",
                )
            self._open_settings()

        self.push_screen(
            ImageGenSettingsScreen(
                resolve_config_path(),
                apply_configuration=apply_image_gen,
            ),
            receive_image_gen,
        )

    def _open_settings_run_guard(self) -> None:
        """打开持续运转设置；保存成功提示后回设置面板。"""

        def apply_run_guard(configuration) -> None:
            self.agent.set_run_guard_configuration(configuration)

        def receive_run_guard(configuration) -> None:
            if configuration is not None:
                state = "已启用" if configuration.enabled else "已停用"
                self._append_message(
                    "status",
                    f"持续运转{state}，配置将在下一次回合生效。",
                )
            self._open_settings()

        self.push_screen(
            RunGuardSettingsScreen(
                resolve_config_path(),
                configuration=getattr(self.agent.config, "run_guard", None),
                apply_configuration=apply_run_guard,
            ),
            receive_run_guard,
        )

    def _open_settings_agent_workspace(self) -> None:
        """打开隔离工作区设置；保存成功提示后回设置面板。"""

        def apply_agent_workspace(configuration) -> None:
            self.agent.set_agent_workspace_configuration(configuration)

        def receive_agent_workspace(configuration) -> None:
            if configuration is not None:
                state = "已启用" if configuration.enabled else "已停用"
                self._append_message(
                    "status",
                    f"隔离工作区{state}，新会话启动时生效。",
                )
            self._open_settings()

        self.push_screen(
            AgentWorkspaceSettingsScreen(
                resolve_config_path(),
                configuration=getattr(self.agent.config, "agent_workspace", None),
                apply_configuration=apply_agent_workspace,
            ),
            receive_agent_workspace,
        )

    def _open_settings_tts(self) -> None:
        """打开 TTS 语音设置；保存成功提示后回设置面板。"""

        def apply_tts(configuration) -> None:
            self.agent.set_tts_configuration(configuration)

        def receive_tts(result: TTSSettingsResult | None) -> None:
            if result is not None:
                state = "已启用" if result.configuration.enabled else "已停用"
                self._append_message(
                    "status",
                    f"TTS 语音合成{state}（音色 {result.configuration.voice}，"
                    f"自动播放{'开' if result.configuration.auto_play else '关'}）。",
                )
            self._open_settings()

        self.push_screen(
            TTSSettingsScreen(
                resolve_config_path(),
                apply_configuration=apply_tts,
            ),
            receive_tts,
        )

    def _receive_mcp_settings(self, action: MCPSettingsAction | None) -> None:
        if action is not None and action.name == "servers":
            self.push_screen(
                MCPServerListScreen(self.agent),
                lambda _result: self._open_settings(),
            )
        else:
            self._open_settings()
        self._refresh_context_summary()

    def _open_model_picker(self, *, refresh: bool = False) -> None:
        """打开双列模型选择界面；结束后返回设置面板。

        模型选择器只能从设置面板进入（``/model`` 命令已移除）。退出
        （切换成功或按 ESC 取消）时与设置面板其他选项页面保持一致：
        重新打开设置面板，且不显示取消/切换提示文案。
        """

        def receive(result: ModelPickerResult | None) -> None:
            if result is not None:
                # 切换后旧模型 token 与新模型上下文上限不应混显。
                self._input_tokens = 0
                self._output_tokens = 0
                self._cached_input_tokens = 0
                self._refresh_context_summary()
            self._drain_pending_inputs()
            # 参考设置面板其他选项页面（渠道/工具/MCP/视觉/子代理）：
            # 关闭当前页后重新打开设置面板回到主菜单。
            self._open_settings()

        # 参考设置面板其他选项页面（渠道/工具/MCP/视觉/子代理）：
        # 关闭当前页后重新打开设置面板回到主菜单。
        self.push_screen(
            ModelPickerScreen(
                self.agent,
                refresh_on_open=refresh,
            ),
            receive,
        )


__all__ = ["SettingsNavigationMixin"]
