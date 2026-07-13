"""SessionStore 跨进程写锁与耐久写辅助。

SESSION-DEBT-001 剩余项：
- 同一 `.agent_sessions` 目录允许多进程（TUI / API）同时访问；
- 写路径通过跨平台文件锁互斥；
- 追加 JSONL 与原子替换 index 时显式 flush + fsync；
- 不改变现有 JSON/JSONL 字段格式，不引入第三方依赖。
"""

from __future__ import annotations

import os
import sys
import tempfile
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, TextIO

from .session_models import SessionStoreError


DEFAULT_LOCK_TIMEOUT_SECONDS = 30.0
DEFAULT_LOCK_POLL_SECONDS = 0.05
LOCK_FILE_NAME = ".session_store.lock"


@dataclass(frozen=True)
class DurableWritePolicy:
    """持久化写策略。

    fsync 默认开启：会话转录与索引是恢复真相源，宁可多一次磁盘同步。
    测试可关闭以减少 I/O 抖动。
    """

    fsync: bool = True
    lock_timeout_seconds: float = DEFAULT_LOCK_TIMEOUT_SECONDS
    lock_poll_seconds: float = DEFAULT_LOCK_POLL_SECONDS


class ProcessFileLock:
    """基于 lock 文件的跨平台互斥锁。

    - Windows：`msvcrt.locking`
    - POSIX：`fcntl.flock`
    - 同一进程内可重入，避免 `SessionStore` 写路径嵌套死锁
      （例如 `start_session` 持锁后调用 `append_event`）。
    """

    def __init__(
        self,
        path: Path,
        *,
        timeout_seconds: float = DEFAULT_LOCK_TIMEOUT_SECONDS,
        poll_seconds: float = DEFAULT_LOCK_POLL_SECONDS,
    ) -> None:
        self.path = path.resolve()
        self.timeout_seconds = max(0.1, float(timeout_seconds))
        self.poll_seconds = max(0.01, float(poll_seconds))
        self._thread_lock = threading.RLock()
        self._handle: TextIO | None = None
        self._depth = 0

    def acquire(self) -> None:
        self._thread_lock.acquire()
        if self._depth > 0:
            self._depth += 1
            return

        self.path.parent.mkdir(parents=True, exist_ok=True)
        deadline = time.monotonic() + self.timeout_seconds
        last_error: Exception | None = None
        while True:
            handle: TextIO | None = None
            try:
                # a+ 保证文件存在；锁住后写入持有者信息便于排障。
                handle = self.path.open("a+", encoding="utf-8")
                _acquire_os_lock(handle)
                handle.seek(0)
                handle.truncate()
                handle.write(f"pid={os.getpid()}\n")
                handle.flush()
                self._handle = handle
                self._depth = 1
                return
            except OSError as exc:
                last_error = exc
                if handle is not None:
                    try:
                        handle.close()
                    except OSError:
                        pass
                if time.monotonic() >= deadline:
                    break
                time.sleep(self.poll_seconds)
            except Exception:
                if handle is not None:
                    try:
                        handle.close()
                    except OSError:
                        pass
                self._thread_lock.release()
                raise

        self._thread_lock.release()
        detail = f"：{last_error}" if last_error is not None else ""
        raise SessionStoreError(
            f"获取会话存储写锁超时（{self.timeout_seconds:.1f}s）：{self.path}{detail}"
        )

    def release(self) -> None:
        if self._depth <= 0:
            return
        self._depth -= 1
        if self._depth > 0:
            self._thread_lock.release()
            return

        handle = self._handle
        self._handle = None
        try:
            if handle is not None:
                try:
                    _release_os_lock(handle)
                finally:
                    handle.close()
        finally:
            self._thread_lock.release()

    def __enter__(self) -> "ProcessFileLock":
        self.acquire()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.release()


_PROCESS_LOCKS: dict[Path, ProcessFileLock] = {}
_PROCESS_LOCKS_GUARD = threading.Lock()


