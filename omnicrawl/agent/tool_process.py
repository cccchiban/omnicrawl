"""在独立子进程中运行普通同步工具调用。

Python 线程无法安全地强制终止：工具若阻塞在不可中断的 Python 调用中，父回合
即使收到 ESC 也只能设置取消标志。这里把一次普通工具调用封装为短生命周期的
Python 子进程，并把该子进程登记到当前回合的资源表。取消时由 Host 终止整个
进程树；工具的 finally 清理不属于取消保证的一部分。

调用协议使用长度前缀的序列化消息，子进程 stdout 同时被重定向，避免工具自身的
print 输出破坏控制协议。只使用 Python 标准库 pickle，生产工具应提供可导入的顶层
callable；绑定 Agent 状态或闭包的工具会由调用层识别并保留原有 Host 路径。
"""

from __future__ import annotations

import contextlib
import functools
import inspect
import io
import os
import pickle
import traceback
import struct
import subprocess
import sys
from pathlib import Path
from typing import Any, Callable

from .types import ToolResult
from ..llm.stream_registry import (
    current_stream_scope,
    registered_resource,
)
from ..workspace.process_control import (
    assign_process_to_kill_on_close_job,
    close_windows_handle,
    terminate_process_tree,
)


_MAX_MESSAGE_BYTES = 256 * 1024 * 1024
_FRAME = struct.Struct("!Q")


class ToolProcessError(RuntimeError):
    """普通同步工具子进程启动、协议或结果错误。"""


class ToolProcessSerializationError(ToolProcessError):
    """工具 callable/参数无法序列化，调用方可安全回退到进程内执行。"""


class ToolProcessCancelled(ToolProcessError):
    """工具子进程因当前回合取消而被强制终止。"""


class ToolProcessTimeout(ToolProcessError):
    """工具子进程超过回合工具超时。"""


def can_serialize_tool_runner(runner: Callable[[dict[str, Any]], ToolResult]) -> bool:
    """判断 runner 能否在子进程安全恢复，避免重复触发 Agent 钩子。"""

    if inspect.ismethod(runner) or inspect.isbuiltin(runner):
        # 绑定 Agent/MCP 方法可能携带锁、线程池、客户端或工作区状态；即使
        # 某些实例碰巧可 pickle，也不能把父进程状态复制成一个不一致的快照。
        return False
    if isinstance(runner, functools.partial):
        if not can_serialize_tool_runner(runner.func):
            return False
        return all(can_serialize_tool_value(value) for value in runner.args) and all(
            can_serialize_tool_value(key) and can_serialize_tool_value(value)
            for key, value in (runner.keywords or {}).items()
        )
    if not (inspect.isfunction(runner) and runner.__module__ not in {None, "__main__"}):
        return False
    try:
        _dumps(runner)
    except Exception:
        return False
    return True


def can_serialize_tool_value(value: Any) -> bool:
    """验证 partial 参数不会把不可复制的进程内状态带入工具子进程。"""

    try:
        _dumps(value)
    except Exception:
        return False
    return True


