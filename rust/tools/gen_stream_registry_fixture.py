#!/usr/bin/env python3
"""生成 stream_registry 对照数据集：期望值全部来自 Python 真实现。

轨迹是「脚本化操作 → 结果序列」：每个用例给出若干 op（注册 / 注销 / 计数 / 关闭 /
进出作用域），脚本真跑一遍 Python 注册表，记录每步的返回值与关闭顺序，Rust 侧按同一
脚本重放再逐字段比对。这样钉住的是行为而不是我对语义的理解。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/stream_registry_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.llm import stream_registry  # noqa: E402

TARGETS = {"streams": "stream", "resources": "resource"}


class FakeStream:
    """带 close 方法的资源，对应 Rust 侧 `CancelHandle::with_close`。"""

    def __init__(self, name: str, log: list[str]) -> None:
        self._name = name
        self._log = log

    def close(self) -> None:
        self._log.append(self._name)


def case(label: str, handles: dict[str, str], ops: list[dict]) -> dict:
    return {"label": label, "handles": handles, "ops": ops}


CASES = [
    case(
        "register_close_unregister_cycle",
        {"s1": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams"},
            {"op": "close", "target": "streams"},
            {"op": "count", "target": "streams"},
        ],
    ),
    case(
        "handle_without_close_is_not_counted",
        {"s1": "bare"},
        [
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams"},
            {"op": "close", "target": "streams"},
            {"op": "count", "target": "streams"},
        ],
    ),
    case(
        "none_handle_is_ignored",
        {},
        [
            {"op": "register", "target": "streams", "handle": None},
            {"op": "count", "target": "streams"},
            {"op": "count", "target": "resources"},
        ],
    ),
    case(
        "scope_owner_is_inherited_and_kept",
        {"s1": "bare"},
        [
            {"op": "enter_scope", "owner": "turnA"},
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams", "owner": "turnA"},
            {"op": "exit_scope"},
            {"op": "count", "target": "streams"},
            {"op": "count", "target": "streams", "owner": "turnA"},
            {"op": "close", "target": "streams", "owner": "turnA"},
        ],
    ),
    case(
        "explicit_owner_beats_current_scope",
        {"s1": "bare", "s2": "bare"},
        [
            {"op": "enter_scope", "owner": "turnA"},
            {"op": "register", "target": "streams", "handle": "s1", "owner": "turnB"},
            {"op": "register", "target": "streams", "handle": "s2"},
            {"op": "count", "target": "streams", "owner": "turnA"},
            {"op": "count", "target": "streams", "owner": "turnB"},
            {"op": "exit_scope"},
        ],
    ),
    case(
        "close_by_owner_keeps_other_owner",
        {"s1": "close", "s2": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1", "owner": "turnA"},
            {"op": "register", "target": "streams", "handle": "s2", "owner": "turnB"},
            {"op": "close", "target": "streams", "owner": "turnA"},
            {"op": "count", "target": "streams"},
            {"op": "close", "target": "streams"},
            {"op": "count", "target": "streams"},
        ],
    ),
    case(
        "close_all_closes_every_owner",
        {"s1": "close", "s2": "close", "r1": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1", "owner": "turnA"},
            {"op": "register", "target": "streams", "handle": "s2", "owner": "turnB"},
            {"op": "register", "target": "resources", "handle": "r1"},
            {"op": "close", "target": "streams"},
            {"op": "close", "target": "resources"},
        ],
    ),
    case(
        "explicit_callback_wins_over_resource_close",
        {"s1": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1", "callback": True},
            {"op": "close", "target": "streams"},
        ],
    ),
    case(
        "duplicate_registration_keeps_one_entry",
        {"s1": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams"},
            {"op": "close", "target": "streams"},
        ],
    ),
    case(
        "unregister_removes_all_matches",
        {"s1": "close"},
        [
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "register", "target": "streams", "handle": "s1", "owner": "turnA"},
            {"op": "count", "target": "streams"},
            {"op": "unregister", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams"},
            {"op": "close", "target": "streams"},
        ],
    ),
    case(
        "resources_tracked_separately",
        {"s1": "close", "r1": "close", "r2": "bare"},
        [
            {"op": "register", "target": "streams", "handle": "s1"},
            {"op": "register", "target": "resources", "handle": "r1"},
            {"op": "register", "target": "resources", "handle": "r2"},
            {"op": "count", "target": "streams"},
            {"op": "count", "target": "resources"},
            {"op": "close", "target": "resources"},
            {"op": "count", "target": "resources"},
            {"op": "count", "target": "streams"},
        ],
    ),
    case(
        "unregister_unknown_handle_is_noop",
        {"s1": "bare"},
        [
            {"op": "unregister", "target": "streams", "handle": "s1"},
            {"op": "count", "target": "streams"},
        ],
    ),
]


def make_handle(kind: str, name: str, log: list[str]) -> object:
    if kind == "close":
        return FakeStream(name, log)
    if kind == "bare":
        return object()
    raise ValueError(f"未知的 handle 类型：{kind}")


def run_case(spec: dict) -> dict:
    stream_registry.close_active_streams()
    stream_registry.close_active_resources()

    log: list[str] = []
    owners: dict[str, object] = {}
    handles: dict[str, object] = {}
    scopes: list = []
    results: list[dict] = []

    def owner_for(name: str | None):
        if name is None:
            return None
        if name not in owners:
            owners[name] = object()
        return owners[name]

    def handle_for(name: str | None):
        if name is None:
            return None
        if name not in handles:
            handles[name] = make_handle(spec["handles"][name], name, log)
        return handles[name]

    register = {
        "streams": stream_registry.register_stream,
        "resources": stream_registry.register_resource,
    }
    unregister = {
        "streams": stream_registry.unregister_stream,
        "resources": stream_registry.unregister_resource,
    }
    close = {
        "streams": stream_registry.close_active_streams,
        "resources": stream_registry.close_active_resources,
    }
    count = {
        "streams": stream_registry.active_stream_count,
        "resources": stream_registry.active_resource_count,
    }

    def record(op: str, value: object) -> None:
        results.append({"op": op, "value": value})

    try:
        for op in spec["ops"]:
            kind = op["op"]
            if kind == "register":
                handle = handle_for(op["handle"])
                callback = None
                if op.get("callback"):
                    callback = _callback_for(op["handle"], log)
                register[op["target"]](handle, owner=owner_for(op.get("owner")), close_callback=callback)
                record(kind, None)
            elif kind == "unregister":
                unregister[op["target"]](handle_for(op["handle"]))
                record(kind, None)
            elif kind == "count":
                record(kind, count[op["target"]](owner=owner_for(op.get("owner"))))
            elif kind == "close":
                record(kind, close[op["target"]](owner=owner_for(op.get("owner"))))
            elif kind == "enter_scope":
                manager = stream_registry.stream_scope(owner_for(op["owner"]))
                manager.__enter__()
                scopes.append(manager)
                record(kind, None)
            elif kind == "exit_scope":
                scopes.pop().__exit__(None, None, None)
                record(kind, None)
            else:
                raise ValueError(f"未知的 op：{kind}")
    finally:
        while scopes:
            scopes.pop().__exit__(None, None, None)
        tail_log = list(log)
        final = {
            "streams": stream_registry.active_stream_count(),
            "resources": stream_registry.active_resource_count(),
        }
        stream_registry.close_active_streams()
        stream_registry.close_active_resources()

    return {
        "label": spec["label"],
        "handles": spec["handles"],
        "ops": spec["ops"],
        "results": results,
        "log": tail_log,
        "final": final,
    }


def _callback_for(name: str, log: list[str]):
    def callback() -> None:
        log.append(f"cb:{name}")

    return callback


def main() -> None:
    module_path = Path(stream_registry.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    payload = {
        "source": "omnicrawl/llm/stream_registry.py",
        "cases": [run_case(spec) for spec in CASES],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}（{len(payload['cases'])} 例）")


if __name__ == "__main__":
    main()
