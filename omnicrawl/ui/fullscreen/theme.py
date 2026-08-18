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
# 顶部 HUD 分隔竖线使用的灰色（ANSI 亮黑）：比 dim 更清晰可见，又比默认
# 前景克制，避免竖线与正文抢注意力。
BORDER_MUTED = "bright_black"

ACCENT_GREEN = "green"
ACCENT_GREEN_SOFT = TRANSPARENT
ACCENT_BLUE = "blue"
ACCENT_BLUE_SOFT = TRANSPARENT
ACCENT_AMBER = "yellow"
ACCENT_AMBER_SOFT = TRANSPARENT
ACCENT_RED = "red"
ACCENT_PURPLE = "magenta"

# 使用 RGBA 颜色表达透明度，避免 ANSI 颜色名后附百分比在不同 Textual
# 解析路径中被当作实色处理。终端支持真彩色时会与底色混合，ANSI 降级时
# 仍保留对应的颜色语义。
USER_BACKGROUND = "rgba(170, 80, 210, 0.26)"
# 工具调用与输出统一使用低不透明度的 RGBA 绿色，叠加终端背景后呈现
# 淡绿色块；聚焦使用更高对比度的独立背景。
TOOL_BACKGROUND = "rgba(0, 170, 90, 0.22)"
TOOL_FOCUS_BACKGROUND = "rgba(0, 140, 80, 0.38)"
# 工具输出正文与折叠提示统一使用灰色（Rich 文本用 bright_black，
# Textual CSS 用 ansi_bright_black，见下方 variables）。
TOOL_TEXT = "bright_black"

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
        "terminal-user-background": USER_BACKGROUND,
        "terminal-tool-background": TOOL_BACKGROUND,
        "terminal-tool-focus-background": TOOL_FOCUS_BACKGROUND,
        "terminal-tool-text": "ansi_bright_black",
        "terminal-overlay": TRANSPARENT,
        "terminal-text": TERMINAL_FOREGROUND,
        "terminal-text-secondary": TERMINAL_FOREGROUND,
        "terminal-text-muted": TERMINAL_FOREGROUND,
        "terminal-text-faint": TERMINAL_FOREGROUND,
        "terminal-text-on-accent": TERMINAL_FOREGROUND,
        "terminal-border": TERMINAL_FOREGROUND,
        "terminal-border-strong": TERMINAL_FOREGROUND,
        "terminal-border-muted": "ansi_bright_black",
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
        # Textual 全屏驱动会隐藏终端硬件光标；输入框依靠软件光标绘制。
        # 使用显式 ANSI 黑白色，避免 ansi_default + reverse 在不同终端中与背景融合。
        "input-cursor-background": "ansi_white",
        "input-cursor-foreground": "ansi_black",
        "input-cursor-text-style": "none",
        "input-selection-background": _CSS_BLUE,
        "input-selection-foreground": TERMINAL_FOREGROUND,
    },
)



def terminal_select_css(selector: str = ".choice-select") -> str:
    """返回终端主题下可点击、可见选中态的下拉框样式。

    Textual 8.x 中 Select 是 can_focus 容器：TAB 聚焦落在 Select 自身而非内部
    SelectCurrent，因此除 SelectCurrent:focus 外还需匹配 :focus 与 :focus-within，
    否则收起状态下聚焦无边框反馈。
    """

    return f"""
    {selector} {{
        height: 3;
        min-height: 3;
        width: 1fr;
        color: $terminal-text;
    }}
    {selector} > SelectCurrent {{
        height: 3;
        min-height: 3;
        padding: 0 1;
        border: solid $terminal-border;
        background: $terminal-background;
        color: $terminal-text;
        pointer: pointer;
    }}
    {selector} > SelectCurrent:hover {{
        border: solid $terminal-blue;
        background: $terminal-hover;
    }}
    {selector} > SelectCurrent:focus,
    {selector}:focus > SelectCurrent,
    {selector}:focus-within > SelectCurrent,
    {selector}.-expanded > SelectCurrent {{
        border: tall $terminal-blue;
        background: $terminal-background;
    }}
    {selector} > SelectCurrent Static#label,
    {selector} > SelectCurrent.-has-value Static#label {{
        color: $terminal-text;
    }}
    {selector} > SelectOverlay {{
        max-height: 12;
        padding: 0;
        border: solid $terminal-blue;
        background: $terminal-background;
        color: $terminal-text;
    }}
    {selector} > SelectOverlay:focus {{
        border: tall $terminal-blue;
    }}
    {selector} > SelectOverlay > .option-list--option {{
        height: 2;
        padding: 0 1;
    }}
    {selector} > SelectOverlay > .option-list--option-hover,
    {selector} > SelectOverlay > .option-list--option-highlighted {{
        background: $terminal-blue;
        color: $terminal-text;
        text-style: bold;
    }}
    """


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
