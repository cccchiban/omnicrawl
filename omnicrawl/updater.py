"""启动自动更新：检测 PyPI 最新版，落后时自动升级并重启 TUI。

流程（仅在 TUI 启动路径、pip 正常安装的环境中执行）：

1. 读取本地配置 ``[update].enabled``（缺省启用），并检查环境变量跳过开关；
2. 复用 :mod:`omnicrawl.version_check` 的 PyPI RSS 检测与 24h 本地缓存；
3. 本地版本落后时打印升级说明，用当前解释器执行
   ``python -m pip install --upgrade omnicrawl-agent==<latest>``；
4. 升级成功后重新探测安装版本，与目标一致才重新拉起 TUI
   （``python -m omnicrawl <原参数>``），并把子进程退出码作为本次启动退出码；
5. 任何失败（离线、pip 不可用、安装报错、探测不一致）都只打印原因与手动
   升级命令，继续用当前版本启动 TUI，绝不让更新流程阻塞工具可用性。

安全护栏：
- 源码目录 / editable 开发环境直接跳过（``pyproject.toml + .git`` 标记）；
- ``OMNICRAWL_SKIP_AUTO_UPDATE=1`` 可彻底关闭；
- 执行过一次升级尝试后写入 ``OMNICRAWL_AUTO_UPDATE_ATTEMPTED=1``，重启后的
  子进程继承该标记不再重复尝试，避免升级失败/版本探测异常时无限重启循环。
"""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path
from typing import Callable, Sequence

from . import version_check as _version_check
from .config.core.settings import load_feature_enabled
from .version_check import VersionCheckResult, check_latest_version, current_version, is_newer_version

PACKAGE_NAME = "omnicrawl-agent"
# 彻底关闭自动更新（用户/CI/故障排查用）。
SKIP_AUTO_UPDATE_ENV = "OMNICRAWL_SKIP_AUTO_UPDATE"
# 标记"本次升级已经尝试过"，重启后的子进程据此不再重复尝试。
AUTO_UPDATE_ATTEMPTED_ENV = "OMNICRAWL_AUTO_UPDATE_ATTEMPTED"

# 控制台脚本执行 pip 升级时的附加参数（不影响功能，仅减少噪音/交互）。
_PIP_QUIET_ARGS = ("--disable-pip-version-check", "--no-input")


def is_source_checkout(package_dir: Path | None = None) -> bool:
    """判断是否从源码目录 / editable 安装运行（此类环境不自动升级）。

    ``package_dir`` 为 omnicrawl 包所在目录；缺省从本模块位置推导。环境根为
    包目录的父目录，源码检出时它同时含 ``pyproject.toml`` 与 ``.git``。
    """

    if package_dir is None:
        package_dir = Path(_version_check.__file__).resolve().parent
    env_root = package_dir.parent
    return (env_root / "pyproject.toml").is_file() and (env_root / ".git").is_dir()


def _load_update_enabled(config_path: str | Path | None = None) -> bool:
    """读取 ``[update].enabled``；缺省开启，配置异常也按开启处理（保底默认）。"""

    try:
        return load_feature_enabled("update", default=True, config_path=config_path)
    except Exception:  # noqa: BLE001 - 更新功能不能因配置问题影响启动
        return True


def _build_pip_command(latest_version: str) -> list[str]:
    return [
        sys.executable,
        "-m",
        "pip",
        "install",
        "--upgrade",
        *_PIP_QUIET_ARGS,
        f"{PACKAGE_NAME}=={latest_version}",
    ]


def install_latest_version(
    latest_version: str,
    *,
    process_runner: Callable[[Sequence[str]], int] | None = None,
) -> bool:
    """用当前解释器的 pip 升级到指定版本；返回是否成功。"""

    command = _build_pip_command(latest_version)
    runner = process_runner if process_runner is not None else _run_pip_default
    try:
        return runner(command) == 0
    except (OSError, subprocess.SubprocessError):
        return False


def _run_pip_default(command: Sequence[str]) -> int:
    """默认执行方式：继承终端输出，让用户看到 pip 进度。"""

    completed = subprocess.run(list(command), check=False)
    return int(completed.returncode)


