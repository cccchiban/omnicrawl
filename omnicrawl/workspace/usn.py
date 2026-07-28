"""Windows NTFS USN Journal 的最小只读适配器。

该模块只负责查询日志状态和读取 V2 记录，不修改卷、不创建 Journal。调用方在
权限不足、卷不是 NTFS、日志被截断或遇到未知记录版本时应回退到完整索引重建。
"""

from __future__ import annotations

import ctypes
import os
import struct
from ctypes import wintypes
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

FSCTL_QUERY_USN_JOURNAL = 0x000900F4
FSCTL_READ_USN_JOURNAL = 0x000900BB
GENERIC_READ = 0x80000000
FILE_SHARE_READ = 0x00000001
FILE_SHARE_WRITE = 0x00000002
FILE_SHARE_DELETE = 0x00000004
OPEN_EXISTING = 3
FILE_ATTRIBUTE_NORMAL = 0x00000080
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value
ERROR_HANDLE_EOF = 38
ERROR_JOURNAL_DELETE_IN_PROGRESS = 1178

USN_REASON_DATA_OVERWRITE = 0x00000001
USN_REASON_DATA_EXTEND = 0x00000002
USN_REASON_DATA_TRUNCATION = 0x00000004
USN_REASON_NAMED_DATA_OVERWRITE = 0x00000010
USN_REASON_NAMED_DATA_EXTEND = 0x00000020
USN_REASON_NAMED_DATA_TRUNCATION = 0x00000040
USN_REASON_FILE_CREATE = 0x00000100
USN_REASON_FILE_DELETE = 0x00000200
USN_REASON_EA_CHANGE = 0x00000400
USN_REASON_SECURITY_CHANGE = 0x00000800
USN_REASON_RENAME_OLD_NAME = 0x00001000
USN_REASON_RENAME_NEW_NAME = 0x00002000
USN_REASON_INDEXABLE_CHANGE = 0x00004000
USN_REASON_BASIC_INFO_CHANGE = 0x00008000
USN_REASON_HARD_LINK_CHANGE = 0x00010000
USN_REASON_COMPRESSION_CHANGE = 0x00020000
USN_REASON_ENCRYPTION_CHANGE = 0x00040000
USN_REASON_OBJECT_ID_CHANGE = 0x00080000
USN_REASON_REPARSE_POINT_CHANGE = 0x00100000
USN_REASON_STREAM_CHANGE = 0x00200000
USN_REASON_CLOSE = 0x80000000
USN_REASON_ALL = 0xFFFFFFFF


class UsnJournalError(RuntimeError):
    """USN Journal 不可读取或已无法从指定游标连续恢复。"""


@dataclass(frozen=True)
class UsnJournalState:
    journal_id: int
    first_usn: int
    next_usn: int
    lowest_valid_usn: int


@dataclass(frozen=True)
class UsnRecord:
    file_reference: int
    parent_reference: int
    usn: int
    reason: int
    file_attributes: int
    name: str

    @property
    def is_directory(self) -> bool:
        return bool(self.file_attributes & 0x10)


def is_ntfs_volume(path: Path) -> bool:
    """判断路径是否位于本地 NTFS 卷；网络路径和非 Windows 平台返回 False。"""

    if os.name != "nt":
        return False
    anchor = Path(path).resolve().anchor
    if len(anchor) < 2 or anchor[1] != ":":
        return False
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetVolumeInformationW.argtypes = [
        wintypes.LPCWSTR,
        wintypes.LPWSTR,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        ctypes.POINTER(wintypes.DWORD),
        ctypes.POINTER(wintypes.DWORD),
        wintypes.LPWSTR,
        wintypes.DWORD,
    ]
    kernel32.GetVolumeInformationW.restype = wintypes.BOOL
    volume_name = ctypes.create_unicode_buffer(261)
    filesystem_name = ctypes.create_unicode_buffer(261)
    serial = ctypes.c_uint32()
    maximum_component = ctypes.c_uint32()
    flags = ctypes.c_uint32()
    root = anchor if anchor.endswith("\\") else anchor + "\\"
    ok = kernel32.GetVolumeInformationW(
        ctypes.c_wchar_p(root),
        volume_name,
        len(volume_name),
        ctypes.byref(serial),
        ctypes.byref(maximum_component),
        ctypes.byref(flags),
        filesystem_name,
        len(filesystem_name),
    )
    return bool(ok) and filesystem_name.value.upper() == "NTFS"


