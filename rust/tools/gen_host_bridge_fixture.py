#!/usr/bin/env python3
"""生成宿主桥接契约 fixture：把 Python 侧真正的宿主接口（回调名与循环端口名）钉成数据。

供 Rust `omnicrawl-ipc` 的 parity 测试校验协议 v1 的覆盖度：内核声明的宿主事件方法必须与
`run_stream` 的回调一一对应，不能多也不能少。接口清单靠反射取得，不靠人工抄写。

用法：``python rust/tools/gen_host_bridge_fixture.py``
输出：``rust/crates/omnicrawl-ipc/tests/fixtures/host_bridge.json``
"""

from __future__ import annotations

import inspect
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-ipc/tests/fixtures/host_bridge.json"

# 必须加载仓库源码：已安装的 omnicrawl 在 site-packages，会对照到另一份实现。
sys.path.insert(0, str(ROOT))

import omnicrawl.agent.controllers.turn.loop as loop_module  # noqa: E402
import omnicrawl.agent.runtime.execution as execution_module  # noqa: E402


def assert_repo_source(module) -> None:
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")


def find_method(module, method_name: str, required_param: str):
    """按方法名与必需参数定位类方法，避免依赖被脱敏的类名。"""

    for _, obj in vars(module).items():
        if not inspect.isclass(obj) or obj.__module__ != module.__name__:
            continue
        func = getattr(obj, method_name, None)
        if func is None:
            continue
        if required_param in inspect.signature(func).parameters:
            return func
    raise SystemExit(f"{module.__name__} 里未找到带 {required_param} 的 {method_name}")


def params_of(func) -> list:
    return [
        name for name in inspect.signature(func).parameters if name not in ("self", "cls")
    ]


def main() -> None:
    for module in (loop_module, execution_module):
        assert_repo_source(module)

    run_stream = find_method(loop_module, "run_stream", "on_delta")
    loop_run = find_method(execution_module, "run", "execute_tool_batch")

    run_stream_params = params_of(run_stream)
    callbacks = sorted(
        name
        for name in run_stream_params
        if name.startswith("on_") or name == "cancel_check"
    )
    other_params = sorted(set(run_stream_params) - set(callbacks))

    ports = sorted(
        name
        for name in params_of(loop_run)
        if name
        in ("request_reply", "execute_tool_batch", "cancel_check", "stop_check")
    )

    fixture = {
        "source": {
            "run_stream": "omnicrawl/agent/controllers/turn/loop.py",
            "loop_run": "omnicrawl/agent/runtime/execution.py",
        },
        "run_stream": {
            "callbacks": callbacks,
            "other_params": other_params,
        },
        "loop_ports": ports,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}："
        f"回调 {len(callbacks)} 个、其他入参 {other_params}、循环端口 {ports}"
    )


if __name__ == "__main__":
    main()
