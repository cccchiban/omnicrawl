"""本地测试公共 fixture。

Python 3.9 的 asyncio.run()（Textual App.run_test 内部使用）结束后会把
主线程 event loop 置空并标记为"已显式设置过"，之后直接构造 Textual
Widget（需要当前 event loop 建 RLock）会抛 RuntimeError。autouse fixture
在每个用例前确保主线程存在可用 event loop，消除测试文件之间的顺序依赖。
"""

from __future__ import annotations

import asyncio

import pytest


@pytest.fixture(autouse=True)
def _ensure_main_event_loop():
    try:
        asyncio.get_event_loop()
    except RuntimeError:
        asyncio.set_event_loop(asyncio.new_event_loop())
    yield
