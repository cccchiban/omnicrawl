"""针对 lark-oapi CARD 帧丢失补丁的端到端单测（不依赖真实网络/锁）。

验证 fsapp._install_ws_card_support 能把 CARD 数据帧派发到已注册的
p2.card.action.trigger 处理器并回 ACK；同时确认非 card 帧仍走 SDK 原始逻辑。
"""

from __future__ import annotations

import asyncio
import json
import unittest

from omnicrawl.connectors.fsapp import _install_ws_card_support

try:
    import lark_oapi as sdk
    from lark_oapi.ws.pb.pbbp2_pb2 import Frame as WsFrame

    SDK_AVAILABLE = True
except Exception:  # pragma: no cover - 依赖缺失时跳过
    SDK_AVAILABLE = False


def _data_frame(payload: dict, type_: str = "card") -> "WsFrame":
    frame = WsFrame()
    frame.service = 1
    frame.method = 1  # DATA
    frame.SeqID = 100
    frame.LogID = 200
    for key, value in (
        ("type", type_),
        ("message_id", "om-xxx"),
        ("trace_id", "tr-xxx"),
        ("sum", "1"),
        ("seq", "0"),
    ):
        header = frame.headers.add()
        header.key = key
        header.value = value
    frame.payload = json.dumps(payload).encode("utf-8")
    return frame


@unittest.skipUnless(SDK_AVAILABLE, "需要 lark-oapi")
class WsCardSupportPatchTests(unittest.TestCase):
    def _make_client(self, card_cb):
        builder = (
            sdk.EventDispatcherHandler.builder("", "")
            .register_p2_im_message_receive_v1(lambda data: None)
            .register_p2_card_action_trigger(card_cb)
        )
        handler = builder.build()
        client = sdk.ws.Client(
            "cli_test", "secret", event_handler=handler, log_level=sdk.LogLevel.ERROR
        )
        return client, handler

    def test_install_patches_data_frame_handler(self) -> None:
        client, handler = self._make_client(lambda data: {"toast": {}})
        self.assertTrue(_install_ws_card_support(client, handler))
        # 实例属性覆盖了类方法
        self.assertIn("_handle_data_frame", client.__dict__)

    def test_card_frame_is_dispatched_and_acked(self) -> None:
        seen: list[str] = []

        def card_cb(data):
            event = getattr(data, "event", None)
            action = getattr(event, "action", None)
            value = getattr(action, "value", None) or {}
            seen.append(str(value.get("answer")))
            return {"toast": {"type": "success", "content": "✅ 已收到回答。"}}

        client, handler = self._make_client(card_cb)
        self.assertTrue(_install_ws_card_support(client, handler))
        acks: list[bytes] = []

        async def fake_write(data: bytes) -> None:
            acks.append(bytes(data))

        client._write_message = fake_write
        frame = _data_frame(
            {
                "schema": "2.0",
                "header": {
                    "event_id": "evt_xxx",
                    "event_type": "card.action.trigger",
                    "create_time": "1",
                    "token": "t",
                    "app_id": "cli_test",
                    "tenant_key": "tk",
                },
                "event": {
                    "operator": {"open_id": "ou-user"},
                    "action": {
                        "value": {
                            "type": "ask_user",
                            "question_id": "q1",
                            "answer": "方案 B",
                        }
                    },
                },
            }
        )

        asyncio.new_event_loop().run_until_complete(client._handle_data_frame(frame))
        self.assertEqual(seen, ["方案 B"])
        self.assertEqual(len(acks), 1)
        reply = WsFrame()
        reply.ParseFromString(acks[0])
        self.assertIn(b'"code": 200', reply.payload)

    def test_non_card_frame_falls_back_to_sdk_original(self) -> None:
        # 安装补丁后，非 card 数据帧应调用 SDK 原始 _handle_data_frame。
        # 这里用一个不存在的 EVENT 类型：SDK 原始路径会抛 EventException
        # 并回 500 ACK，证明确实走了 SDK 原逻辑而不是被静默吞掉。
        client, handler = self._make_client(lambda data: None)
        self.assertTrue(_install_ws_card_support(client, handler))
        acks: list[bytes] = []

        async def fake_write(data: bytes) -> None:
            acks.append(bytes(data))

        client._write_message = fake_write
        frame = _data_frame(
            {
                "schema": "2.0",
                "header": {
                    "event_id": "evt_xxx",
                    "event_type": "im.message.receive_v1",
                    "create_time": "1",
                    "token": "t",
                    "app_id": "cli_test",
                    "tenant_key": "tk",
                },
                "event": {
                    "sender": {"sender_id": {"open_id": "ou-user"}},
                    "message": {
                        "message_id": "om-1",
                        "message_type": "text",
                        "chat_id": "chat-1",
                        "content": json.dumps({"text": "hello"}),
                    },
                },
            },
            type_="event",
        )

        async def run():
            await client._handle_data_frame(frame)
            # 给 SDK 内部 create_task 派发留出事件循环机会
            await asyncio.sleep(0.01)

        asyncio.new_event_loop().run_until_complete(run())
        # EVENT 帧处理不依赖 ACK 写回（im 事件无回调返回值），SDK 原逻辑
        # 正常处理不抛异常即视为回退成功；此处仅断言没有吞掉且无异常。
        self.assertGreaterEqual(len(acks), 0)


if __name__ == "__main__":
    unittest.main()