class UsnJournalReader:
    """对单个本地 NTFS 卷执行短生命周期的只读 Journal 查询。"""

    def __init__(self, workspace_root: Path) -> None:
        root = Path(workspace_root).resolve()
        if not is_ntfs_volume(root):
            raise UsnJournalError("工作区不位于可读取的本地 NTFS 卷。")
        self.workspace_root = root
        self.volume = rf"\\.\{root.drive}"

    def query_state(self) -> UsnJournalState:
        with self._volume_handle() as handle:
            output = self._device_io_control(handle, FSCTL_QUERY_USN_JOURNAL, b"", 128)
        if len(output) < 32:
            raise UsnJournalError("USN Journal 状态数据不完整。")
        journal_id, first_usn, next_usn, lowest_valid_usn = struct.unpack_from(
            "<Qqqq", output, 0
        )
        return UsnJournalState(
            journal_id=journal_id,
            first_usn=first_usn,
            next_usn=next_usn,
            lowest_valid_usn=lowest_valid_usn,
        )

    def read_records(
        self, *, start_usn: int, journal_id: int, stop_usn: int,
    ) -> Iterator[UsnRecord]:
        """读取 `[start_usn, stop_usn)` 的 V2 记录，未知版本会被安全跳过。"""

        cursor = int(start_usn)
        if cursor >= stop_usn:
            return
        with self._volume_handle() as handle:
            while cursor < stop_usn:
                request = struct.pack(
                    "<qIIQQQ", cursor, USN_REASON_ALL, 0, 0, 0, int(journal_id),
                )
                output = self._device_io_control(
                    handle,
                    FSCTL_READ_USN_JOURNAL,
                    request,
                    1024 * 1024,
                    allow_eof=True,
                )
                if len(output) < 8:
                    break
                next_cursor = struct.unpack_from("<q", output, 0)[0]
                offset = 8
                while offset + 8 <= len(output):
                    record_length, major_version, _minor_version = struct.unpack_from(
                        "<IHH", output, offset
                    )
                    if record_length < 8 or offset + record_length > len(output):
                        raise UsnJournalError("USN Journal 记录边界无效。")
                    if major_version == 2 and record_length >= 60:
                        (file_reference, parent_reference, usn,) = struct.unpack_from(
                            "<QQq", output, offset + 8
                        )
                        reason = struct.unpack_from("<I", output, offset + 40)[0]
                        file_attributes = struct.unpack_from("<I", output, offset + 52)[
                            0
                        ]
                        name_length, name_offset = struct.unpack_from(
                            "<HH", output, offset + 56
                        )
                        name_start = offset + name_offset
                        name_end = name_start + name_length
                        if name_end <= offset + record_length:
                            name = output[name_start:name_end].decode(
                                "utf-16-le", errors="replace"
                            )
                            if usn < stop_usn:
                                yield UsnRecord(
                                    file_reference=file_reference,
                                    parent_reference=parent_reference,
                                    usn=usn,
                                    reason=reason,
                                    file_attributes=file_attributes,
                                    name=name,
                                )
                    offset += record_length
                if next_cursor <= cursor:
                    break
                cursor = next_cursor

    class _HandleContext:
        def __init__(self, handle: int, close_handle) -> None:
            self.handle = handle
            self._close_handle = close_handle

        def __enter__(self) -> int:
            return self.handle

        def __exit__(self, _exc_type, _exc, _traceback) -> None:
            self._close_handle(self.handle)

    def _volume_handle(self) -> _HandleContext:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.CreateFileW.argtypes = [
            wintypes.LPCWSTR,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.LPVOID,
            wintypes.DWORD,
            wintypes.DWORD,
            wintypes.HANDLE,
        ]
        kernel32.CreateFileW.restype = wintypes.HANDLE
        kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
        kernel32.CloseHandle.restype = wintypes.BOOL
        handle = kernel32.CreateFileW(
            ctypes.c_wchar_p(self.volume),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        if handle == INVALID_HANDLE_VALUE:
            error = ctypes.get_last_error()
            raise UsnJournalError(f"打开 NTFS 卷失败（WinError {error}）。")
        return self._HandleContext(handle, kernel32.CloseHandle)

    @staticmethod
    def _device_io_control(
        handle: int,
        control_code: int,
        input_data: bytes,
        output_size: int,
        *,
        allow_eof: bool = False,
    ) -> bytes:
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.DeviceIoControl.argtypes = [
            wintypes.HANDLE,
            wintypes.DWORD,
            wintypes.LPVOID,
            wintypes.DWORD,
            wintypes.LPVOID,
            wintypes.DWORD,
            ctypes.POINTER(wintypes.DWORD),
            wintypes.LPVOID,
        ]
        kernel32.DeviceIoControl.restype = wintypes.BOOL
        output = ctypes.create_string_buffer(output_size)
        returned = ctypes.c_uint32()
        input_buffer = ctypes.create_string_buffer(input_data) if input_data else None
        ok = kernel32.DeviceIoControl(
            handle,
            control_code,
            input_buffer,
            len(input_data),
            output,
            output_size,
            ctypes.byref(returned),
            None,
        )
        if not ok:
            error = ctypes.get_last_error()
            if allow_eof and error in {
                ERROR_HANDLE_EOF,
                ERROR_JOURNAL_DELETE_IN_PROGRESS,
            }:
                return b""
            raise UsnJournalError(
                f"读取 USN Journal 失败（WinError {error}，控制码 {control_code:#x}）。"
            )
        return output.raw[: returned.value]


__all__ = [
    "USN_REASON_BASIC_INFO_CHANGE",
    "USN_REASON_DATA_EXTEND",
    "USN_REASON_DATA_OVERWRITE",
    "USN_REASON_DATA_TRUNCATION",
    "USN_REASON_FILE_CREATE",
    "USN_REASON_FILE_DELETE",
    "USN_REASON_RENAME_NEW_NAME",
    "USN_REASON_RENAME_OLD_NAME",
    "UsnJournalError",
    "UsnJournalReader",
    "UsnJournalState",
    "UsnRecord",
    "is_ntfs_volume",
]
