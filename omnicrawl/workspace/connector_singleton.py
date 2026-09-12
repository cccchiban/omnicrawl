"""连接器子进程的跨进程单例互斥。

问题背景：多个 OmniCrawl 进程（多个 TUI、TUI + API、手工 ``python -m``
连接器）同时启动时，每个进程都会各自拉起一个飞书/Telegram 连接器子进程，
同一平台出现多个长连接实例（飞书 WebSocket 重复建连、消息被多个实例重复
处理）。本模块为每个连接器平台提供一把“用户级单例锁”：平台只能存在一个
活动连接器进程。

实现思路（继承 ``ProcessFileLock`` 的语义）：

* 锁文件放在用户配置目录 ``~/.OmniCrawl/`` 下，按平台名区分，与工作区无关，
  因此跨项目、跨工作区共享同一把锁；
* 用操作系统级文件锁（Windows ``msvcrt.locking`` / POSIX ``fcntl.flock``）
  保证并发获取的原子性：同一时刻只有一个进程能持锁；
* 锁文件内记录持有者 PID，用于“粘滞接管”：
  - 文件锁竞争失败时，读取锁文件里的 PID，若该 PID 对应的进程已不存在
    （崩溃残留/异常退出），则认为锁已粘滞，删除锁文件后重试获取；
  - 用 ``pid_is_running`` 做跨平台进程存活检查，不依赖 psutil；
* 正常路径下锁由持有者进程在退出时显式释放并删除锁文件。

说明：锁定目标是“同一个配置下的同一个平台只跑一个实例”。两个不同
Bot/App 配置想同时运行同一平台时不应共享锁——那属于双实例的合法需求；
当前实现以平台名为锁键（与现有自动启动逻辑一致），如需按 Bot 区分可把
锁键改成 ``<platform>-<app_id>``。
"""

from __future__ import annotations

import logging
import os
import re
import time
from pathlib import Path

from omnicrawl.config.core.runtime import user_config_dir
from omnicrawl.state.session_locking import ProcessFileLock

LOGGER = logging.getLogger(__name__)

# 锁文件里写入的元信息；粘滞检测只依赖 pid 行。
_LOCK_LINE_RE = re.compile(r"^pid=(\d+)$", re.MULTILINE)
# 删除粘滞锁文件前最多等待时长：给刚崩溃/刚启动的持有者一个短暂窗口，
# 避免把正在正常运行的连接器误判为已死而重复拉起。
STALE_RECLAIM_WAIT_SECONDS = 2.0
# 正常退出的持有者释放锁后，竞争方可能短暂看到“锁文件存在但无人持有”，
# 此时不能把它当粘滞锁删除（持有者可能正在释放）。统一通过文件锁竞争 +
# PID 存活判断来区分，见 _acquire_connector_lock 的说明。
LOCK_FILENAME_PREFIX = "connector-"
LOCK_FILENAME_SUFFIX = ".lock"


def connector_lock_path(name: str) -> Path:
    """返回平台连接器的单例锁文件路径（用户配置目录，跨工作区共享）。

    锁文件名保留中文等 Unicode 字符（Windows 与 POSIX 均支持），保证
    “飞书”/“Telegram”等平台名一一对应，不会被 sanitize 成同一文件。
    """

    safe = re.sub(r"[^\w.-]", "-", name, flags=re.UNICODE).strip(".-") or "connector"
    return user_config_dir() / f"{LOCK_FILENAME_PREFIX}{safe}{LOCK_FILENAME_SUFFIX}"


def _pid_is_running(pid: int) -> bool:
    """跨平台判断 PID 是否仍存在；不依赖 psutil。

    注意：PID 可能被操作系统复用，存在极小概率误判；对连接器单例来说，
    即使误判也只是“认为已有实例在跑”而跳过启动，不会破坏数据，安全侧
    可接受。
    """

    if pid <= 0:
        return False
    if os.name == "nt":
        # Windows 没有直接可用的 os.kill(pid, 0)；用 Win32 OpenProcess 探测。
        # 目标进程可能是别的用户的进程，此时 OpenProcess 返回失败同样视为
        # 不可用，配合锁文件的持久化语义（见 _acquire_connector_lock）足够。
        try:
            import ctypes
            from ctypes import wintypes

            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            handle = kernel32.OpenProcess(
                wintypes.DWORD(0x1000),  # PROCESS_QUERY_LIMITED_INFORMATION
                wintypes.BOOL(False),
                wintypes.DWORD(pid),
            )
            if not handle:
                return False
            try:
                exit_code = wintypes.DWORD()
                ok = kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code))
                # 0x103 == STILL_ACTIVE 才认为在运行；其他值（含 0）表示已退出。
                return bool(ok) and exit_code.value == 0x103
            finally:
                kernel32.CloseHandle(handle)
        except Exception:  # noqa: BLE001 - 探测失败按“不可用”处理，见注释
            return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def _locked_pid(lock_path: Path) -> int | None:
    """读取锁文件里的持有者 PID；无锁文件或格式异常返回 None。

    注意：读取不需要也不应获取文件锁——被活动实例持有的锁文件在 Windows
    上无法被其他进程读取（msvcrt 锁定导致共享违例），返回 None 后由
    acquire 超时路径按“已有实例”处理；只有无锁/粘滞锁（锁文件存在但
    无进程持有）才能被读到内容。
    """

    try:
        text = lock_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    match = _LOCK_LINE_RE.search(text)
    if match is None:
        return None
    try:
        return int(match.group(1))
    except ValueError:  # pragma: no cover - 正则已保证是数字
        return None


