from __future__ import annotations

import json
import threading
import unittest

from omnicrawl.connectors.fsapp import (
    FeishuBot,
    FeishuConfig,
    _ActiveTask,
    _ToolCallCard,
)


class _FakeAgent:
    def set_confirm_handler(self, handler) -> None:
        pass

    def set_ask_user_handler(self, handler) -> None:
        pass

    def run_stream(self, text, on_delta, **callbacks) -> str:
        from types import SimpleNamespace

        call = SimpleNamespace(name="bash", arguments={"cmd": "echo hi"}, id="tool-1")
        callbacks["on_tool_start"](1, call)
        callbacks["on_tool_result"](call, SimpleNamespace(ok=True, output="hi\n"))
        call = SimpleNamespace(name="read_file", arguments={"path": "a.txt"}, id="tool-2")
        callbacks["on_tool_start"](2, call)
        callbacks["on_tool_result"](
            call, SimpleNamespace(ok=False, output="file not found")
        )
        on_delta("最终回答")
        return "最终回答"


class ToolCallCardTests(unittest.TestCase):
    def _bot(self) -> FeishuBot:
        return FeishuBot(
            FeishuConfig(
                app_id="cli_test",
                app_secret="secret",
                allowed_user_ids=frozenset({"ou-user"}),
                confirmation_timeout_seconds=1,
            ),
            agent=_FakeAgent(),
        )

    def _card(self, bot: FeishuBot) -> _ToolCallCard:
        return _ToolCallCard(bot, "chat-1", "chat_id")

    def test_start_sends_interactive_card_with_tool_name(self) -> None:
        bot = self._bot()
        sent: list[tuple[str, str, str]] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append((receive_id, payload, msg_type)) or "message-1"
        )
        card = self._card(bot)
        self.assertTrue(card.start("read_file"))
        self.assertEqual(card.name, "read_file")
        self.assertEqual(card.message_id, "message-1")
        self.assertEqual(sent[0][0], "chat-1")
        self.assertEqual(sent[0][2], "interactive")
        payload = json.loads(sent[0][1])
        self.assertIn("read_file", sent[0][1])
        self.assertIn("执行中", sent[0][1])
        self.assertEqual(payload["schema"], "2.0")

    def test_finish_patches_same_card_with_success_and_no_output(self) -> None:
        bot = self._bot()
        sent: list[tuple[str, str, str]] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append((receive_id, payload, msg_type)) or "message-1"
        )
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        card = self._card(bot)
        card.start("bash")
        # 成功：只显示状态，不包含工具输出内容
        self.assertTrue(card.finish(ok=True))
        self.assertEqual(len(patched), 1)
        self.assertIn("✅", patched[0])
        self.assertIn("bash", patched[0])
        self.assertNotIn("stdout", patched[0])
        self.assertNotIn("output", patched[0])

    def test_finish_failed_shows_failure_status(self) -> None:
        bot = self._bot()
        bot._send_raw = lambda *_args, **_kwargs: "message-1"
        patched: list[str] = []
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        card = self._card(bot)
        card.start("bash")
        self.assertTrue(card.finish(ok=False))
        self.assertEqual(len(patched), 1)
        self.assertIn("❌", patched[0])
        self.assertIn("bash", patched[0])

    def test_finish_without_start_does_nothing(self) -> None:
        bot = self._bot()
        patched: list[str] = []
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        card = self._card(bot)
        self.assertFalse(card.finish(ok=True))
        self.assertEqual(patched, [])

    def test_abort_marks_interrupted(self) -> None:
        bot = self._bot()
        bot._send_raw = lambda *_args, **_kwargs: "message-1"
        patched: list[str] = []
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        card = self._card(bot)
        card.start("bash")
        self.assertTrue(card.abort())
        self.assertIn("⏹", patched[0])
        self.assertIn("bash", patched[0])


