#!/usr/bin/env python3
"""生成模型运行时管理器（runtime）的对照数据集。

期望值来自 Python 真实现：快照代次、回合占用、切换拒绝/放行、持久化失败与错误文案。

用法：``python rust/tools/gen_llm_runtime_manager_fixture.py``
输出：``rust/crates/omnicrawl-llm/tests/fixtures/llm_runtime_manager_parity.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-llm/tests/fixtures/llm_runtime_manager_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.llm import runtime as RT  # noqa: E402
from omnicrawl.llm.capabilities import ModelCapabilities  # noqa: E402
from omnicrawl.llm.errors import ModelError, ModelErrorCode  # noqa: E402
from omnicrawl.llm.protocol import ModelIdentity  # noqa: E402
from omnicrawl.llm.registry import ModelDescriptor, ProviderProfile  # noqa: E402

if not Path(RT.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")


class FakeRuntime:
    """只实现管理器会用到的 close()。"""

    def __init__(self, name: str) -> None:
        self.name = name
        self.closed = False

    def close(self) -> None:
        self.closed = True


def make_profile(model_id: str) -> ProviderProfile:
    return ProviderProfile(
        id=f"profile-{model_id}",
        provider="openai",
        base_url="https://api.example.com/v1",
        api_key="sk-x",
        default_protocol="openai_chat_completions",
    )


def make_descriptor(step: dict[str, object]) -> ModelDescriptor:
    model_id = str(step["model_id"])
    return ModelDescriptor(
        identity=ModelIdentity(
            profile_id=f"profile-{model_id}",
            provider="openai",
            protocol="openai_chat_completions",
            model_id=model_id,
        ),
        capabilities=ModelCapabilities(
            context_window_tokens=int(step.get("capabilities_window") or 0)
        ),
        context_window_tokens=int(step.get("context_window") or 0),
    )


def observe(manager: RT.ModelRuntimeManager) -> dict[str, object]:
    return {
        "generation": manager.generation,
        "model_id": manager.current_model_id(),
        "context_window": manager.current_context_window(),
        "has_active_turn": manager.has_active_turn,
        "has_snapshot": manager.active_snapshot is not None,
    }


TRACES = [
    {
        "name": "未初始化就取回合",
        "steps": [{"op": "acquire"}],
    },
    {
        "name": "启动与回合占用",
        "steps": [
            {"op": "bootstrap", "model_id": "gpt-5", "context_window": 1000},
            {"op": "observe"},
            {"op": "acquire"},
            {"op": "observe"},
            {"op": "release"},
            {"op": "observe"},
            {"op": "switch", "model_id": "claude", "context_window": 0, "capabilities_window": 200},
            {"op": "observe"},
        ],
    },
    {
        "name": "回合进行中拒绝切换",
        "steps": [
            {"op": "bootstrap", "model_id": "gpt-5", "context_window": 1000},
            {"op": "acquire"},
            {"op": "switch", "model_id": "claude", "context_window": 0},
            {"op": "observe"},
            {"op": "switch", "model_id": "claude", "context_window": 0, "allow_during_turn": True},
            {"op": "observe"},
        ],
    },
    {
        "name": "持久化失败保留旧模型",
        "steps": [
            {"op": "bootstrap", "model_id": "gpt-5", "context_window": 1000},
            {"op": "switch", "model_id": "claude", "context_window": 2048, "persist_fails": True},
            {"op": "observe"},
        ],
    },
    {
        "name": "窗口写回与关闭",
        "steps": [
            {"op": "bootstrap", "model_id": "gpt-5", "context_window": 1000},
            {"op": "set_window", "tokens": 4000},
            {"op": "observe"},
            {"op": "set_window", "tokens": 0},
            {"op": "close"},
            {"op": "observe"},
            {"op": "acquire"},
        ],
    },
]


def run_trace(trace: dict[str, object]) -> dict[str, object]:
    manager = RT.ModelRuntimeManager()
    facts: list[dict[str, object]] = []
    with mock.patch.object(
        RT, "build_runtime", lambda profile, descriptor: FakeRuntime(descriptor.model_id)
    ):
        for step in trace["steps"]:
            record: dict[str, object] = dict(step)
            op = step["op"]
            if op == "bootstrap":
                manager.bootstrap(make_profile(str(step["model_id"])), make_descriptor(step))
            elif op == "acquire":
                try:
                    manager.acquire_turn()
                except ModelError as exc:
                    record["error"] = exc.message
            elif op == "release":
                snapshot = manager.active_snapshot
                if snapshot is not None:
                    manager.release_turn(snapshot)
            elif op == "switch":
                persist = None
                if step.get("persist_fails"):
                    def persist() -> None:
                        raise ModelError(
                            code=ModelErrorCode.CONFIGURATION_ERROR,
                            message="保存模型选择失败。",
                        )

                try:
                    manager.switch(
                        make_profile(str(step["model_id"])),
                        make_descriptor(step),
                        persist=persist,
                        allow_during_turn=bool(step.get("allow_during_turn")),
                    )
                except ModelError as exc:
                    record["error"] = exc.message
                    record["error_code"] = exc.code.value
            elif op == "set_window":
                try:
                    manager.set_context_window_tokens(int(step["tokens"]))
                except ValueError as exc:
                    record["error"] = str(exc)
            elif op == "close":
                manager.close()
            elif op != "observe":
                raise AssertionError(op)
            record["state"] = observe(manager)
            facts.append(record)
    return {"name": trace["name"], "steps": facts}


def main() -> None:
    fixture = [run_trace(trace) for trace in TRACES]
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8", newline="\n") as handle:
        json.dump(fixture, handle, ensure_ascii=False, indent=1)
        handle.write("\n")
    print("wrote %s: %s" % (FIXTURE_PATH, len(fixture)))


if __name__ == "__main__":
    main()