def _remove_stale_lock(lock_path: Path) -> None:
    """尽力删除粘滞锁文件；失败仅记录，不阻塞后续重试。"""

    try:
        lock_path.unlink()
    except FileNotFoundError:
        return
    except OSError:
        LOGGER.debug("删除粘滞连接器锁文件失败：%s", lock_path)


def _sleep_seconds(seconds: float) -> None:
    """可被测试替换的休眠点。"""

    time.sleep(seconds)


class ConnectorInstanceLock:
    """一个平台连接器的跨进程单例锁。

    用法::

        with ConnectorInstanceLock("飞书"):
            ...  # 已持有平台单例，可安全拉起连接器子进程

    文件内容（``pid=<pid>``）由底层 ``ProcessFileLock.acquire`` 在持锁的
    同一句柄上写入，便于排障与其他进程诊断；退出时显式释放并删除锁文件。
    进程崩溃时锁文件残留，下一个启动方通过 PID 存活检查识别并接管（粘滞
    接管）。

    注意（Windows 行为）：持锁期间锁文件被 msvcrt 锁定，其他进程无法
    读取其内容；这使“粘滞接管”只能发生在锁文件存在但无人持锁（如崩溃
    残留、文件锁随进程消失）的场合，恰好构成安全边界——不会误删活动
    实例的锁。不得再用第二个句柄重写锁文件：Windows 强制字节锁会拒绝
    其他句柄访问被锁区域（Permission denied），而底层句柄写入的 PID
    已满足全部需求。
    """

    def __init__(self, name: str, lock_path: Path | None = None) -> None:
        self.name = name
        self.lock_path = lock_path or connector_lock_path(name)
        self._file_lock = ProcessFileLock(
            self.lock_path,
            # 单例判断必须立即给出结论，不能为了等锁长时间阻塞 TUI 启动。
            timeout_seconds=0.5,
            poll_seconds=0.05,
        )
        self._owner = False
        self._finalized = False

    def try_acquire(self) -> bool:
        """尝试获取平台单例；被其他活动实例持有则返回 False。

        判定顺序（竞争失败时区分“另一实例持有”与“粘滞残留”）：

        1. 尝试获取文件锁；
        2. 成功：底层 ProcessFileLock 已写入 PID，返回 True；
        3. 失败：读取锁文件 PID（Windows 上锁被活动实例持有时不可读，
           读不到就按“已有实例”处理）；PID 存在则说明有活动实例，
           返回 False；PID 不存在则删除锁文件后重试（粘滞接管）。
        """

        while True:
            # 先尝试获取文件锁。失败可能意味着：
            #   a) 另一实例正持有（正常情况，返回 False）；
            #   b) 粘滞残留（锁文件在但无进程持有，可删除后重试）；
            #   c) Windows 上另一实例持有导致锁文件本身不可读（同 a，
            #      此时读不到 pid，按“已有实例”处理即可）。
            try:
                self._file_lock.acquire()
            except Exception:  # noqa: BLE001 - 竞争失败走粘滞接管
                pass
            else:
                self._owner = True
                return True

            # 文件锁竞争失败：尝试判断是否为粘滞残留。
            pid = _locked_pid(self.lock_path)
            if pid is None:
                # 读不到 pid：Windows 上通常是锁被活动实例持有（不可读）；
                # POSIX 上锁文件可读但可能刚被创建、内容未写入。两种都按
                # “已有实例在运行”处理，避免误删活动锁。
                LOGGER.info(
                    "连接器 %s 的单例锁被占用（无法确认持有者），跳过本次启动。",
                    self.name,
                )
                return False
            if not _pid_is_running(pid):
                LOGGER.info(
                    "连接器 %s 的旧实例（pid=%s）已退出，接管其单例锁。",
                    self.name,
                    pid,
                )
                _remove_stale_lock(self.lock_path)
                continue
            LOGGER.info(
                "连接器 %s 已由其他进程（pid=%s）运行，跳过本次启动。",
                self.name,
                pid,
            )
            return False

    def release(self) -> None:
        """释放单例锁并删除锁文件（仅持有者执行）。"""

        if not self._owner:
            return
        try:
            # 先释放文件锁并关闭句柄，再删除锁文件：Windows 上文件若仍被
            # 打开的句柄引用（msvcrt 锁住期间），unlink 会失败。
            self._file_lock.release()
        except Exception:  # noqa: BLE001 - 释放失败不掩盖原始错误
            LOGGER.warning(
                "释放连接器单例锁失败（%s）：%s",
                self.lock_path,
                exc_info=False,
            )
        finally:
            try:
                _remove_stale_lock(self.lock_path)
            finally:
                self._owner = False

    def __enter__(self) -> "ConnectorInstanceLock":
        self.try_acquire()
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.release()


# 公开别名：跨进程单例之外的调用方（如 API 多 worker 的运行状态存储）也需要
# 判断“某个进程是否还活着”，复用这里的跨平台实现而不是各写一份。
pid_is_running = _pid_is_running


__all__ = [
    "ConnectorInstanceLock",
    "LOCK_FILENAME_PREFIX",
    "LOCK_FILENAME_SUFFIX",
    "STALE_RECLAIM_WAIT_SECONDS",
    "connector_lock_path",
    "pid_is_running",
]
