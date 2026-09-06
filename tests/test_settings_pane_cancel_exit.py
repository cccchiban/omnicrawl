"""设置页二级表单 pane 的“取消”按钮语义测试。

需求：新版三区 /settings 中，进入右侧二级设置表单（TTS / 持续运转 /
隔离工作区 / 图像生成）后，点底部“取消”按钮应直接关闭整个设置面板、
回到主聊天界面，而不是只回到左侧设置项菜单；Esc 仍只“返回左侧”（请求
返回上一级），语义保持不变。
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from textual.app import App, ComposeResult
from textual.widgets import Static

from omnicrawl.ui.fullscreen.screens.agent_workspace_settings import (
    AgentWorkspaceSettingsPane,
)
from omnicrawl.ui.fullscreen.screens.image_gen_settings import ImageGenSettingsPane
from omnicrawl.ui.fullscreen.screens.panes import SettingsPane
from omnicrawl.ui.fullscreen.screens.run_guard_settings import RunGuardSettingsPane
from omnicrawl.ui.fullscreen.screens.tts_settings import TTSSettingsPane


def _agent() -> SimpleNamespace:
    """挂载 SettingsScreen 所需的较完整 fake agent（不触发保存）。"""
    return SimpleNamespace(
        current_model="demo",
        approval_mode="manual",
        reasoning_effort="none",
        context_window_tokens=128_000,
        workspace_root="D:/workspace",
        current_session_id="s",
        skill_manager=None,
        _memory_store=None,
        _mcp_manager=SimpleNamespace(enabled=False),
        _plugin_manager=SimpleNamespace(enabled=False),
        config=SimpleNamespace(
            context_compaction=SimpleNamespace(trigger_context_tokens=102_400),
            subagents=SimpleNamespace(enabled=False),
            show_thinking=True,
            llm=SimpleNamespace(model_source="legacy", catalog_key=""),
            vision=SimpleNamespace(enabled=False),
            image_gen=SimpleNamespace(enabled=False),
            disabled_tools=frozenset(),
        ),
        _tools={},
    )


def _empty_config(path: Path) -> None:
    path.write_text("", encoding="utf-8")


class _PaneHost(App):
    """把单个 Pane 挂到宿主，观察其回调请求。"""

    def __init__(self, pane: SettingsPane) -> None:
        super().__init__()
        self.pane = pane

    def compose(self) -> ComposeResult:
        yield self.pane


class _HostApp(App):
    def compose(self) -> ComposeResult:
        yield Static("probe")


class FormPaneCancelButtonTests(unittest.IsolatedAsyncioTestCase):
    """四个带“取消/保存”按钮的表单 pane：取消按钮 → request_exit。"""

    def _pane_factories(self, path: Path) -> dict[str, SettingsPane]:
        return {
            "tts": TTSSettingsPane(path),
            "run_guard": RunGuardSettingsPane(path),
            "agent_workspace": AgentWorkspaceSettingsPane(path),
            "image_gen": ImageGenSettingsPane(path),
        }

    @staticmethod
    def _idle(pane: SettingsPane) -> None:
        """重置 TTS 面板后台任务标志，保证路由测试不受 worker 干扰。"""
        for name in ("_downloading", "_gpu_busy", "_clone_busy"):
            if hasattr(pane, name):
                setattr(pane, name, False)

    async def test_cancel_button_requests_exit_not_back(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            _empty_config(path)
            for name, pane in self._pane_factories(path).items():
                with self.subTest(pane=name):
                    exits: list[bool] = []
                    backs: list[bool] = []
                    pane.bind_pane_events(
                        on_back=lambda: backs.append(True),
                        on_exit=lambda: exits.append(True),
                    )
                    # 直接调用按钮处理器路径：取消按钮触发 action_exit。
                    self._idle(pane)
                    pane.action_exit()
                    self.assertEqual(exits, [True], f"{name}: 取消按钮应请求退出设置面板")
                    self.assertEqual(backs, [], f"{name}: 取消按钮不应请求返回左侧")

    async def test_escape_still_requests_back(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            _empty_config(path)
            for name, pane in self._pane_factories(path).items():
                with self.subTest(pane=name):
                    exits: list[bool] = []
                    backs: list[bool] = []
                    pane.bind_pane_events(
                        on_back=lambda: backs.append(True),
                        on_exit=lambda: exits.append(True),
                    )
                    # Esc 仍绑定 action_cancel（返回上一级）。
                    self._idle(pane)
                    pane.action_cancel()
                    self.assertEqual(backs, [True], f"{name}: Esc 应请求返回左侧")
                    self.assertEqual(exits, [], f"{name}: Esc 不应请求退出设置面板")

    async def test_busy_blocks_exit_for_tts(self) -> None:
        """TTS 后台任务进行中，取消按钮不得关闭设置页（防 worker 崩溃）。"""
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            _empty_config(path)
            pane = TTSSettingsPane(path)
            exits: list[bool] = []
            backs: list[bool] = []
            pane.bind_pane_events(
                on_back=lambda: backs.append(True),
                on_exit=lambda: exits.append(True),
            )
            pane._gpu_busy = True
            pane.action_exit()
            self.assertEqual(exits, [], "忙碌中取消按钮应被拦截")
            self.assertEqual(backs, [], "忙碌中取消按钮不应触发返回")

    async def test_button_id_routes_to_exit(self) -> None:
        """按钮 id（on_button_pressed 路由）应命中取消 → action_exit。"""
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "config.toml"
            _empty_config(path)
            from textual.widgets import Button

            cases = {
                "tts": (TTSSettingsPane(path), "tts-cancel"),
                "run_guard": (RunGuardSettingsPane(path), "run-guard-cancel"),
                "agent_workspace": (
                    AgentWorkspaceSettingsPane(path),
                    "agent-workspace-cancel",
                ),
                "image_gen": (ImageGenSettingsPane(path), "image-gen-cancel"),
            }
            for name, (pane, button_id) in cases.items():
                with self.subTest(pane=name):
                    exits: list[bool] = []
                    backs: list[bool] = []
                    pane.bind_pane_events(
                        on_back=lambda: backs.append(True),
                        on_exit=lambda: exits.append(True),
                    )
                    app = _PaneHost(pane)
                    async with app.run_test(size=(110, 50)) as pilot:
                        await pilot.pause()
                        self._idle(pane)
                        button = pane.query_one(f"#{button_id}", Button)
                        # 直接派发按钮事件（与真实点击后冒泡到 pane 等效）。
                        pane.on_button_pressed(Button.Pressed(button))
                        await pilot.pause()
                        self.assertEqual(exits, [True], f"{name}: 取消按钮应触发 request_exit")
                        self.assertEqual(backs, [], f"{name}: 取消按钮不应 request_back")


class SettingsScreenPaneExitTests(unittest.IsolatedAsyncioTestCase):
    """三区 /settings 宿主：二级表单“取消”请求直接关闭整个设置面板。"""

    async def test_host_exit_request_dismisses_settings_screen(self) -> None:
        from omnicrawl.ui.fullscreen.screens.settings import SettingsScreen

        actions: list[object] = []
        app = _HostApp()
        screen = SettingsScreen(_agent(), advanced=False)
        async with app.run_test(size=(110, 45)) as pilot:
            app.push_screen(screen, actions.append)
            await pilot.pause()
            await pilot.pause()
            # 宿主把 pane 的 on_exit 桥接为该方法（取消按钮请求退出）。
            screen._exit_settings_pane()
            await pilot.pause()
        self.assertEqual(actions, [None], "取消按钮应直接关闭整个设置面板")

    async def test_mounted_tts_pane_cancel_dismisses_settings_screen(self) -> None:
        """三区真实挂载 TTS pane 后，点取消直接关闭整个设置面板。"""
        import omnicrawl.ui.fullscreen.screens.settings as settings_module

        from omnicrawl.ui.fullscreen.screens.settings import SettingsScreen

        actions: list[object] = []
        app = _HostApp()
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            _empty_config(config_path)
            original_resolve = settings_module.resolve_config_path
            settings_module.resolve_config_path = lambda: config_path
            try:
                screen = SettingsScreen(_agent(), advanced=False)
                async with app.run_test(size=(110, 50)) as pilot:
                    app.push_screen(screen, actions.append)
                    await pilot.pause()
                    await pilot.pause()
                    screen._focus_row("tts")
                    await pilot.pause()
                    await pilot.pause()
                    pane = screen._pane
                    self.assertIsNotNone(pane)
                    self.assertEqual(type(pane).__name__, "TTSSettingsPane")
                    # 取消按钮 → action_exit → 宿主 dismiss(None)。
                    pane._downloading = False
                    pane._gpu_busy = False
                    pane._clone_busy = False
                    pane.action_exit()
                    await pilot.pause()
                    await pilot.pause()
            finally:
                settings_module.resolve_config_path = original_resolve
        self.assertEqual(actions, [None], "TTS 取消按钮应关闭整个设置面板")


if __name__ == "__main__":
    unittest.main()
