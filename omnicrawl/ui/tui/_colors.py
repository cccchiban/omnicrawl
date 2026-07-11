"""ANSI 颜色常量、真彩色配色表、256 色映射与色彩路由。"""

from __future__ import annotations


import re
from typing import Literal

from ._capabilities import TerminalCapabilities

# ── ANSI 基础常量 ─────────────────────────────────────────────

ANSI_RESET = "\033[0m"

# 基础色
ANSI_BLACK = "\033[30m"
ANSI_RED = "\033[31m"
ANSI_GREEN = "\033[32m"
ANSI_YELLOW = "\033[33m"
ANSI_BLUE = "\033[34m"
ANSI_MAGENTA = "\033[35m"
ANSI_CYAN = "\033[36m"
ANSI_WHITE = "\033[37m"

# 亮色
ANSI_BRIGHT_BLACK = "\033[90m"
ANSI_BRIGHT_RED = "\033[91m"
ANSI_BRIGHT_GREEN = "\033[92m"
ANSI_BRIGHT_YELLOW = "\033[93m"
ANSI_BRIGHT_BLUE = "\033[94m"
ANSI_BRIGHT_MAGENTA = "\033[95m"
ANSI_BRIGHT_CYAN = "\033[96m"
ANSI_BRIGHT_WHITE = "\033[97m"

# 样式
ANSI_BOLD = "\033[1m"
ANSI_DIM = "\033[2m"
ANSI_ITALIC = "\033[3m"
ANSI_UNDERLINE = "\033[4m"

# 控制序列
ANSI_CLEAR_LINE = "\033[2K"
ANSI_CLEAR_TO_LINE_END = "\033[K"
ANSI_PREVIOUS_LINE = "\033[1A"
ANSI_SAVE_CURSOR = "\033[s"
ANSI_RESTORE_CURSOR = "\033[u"
ANSI_ERASE_TO_END = "\033[J"

# ── 语义色角色 ────────────────────────────────────────────────

ColorRole = Literal[
    "primary", "secondary", "success", "warning", "error",
    "muted", "text", "heading", "accent", "surface",
]

# ── Tokyo Night 配色表 ────────────────────────────────────────

_RGB_COLORS: dict[ColorRole, tuple[int, int, int]] = {
    "primary":    (0x7A, 0xA2, 0xF7),   # #7AA2F7 淡蓝
    "secondary":  (0x7D, 0xCF, 0xFF),   # #7DCFFF 天蓝
    "success":    (0x9E, 0xCE, 0x6A),   # #9ECE6A 草绿
    "warning":    (0xE0, 0xAF, 0x68),   # #E0AF68 暖黄
    "error":      (0xF7, 0x76, 0x8F),   # #F7768F 玫红
    "muted":      (0x56, 0x5F, 0x89),   # #565F89 灰蓝
    "text":       (0xC0, 0xCA, 0xF5),   # #C0CAF5 亮灰
    "heading":    (0xC0, 0xCA, 0xF5),   # #C0CAF5 亮灰（+BOLD）
    "accent":     (0xBB, 0x9A, 0xF7),   # #BB9AF7 淡紫
    "surface":    (0x1A, 0x1B, 0x26),   # #1A1B26 暗面
}

_256_COLORS: dict[ColorRole, int] = {
    "primary":    111,
    "secondary":  117,
    "success":    150,
    "warning":    179,
    "error":      210,
    "muted":      60,
    "text":       189,
    "heading":    189,
    "accent":     183,
    "surface":    234,
}

_16_COLORS: dict[ColorRole, str] = {
    "primary":    ANSI_BRIGHT_CYAN,
    "secondary":  ANSI_BRIGHT_BLUE,
    "success":    ANSI_BRIGHT_GREEN,
    "warning":    ANSI_BRIGHT_YELLOW,
    "error":      ANSI_BRIGHT_RED,
    "muted":      ANSI_BRIGHT_BLACK,
    "text":       ANSI_WHITE,
    "heading":    ANSI_BOLD + ANSI_BRIGHT_WHITE,
    "accent":     ANSI_BRIGHT_MAGENTA,
    "surface":    ANSI_BLACK,
}

# 兼容旧名称（供迁移期使用）
COLOR_PRIMARY = _16_COLORS["primary"]
COLOR_SECONDARY = _16_COLORS["secondary"]
COLOR_SUCCESS = _16_COLORS["success"]
COLOR_WARNING = _16_COLORS["warning"]
COLOR_ERROR = _16_COLORS["error"]
COLOR_MUTED = _16_COLORS["muted"]
COLOR_TEXT = _16_COLORS["text"]
COLOR_HEADING = _16_COLORS["heading"]
COLOR_ACCENT = _16_COLORS["accent"]

# 对话前缀
AI_PREFIX = "◆"
USER_PREFIX = "▸"

# ── 256 色映射 ────────────────────────────────────────────────

# 标准 256 色调色板：0-15 基础色，16-231 6×6×6 色立方，232-255 灰度
def _rgb_to_256(r: int, g: int, b: int) -> int:
    """将 RGB 映射到最近的 256 色索引。"""

    # 先尝试灰度匹配（232-255）
    if r == g == b:
        if r < 8:
            return 16
        if r > 248:
            return 231
        return 232 + round((r - 8) / 10)

    # 6×6×6 色立方
    def _scale(v: int) -> int:
        if v < 48:
            return 0
        if v < 115:
            return 1
        return round((v - 35) / 40)

    return 16 + 36 * _scale(r) + 6 * _scale(g) + _scale(b)


# ── 色彩路由 ──────────────────────────────────────────────────

def get_color_sequence(role: ColorRole, caps: TerminalCapabilities) -> str:
    """根据终端能力返回对应色阶的 ANSI 前缀序列。"""

    if caps.truecolor:
        r, g, b = _RGB_COLORS[role]
        return f"\033[38;2;{r};{g};{b}m"

    if caps.color256:
        idx = _256_COLORS[role]
        return f"\033[38;5;{idx}m"

    return _16_COLORS[role]


def color_text(text: str, role: ColorRole, caps: TerminalCapabilities) -> str:
    """用语义色角色着色文本，根据终端能力自动选择色阶。"""

    if not caps.ansi:
        return text
    seq = get_color_sequence(role, caps)
    return f"{seq}{text}{ANSI_RESET}"


# ── ANSI 剥离 ────────────────────────────────────────────────

_ANSI_PATTERN = re.compile(r"\033\[[0-9;]*m")


def strip_ansi(text: str) -> str:
    """剥离文本中的所有 ANSI 转义序列。"""
    return _ANSI_PATTERN.sub("", text)
