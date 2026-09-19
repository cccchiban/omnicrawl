#!/usr/bin/env python3
"""生成会话记录解码与诊断的对照数据集：期望值全部来自 Python 真实现。

覆盖：事件字典迁移、事件解码（字典与 JSONL 单行）、转录整份读取（真实临时文件）、
索引文档解析与构造、Python `str.splitlines()` 的切行规则。

诊断明细里的 `json_error` 来自各自的 JSON 库，文本必然不同：两侧都替换成
`<json_error>` 占位后再比对。
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_records_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_records as records  # noqa: E402

SESSION = "20260918-030529-abcdef"
OTHER_SESSION = "20260918-030529-abcdea"
EVENT_ID = "0123456789abcdef01234567"
CREATED_AT = "2026-09-18T03:05:29.123456+00:00"
DISPLAY_PATH = "sessions/20260918-030529-abcdef.jsonl"
JSON_ERROR_PLACEHOLDER = "<json_error>"


def good_event(**overrides) -> dict:
    payload = {
        "version": 1,
        "session_id": SESSION,
        "event_id": EVENT_ID,
        "parent_id": None,
        "type": "user_message",
        "created_at": CREATED_AT,
        "payload": {"text": "你好"},
    }
    payload.update(overrides)
    return payload


def outcome(call) -> dict:
    try:
        value = call()
    except Exception as exc:  # noqa: BLE001 - 对照需要任何异常的文案
        return {"ok": False, "error": str(exc)}
    return {"ok": True, "value": value}


def plain(value) -> dict:
    """把诊断/事件对象转成 JSON 形状并抹平 JSON 库错误文本。"""
    if hasattr(value, "to_dict"):
        value = value.to_dict()
    if isinstance(value, list):
        return [plain(item) for item in value]
    if isinstance(value, dict):
        return {key: plain(item) for key, item in value.items()}
    return value


def flatten_json_error(value):
    if isinstance(value, list):
        return [flatten_json_error(item) for item in value]
    if isinstance(value, dict):
        result = {key: flatten_json_error(item) for key, item in value.items()}
        details = result.get("details")
        if isinstance(details, dict) and "json_error" in details:
            details["json_error"] = JSON_ERROR_PLACEHOLDER
        return result
    return value


def migrate_cases() -> list[dict]:
    cases = [
        {"label": "current_version", "input": good_event()},
        {"label": "prototype_with_id", "input": {"version": 0, "id": EVENT_ID, **{
            key: value for key, value in good_event().items() if key not in ("version", "event_id")
        }}},
        {"label": "prototype_with_event_id", "input": good_event(version=0)},
        {"label": "unversioned_complete", "input": {
            key: value for key, value in good_event().items() if key != "version"
        }},
        {"label": "unversioned_incomplete", "input": {"session_id": SESSION}},
        {"label": "future_version", "input": good_event(version=2)},
        {"label": "string_version", "input": good_event(version="1")},
        {"label": "not_object", "input": [1, 2]},
    ]
    results = []
    for case in cases:
        result = outcome(lambda data=case["input"]: records.migrate_event_dict(data))
        item = {"label": case["label"], "input": case["input"], "ok": result["ok"]}
        if result["ok"]:
            migrated, tag = result["value"]
            item["value"] = {"data": migrated, "tag": tag}
        else:
            item["error"] = result["error"]
        results.append(item)
    return results


def decode_dict_cases() -> list[dict]:
    inputs = [
        {"label": "good", "data": good_event(), "expected_session_id": None},
        {"label": "good_with_match", "data": good_event(), "expected_session_id": SESSION},
        {"label": "session_mismatch", "data": good_event(), "expected_session_id": OTHER_SESSION},
        {"label": "not_object", "data": [1, 2], "expected_session_id": None},
        {"label": "unsupported_version", "data": good_event(version=2), "expected_session_id": None},
        {"label": "missing_type", "data": {k: v for k, v in good_event().items() if k != "type"},
         "expected_session_id": None},
        {"label": "legacy_migrated", "data": {k: v for k, v in good_event().items() if k != "version"},
         "expected_session_id": None},
        {"label": "bad_payload", "data": good_event(payload="nope"), "expected_session_id": None},
    ]
    results = []
    for case in inputs:
        event, diagnostics = records.decode_session_event_dict(
            case["data"],
            path=DISPLAY_PATH,
            line_no=3,
            expected_session_id=case["expected_session_id"],
        )
        results.append({
            "label": case["label"],
            "input": {
                "data": case["data"],
                "path": DISPLAY_PATH,
                "line_no": 3,
                "expected_session_id": case["expected_session_id"],
            },
            "event": flatten_json_error(plain(event)),
            "diagnostics": flatten_json_error([plain(item) for item in diagnostics]),
        })
    return results


def decode_line_cases() -> list[dict]:
    inputs = [
        {"label": "empty", "line": "", "is_last_nonempty_line": False, "file_ends_with_newline": True},
        {"label": "blank", "line": "   ", "is_last_nonempty_line": False, "file_ends_with_newline": True},
        {"label": "good", "line": json.dumps(good_event(), ensure_ascii=False),
         "is_last_nonempty_line": True, "file_ends_with_newline": True},
        {"label": "broken_middle", "line": "{not json", "is_last_nonempty_line": False,
         "file_ends_with_newline": True},
        {"label": "broken_trailing", "line": '{"version": 1, "session', "is_last_nonempty_line": True,
         "file_ends_with_newline": False},
        {"label": "top_level_array", "line": "[1,2]", "is_last_nonempty_line": False,
         "file_ends_with_newline": True},
    ]
    results = []
    for case in inputs:
        event, diagnostics = records.decode_session_event_line(
            case["line"],
            path=DISPLAY_PATH,
            line_no=7,
            expected_session_id=None,
            is_last_nonempty_line=case["is_last_nonempty_line"],
            file_ends_with_newline=case["file_ends_with_newline"],
        )
        results.append({
            "label": case["label"],
            "input": {
                "line": case["line"],
                "path": DISPLAY_PATH,
                "line_no": 7,
                "expected_session_id": None,
                "is_last_nonempty_line": case["is_last_nonempty_line"],
                "file_ends_with_newline": case["file_ends_with_newline"],
            },
            "event": flatten_json_error(plain(event)),
            "diagnostics": flatten_json_error([plain(item) for item in diagnostics]),
        })
    return results


def read_file_cases(work_root: Path) -> list[dict]:
    good_line = json.dumps(good_event(), ensure_ascii=False)
    second_line = json.dumps(good_event(event_id="0123456789abcdef01234568"), ensure_ascii=False)
    contents = {
        "missing": None,
        "clean": f"{good_line}\n{second_line}\n",
        "blank_lines": f"\n{good_line}\n\n{second_line}\n",
        "no_trailing_newline": f"{good_line}\n{second_line}",
        "broken_middle_then_trailing": f"{good_line}\n{{not json\n{second_line[:-6]}",
        "mismatch": f"{good_line}\n",
    }
    results = []
    for label, text in contents.items():
        path = work_root / f"{label}.jsonl"
        if text is not None:
            path.write_text(text, encoding="utf-8")
        session_id = OTHER_SESSION if label == "mismatch" else None
        result = outcome(
            lambda p=path, s=session_id: records.read_session_events_with_diagnostics(
                p, session_id=s, relative_path=DISPLAY_PATH
            )
        )
        item = {
            "label": label,
            "session_id": session_id,
            "path_exists": text is not None,
            "text": text,
        }
        if result["ok"]:
            item["result"] = flatten_json_error(plain(result["value"]))
        else:
            item["error"] = result["error"]
        results.append(item)
    return results


def parse_index_cases() -> list[dict]:
    entries = [
        {"session_id": SESSION, "title": "一", "updated_at": CREATED_AT},
        {"session_id": OTHER_SESSION, "title": "二", "updated_at": CREATED_AT},
    ]
    inputs = [
        {"label": "with_version", "data": {"schema_version": 1, "sessions": entries}},
        {"label": "legacy_no_version", "data": {"sessions": entries}},
        {"label": "not_object", "data": [1]},
        {"label": "string_version", "data": {"schema_version": "1", "sessions": entries}},
        {"label": "bool_version", "data": {"schema_version": True, "sessions": entries}},
        {"label": "future_version", "data": {"schema_version": 2, "sessions": entries}},
        {"label": "sessions_not_list", "data": {"schema_version": 1, "sessions": 3}},
        {"label": "mixed_sessions", "data": {"schema_version": 1, "sessions": [entries[0], 5, "x"]}},
        {"label": "empty", "data": {"schema_version": 1, "sessions": []}},
    ]
    results = []
    for case in inputs:
        result = outcome(lambda data=case["data"]: records.parse_index_document(data))
        item = {"label": case["label"], "input": case["data"], "ok": result["ok"]}
        if result["ok"]:
            sessions, version = result["value"]
            item["value"] = {"sessions": sessions, "version": version}
        else:
            item["error"] = result["error"]
        results.append(item)
    return results


def build_index_cases() -> list[dict]:
    entries = [
        {"session_id": SESSION, "title": "一"},
        {"session_id": OTHER_SESSION, "title": "二"},
    ]
    return [
        {"label": "empty", "input": [], "expected": records.build_index_document([])},
        {"label": "two", "input": entries, "expected": records.build_index_document(entries)},
    ]


def splitlines_cases() -> list[dict]:
    texts = [
        "",
        "a",
        "a\n",
        "\n",
        "a\n\n",
        "a\nb\r\nc\rd\u000be\u000cf\u001cg\u001dh\u001ei\u0085j\u2028k\u2029l",
        "a\r\nb\r\n",
    ]
    return [{"input": text, "expected": text.splitlines()} for text in texts]


def main() -> None:
    module_path = Path(records.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    work_root = Path(tempfile.mkdtemp(prefix="omnicrawl-session-records-"))
    payload = {
        "source": "omnicrawl/state/session_records.py",
        "json_error_placeholder": JSON_ERROR_PLACEHOLDER,
        "migrate": migrate_cases(),
        "decode_dict": decode_dict_cases(),
        "decode_line": decode_line_cases(),
        "read_file": read_file_cases(work_root),
        "parse_index": parse_index_cases(),
        "build_index": build_index_cases(),
        "splitlines": splitlines_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}")


if __name__ == "__main__":
    main()