def probe_installed_version(
    *,
    probe_runner: Callable[[Sequence[str]], object] | None = None,
) -> str | None:
    """在独立子进程中读取升级后的安装版本；失败返回 None。"""

    command = [
        sys.executable,
        "-c",
        "from omnicrawl.extensions.plugin_models import OMNICRAWL_VERSION;"
        "print(OMNICRAWL_VERSION)",
    ]
    try:
        if probe_runner is None:
            completed = subprocess.run(
                command,
                capture_output=True,
                text=True,
                check=False,
            )
        else:
            completed = probe_runner(command)
        if isinstance(completed, int):
            return None
        raw = getattr(completed, "stdout", None)
        output = str(raw or "").strip()
    except (OSError, subprocess.SubprocessError, AttributeError):
        return None
    return output or None


def _run_relaunch_default(
    command: Sequence[str],
) -> int:
    completed = subprocess.run(list(command), check=False)
    return int(completed.returncode)


def restart_tui(
    argv: Sequence[str],
    *,
    process_runner: Callable[[Sequence[str]], int] | None = None,
) -> int | None:
    """以 ``python -m omnicrawl <原参数>`` 重新拉起 TUI，返回子进程退出码。"""

    runner = process_runner if process_runner is not None else _run_relaunch_default
    try:
        return runner([sys.executable, "-m", "omnicrawl", *argv])
    except (OSError, subprocess.SubprocessError):
        return None


def _manual_install_hint(latest_version: str) -> str:
    return (
        f"可手动执行升级：python -m pip install --upgrade {PACKAGE_NAME}=={latest_version}"
    )


def run_startup_update_if_due(
    argv: Sequence[str],
    *,
    print_fn: Callable[[str], None] = print,
    config_path: str | Path | None = None,
    installed_version: str | None = None,
    latest_checker: Callable[[], VersionCheckResult] | None = None,
    installer: Callable[[str], bool] | None = None,
    version_prober: Callable[[], str | None] | None = None,
    relauncher: Callable[[Sequence[str]], int | None] | None = None,
) -> int | None:
    """启动阶段自动更新入口。

    Returns:
        ``None``：无需更新 / 已跳过 / 更新失败按用户策略继续用当前版本启动；
        其他整数：已升级并重启，返回新 TUI 进程的退出码，调用方应直接退出。
    """

    if os.environ.get(SKIP_AUTO_UPDATE_ENV) == "1":
        return None
    if os.environ.get(AUTO_UPDATE_ATTEMPTED_ENV) == "1":
        # 重启后的子进程：升级已经由父进程完成或尝试过，直接进入 TUI。
        return None
    if is_source_checkout():
        return None
    if not _load_update_enabled(config_path):
        return None

    installed = str(installed_version or current_version())
    result = (
        latest_checker() if latest_checker is not None else check_latest_version()
    )
    if not result.update_available or result.latest_version is None:
        return None

    # 先标记尝试，避免子进程（继承环境变量）在升级后因探测不一致再次升级。
    os.environ[AUTO_UPDATE_ATTEMPTED_ENV] = "1"
    latest = result.latest_version
    print_fn(f"发现新版本 {latest}（当前 {installed}），正在自动升级…")

    install_ok = (
        installer(latest) if installer is not None else install_latest_version(latest)
    )
    if not install_ok:
        print_fn(f"自动升级失败，本次继续使用当前版本 {installed} 启动。")
        print_fn(_manual_install_hint(latest))
        return None

    probe = (
        version_prober() if version_prober is not None else probe_installed_version()
    )
    if probe is None or not is_newer_version(probe, installed):
        # pip 返回成功但版本未变化（或被其他进程改动）：不要无限循环。
        print_fn(
            f"升级完成但版本校验未通过（安装版本 {probe or '未知'}），"
            f"本次继续使用当前版本 {installed} 启动。"
        )
        print_fn(_manual_install_hint(latest))
        return None

    print_fn(f"升级完成（{installed} → {probe}），正在重新启动 TUI…")
    relaunch_code = (
        relauncher(argv) if relauncher is not None else restart_tui(argv)
    )
    if relaunch_code is None:
        print_fn("重新启动 TUI 失败，请手动执行 omnicrawl 重新进入。")
        return 0
    return relaunch_code


__all__ = [
    "AUTO_UPDATE_ATTEMPTED_ENV",
    "PACKAGE_NAME",
    "SKIP_AUTO_UPDATE_ENV",
    "current_version",
    "install_latest_version",
    "is_source_checkout",
    "probe_installed_version",
    "restart_tui",
    "run_startup_update_if_due",
]
