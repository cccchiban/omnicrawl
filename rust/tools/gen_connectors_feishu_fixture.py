#!/usr/bin/env python3
"""生成飞书连接器的对照数据集，供 Rust 侧 `omnicrawl-connectors::feishu` 使用。

期望值来自 Python 真实现 `omnicrawl/connectors/fsapp.py`：文本清理与分段、工具摘要与正文、
文件变更预览（含 `difflib.SequenceMatcher` 统计）、执行计划与子任务进度、卡片 JSON、配置解析、
去重键，以及时间线条目真正发出的消息序列，都在真对象上跑一遍后记录。

用法：``python rust/tools/gen_connectors_feishu_fixture.py``
输出：``rust/crates/omnicrawl-connectors/tests/fixtures/feishu_parity.json``
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-connectors/tests/fixtures/feishu_parity.json"

sys.path.insert(0, str(ROOT))

import omnicrawl.connectors.fsapp as F  # noqa: E402

if not Path(F.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")


def bare_bot(**attributes):
    bot = object.__new__(F.FeishuBot)
    for name, value in attributes.items():
        setattr(bot, name, value)
    return bot


def recording_port(bot):
    """替换平台端口，记录每次调用的方法、负载与参数。"""

    calls: list[dict] = []
    counter = {"value": 0}

    def send_raw(receive_id, payload, *, msg_type="text", receive_id_type="open_id"):
        counter["value"] += 1
        message_id = f"om_{counter['value']}"
        calls.append(
            {
                "call": "send_raw",
                "receive_id": receive_id,
                "payload": payload,
                "msg_type": msg_type,
                "receive_id_type": receive_id_type,
            }
        )
        return message_id

    def patch_card(message_id, payload):
        calls.append({"call": "patch_card", "message_id": message_id, "payload": payload})
        return True

    def send_text(receive_id, text, *, receive_id_type):
        calls.append({"call": "send_text", "receive_id": receive_id, "text": text})
        return True

    bot._send_raw = send_raw
    bot._patch_card = patch_card
    bot._send_text = send_text
    return calls


def constants_case() -> dict:
    return {
        "max_text_chars": F.MAX_TEXT_CHARS,
        "segment_max_chars": F.SEGMENT_MAX_CHARS,
        "stream_patch_interval": F.STREAM_PATCH_INTERVAL_SECONDS,
        "tool_body_max_lines": F.TOOL_BODY_MAX_LINES,
        "file_change_preview_lines": F.FILE_CHANGE_PREVIEW_LINES,
        "reasoning_preview_lines": F.REASONING_PREVIEW_LINES,
        "reasoning_max_chars": F.REASONING_MAX_CHARS,
        "max_todo_lines": F.MAX_TODO_LINES,
        "max_subagent_lines": F.MAX_SUBAGENT_LINES,
        "dedup_ttl_seconds": F._DEDUP_TTL_SECONDS,
        "dedup_max_entries": F._DEDUP_MAX_ENTRIES,
    }


def text_cases() -> dict:
    samples = [
        "普通正文",
        "",
        "  ",
        "前<thinking>推理</thinking>后",
        "只有<tool_use>x</tool_use>",
        "未闭合<thinking>保留",
        "a\n\n\n\nb",
        "行尾空格  \n下一行\t\n",
        "<summary>总结</summary>正文",
        "x" * 3001,
        "\n".join(["段落一", "", "- 列表项", "", "段落二"]),
    ]
    return {
        "clean": [{"input": text, "expected": F._clean_text(text)} for text in samples],
        "display": [{"input": text, "expected": F._display_text(text)} for text in samples],
        "split": [{"input": text, "expected": F._split_text(text)} for text in samples],
        "resolve": [
            {"streamed": "前半", "reply": "前半后半", "expected": F._resolve_final_text("前半", "前半后半")},
            {"streamed": "前半 后半", "reply": "前半", "expected": F._resolve_final_text("前半 后半", "前半")},
            {"streamed": "", "reply": "回复", "expected": F._resolve_final_text("", "回复")},
            {"streamed": "流式", "reply": "", "expected": F._resolve_final_text("流式", "")},
        ],
        "segments": [
            {"input": "短正文", "expected": list(F._split_segment_for_card("短正文"))},
            {
                "input": "a" * 4000 + "\n\n" + "b" * 3000,
                "expected": list(F._split_segment_for_card("a" * 4000 + "\n\n" + "b" * 3000)),
            },
            {
                "input": "x" * 7000,
                "expected": list(F._split_segment_for_card("x" * 7000)),
            },
        ],
        "helpers": {
            "compact": [
                {"value": value, "max_chars": limit, "expected": F._compact_line(value, max_chars=limit)}
                for value, limit in (
                    ("a  b\nc", 48),
                    ("", 10),
                    ("🔧" * 60, 10),
                    (123, 10),
                )
            ],
            "clip": [
                {"line": line, "expected": F._clip_line(line)}
                for line in ("短", " 尾部空格 ", "长" * 200, "")
            ],
            "fenced": [{"body": body, "expected": F._fenced_body(body)} for body in ("x", "a```b", "")],
            "elapsed": [{"seconds": value, "expected": F._format_elapsed(value)} for value in (0, 9, 65, 3661)],
            "operation": [
                {"name": name, "expected": F._operation_of(name)}
                for name in ("read", "mcp.files.read", "", "bash")
            ],
            "stats": [
                {"added": added, "removed": removed, "expected": F._format_line_stats(added=added, removed=removed)}
                for added, removed in ((0, 0), (3, 0), (0, 2), (5, 7))
            ],
        },
    }


def tool_cases() -> dict:
    result_samples = {
        "read": "     1: import os\n     2: x = 1\n    40: print(x)\n",
        "list": "a.py\nb.py\nc.py\n...（已截断）\n",
        "read_image": "已读取图片",
        "grep": "a.py:1: hit\n",
        "bash": "line1\nline2\n",
    }
    samples = [
        ("list", {"path": "src"}, result_samples["list"]),
        ("list", {}, "目录为空。\n"),
        ("read", {"path": "a/b.py", "offset": 1}, result_samples["read"]),
        ("read", {}, ""),
        ("read_image", {"path": "img.png"}, result_samples["read_image"]),
        ("grep", {"path": "src", "pattern": "to" * 30}, result_samples["grep"]),
        ("find", {}, ""),
        ("bash", {"command": "echo  " + "x" * 200}, result_samples["bash"]),
        ("powershell", {"command": "Get-ChildItem"}, ""),
        (
            "Edit_file",
            {"path": "a.py", "old_text": "old\nkeep\n", "new_text": "keep\nnew\n"},
            "已修改 a.py（替换 1 处）",
        ),
        ("write_file", {"path": "b.py", "content": "x\ny\nz\n"}, "已写入 b.py"),
        ("write_file", {"path": "b.py", "content": "", "mode": "append"}, ""),
        ("monitor", {"action": "start", "command": "npm test"}, ""),
        ("subagent", {"action": "run", "tasks": [{}, {}]}, ""),
        ("memory_search", {"query": "会话"}, ""),
        ("memory_read", {"memory_ids": ["a", "b"]}, ""),
        ("kb_read", {"path": "projects/x.md"}, ""),
        ("mcp__x__custom", {"token": "abc", "path": "p"}, "ok"),
        ("unknown_tool", {}, ""),
    ]
    summaries = []
    bodies = []
    for name, arguments, result in samples:
        summaries.append(
            {
                "name": name,
                "arguments": arguments,
                "result": result,
                "expected": F._tool_summary(name, arguments, result),
            }
        )
        bodies.append(
            {
                "name": name,
                "arguments": arguments,
                "result": result,
                "expected": F._tool_body(name, arguments, result),
            }
        )
    outputs = [
        "1\n2\n3\n4\n5\n6\n7\n",
        "\n\n1\n\n2\n\n\n",
        "short",
        "x" * 300,
        "1\n2\n3\n\n\n\n4\n5\n6\n",
    ]
    return {
        "summaries": summaries,
        "bodies": bodies,
        "sample_lines": [{"input": output, "expected": F._sample_output_lines(output)} for output in outputs],
        "read_range": [
            {"input": "     1: x\n    40: y\n", "expected": list(F._read_result_line_range("     1: x\n    40: y\n"))},
            {"input": "no numbers here", "expected": None},
            {"input": "12:x\n", "expected": None},
            {"input": "", "expected": None},
        ],
        "list_summary": [
            {"input": "a\nb\n", "expected": F._list_result_summary("a\nb\n")},
            {"input": "目录为空。\n", "expected": F._list_result_summary("目录为空。\n")},
            {"input": "\n\n", "expected": F._list_result_summary("\n\n")},
            {"input": "a\n...（已截断）\n", "expected": F._list_result_summary("a\n...（已截断）\n")},
            {"input": "", "expected": F._list_result_summary("")},
        ],
        "diff": [
            {
                "old_text": old_text,
                "new_text": new_text,
                "preview": F._diff_preview_lines(old_text, new_text)[0],
                "added": F._diff_preview_lines(old_text, new_text)[1],
                "removed": F._diff_preview_lines(old_text, new_text)[2],
            }
            for old_text, new_text in (
                ("", ""),
                ("a", "a"),
                ("a\nb\nc", "a\nx\nc"),
                ("x\ny\nz\nw", "x\ny"),
                ("1\n2\n3", "1\n2\n3\n4"),
                ("\n".join(f"l{i}" for i in range(20)), "\n".join(f"l{i}" for i in range(20, 40))),
                ("中文一\n中文二", "中文一\n中文三"),
            )
        ],
        "file_change": [
            {
                "operation": operation,
                "arguments": arguments,
                "summary": F._file_change_summary(operation, arguments),
                "preview": F._file_change_preview(operation, arguments),
            }
            for operation, arguments in (
                ("Edit_file", {"old_text": "old", "new_text": "new"}),
                ("Edit_file", {"old_text": "a\nb\nc\nd\ne\nf", "new_text": "a\nz"}),
                ("write_file", {"content": "a\nb\n", "mode": "append"}),
                ("write_file", {"content": ""}),
                ("write_file", {"content": "\n".join(f"line{i}" for i in range(8))}),
            )
        ],
        "file_change_note": [
            {
                "result": result,
                "expected": F._file_change_result_note("Edit_file", result),
            }
            for result in ("已修改 a.py（替换 3 处）", "替换 1 处", "", "普通输出")
        ],
    }


def plan_cases() -> dict:
    items = [
        [{"step": " 第一步 ", "completed": True}, {"step": "", "completed": False}],
        [{"description": "第二步", "status": "done"}, {"title": "第三步"}],
        [],
        [{"step": f"步骤{index}"} for index in range(25)],
        None,
    ]
    return {
        "normalize": [
            {"input": value, "expected": [list(item) for item in F._normalize_todos(value)]}
            for value in items
        ],
        "text": [
            {"todos": [list(item) for item in F._normalize_todos(value)], "expected": F._todos_text(F._normalize_todos(value))}
            for value in items
            if value
        ],
        "reasoning": [
            {"input": "行1\n行2\n行3\n行4\n行5\n行6", "streaming": True, "expected": F._reasoning_panel("行1\n行2\n行3\n行4\n行5\n行6", streaming=True)},
            {"input": "  ", "streaming": True, "expected": F._reasoning_panel("  ", streaming=True)},
            {"input": "完整思考", "streaming": False, "expected": F._reasoning_panel("完整思考", streaming=False)},
            {"input": "x" * 4100, "streaming": False, "expected": F._reasoning_panel("x" * 4100, streaming=False)},
        ],
    }


def subagent_cases() -> list[dict]:
    scenarios = [
        [
            ("subagent.task.queued", {"task_id": "t1", "agent_type": "explore", "description": "看代码"}),
            ("subagent.task.running", {"task_id": "t1"}),
            ("subagent.task.completed", {"task_id": "t1", "description": "看完了"}),
            ("subagent.task.running", {"task_id": "t1"}),
        ],
        [
            ("subagent.task.queued", {"task_id": "t1", "description": "一"}),
            ("subagent.task.queued", {"task_id": "t2", "description": "二"}),
            ("subagent.task.failed", {"task_id": "t2"}),
            ("unknown.event", {"task_id": "t3"}),
        ],
    ]
    recorded = []
    for scenario in scenarios:
        bot = bare_bot()
        calls = recording_port(bot)
        message = F._SubAgentMessage(bot, "oc_1", "chat_id")
        for event_name, payload in scenario:
            message.update(event_name, payload)
        nodes = {key: {"status": node.status} for key, node in message._nodes.items()}
        recorded.append(
            {
                "scenario": [{"event": name, "payload": payload} for name, payload in scenario],
                "nodes": nodes,
                "calls": calls,
            }
        )
    return recorded


def timeline_cases() -> dict:
    text_cases_recorded = []
    for text, suffix in (("第一段", ""), ("", ""), ("x" * 6100, ""), ("收尾", "\n\n⏹ 中断")):
        bot = bare_bot()
        calls = recording_port(bot)
        message = F._TextMessage(bot, "oc_1", "chat_id")
        streamed = message.stream(text)
        remaining = message.seal(text, suffix=suffix)
        text_cases_recorded.append(
            {
                "text": text,
                "suffix": suffix,
                "streamed": streamed,
                "remaining": remaining,
                "calls": calls,
            }
        )

    tool_cases_recorded = []
    with mock.patch.object(F._ToolRecord, "duration_seconds", property(lambda self: 0.136)):
        for name, arguments, ok, output in (
            ("read", {"path": "a.py"}, True, "     1: x\n"),
            ("bash", {"command": "echo hi"}, False, "boom"),
        ):
            bot = bare_bot()
            calls = recording_port(bot)
            record = F._ToolRecord(key="c1", name=name, summary=F._tool_summary(name, arguments))
            record.arguments = arguments
            message = F._ToolMessage(bot, "oc_1", "chat_id", record)
            message.start()
            message.finish(ok=ok, output=output)
            tool_cases_recorded.append(
                {
                    "name": name,
                    "arguments": arguments,
                    "ok": ok,
                    "output": output,
                    "calls": calls,
                }
            )

    bot = bare_bot()
    calls = recording_port(bot)
    reasoning = F._ReasoningMessage(bot, "oc_1", "chat_id")
    reasoning.stream("思考中")
    reasoning.seal("完整思考")
    reasoning_calls = calls

    bot = bare_bot()
    calls = recording_port(bot)
    plan = F._PlanMessage(bot, "oc_1", "chat_id")
    plan.update([{"step": "第一步", "completed": True}])
    plan.update([{"step": "第一步", "completed": True}])
    plan.update([{"step": "第二步"}])
    plan_calls = calls

    return {
        "text": text_cases_recorded,
        "tool": tool_cases_recorded,
        "reasoning": {"calls": reasoning_calls},
        "plan": {"calls": plan_calls},
    }


def file_cases() -> dict:
    names = ["x.PNG", "v.opus", "m.mp4", "s.py", "d.csv", "a.zip", "", "no_ext"]
    classified = [{"input": name, "expected": F.FeishuBot._classify_filename(name)} for name in names]

    recorded = []
    with tempfile.TemporaryDirectory() as workspace:
        root = Path(workspace) / ".omnicrawl" / ".agent_tmp"
        root.mkdir(parents=True, exist_ok=True)
        agent = SimpleNamespace(
            workspace_root=workspace,
            _temp_workspace=SimpleNamespace(root=str(root)),
        )
        bot = bare_bot(_ensure_agent=lambda: agent)
        for filename, existing in (
            ("shot.png", []),
            ("shot.png", ["shot.png", "shot_1.png"]),
            ("../../evil.png", []),
            ("", []),
            ("dir/name.txt", []),
        ):
            for name in existing:
                target = root / "images" / name if name.endswith(".png") else root / "files" / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text("x", encoding="utf-8")
            resolved = bot._resolve_temp_destination(filename)
            recorded.append(
                {
                    "filename": filename,
                    "existing": existing,
                    "expected": resolved.relative_to(root.resolve()).as_posix(),
                }
            )

    markers = [
        {"input": "结果\n[FILE:out/a.png] 与 [FILE: .omnicrawl/x.py ]", "expected": F._FILE_MARKER_PATTERN.findall("结果\n[FILE:out/a.png] 与 [FILE: .omnicrawl/x.py ]")},
        {"input": "没有标记", "expected": F._FILE_MARKER_PATTERN.findall("没有标记")},
    ]
    post_samples = [
        {
            "post": {
                "en_us": {"title": "Title", "content": [[{"tag": "text", "text": "english"}]]},
                "zh_cn": {
                    "title": "标题",
                    "content": [
                        [
                            {"tag": "text", "text": "正文"},
                            {"tag": "at", "user_name": "小明"},
                            {"tag": "img", "image_key": "img_1"},
                        ]
                    ],
                },
            }
        },
        {"post": {"zh_cn": {"title": "标题", "content": [[{"tag": "text", "text": "正文"}]]}}},
        {"zh_cn": {"title": "无 post 包装", "content": [[{"tag": "text", "text": "正文"}]]}},
        {},
    ]
    post = [
        {"input": sample, "expected": list(F.FeishuBot._post_text_and_images(sample))}
        for sample in post_samples
    ]
    return {
        "classify": classified,
        "destination": recorded,
        "markers": markers,
        "post": post,
        "file_type_map": {key: value for key, value in F._FILE_TYPE_MAP.items()},
        "resource_types": sorted(F._MESSAGE_RESOURCE_TYPES),
    }


def config_cases() -> list[dict]:
    section_cases = [
        ({"FEISHU_APP_ID": "cli_env"}, {"feishu": {"app_id": "cli_section", "app_secret": "sec", "allowed_users": ["ou_a"]}}),
        ({}, {"feishu": {"app_id": "cli_1", "app_secret": "sec", "confirmation_timeout_seconds": 7}}),
        ({}, {"feishu": {"fs_app_id": "cli_alias", "fs_app_secret": "sec2", "fs_allowed_users": ["ou_a", "ou_b"]}}),
        ({}, {"fs_app_id": "cli_root", "fs_app_secret": "sec3", "fs_allowed_users": "ou_a,ou_b"}),
        ({"FEISHU_ALLOWED_USER_IDS": "ou_c，ou_d"}, {"feishu": {"allowed_user_ids": ["ou_e"]}}),
        ({}, {"feishu": {"allowed_user_ids": ["*"]}}),
        ({}, {"feishu": "not-an-object"}),
        ({"FEISHU_CONFIRM_TIMEOUT": "soon"}, {}),
        ({"FEISHU_CONFIRM_TIMEOUT": "0.5"}, {}),
    ]
    recorded = []
    for environment, data in section_cases:
        for key in ("FEISHU_APP_ID", "FEISHU_APP_SECRET", "FEISHU_ALLOWED_USER_IDS", "FEISHU_CONFIRM_TIMEOUT"):
            os.environ.pop(key, None)
        os.environ.update(environment)
        with mock.patch.object(F, "_load_runtime_config", lambda: data):
            try:
                config = F.load_feishu_config()
                expected = {
                    "app_id": config.app_id,
                    "app_secret": config.app_secret,
                    "allowed_user_ids": sorted(config.allowed_user_ids),
                    "confirmation_timeout_seconds": config.confirmation_timeout_seconds,
                    "public_access": config.public_access,
                }
            except Exception as exc:  # noqa: BLE001 - 诊断用例保留错误文案
                expected = {"error": str(exc)}
        recorded.append(
            {
                "environment": environment,
                "data": data,
                "expected": expected,
            }
        )
    for key in ("FEISHU_APP_ID", "FEISHU_APP_SECRET", "FEISHU_ALLOWED_USER_IDS", "FEISHU_CONFIRM_TIMEOUT"):
        os.environ.pop(key, None)
    return recorded


def mask_cases() -> list[dict]:
    return [{"input": value, "expected": F._mask_secret(value)} for value in ("", "short", "12345678", "123456789", "cli_a1b2c3d4e5")]


def dedupe_cases() -> dict:
    claimed = []
    for message_ids in (["m1", "m1", "m2"], ["", ""], ["m3", "m4", "m3"]):
        bot = bare_bot(_seen_messages={})
        import threading

        bot._lock = threading.RLock()
        results = [bot._claim_message_once(message_id) for message_id in message_ids]
        claimed.append({"message_ids": message_ids, "results": results})

    keys = []
    samples = [
        ("text", {"message_id": "m1", "create_time": "1700000000", "chat_id": "oc_1"}, {"open_id": "ou_1"}, "你好"),
        ("text", {"message_id": "m2", "create_time": "1700000000", "chat_id": "oc_1"}, {"open_id": "ou_1"}, "你好"),
        ("image", {"message_id": "m3", "create_time": "1700000000", "chat_id": "oc_1"}, {"open_id": "ou_1"}, ""),
        ("image", {"message_id": "", "create_time": "", "chat_id": ""}, {"open_id": ""}, ""),
        ("text", {"message_id": "m4", "create_time": "", "chat_id": "oc_1"}, {"open_id": "ou_1"}, "hi"),
        ("text", {"message_id": "", "create_time": "", "chat_id": ""}, {"open_id": ""}, ""),
    ]
    for message_type, message_fields, sender, user_text in samples:
        message = SimpleNamespace(message_type=message_type, **message_fields)
        event = SimpleNamespace(sender=SimpleNamespace(sender_id=SimpleNamespace(open_id=sender["open_id"])))
        keys.append(
            {
                "message_type": message_type,
                "message_id": message_fields.get("message_id", ""),
                "create_time": message_fields.get("create_time", ""),
                "chat_id": message_fields.get("chat_id", ""),
                "open_id": sender["open_id"],
                "user_text": user_text,
                "expected": F.FeishuBot._inbox_dedupe_key(bot := bare_bot(), event, message, user_text),
            }
        )
    return {"claim": claimed, "keys": keys}


def main() -> int:
    fixture = {
        "constants": constants_case(),
        "text": text_cases(),
        "tools": tool_cases(),
        "plan": plan_cases(),
        "subagents": subagent_cases(),
        "timeline": timeline_cases(),
        "files": file_cases(),
        "config": config_cases(),
        "mask": mask_cases(),
        "dedupe": dedupe_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
