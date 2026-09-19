#!/usr/bin/env python3
"""生成提示历史的对照数据集：期望值全部来自 Python 真实现。

两组：
- `pure`：展示清洗、条目构造与解析（含各类报错）、条目序列化；
- `store`：在真实临时目录上跑 PromptHistoryStore（追加 / 查询 / 读取 / 坏行文件），
  逐例比对每步返回值与最终 `history.jsonl` 字节。

临时根在数据集里是 `<ROOT>` 占位；追加一律传固定时刻，落盘 `timestamp` 才不会两侧不同。
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/prompt_history_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.state import prompt_history as prompt_module  # noqa: E402
from omnicrawl.state.prompt_history import (  # noqa: E402
    PromptHistoryEntry,
    PromptHistoryStore,
)

ROOT_PLACEHOLDER = "<ROOT>"
WORK_ROOT = (
    Path(tempfile.gettempdir()) / f"omnicrawl-prompt-parity-{__import__('os').getpid()}"
).resolve()
SESSION = "20260918-030529-abcdef"
OTHER_SESSION = "20260918-030529-abcdea"
JSON_ERROR_PLACEHOLDER = "<json_error>"
NOW = datetime(2026, 1, 2, 3, 4, 5, 123456, tzinfo=timezone.utc)


def mask(text: object) -> str:
    # JSON 文本里的路径是转义形态（`\\`），两种形态都要替换。
    raw = str(text)
    base = str(WORK_ROOT)
    raw = raw.replace(base.replace("\\", "\\\\"), ROOT_PLACEHOLDER)
    raw = raw.replace(base, ROOT_PLACEHOLDER)
    return raw


def expand(text: str | None) -> str | None:
    if text is None:
        return None
    return text.replace(ROOT_PLACEHOLDER, str(WORK_ROOT))


def mask_value(value):
    if isinstance(value, PromptHistoryEntry):
        return mask_value(value.to_dict())
    if isinstance(value, list):
        return [mask_value(item) for item in value]
    if isinstance(value, dict):
        return {key: mask_value(item) for key, item in value.items()}
    if isinstance(value, (str, Path)):
        return mask(value)
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


def outcome(call) -> dict:
    try:
        value = call()
    except Exception as exc:  # noqa: BLE001 - 对照需要任何异常的文案
        return {"ok": False, "error": mask(exc)}
    return {"ok": True, "value": mask_value(value)}


def pure_cases() -> dict:
    long_text = "字" * 4001
    return {
        "clean_display": [
            {
                "input": text,
                "expected": prompt_module.clean_prompt_display(text),
            }
            for text in [
                "  你好  ",
                "a\r\nb\rc",
                "",
                "   ",
                "字" * 4000,
                long_text,
            ]
        ],
        "entry_create": [
            {
                "label": label,
                "input": {
                    "display": display,
                    "project": "<ROOT>/proj",
                    "session_id": session_id,
                    "pasted_contents": pasted,
                    "now": NOW.isoformat(),
                },
                **outcome(
                    lambda display=display, session_id=session_id, pasted=pasted: PromptHistoryEntry.create(
                        display=display,
                        project=WORK_ROOT / "proj",
                        session_id=session_id,
                        pasted_contents=pasted,
                        now=NOW,
                    )
                ),
            }
            for label, display, session_id, pasted in [
                ("plain", "hello", SESSION, {}),
                ("with_redaction", "token ghp_0123456789abcdefghijklmnopqrstuvwxyz", SESSION,
                 {"password": "hunter2", "note": "x"}),
                ("trimmed", "  hi  ", SESSION, None),
                ("bad_session", "hi", "nope", {}),
                ("crlf", "a\r\nb", SESSION, {}),
            ]
        ],
        "entry_from_dict": [
            {
                "label": label,
                "input": payload,
                **outcome(
                    lambda payload=payload: PromptHistoryEntry.from_dict(
                        {key: (expand(value) if isinstance(value, str) else value)
                         for key, value in payload.items()}
                    )
                ),
            }
            for label, payload in [
                ("good", {"display": "hello", "timestamp": 1767323045123,
                          "project": "<ROOT>/proj", "session_id": SESSION,
                          "pasted_contents": {"note": "x"}}),
                ("missing_optional", {"display": "hello", "timestamp": 0,
                                      "project": "<ROOT>/proj"}),
                ("bool_timestamp", {"display": "hello", "timestamp": True,
                                    "project": "<ROOT>/proj", "session_id": SESSION}),
                ("negative_timestamp", {"display": "hello", "timestamp": -5,
                                        "project": "<ROOT>/proj", "session_id": SESSION}),
                ("blank_display", {"display": "   ", "timestamp": 1,
                                   "project": "<ROOT>/proj", "session_id": SESSION}),
                ("blank_project", {"display": "x", "timestamp": 1, "project": "  ",
                                   "session_id": SESSION}),
                ("pasted_not_object", {"display": "x", "timestamp": 1, "project": "<ROOT>/proj",
                                       "session_id": SESSION, "pasted_contents": []}),
                ("pasted_null", {"display": "x", "timestamp": 1, "project": "<ROOT>/proj",
                                 "session_id": SESSION, "pasted_contents": None}),
                ("bad_session_id", {"display": "x", "timestamp": 1, "project": "<ROOT>/proj",
                                    "session_id": "!!"}),
                ("missing_session_id", {"display": "x", "timestamp": 1,
                                        "project": "<ROOT>/proj"}),
            ]
        ],
    }


def store_cases() -> list[dict]:
    return [
        {"label": "append_and_search", "ops": [
            {"op": "append", "display": "第一条", "project": "<ROOT>/proj", "session_id": SESSION,
             "now": "2026-01-02T03:04:05.000000+00:00"},
            {"op": "append", "display": "第二条", "project": "<ROOT>/proj", "session_id": SESSION,
             "now": "2026-01-02T03:04:06.000000+00:00"},
            {"op": "append", "display": "第二条", "project": "<ROOT>/proj", "session_id": SESSION,
             "now": "2026-01-02T03:04:07.000000+00:00"},
            {"op": "append", "display": "别的项目", "project": "<ROOT>/other",
             "session_id": OTHER_SESSION, "now": "2026-01-02T03:04:08.000000+00:00"},
            {"op": "search", "limit": 20},
            {"op": "search", "project": "<ROOT>/proj", "limit": 20},
            {"op": "search", "query": "第二", "limit": 20},
            {"op": "search", "session_id": OTHER_SESSION, "limit": 20},
            {"op": "search", "limit": 1},
            {"op": "search", "limit": 0},
            {"op": "search", "query": "  ", "limit": 200},
            {"op": "read"},
        ]},
        {"label": "broken_lines", "ops": [
            {"op": "write_file", "text": mask(render_lines([
                {"display": "好的", "timestamp": 1000, "project": "<ROOT>/proj",
                 "session_id": SESSION, "pasted_contents": {}},
                "{not json",
                [1, 2],
                {"display": "", "timestamp": 2000, "project": "<ROOT>/proj",
                 "session_id": SESSION},
                {"display": "尾部", "timestamp": 3000, "project": "<ROOT>/proj",
                 "session_id": SESSION},
            ], '{"display": "half'))},
            {"op": "read"},
            {"op": "search", "limit": 20},
        ]},
        {"label": "empty_prompt", "ops": [
            {"op": "append", "display": "", "project": "<ROOT>/proj", "session_id": SESSION,
             "now": "2026-01-02T03:04:05.000000+00:00"},
            {"op": "append", "display": "   ", "project": "<ROOT>/proj", "session_id": SESSION,
             "now": "2026-01-02T03:04:05.000000+00:00"},
            {"op": "read"},
        ]},
    ]


def render_lines(lines: list, trailing_partial: str | None) -> str:
    rendered = []
    for item in lines:
        if isinstance(item, str):
            rendered.append(item)
        elif isinstance(item, dict):
            rendered.append(json.dumps(
                {key: (expand(value) if isinstance(value, str) else value)
                 for key, value in item.items()},
                ensure_ascii=False,
            ))
        else:
            rendered.append(json.dumps(item, ensure_ascii=False))
    text = "\n".join(rendered) + "\n"
    if trailing_partial:
        text += trailing_partial
    return text


def run_store_case(spec: dict) -> dict:
    root = WORK_ROOT / spec["label"]
    store = PromptHistoryStore(root / "history.jsonl")
    results: list[dict] = []
    for op in spec["ops"]:
        kind = op["op"]
        if kind == "append":
            results.append({
                "op": kind,
                **outcome(lambda op=op: store.append(
                    display=op["display"],
                    project=Path(expand(op["project"])),
                    session_id=op["session_id"],
                    now=datetime.fromisoformat(op["now"]),
                )),
            })
        elif kind == "search":
            results.append({
                "op": kind,
                **outcome(lambda op=op: store.search(
                    project=Path(expand(op["project"])) if op.get("project") else None,
                    session_id=op.get("session_id"),
                    query=op.get("query", ""),
                    limit=op["limit"],
                )),
            })
        elif kind == "read":
            entries, diagnostics = store.read_entries_with_diagnostics()
            results.append({
                "op": kind,
                "ok": True,
                "value": {
                    "entries": mask_value(entries),
                    "diagnostics": flatten_json_error(
                        [item.to_dict() for item in diagnostics]
                    ),
                },
            })
        elif kind == "write_file":
            store.root.mkdir(parents=True, exist_ok=True)
            store.path.write_text(op["text"], encoding="utf-8")
            results.append({"op": kind, "ok": True, "value": None})
        else:
            raise ValueError(f"未知的 op：{kind}")

    text = ""
    if store.path.exists():
        text = mask(store.path.read_text(encoding="utf-8"))
    return {"label": spec["label"], "ops": spec["ops"], "results": results, "file": text}


def main() -> None:
    module_path = Path(prompt_module.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    if WORK_ROOT.exists():
        shutil.rmtree(WORK_ROOT, ignore_errors=True)
    (WORK_ROOT / "proj").mkdir(parents=True, exist_ok=True)
    (WORK_ROOT / "other").mkdir(parents=True, exist_ok=True)

    payload = {
        "source": "omnicrawl/state/prompt_history.py",
        "json_error_placeholder": JSON_ERROR_PLACEHOLDER,
        "pure": pure_cases(),
        "store": [run_store_case(spec) for spec in store_cases()],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}")
    shutil.rmtree(WORK_ROOT, ignore_errors=True)


if __name__ == "__main__":
    main()
