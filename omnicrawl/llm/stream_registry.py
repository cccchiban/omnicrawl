"""当前 Agent 回合的可取消模型流与外部资源注册表。"""

from __future__ import annotations

import threading
from contextvars import ContextVar, Token
from contextlib import contextmanager
from typing import Any, Callable, Iterator, Optional, Tuple


ResourceEntry = Tuple[Any, Optional[object], Optional[Callable[[], None]]]

_lock = threading.Lock()
_current_scope: ContextVar[Optional[object]] = ContextVar(
    "omnicrawl_stream_scope",
    default=None,
)
_active_streams: list[ResourceEntry] = []
_active_resources: list[ResourceEntry] = []


@contextmanager
def stream_scope(owner: object) -> Iterator[None]:
    """把当前线程创建的流和外部资源绑定到一个可独立取消的回合。"""

    token: Token[Optional[object]] = _current_scope.set(owner)
    try:
        yield
    finally:
        _current_scope.reset(token)


def current_stream_scope() -> Optional[object]:
    """返回当前线程的取消归属。"""

    return _current_scope.get()


def stream_owner_for(cancel_check: Any = None) -> Optional[object]:
    """从取消检查器或线程上下文取得资源的取消归属。"""

    owner = getattr(cancel_check, "stream_owner", None)
    return owner if owner is not None else current_stream_scope()


def register_stream(
    stream: Any,
    *,
    owner: Optional[object] = None,
    close_callback: Optional[Callable[[], None]] = None,
) -> None:
    """注册一个进行中的模型响应流，取消时需要关闭它。"""

    _register(_active_streams, stream, owner=owner, close_callback=close_callback)


def unregister_stream(stream: Any) -> None:
    """注销模型响应流，避免悬挂引用。"""

    _unregister(_active_streams, stream)


def registered_stream_events(
    stream: Any,
    *,
    owner: Optional[object] = None,
    close_callback: Optional[Callable[[], None]] = None,
) -> Iterator[Any]:
    """迭代模型流并保证正常结束、异常和关闭路径都会注销它。"""

    register_stream(stream, owner=owner, close_callback=close_callback)
    try:
        yield from stream
    finally:
        unregister_stream(stream)


def close_active_streams(*, owner: Optional[object] = None) -> int:
    """关闭指定回合的活跃模型流，返回成功关闭的数量。"""

    return _close_active(_active_streams, owner=owner)


def active_stream_count(*, owner: Optional[object] = None) -> int:
    """返回当前活跃模型流数量，可按回合归属过滤。"""

    return _count_active(_active_streams, owner=owner)


def register_resource(
    resource: Any,
    *,
    owner: Optional[object] = None,
    close_callback: Optional[Callable[[], None]] = None,
) -> None:
    """注册一个可被回合取消的外部资源，例如正在运行的子进程。"""

    _register(_active_resources, resource, owner=owner, close_callback=close_callback)


def unregister_resource(resource: Any) -> None:
    """注销外部资源。"""

    _unregister(_active_resources, resource)


@contextmanager
def registered_resource(
    resource: Any,
    *,
    owner: Optional[object] = None,
    close_callback: Optional[Callable[[], None]] = None,
) -> Iterator[Any]:
    """在资源使用期间注册其取消句柄，并在请求结束后注销。"""

    register_resource(resource, owner=owner, close_callback=close_callback)
    try:
        yield resource
    finally:
        unregister_resource(resource)


def close_active_resources(*, owner: Optional[object] = None) -> int:
    """关闭指定回合的可取消外部资源。"""

    return _close_active(_active_resources, owner=owner)


def active_resource_count(*, owner: Optional[object] = None) -> int:
    """返回当前可取消外部资源数量。"""

    return _count_active(_active_resources, owner=owner)


def _register(
    collection: list[ResourceEntry],
    resource: Any,
    *,
    owner: Optional[object],
    close_callback: Optional[Callable[[], None]],
) -> None:
    if resource is None:
        return
    resolved_owner = current_stream_scope() if owner is None else owner
    with _lock:
        if not any(existing is resource for existing, _, _ in collection):
            collection.append((resource, resolved_owner, close_callback))


def _unregister(collection: list[ResourceEntry], resource: Any) -> None:
    with _lock:
        collection[:] = [
            (existing, owner, close_callback)
            for existing, owner, close_callback in collection
            if existing is not resource
        ]


def _close_active(collection: list[ResourceEntry], *, owner: Optional[object]) -> int:
    with _lock:
        if owner is None:
            targets = list(collection)
        else:
            targets = [
                entry for entry in collection if entry[1] is owner
            ]

    closed = 0
    for resource, _resource_owner, close_callback in targets:
        try:
            close = close_callback or getattr(resource, "close", None)
            if callable(close):
                close()
                closed += 1
        except Exception:  # noqa: BLE001 - 资源关闭失败仍继续回收其余资源
            pass
        finally:
            _unregister(collection, resource)
    return closed


def _count_active(collection: list[ResourceEntry], *, owner: Optional[object]) -> int:
    with _lock:
        if owner is None:
            return len(collection)
        return sum(1 for _, resource_owner, _ in collection if resource_owner is owner)


__all__ = [
    "active_resource_count",
    "active_stream_count",
    "close_active_resources",
    "close_active_streams",
    "current_stream_scope",
    "register_resource",
    "register_stream",
    "registered_resource",
    "registered_stream_events",
    "stream_owner_for",
    "stream_scope",
    "unregister_resource",
    "unregister_stream",
]
