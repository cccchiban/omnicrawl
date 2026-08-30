"""TUI 启动时自动管理 Telegram 与飞书连接器子进程。

连接器本身各自拥有独立的 Agent、网络客户端和关闭流程，不应在线程中直接
嵌入 TUI 进程。这里仅负责三件事：

1. 无网络地读取两个连接器的本地配置，判断平台是否已配置；
2. 对已配置的平台启动 ``python -m`` 独立子进程；
3. 在 TUI 退出时终止子进程及其后代，避免留下轮询进程。

连接器的 App Secret、Bot Token 等凭证只由子进程从继承的环境变量或
``config.toml`` 读取，绝不出现在 ``Popen`` 的命令行参数中。
"""

from __future__ import annotations

import logging
import os
import subprocess
import sys
import threading
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from omnicrawl.state.session_artifacts import redact_sensitive_text
from omnicrawl.workspace.process_control import (
    assign_process_to_kill_on_close_job,
    close_windows_handle,
    terminate_process_tree,
)

from .fsapp import load_feishu_config
from .telegram import load_telegram_config


LOGGER = logging.getLogger(__name__)

# 默认自动启动；设置为 0/false/no/off 可在需要单独运行连接器或排障时关闭。
AUTO_START_ENV = "OMNICRAWL_AUTO_START_CONNECTORS"
_DISABLED_VALUES = frozenset({"0", "false", "no", "off", "disabled"})
_ENABLED_VALUES = frozenset({"1", "true", "yes", "on", "enabled"})

# 子进程已被要求自行退出时，等待其 watcher 线程收尾的上限。
WATCHER_JOIN_TIMEOUT_SECONDS = 5.0


@dataclass(frozen=True)
class ConnectorSpec:
    """一个可由 ``python -m`` 启动的消息平台连接器。"""

    name: str
    module: str


@dataclass
class _ManagedConnector:
    """监督器持有的子进程及其平台相关回收资源。"""

    spec: ConnectorSpec
    process: subprocess.Popen[Any]
    job_handle: int | None
    watcher: threading.Thread | None = None


_CONNECTOR_SPECS = (
    ConnectorSpec("Telegram", "omnicrawl.connectors.telegram"),
    ConnectorSpec("飞书", "omnicrawl.connectors.fsapp"),
)


