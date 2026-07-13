"""跨平台进程树控制辅助。

Windows 使用 Job Object 的 Kill-On-Job-Close，保证后台任务及其子进程在句柄关闭时
被回收；非 Windows 平台返回空句柄，由调用方退回会话/进程组终止策略。
"""

from __future__ import annotations

import ctypes
import os
import subprocess
from ctypes import wintypes


JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS = 9
JOB_OBJECT_LIMIT_KILL_ON_CLOSE = 0x00002000


class _JobObjectBasicLimitInformation(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_longlong),
        ("PerJobUserTimeLimit", ctypes.c_longlong),
        ("LimitFlags", wintypes.DWORD),
        ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t),
        ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t),
        ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _IoCounters(ctypes.Structure):
    _fields_ = [
        ("ReadOperationCount", ctypes.c_ulonglong),
        ("WriteOperationCount", ctypes.c_ulonglong),
        ("OtherOperationCount", ctypes.c_ulonglong),
        ("ReadTransferCount", ctypes.c_ulonglong),
        ("WriteTransferCount", ctypes.c_ulonglong),
        ("OtherTransferCount", ctypes.c_ulonglong),
    ]


class _JobObjectExtendedLimitInformation(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _JobObjectBasicLimitInformation),
        ("IoInfo", _IoCounters),
        ("ProcessMemoryLimit", ctypes.c_size_t),
        ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t),
        ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


def assign_process_to_kill_on_close_job(process: subprocess.Popen[str]) -> int | None:
    """把 Windows 后台任务树纳入 Job Object，关闭句柄时强制回收。

    `taskkill` 在受限桌面会话中可能没有足够权限，即使目标是当前进程启动的
    子进程。Job Object 直接使用当前进程持有的句柄，不依赖按 PID 重新查找进程；
    成功加入后，后续由 cmd、Bash 或 PowerShell 再启动的子进程也会自动归属该 Job。
    """

    if os.name != "nt":
        return None
    try:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateJobObjectW.argtypes = [wintypes.LPVOID, wintypes.LPCWSTR]
        kernel32.CreateJobObjectW.restype = wintypes.HANDLE
        kernel32.SetInformationJobObject.argtypes = [
            wintypes.HANDLE,
            wintypes.INT,
            wintypes.LPVOID,
            wintypes.DWORD,
        ]
        kernel32.SetInformationJobObject.restype = wintypes.BOOL
        kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
        kernel32.AssignProcessToJobObject.restype = wintypes.BOOL

        handle = kernel32.CreateJobObjectW(None, None)
        if not handle:
            return None

        info = _JobObjectExtendedLimitInformation()
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_CLOSE
        configured = kernel32.SetInformationJobObject(
            handle,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
            ctypes.byref(info),
            ctypes.sizeof(info),
        )
        assigned = configured and kernel32.AssignProcessToJobObject(handle, process._handle)
        if assigned:
            return int(handle)
        close_windows_handle(int(handle))
    except (AttributeError, OSError):
        return None
    return None


def close_windows_handle(handle: int | None) -> None:
    if handle is None or os.name != "nt":
        return
    try:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        kernel32.CloseHandle(handle)
    except (AttributeError, OSError):
        return


# 兼容 Monitor/WorkspaceTools 既有私有函数名与 monkeypatch 点。
_assign_process_to_kill_on_close_job = assign_process_to_kill_on_close_job
_close_windows_handle = close_windows_handle


__all__ = [
    "JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS",
    "JOB_OBJECT_LIMIT_KILL_ON_CLOSE",
    "assign_process_to_kill_on_close_job",
    "close_windows_handle",
]
