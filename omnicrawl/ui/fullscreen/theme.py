"""OmniCrawl 全屏 TUI 的终端原生透明主题令牌。"""

from __future__ import annotations

from textual.theme import Theme


THEME_NAME = "omnicrawl-terminal"

TRANSPARENT = "transparent"
TERMINAL_FOREGROUND = "ansi_default"
TERMINAL_BACKGROUND = "ansi_default"

# Rich 使用 default/标准色名，Textual CSS 使用 ansi_default/ansi_*；
# 两种表示最终都交给终端自己的前景色与 ANSI 调色板解析。
TEXT_PRIMARY = "default"
TEXT_SECONDARY = "default"
TEXT_MUTED = "dim"
TEXT_FAINT = "dim"
TEXT_ON_ACCENT = "default"

BORDER_SUBTLE = "dim"
BORDER_STRONG = "default"
SCROLLBAR = "default"

ACCENT_GREEN = "green"
ACCENT_GREEN_SOFT = TRANSPARENT
ACCENT_BLUE = "blue"
ACCENT_BLUE_SOFT = TRANSPARENT
ACCENT_AMBER = "yellow"
ACCENT_AMBER_SOFT = TRANSPARENT
ACCENT_RED = "red"
ACCENT_PURPLE = "magenta"

_CSS_GREEN = "ansi_green"
_CSS_BLUE = "ansi_blue"
_CSS_AMBER = "ansi_yellow"
_CSS_RED = "ansi_red"
_CSS_PURPLE = "ansi_magenta"


TERMINAL_THEME = Theme(
    name=THEME_NAME,
    primary=_CSS_GREEN,
    secondary=_CSS_BLUE,
    accent=_CSS_PURPLE,
    warning=_CSS_AMBER,
    error=_CSS_RED,
    success=_CSS_GREEN,
    foreground=TERMINAL_FOREGROUND,
    background=TERMINAL_BACKGROUND,
    surface=TERMINAL_BACKGROUND,
    panel=TERMINAL_BACKGROUND,
    boost=TERMINAL_BACKGROUND,
    ansi=True,
    dark=True,
    variables={
        "ansi-background": TERMINAL_BACKGROUND,
        "ansi-foreground": TERMINAL_FOREGROUND,
        "terminal-canvas": TRANSPARENT,
        "terminal-background": TRANSPARENT,
        "terminal-surface": TRANSPARENT,
        "terminal-panel": TRANSPARENT,
        "terminal-hover": TRANSPARENT,
        "terminal-overlay": TRANSPARENT,
        "terminal-text": TERMINAL_FOREGROUND,
        "terminal-text-secondary": TERMINAL_FOREGROUND,
        "terminal-text-muted": TERMINAL_FOREGROUND,
        "terminal-text-faint": TERMINAL_FOREGROUND,
        "terminal-text-on-accent": TERMINAL_FOREGROUND,
        "terminal-border": TERMINAL_FOREGROUND,
        "terminal-border-strong": TERMINAL_FOREGROUND,
        "terminal-scrollbar": TERMINAL_FOREGROUND,
        "terminal-green": _CSS_GREEN,
        "terminal-green-soft": ACCENT_GREEN_SOFT,
        "terminal-blue": _CSS_BLUE,
        "terminal-blue-soft": ACCENT_BLUE_SOFT,
        "terminal-amber": _CSS_AMBER,
        "terminal-amber-soft": ACCENT_AMBER_SOFT,
        "terminal-red": _CSS_RED,
        "terminal-purple": _CSS_PURPLE,
        "button-color-foreground": TERMINAL_FOREGROUND,
        "block-cursor-background": TERMINAL_FOREGROUND,
        "block-cursor-foreground": TERMINAL_BACKGROUND,
        "block-cursor-text-style": "reverse",
        "footer-key-foreground": _CSS_BLUE,
        "input-cursor-background": TERMINAL_FOREGROUND,
        "input-cursor-foreground": TERMINAL_BACKGROUND,
        "input-cursor-text-style": "reverse",
        "input-selection-background": _CSS_BLUE,
        "input-selection-foreground": TERMINAL_FOREGROUND,
    },
)


def terminal_css(source: str) -> str:
    """展开终端主题占位符，使独立 Screen 不依赖父 App 注册主题。"""

    rendered = source
    variables = sorted(
        TERMINAL_THEME.variables.items(),
        key=lambda item: len(item[0]),
        reverse=True,
    )
    for name, value in variables:
        rendered = rendered.replace(f"${name}", value)
    return rendered