def process_lock_for_root(
    root: Path,
    *,
    timeout_seconds: float = DEFAULT_LOCK_TIMEOUT_SECONDS,
    poll_seconds: float = DEFAULT_LOCK_POLL_SECONDS,
) -> ProcessFileLock:
    """同一会话根目录共享一份跨进程锁实例。"""

    resolved = root.resolve()
    with _PROCESS_LOCKS_GUARD:
        lock = _PROCESS_LOCKS.get(resolved)
        if lock is None:
            lock = ProcessFileLock(
                resolved / LOCK_FILE_NAME,
                timeout_seconds=timeout_seconds,
                poll_seconds=poll_seconds,
            )
            _PROCESS_LOCKS[resolved] = lock
        else:
            # 后创建实例沿用首次超时配置，避免同一路径上策略漂移。
            lock.timeout_seconds = max(lock.timeout_seconds, float(timeout_seconds))
            lock.poll_seconds = min(lock.poll_seconds, max(0.01, float(poll_seconds)))
        return lock


@contextmanager
def exclusive_session_write(
    root: Path,
    thread_lock: threading.RLock,
    *,
    timeout_seconds: float = DEFAULT_LOCK_TIMEOUT_SECONDS,
    poll_seconds: float = DEFAULT_LOCK_POLL_SECONDS,
) -> Iterator[None]:
    """先拿进程内锁，再拿跨进程文件锁。

    顺序固定：thread lock → file lock，避免两种锁交叉获取导致死锁。
    """

    process_lock = process_lock_for_root(
        root,
        timeout_seconds=timeout_seconds,
        poll_seconds=poll_seconds,
    )
    with thread_lock:
        with process_lock:
            yield


def append_text_line(path: Path, line: str, *, fsync: bool = True) -> None:
    """向 JSONL 追加一行，可选 fsync 到磁盘。"""

    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with path.open("a", encoding="utf-8", newline="\n") as file:
            file.write(line if line.endswith("\n") else line + "\n")
            file.flush()
            if fsync:
                os.fsync(file.fileno())
    except OSError as exc:
        raise SessionStoreError(f"写入文件失败：{path}，{exc}") from exc


def atomic_write_text(
    path: Path,
    text: str,
    *,
    fsync: bool = True,
    prefix: str | None = None,
    suffix: str = ".tmp",
) -> None:
    """同目录临时文件写入后原子替换。

    Windows 上 `Path.replace` 可覆盖已存在目标；POSIX 亦为原子 rename。
    """

    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    temp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            newline="\n",
            dir=path.parent,
            prefix=prefix or f"{path.stem}.",
            suffix=suffix,
            delete=False,
        ) as file:
            temp_path = Path(file.name)
            file.write(text)
            file.flush()
            if fsync:
                os.fsync(file.fileno())
        temp_path.replace(path)
        if fsync:
            _fsync_directory(path.parent)
    except OSError as exc:
        if temp_path is not None and temp_path.exists():
            try:
                temp_path.unlink()
            except OSError:
                pass
        raise SessionStoreError(f"原子写入失败：{path}，{exc}") from exc


def _fsync_directory(directory: Path) -> None:
    """尽量把目录项（rename 结果）刷到磁盘。Windows 上可能不支持，忽略。"""

    try:
        fd = os.open(str(directory), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(fd)
    except OSError:
        # Windows 目录句柄通常不能 fsync；忽略即可。
        return
    finally:
        os.close(fd)


def _acquire_os_lock(handle: TextIO) -> None:
    if sys.platform == "win32":
        import msvcrt

        # msvcrt.locking 要求锁定区域至少 1 字节；空文件先写占位。
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write("\0")
            handle.flush()
        handle.seek(0)
        msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        return

    import fcntl

    fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)


def _release_os_lock(handle: TextIO) -> None:
    if sys.platform == "win32":
        import msvcrt

        try:
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        except OSError:
            # 进程退出或句柄已失效时忽略解锁失败。
            return
        return

    import fcntl

    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    except OSError:
        return


__all__ = [
    "DEFAULT_LOCK_POLL_SECONDS",
    "DEFAULT_LOCK_TIMEOUT_SECONDS",
    "DurableWritePolicy",
    "LOCK_FILE_NAME",
    "ProcessFileLock",
    "append_text_line",
    "atomic_write_text",
    "exclusive_session_write",
    "process_lock_for_root",
]
