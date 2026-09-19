#!/usr/bin/env python3
"""生成飞书入站队列的对照数据集：期望值全部来自 Python 真实现。

每个用例给出一份「动作脚本」（入队 / 确认 / 回放 / 重启 / 损坏文件），脚本真跑一遍
Python 的 `FeishuInbox`，记录每步结果与最终 `pending.jsonl` / `done.jsonl` / `state.json`
的字节。时钟可注入并随脚本推进，因此过期与紧凑化都能确定地重放。
"""

from __future__ import annotations

import json
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-connectors/tests/fixtures/feishu_inbox_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.connectors import feishu_inbox as inbox_module  # noqa: E402
from omnicrawl.connectors.feishu_inbox import FeishuInbox  # noqa: E402

WORK_ROOT = (
    Path(tempfile.gettempdir()) / f"omnicrawl-inbox-parity-{__import__('os').getpid()}"
).resolve()
FILE_NAMES = ("pending.jsonl", "done.jsonl", "state.json")


def op(kind: str, **extra) -> dict:
    return {"op": kind, **extra}


CASES = [
    {
        "label": "basic_cycle",
        "config": {"ttl": 86400.0, "max_records": 10000, "compact_keep": 2000},
        "ops": [
            op("enqueue", event_id="e1", dedupe_key="k1", payload={"text": "一"}),
            op("enqueue", event_id="e2", dedupe_key="k1", payload={"text": "重投"}),
            op("enqueue", event_id="  ", dedupe_key=None, payload={}),
            op("enqueue", event_id="e3", dedupe_key=None, payload={"text": "三"}),
            op("pending_count"),
            op("count_done"),
            op("recover"),
            op("is_duplicate", key="k1"),
            op("is_duplicate", key="e3"),
            op("is_duplicate", key="  "),
            op("confirm", key="k1"),
            op("pending_count"),
            op("recover"),
            op("close"),
        ],
    },
    {
        "label": "restart_replay",
        "config": {"ttl": 86400.0, "max_records": 10000, "compact_keep": 2000},
        "ops": [
            op("enqueue", event_id="e1", dedupe_key="k1", payload={"text": "一"}),
            op("enqueue", event_id="e2", dedupe_key="k2", payload={"text": "二"}),
            op("close"),
            op("reopen"),
            op("pending_count"),
            op("count_done"),
            op("recover"),
            op("confirm", key="k2"),
            op("close"),
        ],
    },
    {
        "label": "expiry_window",
        "config": {"ttl": 100.0, "max_records": 10000, "compact_keep": 2000},
        "ops": [
            op("set_now", value=1000.0),
            op("enqueue", event_id="e1", dedupe_key="k1", payload={}),
            op("set_now", value=1050.0),
            op("enqueue", event_id="e2", dedupe_key="k2", payload={}),
            op("close"),
            op("set_now", value=1120.0),
            op("reopen"),
            op("pending_count"),
            op("count_done"),
            op("is_duplicate", key="k1"),
            op("is_duplicate", key="k2"),
            op("close"),
        ],
    },
    {
        "label": "compact_on_load",
        "config": {"ttl": 86400.0, "max_records": 3, "compact_keep": 2},
        "ops": [
            op("set_now", value=1000.0),
            op("enqueue", event_id="e0", dedupe_key="k0", payload={}),
            op("set_now", value=1001.0),
            op("enqueue", event_id="e1", dedupe_key="k1", payload={}),
            op("set_now", value=1002.0),
            op("enqueue", event_id="e2", dedupe_key="k2", payload={}),
            op("set_now", value=1003.0),
            op("enqueue", event_id="e3", dedupe_key="k3", payload={}),
            op("count_done"),
            op("close"),
            op("reopen"),
            op("count_done"),
            op("is_duplicate", key="k3"),
            op("is_duplicate", key="k1"),
            op("close"),
        ],
    },
    {
        "label": "memory_only",
        "config": {"ttl": 86400.0, "max_records": 10000, "compact_keep": 2000, "memory": True},
        "ops": [
            op("memory_only"),
            op("enqueue", event_id="e1", dedupe_key="k1", payload={}),
            op("enqueue", event_id="e1", dedupe_key="k1", payload={}),
            op("pending_count"),
            op("recover"),
            op("close"),
        ],
    },
    {
        "label": "corrupted_pending",
        "config": {"ttl": 86400.0, "max_records": 10000, "compact_keep": 2000},
        "ops": [
            op("set_now", value=1000.0),
            op("write_file", name="pending.jsonl", text=(
                '{"version":1,"seq":1,"event_id":"e1","dedupe_key":"k1","created_at":1000.0,'
                '"payload":{}}\n'
                "{not json\n"
                '{"version":9,"seq":2,"event_id":"e2","dedupe_key":"k2","created_at":1000.0,'
                '"payload":{}}\n'
                '{"version":1,"seq":3,"event_id":"","dedupe_key":"k3","created_at":1000.0,'
                '"payload":{}}\n'
                '{"version":1,"seq":4,"event_id":"e4","dedupe_key":"","created_at":1000.0,'
                '"payload":{}}\n'
                '{"version":1,"seq":5,"event_id":"e5","dedupe_key":"k5","created_at":900.0,'
                '"payload":{"text":"旧"}}\n'
                '{"version":1,"seq":6,"event_id":"e6","dedupe_key":"k6","created_at":1000.0,'
                '"payload":"不是对象"}\n'
            )),
            op("write_file", name="done.jsonl", text=(
                '{"key":"k1","ts":1000.0}\n'
                '{"key":"k1","ts":900.0}\n'
                "{not json\n"
                '{"key":"  ","ts":1000.0}\n'
                '{"key":"old","ts":100.0}\n'
            )),
            op("write_file", name="state.json", text='{"seq": 4}'),
            op("reopen"),
            op("pending_count"),
            op("count_done"),
            op("recover"),
            op("is_duplicate", key="k1"),
            op("is_duplicate", key="old"),
            op("close"),
        ],
    },
]


