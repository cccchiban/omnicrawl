from __future__ import annotations

import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

RIPGREP_BINARY_NAME = "rg.exe" if os.name == "nt" else "rg"

# 随包分发的 ripgrep 二进制，按 (系统, 架构) 选择：Windows x86_64 沿用既有
# rg.exe；Linux/macOS 使用官方静态/预编译构建，用户无需预先安装 ripgrep。
# 未覆盖的平台（如 Windows ARM、Linux armv7）回退到 RIPGREP_BINARY_NAME，
# 随包路径下不存在该文件时会继续回退到 PATH 中的 rg。
BUNDLED_RIPGREP_BINARIES: dict[tuple[str, str], str] = {
    ("win32", "x86_64"): "rg.exe",
    ("linux", "x86_64"): "rg-linux-x86_64",
    ("linux", "arm64"): "rg-linux-arm64",
    ("darwin", "x86_64"): "rg-macos-x86_64",
    ("darwin", "arm64"): "rg-macos-arm64",
}

# 单批传给 rg 的文件路径字符预算：Windows CreateProcess 命令行长度上限约 32K
# 字符，按字符预算分批可同时兼顾大目录搜索与命令行长度安全。
RIPGREP_BATCH_CHAR_BUDGET = 16_000


class RipgrepError(RuntimeError):
    """ripgrep 二进制缺失、超时或执行失败。"""


def _platform_arch_key() -> tuple[str, str]:
    """返回当前 (系统, 架构) 归一化键，用于选择随包二进制。"""
    machine = platform.machine().lower()
    if machine in {"amd64", "x86_64"}:
        machine = "x86_64"
    elif machine in {"arm64", "aarch64"}:
        machine = "arm64"
    return sys.platform, machine


def bundled_ripgrep_name() -> str:
    """返回当前平台对应的随包二进制文件名。"""
    return BUNDLED_RIPGREP_BINARIES.get(_platform_arch_key(), RIPGREP_BINARY_NAME)


def bundled_ripgrep_path() -> Path:
    """返回随包分发的 ripgrep 二进制路径（不存在时也返回该路径）。"""
    return Path(__file__).resolve().parent.parent / "bin" / bundled_ripgrep_name()


def ensure_bundled_executable(path: Path) -> None:
    """补齐随包二进制的可执行权限。

    wheel 解压或部分安装器可能丢失 Unix 执行位，导致 Linux/macOS 上首次
    调用报 Permission denied；这里在使用前统一补一次 0755。Windows 无需
    处理（由扩展名与 PE 头决定可执行性）。
    """

    if os.name == "nt":
        return
    try:
        if not os.access(path, os.X_OK):
            path.chmod(0o755)
    except OSError:
        # 只读文件系统等场景下补权限失败不应阻断回退路径。
        pass


def resolve_ripgrep_binary() -> Path | None:
    """优先返回随包二进制，其次返回 PATH 中的 rg；都找不到时返回 None。"""
    bundled = bundled_ripgrep_path()
    if bundled.is_file():
        ensure_bundled_executable(bundled)
        return bundled
    found = shutil.which("rg")
    return Path(found) if found else None


def run_ripgrep(
    binary: Path,
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
) -> tuple[str, int]:
    """运行一次 ripgrep，返回 (stdout, returncode)。

    rg 返回码 0 表示至少一个匹配，1 表示没有匹配，二者都不算执行失败；
    其他返回码（通常为 2）表示参数或搜索错误，抛出 RipgrepError。
    """
    try:
        completed = subprocess.run(
            [str(binary), *args],
            cwd=str(cwd),
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as exc:
        raise RipgrepError(f"ripgrep 二进制不存在：{binary}") from exc
    except subprocess.TimeoutExpired as exc:
        raise RipgrepError(
            f"ripgrep 执行超时（{timeout:.0f} 秒）：{exc.cmd}"
        ) from exc
    stdout = completed.stdout.decode("utf-8", errors="replace")
    if completed.returncode not in (0, 1):
        stderr = completed.stderr.decode("utf-8", errors="replace").strip()
        raise RipgrepError(
            f"ripgrep 执行失败（退出码 {completed.returncode}）：{stderr}"
        )
    return stdout, completed.returncode


def batch_paths(paths: list[Path]) -> list[list[str]]:
    """按字符预算分批路径，避免 Windows 命令行过长。"""
    batches: list[list[str]] = []
    current: list[str] = []
    current_chars = 0
    for path in paths:
        text = str(path)
        if current and current_chars + len(text) + 1 > RIPGREP_BATCH_CHAR_BUDGET:
            batches.append(current)
            current = []
            current_chars = 0
        current.append(text)
        current_chars += len(text) + 1
    if current:
        batches.append(current)
    return batches
