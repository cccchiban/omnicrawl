from __future__ import annotations

import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch


class FullscreenTUITest(unittest.IsolatedAsyncioTestCase):
    async def test_fullscreen_layout_uses_single_line_hud_and_compact_composer(self) -> None:
        """界面应使用单行 HUD、无侧栏和三行高的紧凑输入舱。"""

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
            self.assertEqual(len(app.query("#sidebar")), 0)
            self.assertEqual(len(app.query("#header")), 0)
            self.assertEqual(len(app.query("#hint")), 0)
            self.assertEqual(app.query_one("#topbar").region.y, 0)
            context = str(app.query_one("#context-summary", Static).content)
            self.assertIn("D:/workspace", context)
            self.assertIn("demo-model", context)
            self.assertNotIn(".agent_tmp", context)
            self.assertEqual(app.query_one("#composer-wrap").region.height, 3)
            self.assertGreater(app.query_one("#composer", Input).region.height, 0)

    async def test_slash_menu_filters_commands_and_completion_does_not_submit(self) -> None:
        """斜杠菜单应包含动态 Skill，最多八项，Enter/Tab 只补全不执行。"""

        from textual.widgets import Input, Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        class FakeSkillManager:
            def list_all(self):
                return [
                    SimpleNamespace(name=f"skill-{index}", description=f"动态技能 {index}")
                    for index in range(10)
                ] + [SimpleNamespace(name="ui-design", description="界面设计")]

        class FakeAgent:
            current_model = "demo-model"
            approval_mode = "manual"
            reasoning_effort = "max"
            workspace_root = "D:/workspace"
            current_session_id = "session-demo"
            skill_manager = FakeSkillManager()

            def set_confirm_handler(self, _handler) -> None:
                pass

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )
        submitted: list[str] = []
        app._submit = submitted.append  # type: ignore[method-assign]

        async with app.run_test(size=(120, 40)) as pilot:
            composer = app.query_one("#composer", Input)
            menu = app.query_one("#command-menu", Static)
            await pilot.press("/")
            await pilot.pause()
            self.assertTrue(menu.display)
            self.assertEqual(len(app._command_matches), 8)
            self.assertIn("/new", str(menu.content))

            composer.value = "/skill:ui"
            await pilot.pause()
            self.assertEqual([item["command"] for item in app._command_matches], ["/skill:ui-design"])
            await pilot.press("enter")
            await pilot.pause()
            self.assertEqual(composer.value, "/skill:ui-design ")
            self.assertEqual(submitted, [])
            self.assertFalse(menu.display)

            composer.value = "/mem"
            await pilot.pause()
            await pilot.press("tab")
            await pilot.pause()
            self.assertEqual(composer.value, "/memory:clean")
            self.assertEqual(submitted, [])

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
            await pilot.pause()
            self.assertTrue(app._cancel_requested.is_set())

            for _ in range(40):
                if not worker.is_alive():
                    break
                await pilot.pause(0.05)
            worker.join(timeout=0.1)

            self.assertFalse(worker.is_alive())
            self.assertEqual(decision, [False])
            self.assertEqual(len(app.screen_stack), 1)

    async def test_runtime_indicator_blinks_only_while_working(self) -> None:
        """等待 AI 时状态点应闪烁，恢复就绪后必须稳定显示。"""

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

        async with app.run_test(size=(100, 32)) as pilot:
            app._set_runtime_status("正在思考", "working")
            app._tick_status_indicator()
            await pilot.pause()
            self.assertEqual(str(app.query_one("#runtime-status", Static).content), "  正在思考")

            app._tick_status_indicator()
            self.assertEqual(str(app.query_one("#runtime-status", Static).content), "● 正在思考")

            app._set_runtime_status("完成", "complete")
            app._tick_status_indicator()
            self.assertEqual(str(app.query_one("#runtime-status", Static).content), "● 完成")

    async def test_mount_preloads_mcp_before_accepting_input(self) -> None:
        """首屏显示后应后台发现 MCP，并在完成前锁定输入。"""

        from textual.widgets import Static

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        preload_started = threading.Event()
        preload_release = threading.Event()

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

            def preload_mcp_tools(self) -> None:
                preload_started.set()
                preload_release.wait(timeout=1)

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(100, 32)) as pilot:
            self.assertTrue(preload_started.wait(timeout=1))
            self.assertTrue(app.is_generating)
            self.assertIn("等待", str(app.query_one("#runtime-status", Static).content))

            preload_release.set()
            await pilot.pause(0.2)
            self.assertFalse(app.is_generating)
            self.assertEqual(str(app.query_one("#runtime-status", Static).content), "● 完成")

    async def test_reasoning_sections_are_separate_collapsed_and_clickable(self) -> None:
        """每次模型推理应独立成段、默认折叠，并且只能通过点击切换正文。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ReasoningDisclosure

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

        async with app.run_test(size=(100, 32)) as pilot:
            app._append_reasoning_delta("先检查配置。")
            first = app.query_one(ReasoningDisclosure)
            self.assertFalse(first.expanded)
            self.assertNotIn("先检查配置", str(first.content))

            await pilot.pause()
            await pilot.click(".reasoning-message")
            await pilot.pause()
            self.assertTrue(first.expanded)
            self.assertEqual(first.reasoning_text, "先检查配置。")

            app._handle_tool_start(1, SimpleNamespace(name="read_file", arguments={}))
            app._append_reasoning_delta("再整理结果。")
            self.assertEqual(len(app.query(ReasoningDisclosure)), 2)
            second = app.query(ReasoningDisclosure)[1]
            self.assertFalse(second.expanded)
            self.assertNotIn("再整理结果", str(second.content))

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
                callbacks["on_reasoning_delta"]("先分析问题。")
                self.seen_statuses = ["正在思考"]
                on_delta(f"回复：{text}")
                self.seen_statuses.append("正在回复")
                tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})
                callbacks["on_protocol_wait"]()
                self.seen_statuses.append("等待")
                callbacks["on_tool_start"](1, tool_call)
                self.seen_statuses.append("正在调用")
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
            collapsed = str(tool_record.content)
            self.assertIn("⌁ read_file · 成功 ·", collapsed)
            self.assertNotIn("步骤", collapsed)
            self.assertNotIn("读取完成", collapsed)
            await pilot.click(".tool-message")
            await pilot.pause()
            expanded = str(tool_record.content)
            self.assertIn("参数：{'path': 'README.md'}", expanded)
            self.assertIn("读取完成", expanded)
            self.assertEqual(app.agent.seen_statuses, ["正在思考", "正在回复", "等待", "正在调用"])
            self.assertEqual(str(app.query_one("#runtime-status").content), "● 完成")

    async def test_stream_records_preserve_model_tool_model_visual_order(self) -> None:
        """工具边界后的推理和最终回复不得写回工具之前的旧回复组件。"""

        from omnicrawl.ui.fullscreen import (
            FullscreenStartup,
            OmniCrawlApp,
            ReasoningDisclosure,
            ToolDisclosure,
        )

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            app._append_reasoning_delta("第一轮思考")
            app._append_delta("调用前说明")
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))
            app._append_reasoning_delta("第二轮思考")
            app._append_delta("最终回复")
            app._render_stream_markdown()
            await pilot.pause()

            records = list(app.query("#conversation > *"))
            self.assertEqual(
                [
                    "reasoning" if isinstance(record, ReasoningDisclosure)
                    else "tool" if isinstance(record, ToolDisclosure)
                    else "assistant"
                    for record in records
                ],
                ["reasoning", "assistant", "tool", "reasoning", "assistant"],
            )
            first_reply = "".join(segment.text for segment in records[1].render_line(0))
            final_reply = "".join(segment.text for segment in records[4].render_line(0))
            self.assertIn("调用前说明", first_reply)
            self.assertNotIn("最终回复", first_reply)
            self.assertIn("最终回复", final_reply)

    async def test_stream_records_preserve_order_across_multiple_tools(self) -> None:
        """连续工具调用时，每个模型 pass 都应追加到上一工具记录之后。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

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
            for index, name in enumerate(("read_file", "search_text"), start=1):
                app._append_reasoning_delta(f"思考 {index}")
                call = SimpleNamespace(name=name, arguments={})
                app._handle_tool_start(index, call)
                app._handle_tool_result(call, SimpleNamespace(ok=True, output="完成"))
            app._append_reasoning_delta("总结思考")
            app._append_delta("最终答案")
            app._render_stream_markdown()
            await pilot.pause()

            records = list(app.query("#conversation > *"))
            self.assertEqual(
                ["tool" if isinstance(record, ToolDisclosure) else "message" for record in records],
                ["message", "tool", "message", "tool", "message", "message"],
            )
            self.assertIn("最终答案", "".join(segment.text for segment in records[-1].render_line(0)))

    async def test_context_summary_refreshes_runtime_values_after_commands(self) -> None:
        """运行时切换模型、审批和推理强度后，顶部上下文条应展示当前实际状态。"""

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
            app._refresh_context_summary()
            await pilot.pause()

            context = str(app.query_one("#context-summary", Static).content)
            self.assertIn("demo-model", context)
            self.assertIn("LOW", context)
            self.assertIn("完全自动批准", context)

    async def test_parallel_tool_results_update_their_matching_disclosures(self) -> None:
        """并发工具即使逆序完成，也必须更新各自的调用记录。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

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
        first = SimpleNamespace(name="read_file", arguments={}, id="call_1")
        second = SimpleNamespace(name="search_text", arguments={}, id="call_2")

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, first)
            app._handle_tool_start(2, second)
            app._handle_tool_result(second, SimpleNamespace(ok=False, output="search-failed"))
            app._handle_tool_result(first, SimpleNamespace(ok=True, output="read-ok"))
            await pilot.pause()

            records = list(app.query(ToolDisclosure))
            self.assertEqual(len(records), 2)
            self.assertIn("read_file · 成功", str(records[0].content))
            self.assertIn("search_text · 失败", str(records[1].content))
            records[0].on_click()
            records[1].on_click()
            self.assertIn("read-ok", str(records[0].content))
            self.assertIn("search-failed", str(records[1].content))

    async def test_tool_disclosure_title_is_visible_in_real_terminal_render(self) -> None:
        """折叠标题不能只存在于 content 属性中，必须实际绘制到终端单元格。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            rendered_text = "".join(segment.text for segment in record.render_line(0))
            self.assertIn("read_file", rendered_text)
            self.assertIn("成功", rendered_text)

            await pilot.click(".tool-message")
            await pilot.pause()
            expanded_text = "".join(
                segment.text
                for line_number in range(record.region.height)
                for segment in record.render_line(line_number)
            )
            self.assertIn("README.md", expanded_text)
            self.assertIn("读取完成", expanded_text)

    async def test_tool_disclosure_stays_collapsed_while_running_and_after_completion(self) -> None:
        """工具执行中和完成后均应默认折叠，标题展示状态及耗时而不展示步骤。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

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
        tool_call = SimpleNamespace(name="read_file", arguments={"path": "README.md"})

        async with app.run_test(size=(120, 40)) as pilot:
            with patch("omnicrawl.ui.fullscreen.time.perf_counter", side_effect=[10.0, 10.126]):
                app._handle_tool_start(7, tool_call)
                record = app.query_one(ToolDisclosure)
                self.assertFalse(record.expanded)
                self.assertEqual(str(record.content), "▸ ⌁ read_file · 调用中 · 0.00s")

                app._handle_tool_result(tool_call, SimpleNamespace(ok=True, output="读取完成"))

            self.assertFalse(record.expanded)
            self.assertEqual(str(record.content), "▸ ⌁ read_file · 成功 · 0.13s")
            self.assertNotIn("步骤", str(record.content))
            self.assertNotIn("读取完成", str(record.content))

            await pilot.pause()
            await pilot.click(".tool-message")
            await pilot.pause()
            self.assertTrue(record.expanded)
            self.assertIn("读取完成", str(record.content))

    async def test_tool_result_truncates_large_output_for_rendering(self) -> None:
        """工具的大输出只能保留预览，且仅在展开调用记录后显示。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp, ToolDisclosure

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
        tool_call = SimpleNamespace(name="powershell", arguments={"command": "Write-Output long"})
        result = SimpleNamespace(ok=True, output="x" * 20_000)

        async with app.run_test(size=(120, 40)) as pilot:
            app._handle_tool_start(1, tool_call)
            app._handle_tool_result(tool_call, result)
            await pilot.pause()

            record = app.query_one(ToolDisclosure)
            self.assertNotIn("界面展示已截断", str(record.content))
            await pilot.click(".tool-message")
            await pilot.pause()
            rendered = str(record.content)
            self.assertIn("界面展示已截断", rendered)
            self.assertLess(len(rendered), 5_000)

    async def test_fullscreen_app_appends_monitor_events_without_starting_agent_turn(self) -> None:
        """后台日志到达时应直接显示，不应触发模型新回合。"""

        from omnicrawl.ui.fullscreen import FullscreenStartup, OmniCrawlApp

        monitor_id = "monitor-demo"

        class FakeAgent:
            current_model = "demo-model"
            current_session_id = "session-demo"
            skill_manager = None

            def set_confirm_handler(self, _handler) -> None:
                pass

            def list_monitor_tasks(self):
                return [SimpleNamespace(monitor_id=monitor_id)]

            def poll_monitor_events(self, _monitor_id: str, *, cursor: int, max_events: int):
                del max_events
                events = () if cursor else (
                    SimpleNamespace(stream="stdout", text="monitor-ready", sequence=1),
                )
                return SimpleNamespace(
                    snapshot=SimpleNamespace(status="running"),
                    events=events,
                    next_cursor=1 if events else cursor,
                )

        app = OmniCrawlApp(
            FakeAgent(),
            FullscreenStartup(True, "max", "人工确认", "D:/workspace", ".agent_tmp"),
        )

        async with app.run_test(size=(120, 40)) as pilot:
            app._refresh_monitor_events()
            await pilot.pause()

            self.assertIn("monitor-ready", app.conversation_text)
            self.assertFalse(app.is_generating)

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
