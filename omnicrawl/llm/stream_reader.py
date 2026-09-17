"""可放弃的模型流读取线程。

HTTP/2 下关闭单个 stream 不会中断阻塞在 socket 读上的迭代（见
``omnicrawl/net/http_client.py`` 对 ``http2`` 的说明），直接迭代会把整个回合
卡在网络读取里，直到下一个数据包到达或连接断开。

这里把建连与迭代放进后台线程，用队列把事件转发给回合线程：消费方每
``poll_seconds`` 重新检查一次取消状态，取消后立即返回；被放弃的读取线程在收到
下一个事件、或底层连接被取消路径关闭时自行退出。
"""

from __future__ import annotations

import queue
import threading
from typing import Any, Callable, Iterator, Optional

# 取消检查间隔：与审批等待的轮询节奏一致，决定取消后的最坏感知延迟。
DEFAULT_POLL_SECONDS = 0.05

_ITEM = "item"
_END = "end"
_ERROR = "error"


def interruptible_stream_events(
    open_stream: Callable[[threading.Event], Iterator[Any]],
    *,
    cancel_check: Optional[Callable[[], None]] = None,
    poll_seconds: float = DEFAULT_POLL_SECONDS,
) -> Iterator[Any]:
    """在后台线程驱动 ``open_stream``，把事件转发给当前线程。

    ``open_stream`` 收到一个「已放弃」信号：读取线程在建连后与每次产出前检查它，
    被放弃时立刻停止产出并关闭刚建立的流，避免取消后留下未关闭的响应。
    """

    events: "queue.Queue[tuple[str, Any]]" = queue.Queue()
    abandoned = threading.Event()

    def run() -> None:
        try:
            for item in open_stream(abandoned):
                if abandoned.is_set():
                    break
                events.put((_ITEM, item))
            events.put((_END, None))
        except BaseException as exc:  # noqa: BLE001 - 原样交给消费线程
            events.put((_ERROR, exc))

    reader = threading.Thread(
        target=run,
        name="omnicrawl-stream-reader",
        daemon=True,
    )
    reader.start()
    try:
        while True:
            try:
                kind, payload = events.get(timeout=poll_seconds)
            except queue.Empty:
                if cancel_check is not None:
                    cancel_check()
                continue
            if kind == _ITEM:
                yield payload
            elif kind == _END:
                return
            else:
                raise payload
    finally:
        abandoned.set()


__all__ = ["DEFAULT_POLL_SECONDS", "interruptible_stream_events"]