class ExecuteTaskToolCardsTests(unittest.TestCase):
    def _bot(self) -> FeishuBot:
        return FeishuBot(
            FeishuConfig(
                app_id="cli_test",
                app_secret="secret",
                allowed_user_ids=frozenset({"ou-user"}),
                confirmation_timeout_seconds=1,
            ),
            agent=_FakeAgent(),
        )

    def test_tool_calls_become_independent_cards_not_task_steps(self) -> None:
        bot = self._bot()
        sent: list[str] = []  # 初始卡片 + 两个独立工具卡片
        patched: list[str] = []  # 任务卡片 patch（最终回答）+ 两个工具卡片完成 patch
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append(payload) or f"message-{len(sent)}"
        )
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        bot._send_generated_files = lambda *_args, **_kwargs: None
        task = _ActiveTask(
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
            text="执行任务",
        )
        bot._active_task = task
        bot._execute_task(task)

        # 任务卡片只保留最终回答：done patch 里没有工具步骤折叠面板
        # patched 顺序：工具卡片1完成 → 工具卡片2完成 → 任务卡片 done
        done_payload = patched[-1]
        self.assertIn("✅ 已完成", done_payload)
        self.assertIn("最终回答", done_payload)
        self.assertNotIn("collapsible_panel", done_payload)

        # 两个工具调用各有一张独立卡片：开始(执行中) + 完成(✅/❌)
        tool_starts = [p for p in sent if "执行中" in p]
        self.assertEqual(len(tool_starts), 2)
        # 工具名已汉化展示（读取文件/执行命令）
        self.assertTrue(any("读取文件" in p for p in tool_starts))
        self.assertTrue(any("执行命令" in p for p in tool_starts))
        # 输出内容不出现在任何卡片里
        all_payloads = "".join(sent + patched)
        self.assertNotIn("echo hi", all_payloads)
        self.assertNotIn("file not found", all_payloads)
        # 状态卡片：成功 ✅ 与失败 ❌ 各一张
        self.assertTrue(any("✅ 工具调用完成" in p for p in patched))
        self.assertTrue(any("❌ 工具调用失败" in p for p in patched))

    def test_abort_residual_tool_cards_on_cancel(self) -> None:
        from omnicrawl.connectors.fsapp import FeishuTaskCancelled

        bot = self._bot()
        sent: list[str] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append(payload) or f"message-{len(sent)}"
        )
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        bot._send_generated_files = lambda *_args, **_kwargs: None

        class _CancellingAgent(_FakeAgent):
            def run_stream(self, text, on_delta, **callbacks) -> str:
                from types import SimpleNamespace

                call = SimpleNamespace(name="bash", arguments={}, id="tool-1")
                callbacks["on_tool_start"](1, call)
                raise FeishuTaskCancelled("任务已被取消。")

        task = _ActiveTask(
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
            text="任务",
        )
        bot._active_task = task
        agent = _CancellingAgent()
        bot._agent = agent
        bot._agent_confirm_bound = True
        bot._agent_ask_user_bound = True
        bot._execute_task(task)
        # 未完成的工具卡片在 finally 中被置为“已中断”
        self.assertTrue(any("已中断" in p for p in patched))
        self.assertTrue(any("执行命令" in p for p in patched))


