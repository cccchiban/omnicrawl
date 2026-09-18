#!/usr/bin/env python3
"""生成 Telegram 连接器的对照数据集，供 Rust 侧 `omnicrawl-connectors::telegram` 使用。

期望值来自 Python 真实现 `omnicrawl/connectors/telegram.py`：
分段与裁剪、流式收尾、文件提取/分类/落盘命名、配置解析、更新路由、`/thinking` 与 `/workspace`
的判定，都在**真对象**上跑一遍后记录（`_abort_stream` / `_finalize_stream` 通过替换消息发送
端口记录调用，不猜测实现）。

用法：``python rust/tools/gen_connectors_telegram_fixture.py``
输出：``rust/crates/omnicrawl-connectors/tests/fixtures/telegram_parity.json``
"""

from __future__ import annotations

import json
import logging
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-connectors/tests/fixtures/telegram_parity.json"

sys.path.insert(0, str(ROOT))

import omnicrawl.connectors.telegram as T  # noqa: E402
import omnicrawl.config.core.runtime as RUNTIME  # noqa: E402
import omnicrawl.config.core.workspace as WORKSPACE  # noqa: E402

if not Path(T.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")


def bare_bot(**attributes):
    """构造未走 __init__ 的 Bot，只装用例需要的属性与端口。"""

    bot = object.__new__(T.TelegramAgentBot)
    bot._allowed = set()
    bot._show_thinking = False
    for name, value in attributes.items():
        setattr(bot, name, value)
    return bot


def capture_messages(bot):
    """替换消息端口，返回记录列表（记录调用了哪个方法、参数是什么）。"""

    recorded: list[dict] = []
    bot._edit_stream_message = lambda chat_id, message_id, text: recorded.append(
        {"call": "edit", "chat_id": chat_id, "message_id": message_id, "text": text}
    )
    bot._send_message = lambda chat_id, text: recorded.append(
        {"call": "send", "chat_id": chat_id, "text": text}
    )
    return recorded


def split_cases() -> list[dict]:
    texts = [
        "hello",
        "",
        "a\nb\nc",
        "中文汉字测试",
        "emoji 🙂🙂🙂 x",
        "x" * 12,
        "line\n",
        "\n".join(["x" * 8, "y", "z" * 8]),
        "abcdefghij",
    ]
    cases = []
    for text in texts:
        for limit in (4, 10, 4000):
            cases.append(
                {
                    "input": text,
                    "limit": limit,
                    "expected": T.TelegramAgentBot._split_message(text, limit),
                }
            )
    long_line = "x" * 9000
    cases.append(
        {
            "input": long_line,
            "limit": 4000,
            "expected": T.TelegramAgentBot._split_message(long_line, 4000),
        }
    )
    return cases


def truncate_cases() -> list[dict]:
    texts = [
        "short",
        "字" * 3980,
        "字" * 3981,
        "字" * 5000,
    ]
    return [
        {"input": text, "expected": T.TelegramAgentBot._truncate_for_stream(text)}
        for text in texts
    ]


def stream_tail_cases() -> dict:
    """`_abort_stream` / `_finalize_stream` 的分支：记录端口调用序列。"""

    abort_cases = []
    for partial, has_message in (("部分输出", True), ("", True), ("部分输出", False)):
        bot = bare_bot()
        recorded = capture_messages(bot)
        bot._abort_stream(7, 21 if has_message else None, list(partial), "❌ 出错了")
        abort_cases.append({"deltas": partial, "has_message": has_message, "calls": recorded})

    long_partial = "x" * 5000
    bot = bare_bot()
    recorded = capture_messages(bot)
    bot._abort_stream(7, 21, list(long_partial), "❌ 出错了")
    abort_cases.append({"deltas": long_partial, "has_message": True, "calls": recorded})

    finalize_cases = []
    for text, has_message in (("短回答", True), ("x" * 3992, True), ("x" * 3993, True), ("回答", False)):
        bot = bare_bot()
        recorded = capture_messages(bot)
        bot._finalize_stream(7, 21 if has_message else None, text)
        finalize_cases.append({"text": text, "has_message": has_message, "calls": recorded})

    return {"abort": abort_cases, "finalize": finalize_cases}


def file_cases() -> dict:
    messages = [
        {"document": {"file_id": "d1", "file_name": "报告.pdf"}},
        {"document": {"file_id": "d2"}},
        {
            "document": {"file_id": "d3", "file_name": "a.zip"},
            "photo": [{"file_id": "p1", "file_size": 99}],
        },
        {"video": {"file_id": "v1", "file_name": "clip.mp4"}},
        {"audio": {"file_id": "a1"}},
        {"animation": {"file_id": "g1"}},
        {"voice": {"file_id": "vo1", "mime_type": "audio/ogg"}},
        {"sticker": {"file_id": "st1"}},
        {"photo": [{"file_id": "small", "file_size": 10}, {"file_id": "big", "file_size": 99}]},
        {"photo": [{"file_id": "tie1", "file_size": 5}, {"file_id": "tie2", "file_size": 5}]},
        {"photo": []},
        {"text": "无文件"},
        {"document": {"file_id": "d9"}, "video": {"file_id": "v9", "file_name": "v.mp4"}},
    ]
    extracted = []
    for message in messages:
        found = T.TelegramAgentBot._extract_telegram_file(message)
        extracted.append(
            {
                "message": message,
                "expected": None if found is None else {"file_id": found[0], "file_name": found[1]},
            }
        )

    names = ["x.PNG", ".gitignore", "trailing.", "archive.tar.gz", "内 含空格.txt", ""]
    classified = [
        {"input": name, "expected": T.TelegramAgentBot._classify_file_name(name)} for name in names
    ]
    return {"extract": extracted, "classify": classified}


def destination_cases() -> list[dict]:
    """落盘命名：真建临时目录，记录相对 `.agent_tmp` 根的结果路径。"""

    cases = [
        {"subdir": "images", "file_name": "shot.png", "existing": []},
        {"subdir": "images", "file_name": "shot.png", "existing": ["shot.png", "shot_1.png"]},
        {"subdir": "files", "file_name": "../../evil.txt", "existing": []},
        {"subdir": "files", "file_name": "  ", "existing": ["telegram_1"]},
        {"subdir": "code", "file_name": "a.b.c.json", "existing": []},
    ]
    recorded = []
    with tempfile.TemporaryDirectory() as workspace:
        root = Path(workspace) / ".omnicrawl" / ".agent_tmp"
        root.mkdir(parents=True, exist_ok=True)
        agent = SimpleNamespace(
            workspace_root=workspace,
            _temp_workspace=SimpleNamespace(root=str(root)),
        )
        bot = bare_bot(_ensure_agent=lambda: agent)
        for case in cases:
            for name in case["existing"]:
                target = root / case["subdir"] / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text("x", encoding="utf-8")
            resolved = bot._resolve_temp_destination(case["subdir"], case["file_name"])
            recorded.append(
                {
                    **case,
                    "expected": resolved.relative_to(root).as_posix(),
                }
            )
    return recorded


def config_cases() -> list[dict]:
    section_cases = [
        ({"TELEGRAM_BOT_TOKEN": "from-env", "TELEGRAM_CONFIRM_TIMEOUT": "12"}, {"bot_token": "cfg", "allowed_user_ids": [1, 2]}),
        ({}, {"bot_token": "cfg", "allowed_user_ids": [1, 2], "confirmation_timeout_seconds": 7}),
        ({"TELEGRAM_ALLOWED_USER_IDS": "1，2, 3 ,"}, {"bot_token": "cfg"}),
        ({}, {"bot_token": "cfg", "allowed_user_ids": []}),
        ({"TELEGRAM_ALLOWED_USER_IDS": "1,abc"}, {}),
        ({}, {"allowed_user_ids": ["x"]}),
        ({"TELEGRAM_CONFIRM_TIMEOUT": "soon"}, {}),
        ({}, {"bot_token": 123, "allowed_user_ids": [7]}),
        ({}, {"bot_token": "cfg", "allowed_user_ids": 9}),
        ({}, {}),
    ]
    recorded = []
    for environment, section in section_cases:
        for key in ("TELEGRAM_BOT_TOKEN", "TELEGRAM_ALLOWED_USER_IDS", "TELEGRAM_CONFIRM_TIMEOUT"):
            os.environ.pop(key, None)
        os.environ.update(environment)
        with mock.patch.object(RUNTIME, "load_config_data", lambda *a, **k: {"telegram": section}):
            try:
                config = T.load_telegram_config()
                expected = {
                    "bot_token": config["bot_token"],
                    "allowed_user_ids": config["allowed_user_ids"],
                    "confirm_timeout_seconds": config["confirm_timeout_seconds"],
                }
            except ValueError as exc:
                expected = {"error": str(exc)}
        recorded.append({"environment": environment, "section": section, "expected": expected})
    for key in ("TELEGRAM_BOT_TOKEN", "TELEGRAM_ALLOWED_USER_IDS", "TELEGRAM_CONFIRM_TIMEOUT"):
        os.environ.pop(key, None)
    return recorded


def update_cases() -> list[dict]:
    updates = [
        {"update_id": 1, "message": {"chat": {"id": 11}, "from": {"id": 7}, "text": "  /status  "}},
        {"update_id": 2, "message": {"chat": {"id": 11}, "from": {"id": 7}, "text": "普通任务"}},
        {"update_id": 3, "message": {"chat": {"id": 11}, "from": {"id": 7}, "text": "/start@MyBot"}},
        {"update_id": 4, "message": {"chat": {"id": 11}, "from": {"id": 9}, "text": "未授权"}},
        {"update_id": 5, "message": {"chat": {"id": 11}, "from": {"id": 9}, "text": ""}},
        {"update_id": 6, "message": {"chat": {"id": 11}, "from": {"id": 7}, "text": ""}},
        {"update_id": 7, "message": {"chat": {"id": 11}, "from": {"id": 7}}},
        {"update_id": 8, "message": {"chat": {"id": 11}, "from": {"id": 7}, "photo": [{"file_id": "p"}]}},
        {"update_id": 9, "message": {"chat": {"id": 11}, "from": {"id": 7}, "text": "", "document": {"file_id": "d", "file_name": "a.txt"}, "caption": " 说明 "}},
        {"update_id": 10, "message": {"chat": {"id": 11}, "from": {"id": 7}, "sticker": {"file_id": "s"}}},
        {"update_id": 11, "message": {"chat": {"id": 11}, "from": {"id": 7}, "voice": {"file_id": "v"}}},
    ]
    recorded = []
    for update in updates:
        bot = bare_bot(_allowed={7})
        calls: list[dict] = []
        bot._dispatch = lambda chat_id, user_id, text: calls.append(
            {"kind": "text", "chat_id": chat_id, "user_id": user_id, "text": text}
        )

        def handle_file(chat_id, user_id, message):
            """真实入口先提取文件，无文件就静默忽略——stub 必须保留这条判定。"""

            if T.TelegramAgentBot._extract_telegram_file(message) is None:
                return
            calls.append(
                {"kind": "file", "chat_id": chat_id, "user_id": user_id, "message": message}
            )

        bot._handle_file_message = handle_file
        warnings: list[str] = []

        class Collector(logging.Handler):
            def emit(self, record):
                warnings.append(record.getMessage())

        handler = Collector()
        T.LOGGER.addHandler(handler)
        try:
            bot._handle_update(update)
        finally:
            T.LOGGER.removeHandler(handler)
        recorded.append({"update": update, "calls": calls, "warnings": warnings})
    return recorded


def thinking_cases() -> list[dict]:
    cases = []
    for text in ("/thinking", "/thinking on", "/thinking  ON ", "/thinking off", "/thinking 关", "/thinking 呀"):
        bot = bare_bot()
        recorded = capture_messages(bot)
        bot._handle_thinking_command(11, text)
        cases.append({"text": text, "show_thinking": bot._show_thinking, "calls": recorded})
    return cases


def workspace_cases() -> list[dict]:
    cases = []
    for text in ("/workspace", "/workspace   ", "/workspace  D:/demo 双 空格 ", "/workspace bad"):
        bot = bare_bot()
        recorded = capture_messages(bot)
        switched: list[str] = []
        agent = SimpleNamespace(
            workspace_root="D:/start",
            switch_workspace=lambda path: (switched.append(path), setattr(agent, "workspace_root", path))[0]
            if path != "bad"
            else (_ for _ in ()).throw(RuntimeError("切换失败：路径不可用")),
        )
        bot._ensure_agent = lambda: agent
        with mock.patch.object(WORKSPACE, "save_workspace_root", lambda path: f"{path}/config.toml"):
            bot._handle_workspace_command(11, text)
        cases.append({"text": text, "switched": switched, "calls": recorded})
    return cases


def main() -> int:
    fixture = {
        "constants": {
            "max_message_len": T.MAX_MESSAGE_LEN,
            "polling_timeout": T.POLLING_TIMEOUT,
            "stream_edit_interval": T.STREAM_EDIT_INTERVAL,
        },
        "split": split_cases(),
        "truncate": truncate_cases(),
        "stream_tail": stream_tail_cases(),
        "files": file_cases(),
        "destination": destination_cases(),
        "config": config_cases(),
        "updates": update_cases(),
        "thinking": thinking_cases(),
        "workspace": workspace_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