def run_tool_in_subprocess(
    runner: Callable[[dict[str, Any]], ToolResult],
    arguments: dict[str, Any],
    *,
    timeout_seconds: int,
    owner: object | None = None,
) -> ToolResult:
    """在独立 Python 子进程中执行 ``runner(arguments)``。

    ``runner`` 和参数会被序列化后发送给新的 Python 解释器，因此父线程不会执行
    目标 Python 代码。子进程及其后代纳入 Windows Job Object（其他平台使用独立
    session/进程组），并通过当前 stream scope 绑定到本轮 ESC 取消资源。
    """

    if not callable(runner):
        raise ToolProcessError("工具子进程 runner 不可调用。")
    try:
        payload = _dumps((runner, arguments))
    except Exception as exc:  # noqa: BLE001 - 序列化边界统一包装
        raise ToolProcessSerializationError(f"工具无法转移到子进程：{exc}") from exc

    if len(payload) > _MAX_MESSAGE_BYTES:
        raise ToolProcessError("工具子进程请求过大，已拒绝执行。")

    process_environment = os.environ.copy()
    process_environment.update(
        {
            "PYTHONIOENCODING": "utf-8",
            "PYTHONUNBUFFERED": "1",
        }
    )
    popen_kwargs: dict[str, Any] = {
        "cwd": str(Path.cwd()),
        "env": process_environment,
        "stdin": subprocess.PIPE,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "shell": False,
    }
    if os.name == "nt":
        popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        popen_kwargs["start_new_session"] = True

    try:
        process = subprocess.Popen(
            [sys.executable, "-m", "omnicrawl.agent.tool_process"],
            **popen_kwargs,
        )
    except OSError as exc:
        raise ToolProcessError(f"工具子进程启动失败：{exc}") from exc

    job_handle = assign_process_to_kill_on_close_job(process)
    state = {"cancelled": False, "terminated": False}

    def terminate() -> None:
        nonlocal job_handle
        if state["terminated"]:
            return
        state["terminated"] = True
        state["cancelled"] = True
        terminate_process_tree(process, job_handle=job_handle, wait=False)
        job_handle = None

    resolved_owner = current_stream_scope() if owner is None else owner
    try:
        with registered_resource(
            process,
            owner=resolved_owner,
            close_callback=terminate,
        ):
            try:
                stdout, stderr = process.communicate(
                    input=_frame_message(payload),
                    timeout=max(1, int(timeout_seconds)),
                )
            except subprocess.TimeoutExpired as exc:
                state["terminated"] = True
                terminate_process_tree(process, job_handle=job_handle, wait=True)
                job_handle = None
                raise ToolProcessTimeout(
                    f"工具执行超过 {timeout_seconds} 秒，已终止。"
                ) from exc
    finally:
        close_windows_handle(job_handle)

    if state["cancelled"]:
        raise ToolProcessCancelled("用户取消当前任务，工具子进程及其子进程树已终止。")
    if process.returncode != 0:
        detail = _decode_stderr(stderr)
        raise ToolProcessError(
            f"工具子进程异常退出（退出码：{process.returncode}）"
            + (f"：{detail}" if detail else "。")
        )
    try:
        result_payload = _unframe_message(stdout)
        result = _loads(result_payload)
    except Exception as exc:  # noqa: BLE001
        detail = _decode_stderr(stderr)
        raise ToolProcessError(
            "工具子进程返回了无效结果"
            + (f"：{detail}" if detail else "。")
        ) from exc
    if not isinstance(result, ToolResult):
        raise ToolProcessError("工具子进程返回值不是 ToolResult。")
    return result


def _worker_main() -> int:
    try:
        payload = _unframe_message(sys.stdin.buffer.read())
        runner, arguments = _loads(payload)
        if not callable(runner) or not isinstance(arguments, dict):
            raise ToolProcessError("工具子进程请求格式错误。")
        stdout_capture = io.StringIO()
        stderr_capture = io.StringIO()
        with contextlib.redirect_stdout(stdout_capture), contextlib.redirect_stderr(stderr_capture):
            result = runner(arguments)
        if not isinstance(result, ToolResult):
            raise ToolProcessError("工具 runner 必须返回 ToolResult。")
        sys.stdout.buffer.write(_frame_message(_dumps(result)))
        sys.stdout.buffer.flush()
        return 0
    except BaseException as exc:  # noqa: BLE001 - 子进程必须把异常交给父进程
        try:
            sys.stderr.write(f"{type(exc).__name__}: {exc}\n{traceback.format_exc()}")
            sys.stderr.flush()
        except Exception:
            pass
        return 2


def _frame_message(payload: bytes) -> bytes:
    return _FRAME.pack(len(payload)) + payload


def _unframe_message(data: bytes) -> bytes:
    if len(data) < _FRAME.size:
        raise ToolProcessError("工具子进程消息不完整。")
    (size,) = _FRAME.unpack(data[: _FRAME.size])
    if size > _MAX_MESSAGE_BYTES or len(data) - _FRAME.size < size:
        raise ToolProcessError("工具子进程消息长度非法。")
    return data[_FRAME.size : _FRAME.size + size]


def _decode_stderr(value: bytes | str) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8", errors="replace").strip()[:2_000]
    return str(value or "").strip()[:2_000]


def _dumps(value: Any) -> bytes:
    return pickle.dumps(value, protocol=pickle.HIGHEST_PROTOCOL)


def _loads(value: bytes) -> Any:
    return pickle.loads(value)


if __name__ == "__main__":  # pragma: no cover - exercised by the parent process
    raise SystemExit(_worker_main())


__all__ = [
    "ToolProcessCancelled",
    "ToolProcessError",
    "ToolProcessSerializationError",
    "ToolProcessTimeout",
    "can_serialize_tool_runner",
    "can_serialize_tool_value",
    "run_tool_in_subprocess",
]
