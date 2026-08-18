from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path

RIPGREP_BINARY_NAME = "rg.exe" if os.name == "nt" else "rg"

# 单批传给 rg 的文件路径字符预算：Windows CreateProcess 命令行长度上限约 32K
# 字符，按字符预算分批可同时兼顾大目录搜索与命令行长度安全。
RIPGREP_BATCH_CHAR_BUDGET = 16_000


class RipgrepError(RuntimeError):
    """ripgrep 二进制缺失、超时或执行失败。"""


def bundled_ripgrep_path() -> Path:
    """返回随包分发的 ripgrep 二进制路径（不存在时也返回该路径）。"""
    return Path(__file__).resolve().parent.parent / "bin" / RIPGREP_BINARY_NAME


def resolve_ripgrep_binary() -> Path | None:
    """优先返回随包二进制，其次返回 PATH 中的 rg；都找不到时返回 None。"""
    bundled = bundled_ripgrep_path()
    if bundled.is_file():
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
