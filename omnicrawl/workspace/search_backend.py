"""工作区搜索后端：优先使用随包分发的 Go 原生扩展，缺失时回退到 PATH 上的 rg。

历史实现把 ripgrep 二进制随 wheel 分发（5 个平台共约 26MB）。现在搜索核心改由
随包编译的 Go 原生扩展承担（abi3 稳定 ABI，单平台约 2.5MB，无需按 Python 版本
各出一份）。扩展未构建、协议版本不符或平台不受支持时，仍可复用用户自行安装的
ripgrep；两者都不可用时给出与旧实现同样明确的错误，不静默返回空结果。

对外只需保证两件事：
1. 参数仍是项目一直在用的 ripgrep 参数子集；
2. 返回 (stdout, 退出码)，退出码 0/1 分别表示有/无匹配，其它值视为失败。
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

# 原生扩展的协议版本，与 native/cmod/main.go 的 version 常量对应；不匹配时
# 视为后端不可用，避免原生核心与 Python 包装层版本错配导致结果不一致。
NATIVE_PROTOCOL_VERSION = "1"

# 外部 rg 的单批路径字符预算：Windows CreateProcess 命令行长度上限约 32K
# 字符，按字符预算分批可兼顾大目录搜索与命令行长度安全。原生后端不经命令行
# 传参，不需要分批。
RIPGREP_BATCH_CHAR_BUDGET = 16_000

# 回退用的可执行文件名。
RIPGREP_BINARY_NAME = "rg.exe" if os.name == "nt" else "rg"

# 原生扩展的导入结果缓存：首次探测后固定，避免每次搜索都走一次 import。
_native_module: Any | None = None
_native_probed = False


class SearchBackendError(RuntimeError):
    """搜索后端不可用、超时或执行失败。"""


class SearchRegexError(SearchBackendError):
    """搜索 pattern 不被后端的正则引擎接受。

    单独成一类是因为 pattern 是用户输入，调用方需要把它翻译成"换用精确
    子串匹配"之类的可操作提示，而不是当成后端故障。原生扩展与外部 rg 的
    报错文案不同，这里统一归一成中文前缀。
    """


# 后端把 pattern 判为非法正则时的报错特征：原生扩展固定带中文前缀，
# 外部 ripgrep（Rust regex）输出 "regex parse error" 开头的信息。
_REGEX_ERROR_MARKERS = ("无效的正则表达式", "regex parse error", "error parsing regexp")


def _looks_like_regex_error(message: str) -> bool:
    """判断后端报错是否来自 pattern 的正则语法。"""

    return any(marker in message for marker in _REGEX_ERROR_MARKERS)


def _probe_native_module() -> Any | None:
    """导入原生搜索扩展；不可用或协议版本不符时返回 None。"""

    global _native_module, _native_probed
    if _native_probed:
        return _native_module
    _native_probed = True
    try:
        from .. import _ocsearch  # type: ignore[attr-defined]
    except (ImportError, OSError):
        # 未构建（ImportError）或动态库无法加载（Windows 上的 OSError）都降级，
        # 不能让搜索后端缺失阻断整个工具调用。
        _native_module = None
        return None
    try:
        version = str(_ocsearch.version())
    except (AttributeError, TypeError, ValueError):
        version = ""
    _native_module = _ocsearch if version == NATIVE_PROTOCOL_VERSION else None
    return _native_module


def native_backend_available() -> bool:
    """判断原生搜索扩展是否可用。"""

    return _probe_native_module() is not None


def resolve_ripgrep_binary() -> Path | None:
    """返回 PATH 中的 rg；未安装时返回 None（不再有随包二进制）。"""

    found = shutil.which(RIPGREP_BINARY_NAME)
    return Path(found) if found else None


def run_search(
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
    ripgrep_binary: Path | None = None,
) -> tuple[str, int]:
    """执行一次搜索，返回 (stdout, 退出码)。

    默认走原生扩展；显式传入 ripgrep_binary 或扩展不可用时改用外部 rg。
    """

    if ripgrep_binary is None:
        native = _probe_native_module()
        if native is not None:
            return _run_native(native, args, cwd=cwd, timeout=timeout)
    binary = ripgrep_binary or resolve_ripgrep_binary()
    if binary is None:
        raise SearchBackendError(
            "未找到可用的搜索后端：omnicrawl 原生搜索扩展不可用，PATH 中也没有 rg。"
            "请重新安装 omnicrawl-agent（或安装 ripgrep 作为回退）。"
        )
    return _run_external(binary, args, cwd=cwd, timeout=timeout)


def _run_native(
    native: Any,
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
) -> tuple[str, int]:
    """调用原生扩展并把它的结构化响应还原成 (stdout, 退出码)。"""

    request = json.dumps(
        {
            "args": list(args),
            "cwd": str(cwd),
            "timeout_seconds": float(timeout),
        }
    )
    try:
        raw = native.run(request)
    except Exception as exc:  # noqa: BLE001 - 原生层异常统一转成工具错误
        raise SearchBackendError(f"原生搜索扩展执行失败：{exc}") from exc
    try:
        response = json.loads(raw)
    except ValueError as exc:
        raise SearchBackendError("原生搜索扩展返回了无法解析的响应。") from exc
    error = response.get("error")
    if error:
        if response.get("timed_out"):
            raise SearchBackendError(f"搜索执行超时（{timeout:.0f} 秒）。")
        if _looks_like_regex_error(error):
            raise SearchRegexError(error)
        raise SearchBackendError(f"搜索执行失败：{error}")
    stdout = response.get("stdout")
    return (stdout if isinstance(stdout, str) else ""), int(response.get("returncode", 1))


def _run_external(
    binary: Path,
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
) -> tuple[str, int]:
    """运行外部 ripgrep，返回 (stdout, returncode)。

    rg 返回码 0 表示至少一个匹配，1 表示没有匹配，二者都不算执行失败；
    其他返回码表示参数或搜索错误，抛出 SearchBackendError。
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
        raise SearchBackendError(f"ripgrep 二进制不存在：{binary}") from exc
    except subprocess.TimeoutExpired as exc:
        raise SearchBackendError(f"搜索执行超时（{timeout:.0f} 秒）：{exc.cmd}") from exc
    stdout = completed.stdout.decode("utf-8", errors="replace")
    if completed.returncode not in (0, 1):
        stderr = completed.stderr.decode("utf-8", errors="replace").strip()
        if _looks_like_regex_error(stderr):
            raise SearchRegexError(f"无效的正则表达式：{stderr}")
        raise SearchBackendError(
            f"搜索执行失败（退出码 {completed.returncode}）：{stderr}"
        )
    return stdout, completed.returncode


def batch_paths(paths: list[Path]) -> list[list[str]]:
    """按字符预算分批路径，避免外部 rg 在 Windows 上命令行过长。"""

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