class SplitAndDisplayTextTests(unittest.TestCase):
    """B 方向：长文本分段与展示清理。"""

    def test_short_text_single_part(self) -> None:
        from omnicrawl.connectors.fsapp import _split_text

        self.assertEqual(_split_text("你好"), ["你好"])
        self.assertEqual(_split_text(""), [])
        # 纯空白文本保留原行为：不强制清空（避免误删有意义内容）
        self.assertEqual(_split_text("   "), ["   "])

    def test_long_text_split_at_boundaries(self) -> None:
        from omnicrawl.connectors.fsapp import _split_text

        # 超过 3000 字，且中间有列表项边界 → 应拆成多段
        long = ("段落A" * 700) + "\n\n- 列表项一\n- 列表项二\n\n" + ("段落B" * 700)
        parts = _split_text(long)
        self.assertGreater(len(parts), 1)
        # 每段都不超过上限
        self.assertTrue(all(len(p) <= 3000 for p in parts))
        # 不丢内容
        self.assertEqual("".join(parts).replace("\n\n", ""), long.replace("\n\n", ""))

    def test_display_text_collapses_blank_lines(self) -> None:
        from omnicrawl.connectors.fsapp import _display_text

        result = _display_text("第一行\n\n\n\n第二行  \n第三行  ")
        self.assertEqual(result, "第一行\n\n第二行\n第三行")
        # 空文本兜底
        self.assertEqual(_display_text(""), "（任务完成，无文本输出）")

    def test_display_text_strips_internal_tags(self) -> None:
        from omnicrawl.connectors.fsapp import _display_text

        result = _display_text("<thinking>内部思考</thinking>正文内容")
        self.assertEqual(result, "正文内容")

    def test_tool_name_humanized(self) -> None:
        from omnicrawl.connectors.fsapp import _TOOL_NAME_LABELS

        self.assertEqual(_TOOL_NAME_LABELS.get("read_file", "read_file"), "读取文件")
        self.assertEqual(_TOOL_NAME_LABELS.get("bash", "bash"), "执行命令")
        # 未知工具回退原名
        self.assertEqual(_TOOL_NAME_LABELS.get("unknown_tool", "unknown_tool"), "unknown_tool")


class StreamingCardTests(unittest.TestCase):
    """A 方向：任务卡片流式预览。"""

    def _bot(self) -> FeishuBot:
        return FeishuBot(
            FeishuConfig(
                app_id="cli_test",
                app_secret="secret",
                allowed_user_ids=frozenset({"ou-user"}),
                confirmation_timeout_seconds=1,
            ),
            agent=_FakeAgent(),
        )

    def test_set_stream_adds_generating_section(self) -> None:
        from omnicrawl.connectors.fsapp import _TaskCard

        bot = self._bot()
        sent: list[tuple[str, str, str]] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append((receive_id, payload, msg_type)) or "message-1"
        )
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        card = _TaskCard(bot, "chat-1", "chat_id")
        self.assertTrue(card.start())
        self.assertTrue(card.set_stream("正在写答案..."))
        # 流式节拍 patch 一次，包含“正在生成”节
        self.assertEqual(len(patched), 1)
        self.assertIn("⏳ 正在生成", patched[0])
        self.assertIn("正在写答案", patched[0])
        # 定型后不再展示流式节
        card.done("最终答案")
        self.assertEqual(len(patched), 2)
        self.assertNotIn("正在生成", patched[1])
        self.assertIn("最终答案", patched[1])

    def test_stream_throttled_in_execute_task(self) -> None:
        from omnicrawl.connectors.fsapp import _TaskCard

        bot = self._bot()
        sent: list[str] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append(payload) or f"message-{len(sent)}"
        )
        bot._patch_card = lambda message_id, payload: patched.append(payload) or True
        bot._send_generated_files = lambda *_args, **_kwargs: None

        class _StreamingAgent(_FakeAgent):
            def run_stream(self, text, on_delta, **callbacks) -> str:
                # 多次增量；节流阈值 1.5s，模拟时间流逝后应 patch
                import time as _t

                for i in range(3):
                    on_delta(f"增量{i}")
                    _t.sleep(1.6)
                return "最终回答"

        bot._agent = _StreamingAgent()
        bot._agent_confirm_bound = True
        bot._agent_ask_user_bound = True
        task = _ActiveTask(
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
            text="任务",
        )
        bot._active_task = task
        bot._execute_task(task)
        # 每个节流窗口 patch 一次：3 次增量 → 至少 2 次流式 patch + 最终 done
        stream_patches = [p for p in patched if "⏳ 正在生成" in p]
        self.assertGreaterEqual(len(stream_patches), 2)
        # 最终卡片不含流式节
        self.assertTrue(any("✅ 已完成" in p and "最终回答" in p for p in patched))


if __name__ == "__main__":
    unittest.main()