class ConnectorProcessManager:
    """管理 TUI 自动拉起的连接器子进程。

    该类不负责连接器内部的网络通信，也不创建连接器 Agent。配置探测和
    ``Popen`` 失败均降级为警告；因此外部平台故障不会阻止本地 TUI 启动。
    """

    def __init__(
        self,
        workspace_root: Path,
        *,
        popen_factory: Callable[..., subprocess.Popen[Any]] = subprocess.Popen,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve()
        self._popen = popen_factory
        self._lock = threading.RLock()
        self._closed = False
        self._processes: list[_ManagedConnector] = []

    @property
    def started_connectors(self) -> tuple[str, ...]:
        """返回已成功调用 ``Popen`` 的平台名称，供启动诊断和测试使用。"""

        with self._lock:
            return tuple(entry.spec.name for entry in self._processes)

    def start(self) -> tuple[str, ...]:
        """探测配置并启动已配置的平台，返回成功启动的平台名称。"""

        if not _auto_start_enabled():
            LOGGER.info("已通过 %s 关闭 Telegram/飞书自动启动。", AUTO_START_ENV)
            return ()

        for spec, configured in _configured_connectors():
            if not configured:
                continue
            try:
                self._start_one(spec)
            except Exception as exc:  # noqa: BLE001 - 单个平台故障不能阻塞 TUI
                LOGGER.warning(
                    "%s 连接器自动启动异常，TUI 将继续运行：%s",
                    spec.name,
                    _safe_error_text(exc),
                )
        return self.started_connectors

    def close(self) -> None:
        """终止全部连接器进程及其子进程，并等待监督线程退出。

        关闭顺序先标记监督器、再发出进程树终止请求，最后等待 watcher；这样
        watcher 不会在主 Agent 已关闭后继续持有远程连接器资源，也不会因重复
        调用 ``close`` 而重复关闭 Windows Job Object 句柄。
        """

        with self._lock:
            if self._closed:
                return
            self._closed = True
            processes = tuple(self._processes)

        for entry in processes:
            # 句柄只能由 watcher 或 close() 其中一方取得，避免 Windows 下
            # 两个线程同时 CloseHandle 造成无效句柄或误关其他资源。
            job_handle = self._take_job_handle(entry)
            try:
                terminate_process_tree(
                    entry.process,
                    job_handle=job_handle,
                    wait=True,
                )
            except Exception as exc:  # noqa: BLE001 - 退出阶段不阻塞主流程
                LOGGER.warning(
                    "%s 连接器进程回收失败：%s",
                    entry.spec.name,
                    _safe_error_text(exc),
                )

        for entry in processes:
            watcher = entry.watcher
            if watcher is not None and watcher.is_alive():
                watcher.join(timeout=WATCHER_JOIN_TIMEOUT_SECONDS)
                if watcher.is_alive():
                    LOGGER.warning(
                        "%s 连接器监督线程未在 %.1f 秒内退出。",
                        entry.spec.name,
                        WATCHER_JOIN_TIMEOUT_SECONDS,
                    )

    def _start_one(self, spec: ConnectorSpec) -> None:
        """启动单个平台；锁住注册过程，避免 close 与 Popen 竞态。"""

        command = [sys.executable, "-m", spec.module]
        environment = _child_environment(self.workspace_root)
        popen_kwargs: dict[str, Any] = {
            "cwd": str(self.workspace_root),
            "env": environment,
            "stdin": subprocess.DEVNULL,
            # 连接器不直接向 Textual 终端写日志，避免破坏全屏界面；退出状态
            # 由 watcher 记录，详细日志仍可单独运行连接器查看。
            "stdout": subprocess.DEVNULL,
            "stderr": subprocess.DEVNULL,
        }
        if os.name == "nt":
            # Windows 没有 Unix 的 session/process group 语义；Job Object
            # 负责递归回收，CREATE_NEW_PROCESS_GROUP 作为兼容性兜底。
            popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            # 让 Unix 的 killpg 只作用于当前连接器及其后代，不误伤 TUI。
            popen_kwargs["start_new_session"] = True

        process: subprocess.Popen[Any] | None = None
        entry: _ManagedConnector | None = None
        try:
            with self._lock:
                if self._closed:
                    return
                process = self._popen(command, **popen_kwargs)
                try:
                    job_handle = assign_process_to_kill_on_close_job(process)
                except Exception as exc:  # noqa: BLE001 - 回收增强失败不阻断启动
                    LOGGER.warning(
                        "%s 连接器进程树保护初始化失败，将使用普通进程组回收：%s",
                        spec.name,
                        _safe_error_text(exc),
                    )
                    job_handle = None

                entry = _ManagedConnector(spec, process, job_handle)
                self._processes.append(entry)
                watcher = threading.Thread(
                    target=self._watch_process,
                    args=(entry,),
                    name=f"omnicrawl-{spec.name.lower()}-connector-watch",
                    daemon=True,
                )
                entry.watcher = watcher
                watcher.start()
        except (OSError, ValueError) as exc:
            LOGGER.warning(
                "%s 连接器自动启动失败，TUI 将继续运行：%s",
                spec.name,
                _safe_error_text(exc),
            )
        except Exception:
            # Popen 成功后若 watcher/注册过程异常，必须回收刚启动的进程；否则
            # 入口层虽会继续启动 TUI，却会遗留一个没有监督者的连接器孤儿。
            if process is not None:
                job_handle = None
                with self._lock:
                    if entry is not None and entry in self._processes:
                        self._processes.remove(entry)
                        job_handle = entry.job_handle
                        entry.job_handle = None
                try:
                    terminate_process_tree(process, job_handle=job_handle, wait=True)
                except Exception as cleanup_exc:  # noqa: BLE001
                    LOGGER.warning(
                        "%s 连接器异常后的进程回收失败：%s",
                        spec.name,
                        _safe_error_text(cleanup_exc),
                    )
            raise

    def _watch_process(self, entry: _ManagedConnector) -> None:
        """记录连接器意外退出并释放自然退出后的 Windows 句柄。"""

        try:
            return_code = entry.process.wait()
        except Exception as exc:  # noqa: BLE001 - watcher 不能影响主线程
            with self._lock:
                closing = self._closed
            if not closing:
                LOGGER.warning(
                    "%s 连接器状态监测失败：%s",
                    entry.spec.name,
                    _safe_error_text(exc),
                )
            return

        job_handle = self._take_job_handle(entry)
        close_windows_handle(job_handle)
        with self._lock:
            closing = self._closed
        if not closing and return_code != 0:
            LOGGER.warning(
                "%s 连接器已退出（代码 %s），TUI 将继续运行。",
                entry.spec.name,
                return_code,
            )

    def _take_job_handle(self, entry: _ManagedConnector) -> int | None:
        with self._lock:
            handle = entry.job_handle
            entry.job_handle = None
            return handle


def start_configured_connectors(workspace_root: Path) -> ConnectorProcessManager:
    """创建并启动自动连接器监督器。

    连接器的配置读取只访问本地 TOML 和环境变量，不会创建 Agent、建立网络
    连接或加载飞书 WebSocket。调用方必须在应用退出时调用返回对象的 ``close``。
    """

    manager = ConnectorProcessManager(workspace_root)
    try:
        manager.start()
    except Exception:
        # 即使未来新增的平台探测或启动钩子抛出未预期异常，也不能让
        # 已经启动的前序连接器脱离监督；交给调用方继续抛出以记录警告。
        manager.close()
        raise
    return manager


def _configured_connectors() -> tuple[tuple[ConnectorSpec, bool], ...]:
    """无网络判断 Telegram/飞书是否具备启动所需的最小凭证。"""

    results: list[tuple[ConnectorSpec, bool]] = []

    try:
        telegram = load_telegram_config()
        configured = bool(
            str(telegram.get("bot_token", "")).strip()
            and telegram.get("allowed_user_ids")
        )
        results.append((_CONNECTOR_SPECS[0], configured))
    except Exception as exc:  # noqa: BLE001 - 外部连接器配置不能阻塞 TUI
        LOGGER.warning(
            "检查 Telegram 自动启动配置失败，跳过该连接器：%s",
            _safe_error_text(exc),
        )
        results.append((_CONNECTOR_SPECS[0], False))

    try:
        feishu = load_feishu_config()
        configured = bool(
            str(getattr(feishu, "app_id", "")).strip()
            and str(getattr(feishu, "app_secret", "")).strip()
        )
        results.append((_CONNECTOR_SPECS[1], configured))
    except Exception as exc:  # noqa: BLE001 - 外部连接器配置不能阻塞 TUI
        LOGGER.warning(
            "检查飞书自动启动配置失败，跳过该连接器：%s",
            _safe_error_text(exc),
        )
        results.append((_CONNECTOR_SPECS[1], False))

    return tuple(results)


def _auto_start_enabled() -> bool:
    raw_value = os.getenv(AUTO_START_ENV, "").strip().casefold()
    if not raw_value:
        return True
    if raw_value in _DISABLED_VALUES:
        return False
    if raw_value not in _ENABLED_VALUES:
        LOGGER.warning(
            "%s=%r 不是有效的开关值，将按启用处理。可使用 0/false/off 关闭。",
            AUTO_START_ENV,
            raw_value,
        )
    return True


def _child_environment(workspace_root: Path) -> dict[str, str]:
    """构造子进程环境，固定工作区并确保源码运行时可导入包。"""

    environment = os.environ.copy()
    # 继承当前入口最终选定的工作区，避免 Windows 弹窗保留的
    # AI_VOICE_CHAT_LAUNCH_CWD 让连接器重新检测到另一个目录。
    environment["AI_WORKSPACE_ROOT"] = str(workspace_root)

    # ``python main.py`` 从任意工作区启动时，子进程 cwd 可能不是源码根目录；
    # 把 omnicrawl 包的父目录加入 PYTHONPATH，同时保留用户已有设置。
    package_parent = str(Path(__file__).resolve().parents[2])
    existing = environment.get("PYTHONPATH", "").strip()
    environment["PYTHONPATH"] = (
        os.pathsep.join((package_parent, existing)) if existing else package_parent
    )
    return environment


def _safe_error_text(error: BaseException) -> str:
    text = redact_sensitive_text(str(error)).strip()
    return text[:300] if text else type(error).__name__


__all__ = [
    "AUTO_START_ENV",
    "ConnectorProcessManager",
    "start_configured_connectors",
]
