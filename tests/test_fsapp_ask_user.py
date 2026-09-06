from __future__ import annotations

import json
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from omnicrawl.agent import AskUserRequest
from omnicrawl.connectors.fsapp import (
    FeishuBot,
    FeishuConfig,
    _PendingUserQuestion,
    _question_card_json,
)


class _FakeAgent:
    def __init__(self) -> None:
        self.confirm_handler = None
        self.ask_user_handler = None

    def set_confirm_handler(self, handler) -> None:
        self.confirm_handler = handler

    def set_ask_user_handler(self, handler) -> None:
        self.ask_user_handler = handler


class FeishuAskUserTests(unittest.TestCase):
    def _bot(self) -> tuple[FeishuBot, _FakeAgent]:
        agent = _FakeAgent()
        bot = FeishuBot(
            FeishuConfig(
                app_id="cli_test",
                app_secret="secret",
                allowed_user_ids=frozenset({"ou-user"}),
                confirmation_timeout_seconds=1,
            ),
            agent=agent,
        )
        bot._ensure_agent()
        bot._active_task = SimpleNamespace(
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
            cancel_event=threading.Event(),
        )
        return bot, agent

    def test_ensure_agent_binds_ask_user_handler(self) -> None:
        bot, agent = self._bot()
        self.assertEqual(agent.ask_user_handler.__self__, bot)
        self.assertEqual(agent.ask_user_handler.__func__, bot._ask_user.__func__)
        self.assertEqual(agent.confirm_handler.__self__, bot)
        self.assertEqual(agent.confirm_handler.__func__, bot._confirm_tool_call.__func__)

    def test_select_question_sends_card_and_accepts_authorized_action(self) -> None:
        bot, _agent = self._bot()
        sent: list[tuple[str, str, str]] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append((receive_id, payload, msg_type)) or "message-1"
        )
        result: list[str | None] = []
        worker = threading.Thread(
            target=lambda: result.append(
                bot._ask_user(
                    AskUserRequest(
                        question="选择方案",
                        kind="select",
                        options=("方案 A", "方案 B"),
                        request_id="question-1",
                    )
                )
            )
        )
        worker.start()
        for _ in range(100):
            if bot._pending_user_question is not None:
                break
            time.sleep(0.01)
        question = bot._pending_user_question
        self.assertIsNotNone(question)
        self.assertEqual(sent[0][0], "chat-1")
        self.assertEqual(sent[0][2], "interactive")
        card = json.loads(sent[0][1])
        self.assertIn("方案 A", sent[0][1])
        self.assertEqual(card["schema"], "2.0")

        handled = bot._answer_user_question_action(
            {
                "action": {
                    "value": {
                        "type": "ask_user",
                        "question_id": question.question_id,
                        "answer": "方案 B",
                    }
                },
                "operator": {"open_id": "ou-user"},
            }
        )
        worker.join(timeout=1)
        self.assertIsInstance(handled, dict)
        self.assertEqual(handled["toast"]["content"], "✅ 已收到回答。")
        self.assertEqual(result, ["方案 B"])

    def test_question_text_is_routed_without_starting_new_task(self) -> None:
        bot, _agent = self._bot()
        sent: list[str] = []
        bot._send_text = lambda _receive_id, text, *, receive_id_type: sent.append(text) or True
        result: list[str | None] = []
        worker = threading.Thread(
            target=lambda: result.append(
                bot._ask_user(AskUserRequest(question="补充信息", kind="question"))
            )
        )
        worker.start()
        for _ in range(100):
            if bot._pending_user_question is not None:
                break
            time.sleep(0.01)
        handled = bot._answer_pending_user_question(
            "chat-1", "chat_id", "ou-user", "补充内容", message_type="text"
        )
        worker.join(timeout=1)
        self.assertTrue(handled)
        self.assertEqual(result, ["补充内容"])
        self.assertTrue(any("已收到回答" in item for item in sent))

    def test_text_question_send_failure_aborts_wait(self) -> None:
        bot, _agent = self._bot()
        bot._send_text = lambda *_args, **_kwargs: False
        result = bot._ask_user(AskUserRequest(question="补充信息", kind="question"))
        self.assertIsNone(result)
        self.assertIsNone(bot._pending_user_question)

    def test_card_rejects_wrong_user_and_duplicate_answer(self) -> None:
        bot, _agent = self._bot()
        question = _PendingUserQuestion(
            question_id="question-1",
            kind="select",
            question="选择",
            options=("A", "B"),
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
        )
        bot._pending_user_question = question
        payload = {
            "action": {
                "value": {
                    "type": "ask_user",
                    "question_id": "question-1",
                    "answer": "A",
                }
            },
            "operator": {"open_id": "ou-other"},
        }
        with patch.object(bot, "_send_text"):
            self.assertTrue(bot._answer_user_question_action(payload))
        self.assertFalse(question.event.is_set())
        payload["operator"]["open_id"] = "ou-user"
        with patch.object(bot, "_send_text"):
            ack1 = bot._answer_user_question_action(payload)
            ack2 = bot._answer_user_question_action(payload)
        self.assertEqual(question.answer, "A")
        self.assertEqual(ack1["toast"]["type"], "success")
        self.assertEqual(ack2["toast"]["content"], "该问题已处理或已失效。")

    def test_question_card_contains_only_structured_button_values(self) -> None:
        question = _PendingUserQuestion(
            question_id="question-1",
            kind="select",
            question="选择",
            options=("A", "B"),
            receive_id="chat-1",
            receive_id_type="chat_id",
            sender_open_id="ou-user",
        )
        payload = json.loads(_question_card_json(question))
        values = [
            element["value"]
            for element in payload["body"]["elements"]
            if element.get("tag") == "button"
        ]
        self.assertEqual(values, [
            {"type": "ask_user", "question_id": "question-1", "answer": "A"},
            {"type": "ask_user", "question_id": "question-1", "answer": "B"},
        ])

    def test_question_card_is_settled_after_timeout(self) -> None:
        bot, _agent = self._bot()
        sent: list[str] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append(payload) or "message-1"
        )
        bot._patch_card = lambda message_id, payload: (
            patched.append((message_id, payload)) or True
        )
        result: list[str | None] = []
        worker = threading.Thread(
            target=lambda: result.append(
                bot._ask_user(
                    AskUserRequest(
                        question="选择方案", kind="select",
                        options=("方案 A", "方案 B"),
                    )
                )
            )
        )
        worker.start()
        for _ in range(100):
            if bot._pending_user_question is not None:
                break
            time.sleep(0.01)
        question = bot._pending_user_question
        self.assertIsNotNone(question)
        self.assertEqual(question.message_id, "message-1")
        worker.join(timeout=3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(result, [None])
        self.assertEqual([mid for mid, _ in patched], ["message-1"])
        settled = json.loads(patched[0][1])
        self.assertEqual(settled["schema"], "2.0")
        self.assertNotIn("方案 A", patched[0][1])
        self.assertIn("问题超时", patched[0][1])

    def test_question_card_is_settled_on_task_cancel(self) -> None:
        bot, _agent = self._bot()
        sent: list[str] = []
        patched: list[str] = []
        bot._send_raw = lambda receive_id, payload, *, msg_type, receive_id_type: (
            sent.append(payload) or "message-1"
        )
        bot._patch_card = lambda message_id, payload: (
            patched.append((message_id, payload)) or True
        )
        result: list[str | None] = []
        worker = threading.Thread(
            target=lambda: result.append(
                bot._ask_user(
                    AskUserRequest(
                        question="选择方案", kind="select",
                        options=("方案 A", "方案 B"),
                    )
                )
            )
        )
        worker.start()
        for _ in range(100):
            if bot._pending_user_question is not None:
                break
            time.sleep(0.01)
        question = bot._pending_user_question
        self.assertIsNotNone(question)
        hold = threading.Event()
        task_thread = threading.Thread(target=hold.wait, daemon=True)
        task_thread.start()
        bot._active_task.thread = task_thread
        bot._request_cancel("chat-1", "chat_id")
        worker.join(timeout=3)
        hold.set()
        self.assertFalse(worker.is_alive())
        self.assertEqual(result, [None])
        self.assertEqual([mid for mid, _ in patched], ["message-1"])
        settled = json.loads(patched[0][1])
        self.assertNotIn("方案 A", patched[0][1])
        self.assertIn("已取消", patched[0][1])


if __name__ == "__main__":
    unittest.main()
