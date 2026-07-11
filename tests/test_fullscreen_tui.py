from __future__ import annotations

import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch


class FullscreenTUITest(unittest.IsolatedAsyncioTestCase):
    async def test_fullscreen_layout_reserves_visible_composer_and_hides_temp_cleanup_note(self) -> None:
        """输入区必须完整可见，侧栏不应暴露临时目录清理策略。"""

        from textual.widgets import Input, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp，每 24 小时自动清理"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            self.assertEqual(len(app.query("#sidebar .temp-summary")), 0)
            self.assertEqual(app.query_one("#topbar").region.y, 1)
            self.assertGreater(app.query_one("#composer-wrap").region.height, 5)
            self.assertGreater(app.query_one("#composer", Input).region.height, 0)
            self.assertGreater(app.query_one("#hint", Static).region.height, 0)

    async def test_confirm_tool_returns_when_cancelled_while_waiting_for_approval(self) -> None:
        """审批模态框等待期间取消必须解除后台线程，不能永久停在确认页。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        decision: list[bool] = []

        async with app.run_test(size=(120, 40)) as pilot:
            worker = threading.Thread(
                target=lambda: decision.append(app._confirm_tool("read_file", {"path": "README.md"})),
                daemon=True,
            )
            worker.start()
            await pilot.pause()

            self.assertTrue(app.screen_stack[-1].is_modal)
            app.is_generating = True
            await pilot.press("ctrl+c")
            self.assertTrue(app._cancel_requested.is_set())

            for _ in range(40):
                if not worker.is_alive():
                    break
                await pilot.pause(0.05)
            worker.join(timeout=0.1)

            self.assertFalse(worker.is_alive())
            self.assertEqual(decision, [False])
            self.assertEqual(len(app.screen_stack), 1)

    async def test_confirmation_screen_returns_explicit_approval(self) -> None:
        """人工审批必须通过全屏模态框返回明确结果。"""

        from omnicrawl.ui.fullscreen import ConfirmationScreen, FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        approved: list[bool | None] = []

        async with app.run_test(size=(120, 40)) as pilot:
            app.push_screen(ConfirmationScreen("允许读取 README.md？"), approved.append)
            await pilot.pause()
            await pilot.click("#approve")
            await pilot.pause()

        self.assertEqual(approved, [True])

    async def test_fullscreen_app_renders_stream_events_with_merged_tool_record(self) -> None:
        """全屏界面必须将同一工具的开始与结果聚合为一条可扫描记录。"""

        from textual.widgets import Input, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = SimpleNamespace(count=2)

            def set_confirm_handler(self, _handler) -> None:
                pass

            def run_stream(self, text, on_delta, **callbacks) -> str:
                callbacks["on_status"]("正在分析请求")
                on_delta(f"回复：{text}")
                tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})
                callbacks["on_tool_start"](1, tool_call)
                callbacks["on_tool_result"](
                    tool_call,
                    SimpleNamespace(ok=True, output="读取完成"),
                )
                callbacks["on_token_usage"](12, 8, 3)
                return f"回复：{text}"

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(
                thinking_enabled=True,
                reasoning_effort="max",
                approval_label="人工确认",
                workspace_label="D:/workspace",
                temp_label=".agent_tmp",
            ),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", Input)
            composer.value = "你好"
            await pilot.press("enter")
            await pilot.pause(0.8)

            self.assertFalse(app.is_generating)
            self.assertIn("回复：你好", app.conversation_text)
            self.assertIn("read_file", app.conversation_text)
            self.assertEqual(len(app.query(".tool-message")), 1)
            tool_record = app.query_one(".tool-message", Static)
            self.assertIn("步骤 1 · read_file", str(tool_record.content))
            self.assertIn("结果  成功", str(tool_record.content))
            self.assertIn("读取完成", str(tool_record.content))
            self.assertEqual(str(app.query_one("#runtime-status").content), "就绪")

    async def test_sidebar_refreshes_runtime_values_after_commands(self) -> None:
        """运行时切换审批和推理强度后，侧栏必须展示当前实际状态。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "auto"
            reasoning_effort = "low"
            workspace_root = "D:/workspace"
            current_session_id = "session-updated"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._refresh_sidebar()
            await pilot.pause()

            self.assertEqual(str(app.query_one("#approval-summary", Static).content), "审批  完全自动批准")
            self.assertEqual(str(app.query_one("#reasoning-summary", Static).content), "深度  low")
            self.assertEqual(str(app.query_one("#session-summary", Static).content), "会话  session-updated")

    async def test_tool_result_truncates_large_output_for_rendering(self) -> None:
        """工具的大输出只能保留预览，不能直接灌入全屏布局。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        tool_call = SimpleNamespace(name="run_command", arguments={"command": "echo long"})
        result = SimpleNamespace(ok=True, output="x" * 20_000)

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, result)
            await pilot.pause()

            rendered = str(app.query_one(".tool-message", Static).content)
            self.assertIn("界面展示已截断", rendered)
            self.assertLess(len(rendered), 5_000)

    async def test_streaming_markdown_batches_rapid_deltas(self) -> None:
        """连续分片到达时，不应逐片重建完整 Markdown。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            with patch.object(app, "_render_stream_markdown", wraps=app._render_stream_markdown) as render:
                for _ in range(20):
                    app._append_delta("x")
                self.assertLessEqual(render.call_count, 1)
                await pilot.pause(0.2)
                self.assertEqual(render.call_count, 1)

    async def test_model_command_runs_outside_event_loop(self) -> None:
        """模型列表检测较慢时，事件循环仍必须能够继续处理界面事件。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        def delayed_model_command(_agent, _text: str) -> str:
            time.sleep(0.25)
            return "模型列表已加载"

        async with app.run_test(size=(120, 40)) as pilot:
            with patch("omnicrawl.ui.fullscreen.handle_model_command", side_effect=delayed_model_command):
                started = time.monotonic()
                self.assertTrue(app._handle_command("/model"))
                elapsed = time.monotonic() - started
                self.assertLess(elapsed, 0.1)
                await pilot.pause(0.35)

            self.assertIn("模型列表已加载", app.conversation_text)
