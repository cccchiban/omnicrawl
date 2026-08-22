"""斜杠命令菜单的实时筛选与渲染。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：``CommandMenuMixin``
的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；编辑器、命令选项源与
布局高度计算仍通过 ``self`` 在 MRO 上解析（``Composer``、``CommandDispatcher``、
``_resize_composer_to_text`` 等）。
"""

from __future__ import annotations

from rich.text import Text
from textual import events
from textual.widgets import Static, TextArea

from ....commands.slash import build_slash_command_options
from ..terminal.theme import ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY


class CommandMenuMixin:
    """原 ``OmniCrawlApp`` 的斜杠命令菜单方法。"""

    def _handle_composer_command_key(self, event: events.Key) -> bool:
        """菜单打开时消费选择键；Enter/Tab 只补全，不提交命令。"""

        if not self._command_matches:
            return False
        composer = self.query_one("#composer", TextArea)
        if event.key in {"up", "down"}:
            offset = -1 if event.key == "up" else 1
            self._command_selection = (self._command_selection + offset) % len(self._command_matches)
            self._render_command_menu()
            return True
        if event.key in {"enter", "tab"}:
            selected = self._command_matches[self._command_selection]
            target = selected["insert"]
            # 输入已是完整命令时，Enter 必须提交执行；否则会反复“补全”同一文本，
            # 导致 /settings、/new 这类无参数命令永远打不开。
            if composer.text == target or composer.text.strip() == selected["command"]:
                self._hide_command_menu()
                return event.key != "enter"
            composer.text = target
            composer.cursor_location = (0, len(composer.text))
            self._hide_command_menu()
            return True
        return False

    def _refresh_command_menu(self, value: str) -> None:
        """根据当前斜杠前缀实时筛选统一命令源，并保留完整候选供上下键选择。"""

        query = value.strip().lower()
        if not query.startswith("/") or any(char.isspace() for char in value):
            self._hide_command_menu()
            return
        matches = [
            option
            for option in build_slash_command_options(self.agent)
            if query in option["search"].lower()
        ]
        # Python 排序稳定：仅把前缀命中提到前面，同级保留统一命令源的产品顺序。
        matches.sort(key=lambda option: not option["command"].lower().startswith(query))
        self._command_matches = matches
        self._command_selection = 0
        if not self._command_matches:
            self._hide_command_menu()
            return
        self._render_command_menu()

    def _render_command_menu(self) -> None:
        menu = self.query_one("#command-menu", Static)
        lines = Text(no_wrap=True, overflow="ellipsis")
        visible_limit = self.COMMAND_MENU_VISIBLE_OPTIONS
        visible_start = max(
            0,
            min(
                self._command_selection - visible_limit + 1,
                len(self._command_matches) - visible_limit,
            ),
        )
        visible_matches = self._command_matches[
            visible_start : visible_start + visible_limit
        ]
        for offset, option in enumerate(visible_matches):
            index = visible_start + offset
            marker = "›" if index == self._command_selection else " "
            style = f"bold {ACCENT_AMBER}" if index == self._command_selection else TEXT_SECONDARY
            lines.append(f"{marker} {option['command']}", style=style)
            description = " ".join(option.get("description", "").split())
            if description:
                lines.append(f"  · {description}", style=TEXT_MUTED)
            if offset < len(visible_matches) - 1:
                lines.append("\n")
        menu.update(lines)
        menu.display = True
        self._resize_composer_to_text()

    def _hide_command_menu(self) -> None:
        self._command_matches = []
        self._command_selection = 0
        menu = self.query_one("#command-menu", Static)
        menu.display = False
        menu.update("")
        self._resize_composer_to_text()


__all__ = ["CommandMenuMixin"]
