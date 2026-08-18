#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""启动画面：黑色背景 + 左侧黄色 Logo + 右侧系统信息 + Windows XP 风格滚动条。

布局类似 fastfetch：左侧显示 Logo，右侧按行显示系统信息（OS、Host、Kernel、
Uptime、Shell、Screen、Terminal、Python、CPU、GPU、Memory、Swap、Disk），
画面底部为黄色滑块滚动条。

在 TUI 接管终端前显示约 3 秒，期间后台线程并行执行启动准备
（加载 git 等），避免纯等待浪费启动时间。

系统信息采集全部使用标准库（Windows / macOS / Linux 均可运行），每项失败时
显示 "N/A"，绝不因环境差异导致启动画面崩溃。
"""

from __future__ import annotations

import os
import platform
import shutil
import sys
import threading
import time
from typing import Any, Callable, TextIO

# ANSI 转义序列
_CLEAR = "\x1b[2J"
_HOME = "\x1b[H"
_RESET = "\x1b[0m"
_BG_BLACK = "\x1b[40m"
_FG_YELLOW = "\x1b[33m"  # 标准黄
_FG_BRIGHT_YELLOW = "\x1b[93m"  # 亮黄（更醒目）
_HIDE_CURSOR = "\x1b[?25l"
_SHOW_CURSOR = "\x1b[?25h"

# Windows XP 滚动条：灰色轨道 + 黄色小方块
_TRACK_BG = "\x1b[100m"  # 亮黑/灰
_SLIDER_BG = "\x1b[103m"  # 亮黄

# Logo（来自用户指定的桌面样式文件，去除行尾空白）
LOGO_LINES = [
    '                       !cpmZmmn_',
    '                     tdO0ZmOZwOmZOt,',
    '                   .wmZOZmO0pwmpmwmqqZ+',
    '                   qZwOmwwwmdpwdqpbkkbbbpO>.                    ..',
    "                  ?bwmpZwwwpdkbpdbhahkhaohoaaap-;.        .I{kO0mOL'",
    '                  Qpmwbqpbdpdkabhbokhooooh*o*#**######MWWWaOZOOOO0Y',
    '                  Oqwkbqpkkbdboahoao***M*###*##MMWWWWWWMp] `I!:',
    "                  xpwhbwpkpoooaao***##*M####MMWMWWWWW#L'",
    '                  .bqakhada*oao*o**##*#WMMWWWW&&&&War.',
    '                   1kkkhaoo***o#*#*M##MMWWWWWW&WWb]',
    '                    )hha*oa**o*#**#M#MWMWMW&&&#Z:',
    '                     :do**o*#o**#MMMWWWMWWWMb/.',
    '                       Im*****###M#MWWWMMkv"',
    '                          :jpo**#*##obv_',
]

DEFAULT_DURATION = 3.0

# Logo 左侧留白列数与 Logo / 系统信息之间的列间距
_LOGO_MARGIN = 2
_INFO_GAP = 4


def _logo_render_lines() -> list[str]:
    """返回实际渲染的 Logo 行：去掉所有行共有的前导空格。

    原 Logo 文本自带大量前导空白，会把图形推到第 25 列附近，挤占右侧
    系统信息面板；统一去掉公共前导空格后，图形从 ``_LOGO_MARGIN`` 处开始，
    与布局注释「从第 3 列开始」一致，并为 CPU/GPU 长名称留出更多宽度。
    """
    indent = min(len(line) - len(line.lstrip(" ")) for line in LOGO_LINES)
    return [line[indent:] for line in LOGO_LINES]


def _wrap_text(text: str, width: int) -> list[str]:
    """把文本按 ``width`` 列折行：优先在空格处断行，超长单词按字符拆分。"""
    words = text.split()
    if not words:
        return [""]
    lines: list[str] = []
    current = ""
    for word in words:
        candidate = f"{current} {word}" if current else word
        if len(candidate) <= width:
            current = candidate
            continue
        if current:
            lines.append(current)
            current = ""
        while len(word) > width:
            lines.append(word[:width])
            word = word[width:]
        current = word
    if current:
        lines.append(current)
    return lines


def _wrap_info_row(key: str, value: str, width: int) -> list[tuple[str, str]]:
    """把 ``key: value`` 拆成不超过 ``width`` 列的显示行，防止右边界截断。

    返回 ``[(prefix, body), ...]``：首行 ``prefix`` 为 ``"key: "``，
    续行 ``prefix`` 为等长空格，使值列对齐；拼接后总宽不超过 ``width``。
    信息区过窄时键名单独一行，值从下一行顶格折行。
    """
    prefix = f"{key}: "
    if width <= len(prefix):
        chunks = _wrap_text(value, width)
        lines = [(prefix.rstrip(), "")]
        lines.extend(("", chunk) for chunk in chunks)
        return lines
    value_width = width - len(prefix)
    chunks = _wrap_text(value, value_width)
    lines = [(prefix, chunks[0])]
    lines.extend((" " * len(prefix), chunk) for chunk in chunks[1:])
    return lines


def _is_tty(stream: TextIO | None) -> bool:
    """判断输出流是否支持交互式终端渲染。"""
    if stream is None:
        return False
    isatty = getattr(stream, "isatty", None)
    return callable(isatty) and bool(isatty())


def run_startup_splash(
    prepare: Callable[[], Any],
    duration: float = DEFAULT_DURATION,
    stream: TextIO | None = None,
) -> Any:
    """显示 3 秒启动画面，同时后台执行 ``prepare``（加载 git 等）。

    Args:
        prepare: 启动准备回调，在 splash 显示期间于后台线程执行；
            返回值会透传给调用方，异常会在 splash 结束后重新抛出。
        duration: splash 最短显示秒数，默认 3 秒；若 prepare 耗时超过
            duration，则继续显示滚动条直到 prepare 完成。
        stream: 输出流，默认 ``sys.stdout``；非交互流（如测试管道）时
            直接同步执行 ``prepare`` 并返回，不显示动画。

    Returns:
        ``prepare()`` 的返回值。
    """

    out = stream if stream is not None else sys.stdout
    if not _is_tty(out):
        # 非交互环境（测试、管道、重定向）：同步执行，不渲染动画。
        return prepare()

    result: dict[str, Any] = {}
    error_box: dict[str, BaseException] = {}

    def _worker() -> None:
        try:
            result["value"] = prepare()
        except BaseException as exc:  # noqa: BLE001
            error_box["error"] = exc

    thread = threading.Thread(target=_worker, name="omnicrawl-splash-prepare", daemon=True)
    thread.start()
    try:
        _render_splash(
            out,
            duration,
            is_done=lambda: "value" in result,
            has_error=lambda: bool(error_box),
        )
    finally:
        thread.join(timeout=max(1.0, duration + 5.0))
    if error_box:
        raise error_box["error"]
    return result.get("value")


def _render_splash(
    out: TextIO,
    duration: float,
    is_done: Callable[[], bool],
    has_error: Callable[[], bool],
) -> None:
    """绘制 fastfetch 式启动画面：左侧 Logo + 右侧系统信息 + 底部 XP 滚动条。

    持续到时长满且准备完成；任何异常都不允许影响启动主流程。
    """

    width, height = shutil.get_terminal_size(fallback=(80, 24))
    info = _collect_sysinfo()
    logo_lines = _logo_render_lines()
    logo_width = max(len(line) for line in logo_lines)
    logo_rows = len(logo_lines)

    # 左侧 Logo：从第 3 列开始，不再居中；右侧信息紧跟 Logo 顶部对齐
    left = _LOGO_MARGIN
    info_left = left + logo_width + _INFO_GAP
    # 窄终端保护：信息区至少保留 24 列；放不下时整体左移。
    # 但下限必须保证 Logo 与信息区之间至少 1 列间隙，绝不能覆盖 Logo；
    # 若终端仍不够宽，允许信息区超出右边界（由终端换行），优先保证 Logo 完整可见。
    if info_left + 24 > width:
        info_left = max(left + logo_width + 1, width - 24)

    # 右侧系统信息按信息区宽度折行，避免 CPU/GPU 等长名称越过终端右边界被截断
    info_width = max(1, width - info_left)
    info_lines: list[tuple[str, str]] = []
    for key, value in info:
        info_lines.extend(_wrap_info_row(key, value, info_width))
    info_rows = len(info_lines)

    # 垂直布局：以 Logo / 信息两者的最大高度为基准居中，滚动条置于下方
    body_rows = max(logo_rows, info_rows)
    total_rows = body_rows + 4
    top = max(0, (height - total_rows) // 2)
    bar_row = top + body_rows + 2

    out.write(_BG_BLACK + _CLEAR + _HOME + _HIDE_CURSOR)
    out.flush()

    # 左侧 logo（亮黄色）
    for i, line in enumerate(logo_lines):
        row = top + i + 1
        out.write(f"\x1b[{row};{left + 1}H" + _FG_BRIGHT_YELLOW + line + _RESET)

    # 右侧系统信息：键为黄色，值为终端默认前景色；长值自动折行
    for i, (prefix, body) in enumerate(info_lines):
        row = top + i + 1
        colored_prefix = _FG_YELLOW + prefix + _RESET if prefix.strip() else prefix
        out.write(f"\x1b[{row};{info_left + 1}H" + colored_prefix + body)
    out.flush()

    bar_width = min(30, max(10, (width - 4) // 2))
    slider_width = max(4, bar_width // 5)
    bar_left = max(0, (width - bar_width) // 2)

    start = time.monotonic()
    frame = 0
    while True:
        if has_error():
            # 准备失败：立即结束画面，让调用方快速看到错误。
            break
        elapsed = time.monotonic() - start
        if elapsed >= duration and is_done():
            break
        _draw_progress_bar(out, bar_row, bar_left, bar_width, slider_width, frame)
        frame += 1
        time.sleep(0.05)

    # 复位：显示光标并清屏，避免残留
    out.write(_SHOW_CURSOR + _RESET + _CLEAR + _HOME)
    out.flush()


def _draw_progress_bar(
    out: TextIO,
    row: int,
    left: int,
    width: int,
    slider_width: int,
    frame: int,
) -> None:
    """在指定行绘制一帧滚动条（灰色轨道 + 黄色滑块循环滑动）。

    动画流程（每周期 39 帧，约 2 秒）：
      1. 滑入：滑块从轨道左侧外右移进入，可见部分逐帧变宽，直到完整出现在左端；
      2. 横穿：滑块保持全宽匀速滑过整个轨道，到达右端；
      3. 滑出：滑块整体继续右移、滑出右边界，可见部分逐帧变窄直至完全消失；
      4. 空档：轨道上无滑块，停顿片刻后重新从左侧滑入，形成单向循环。
    """

    fade = slider_width  # 滑入/滑出各占 slider_width 帧
    travel = max(1, width - slider_width)  # 全宽横穿帧数
    gap = max(2, slider_width // 2)  # 完全消失后的空档帧数
    cycle = fade + travel + fade + gap
    t = frame % cycle

    if t < fade:
        # 阶段 1：滑入 —— 滑块左端从 -slider_width 推进到 0，逐渐出现在左端
        pos = t - slider_width
    elif t < fade + travel:
        # 阶段 2：横穿 —— 滑块左端从 0 推进到 width - slider_width
        pos = t - fade
    elif t < fade + travel + fade:
        # 阶段 3：滑出 —— 滑块左端从 width - slider_width 推进到 width，逐渐消失
        pos = (t - fade - travel) + (width - slider_width)
    else:
        # 阶段 4：空档 —— 滑块完全移出轨道，不绘制
        pos = width  # 超出轨道右边界，裁剪区间为空

    # 统一裁剪：只绘制滑块与轨道 [0, width) 相交的可见部分
    start = max(0, pos)
    end = min(width, pos + slider_width)

    line_parts: list[str] = []
    line_parts.append(f"\x1b[{row};{left + 1}H")
    line_parts.append(_TRACK_BG)
    line_parts.append(" " * start)
    if end > start:
        line_parts.append(_SLIDER_BG)
        line_parts.append(" " * (end - start))
    # 滑块右侧的轨道必须重新声明灰色背景，否则终端会沿用滑块黄色背景，
    # 导致滑块右侧整段都被染黄（此前误渲染成"进度条填充"观感）。
    line_parts.append(_TRACK_BG)
    line_parts.append(" " * (width - end))
    line_parts.append(_RESET)
    out.write("".join(line_parts))
    out.flush()


def _collect_sysinfo() -> list[tuple[str, str]]:
    """收集 fastfetch 风格的系统信息键值对（跨 Windows / macOS / Linux）。

    全部使用标准库实现、逐项独立容错：任何一项获取失败都显示 "N/A"，
    保证启动画面在陌生环境也不会因系统信息崩溃。
    """

    system = platform.system()
    items: list[tuple[str, str]] = []

    def add(key: str, value: str) -> None:
        items.append((key, value))

    add("OS", _os_display_name(system))
    add("Host", platform.node() or "N/A")
    add("Kernel", _kernel_version(system))
    add("Uptime", _uptime())
    add("Shell", _current_shell())
    add("Screen", _screen_resolution(system))
    add("Terminal", _terminal_name())
    add("Python", sys.version.split()[0])
    add("CPU", _cpu_name(system))
    add("GPU", _gpu_name(system))
    add("Memory", _memory_usage(system))
    add("Swap", _swap_usage(system))
    add("Disk", _disk_usage("."))
    return items


def _os_display_name(system: str) -> str:
    """操作系统显示名，如 Windows 11 / macOS 14.5 / Ubuntu 24.04。"""
    try:
        if system == "Windows":
            release = platform.release()
            if release == "10":
                build = 0
                try:
                    build = int(platform.version().split(".")[2])
                except (IndexError, ValueError):
                    pass
                return "Windows 11" if build >= 22000 else "Windows 10"
            return f"Windows {release}"
        if system == "Darwin":
            ver = platform.mac_ver()[0]
            return f"macOS {ver}" if ver else "macOS"
        if system == "Linux":
            name = _linux_pretty_name()
            return name if name else "Linux"
        return f"{system} {platform.release()}"
    except Exception:  # noqa: BLE001
        return "N/A"


def _linux_pretty_name() -> str:
    """从 /etc/os-release 读取发行版名称（如 "Ubuntu 24.04.1 LTS"）。"""
    for path in ("/etc/os-release", "/usr/lib/os-release"):
        try:
            with open(path, encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    if line.startswith("PRETTY_NAME="):
                        name = line.split("=", 1)[1].strip().strip('"')
                        if name:
                            return name
        except OSError:
            continue
    return ""


def _kernel_version(system: str) -> str:
    """内核版本：Windows 显示内部版本号，macOS/Linux 显示 release。"""
    try:
        if system == "Windows":
            version = platform.version()  # 形如 "10.0.22631"
            return version.split()[0] if version else "N/A"
        return platform.release() or "N/A"
    except Exception:  # noqa: BLE001
        return "N/A"


def _uptime() -> str:
    """系统运行时长（跨平台，失败返回 N/A）。"""
    try:
        if sys.platform == "win32":
            import ctypes

            ms = int(ctypes.windll.kernel32.GetTickCount64())
        elif sys.platform == "darwin":
            import subprocess

            out = subprocess.check_output(
                ["sysctl", "-n", "kern.boottime"], text=True, timeout=3
            )
            # 形如 "{ sec = 1751763158, usec = 0 } Fri Jul  5 10:00:00 2024"
            sec = int(out.split("sec = ", 1)[1].split(",", 1)[0])
            ms = (time.time() - sec) * 1000
        else:
            with open("/proc/uptime", encoding="utf-8") as fh:
                ms = float(fh.read().split()[0]) * 1000
    except Exception:  # noqa: BLE001
        return "N/A"
    return _format_uptime(int(ms) // 1000)


def _format_uptime(seconds: int) -> str:
    """秒数格式化为 "1d 2h 3m" 形式。"""
    days, rem = divmod(seconds, 86400)
    hours, rem = divmod(rem, 3600)
    minutes = rem // 60
    parts: list[str] = []
    if days:
        parts.append(f"{days}d")
    if hours or days:
        parts.append(f"{hours}h")
    parts.append(f"{minutes}m")
    return " ".join(parts)


def _current_shell() -> str:
    """当前 Shell 名称：Windows 取 COMSPEC，macOS/Linux 取 SHELL。"""
    try:
        if sys.platform == "win32":
            shell = os.environ.get("COMSPEC") or os.environ.get("SHELL") or ""
        else:
            shell = os.environ.get("SHELL") or ""
        if not shell:
            return "N/A"
        return os.path.basename(shell.replace("\\", "/"))
    except Exception:  # noqa: BLE001
        return "N/A"


def _terminal_name() -> str:
    """终端名称：Windows Terminal / VS Code / xterm 等。"""
    try:
        if os.environ.get("WT_SESSION"):
            return "Windows Terminal"
        prog = os.environ.get("TERM_PROGRAM")
        if prog:
            return prog
        term = os.environ.get("TERM")
        if term:
            return term.split("-")[0]
        if sys.platform == "win32":
            return "Console"
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _cpu_name(system: str) -> str:
    """CPU 品牌名：Windows 读注册表，macOS 用 sysctl，Linux 读 /proc/cpuinfo。"""
    try:
        if system == "Windows":
            try:
                import winreg

                key = winreg.OpenKey(
                    winreg.HKEY_LOCAL_MACHINE,
                    r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                )
                try:
                    name, _ = winreg.QueryValueEx(key, "ProcessorNameString")
                finally:
                    winreg.CloseKey(key)
                if name and name.strip():
                    return name.strip()
            except OSError:
                pass
            return os.environ.get("PROCESSOR_IDENTIFIER") or platform.processor() or "N/A"
        if system == "Darwin":
            import subprocess

            name = subprocess.check_output(
                ["sysctl", "-n", "machdep.cpu.brand_string"], text=True, timeout=3
            )
            return name.strip() or "N/A"
        if system == "Linux":
            with open("/proc/cpuinfo", encoding="utf-8", errors="replace") as fh:
                for line in fh:
                    if line.lower().startswith("model name"):
                        return line.split(":", 1)[1].strip() or "N/A"
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _memory_usage(system: str) -> str:
    """内存使用情况（已用 / 总量），跨平台标准库实现。"""
    try:
        if system == "Windows":
            import ctypes

            class _MEMORYSTATUSEX(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat = _MEMORYSTATUSEX()
            stat.dwLength = ctypes.sizeof(_MEMORYSTATUSEX)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat)):
                total = stat.ullTotalPhys
                used = total - stat.ullAvailPhys
                return f"{_fmt_bytes(used)} / {_fmt_bytes(total)}"
        elif system == "Linux":
            with open("/proc/meminfo", encoding="utf-8") as fh:
                data: dict[str, int] = {}
                for line in fh:
                    key, _, rest = line.partition(":")
                    data[key] = int(rest.split()[0]) * 1024
            total = data.get("MemTotal", 0)
            avail = data.get("MemAvailable", data.get("MemFree", 0))
            if total:
                return f"{_fmt_bytes(total - avail)} / {_fmt_bytes(total)}"
        elif system == "Darwin":
            import subprocess

            total = int(
                subprocess.check_output(["sysctl", "-n", "hw.memsize"], text=True, timeout=3)
            )
            page = int(
                subprocess.check_output(["sysctl", "-n", "hw.pagesize"], text=True, timeout=3)
            )
            free = 0
            vm = subprocess.check_output(["vm_stat"], text=True, timeout=3)
            for line in vm.splitlines():
                line = line.strip()
                if line.startswith("Pages free"):
                    free = int(line.split(":")[1].strip().rstrip(".")) * page
                    break
            if total:
                return f"{_fmt_bytes(total - free)} / {_fmt_bytes(total)}"
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _screen_resolution(system: str) -> str:
    """主屏幕分辨率（如 1920x1080），跨平台标准库实现。"""
    try:
        if system == "Windows":
            import ctypes

            w = ctypes.windll.user32.GetSystemMetrics(0)  # SM_CXSCREEN
            h = ctypes.windll.user32.GetSystemMetrics(1)  # SM_CYSCREEN
            if w and h:
                return f"{w}x{h}"
        elif system == "Darwin":
            import subprocess

            out = subprocess.check_output(
                ["system_profiler", "SPDisplaysDataType"], text=True, timeout=8
            )
            # 形如 "Resolution: 2560 x 1080"，可能有多显示器多行
            for line in out.splitlines():
                line = line.strip()
                if line.lower().startswith("resolution:"):
                    res = line.split(":", 1)[1].strip().replace(" ", "")
                    if res and "x" in res:
                        return res
        elif system == "Linux":
            import re
            import subprocess

            out = subprocess.check_output(["xrandr"], text=True, timeout=3)
            # 形如 "HDMI-1 connected primary 1920x1080+0+0"
            m = re.search(r"(\d{3,5}x\d{3,5})", out)
            if m:
                return m.group(1)
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _gpu_name(system: str) -> str:
    """GPU 型号名：Windows 读注册表，macOS 用 system_profiler，Linux 用 lspci。"""
    try:
        if system == "Windows":
            import winreg

            names: list[str] = []
            base = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}"
            # 枚举 0000-0009 子键，收集所有显卡 DriverDesc
            for i in range(10):
                sub = f"{base}\\{i:04d}"
                try:
                    key = winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, sub)
                except OSError:
                    break
                try:
                    desc, _ = winreg.QueryValueEx(key, "DriverDesc")
                    if desc and desc.strip() and desc.strip() not in names:
                        names.append(desc.strip())
                except OSError:
                    pass
                finally:
                    winreg.CloseKey(key)
            if names:
                return " + ".join(names[:2])  # 最多显示两张（如核显+独显）
        elif system == "Darwin":
            import subprocess

            out = subprocess.check_output(
                ["system_profiler", "SPDisplaysDataType"], text=True, timeout=8
            )
            # 形如 "Chipset Model: Apple M1 Pro"，可能多行
            names = []
            for line in out.splitlines():
                line = line.strip()
                if line.lower().startswith("chipset model:"):
                    name = line.split(":", 1)[1].strip()
                    if name and name not in names:
                        names.append(name)
            if names:
                return " + ".join(names[:2])
        elif system == "Linux":
            import subprocess

            out = subprocess.check_output(["lspci"], text=True, timeout=3)
            # 形如 "01:00.0 VGA compatible controller: NVIDIA ..."
            names = []
            for line in out.splitlines():
                low = line.lower()
                if "vga compatible" in low or "3d controller" in low:
                    name = line.split(":", 2)[-1].strip() if line.count(":") >= 2 else line
                    if name and name not in names:
                        names.append(name)
            if names:
                return " + ".join(names[:2])
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _swap_usage(system: str) -> str:
    """交换空间使用情况（已用 / 总量），跨平台标准库实现。"""
    try:
        if system == "Windows":
            import ctypes

            class _MEMORYSTATUSEX(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat = _MEMORYSTATUSEX()
            stat.dwLength = ctypes.sizeof(_MEMORYSTATUSEX)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat)):
                total = stat.ullTotalPageFile - stat.ullTotalPhys
                if total <= 0:
                    return "0B / 0B"
                used = max(0, total - stat.ullAvailPageFile)
                return f"{_fmt_bytes(used)} / {_fmt_bytes(total)}"
        elif system == "Linux":
            with open("/proc/meminfo", encoding="utf-8") as fh:
                data: dict[str, int] = {}
                for line in fh:
                    key, _, rest = line.partition(":")
                    data[key] = int(rest.split()[0]) * 1024
            total = data.get("SwapTotal", 0)
            free = data.get("SwapFree", 0)
            if total:
                return f"{_fmt_bytes(total - free)} / {_fmt_bytes(total)}"
            return "0B / 0B"
        elif system == "Darwin":
            import re
            import subprocess

            out = subprocess.check_output(
                ["sysctl", "-n", "vm.swapusage"], text=True, timeout=3
            )
            # 形如 "total = 1024.00M  used = 512.00M  avail = 512.00M"
            m = re.search(r"total = ([\d.]+)(\w).*?used = ([\d.]+)(\w)", out)
            if m:
                total = _parse_swap_size(m.group(1), m.group(2))
                used = _parse_swap_size(m.group(3), m.group(4))
                if total:
                    return f"{_fmt_bytes(used)} / {_fmt_bytes(total)}"
    except Exception:  # noqa: BLE001
        pass
    return "N/A"


def _parse_swap_size(num: str, unit: str) -> float:
    """把 "1024.00M" 这类 swap 数值换算为字节。"""
    mult = {"B": 1, "K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4}
    return float(num) * mult.get(unit.upper(), 1)


def _disk_usage(path: str = ".") -> str:
    """磁盘使用情况（已用 / 总量），作用于 ``path`` 所在文件系统。"""
    try:
        usage = shutil.disk_usage(os.path.abspath(path))
        used = usage.total - usage.free
        return f"{_fmt_bytes(used)} / {_fmt_bytes(usage.total)}"
    except Exception:  # noqa: BLE001
        return "N/A"


def _fmt_bytes(size: float) -> str:
    """字节数格式化为易读单位（B/K/M/G/T），保留 1 位小数。"""
    value = float(size)
    for unit in ("B", "K", "M", "G", "T"):
        if value < 1024.0 or unit == "T":
            if unit == "B":
                return f"{int(value)}B"
            return f"{value:.1f}{unit}"
        value /= 1024.0
    return f"{value:.1f}T"