class Clock:
    def __init__(self) -> None:
        self.value = 1000.0

    def __call__(self) -> float:
        return self.value


def record_to_dict(record) -> dict:
    return {
        "seq": record.seq,
        "event_id": record.event_id,
        "dedupe_key": record.dedupe_key,
        "payload": record.payload,
        "created_at": record.created_at,
        "version": record.version,
    }


def run_case(spec: dict) -> dict:
    root = WORK_ROOT / spec["label"]
    if root.exists():
        shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True, exist_ok=True)

    config = spec["config"]
    clock = Clock()
    memory = bool(config.get("memory"))

    def build() -> FeishuInbox:
        return FeishuInbox(
            root=None if memory else root,
            dedup_ttl_seconds=config["ttl"],
            max_records=config["max_records"],
            compact_keep=config["compact_keep"],
            now=clock,
        )

    inbox = build()
    results: list[dict] = []
    for step in spec["ops"]:
        kind = step["op"]
        if kind == "enqueue":
            results.append({
                "op": kind,
                "value": inbox.enqueue(
                    event_id=step["event_id"],
                    dedupe_key=step.get("dedupe_key"),
                    payload=step.get("payload"),
                ),
            })
        elif kind == "confirm":
            inbox.confirm(step["key"])
            results.append({"op": kind, "value": None})
        elif kind == "recover":
            results.append({"op": kind, "value": [record_to_dict(r) for r in inbox.recover()]})
        elif kind == "is_duplicate":
            results.append({"op": kind, "value": inbox.is_duplicate(step["key"])})
        elif kind == "pending_count":
            results.append({"op": kind, "value": inbox.pending_count})
        elif kind == "count_done":
            results.append({"op": kind, "value": len(inbox._done)})
        elif kind == "memory_only":
            results.append({"op": kind, "value": inbox.memory_only})
        elif kind == "set_now":
            clock.value = step["value"]
            results.append({"op": kind, "value": None})
        elif kind == "close":
            inbox.close()
            results.append({"op": kind, "value": None})
        elif kind == "reopen":
            inbox.close()
            inbox = build()
            results.append({"op": kind, "value": None})
        elif kind == "write_file":
            (root / step["name"]).write_text(step["text"], encoding="utf-8")
            results.append({"op": kind, "value": None})
        else:
            raise ValueError(f"未知的 op：{kind}")

    inbox.close()
    files = {}
    for name in FILE_NAMES:
        path = root / name
        files[name] = path.read_text(encoding="utf-8") if path.exists() else None

    return {"label": spec["label"], "config": config, "ops": spec["ops"], "results": results,
            "files": files}


def main() -> None:
    module_path = Path(inbox_module.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    if WORK_ROOT.exists():
        shutil.rmtree(WORK_ROOT, ignore_errors=True)
    payload = [run_case(spec) for spec in CASES]
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}（{len(payload)} 例）")
    shutil.rmtree(WORK_ROOT, ignore_errors=True)


if __name__ == "__main__":
    main()
