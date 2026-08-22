"""会话列表的输入框上方可选菜单。

P3 新增：``/sessions`` 不再把会话列表追加到对话区，而是以可导航的
预选项列表形式展示在输入框上方；用户用上下键选择后按 Enter 即可直接
恢复会话（等价于 ``/resume <session_id>``），无需手动复制 ID。
"""

from __future__ import annotations

import logging
import shutil
from dataclasses import dataclass
from typing import Any

from rich.text import Text
from textual import events
from textual.widgets import Static, TextArea

from ..terminal.theme import ACCENT_AMBER, TEXT_MUTED, TEXT_SECONDARY

_LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True)
class _SessionMenuItem:
    """会话菜单中的一条可选项。"""

    session_id: str
    title: str
    updated_at: str
    message_count: int
    is_current: bool


class SessionsMenuMixin:
    """输入框上方的会话列表选择菜单。"""

    def _show_sessions_menu(self) -> None:
        """获取会话列表并渲染为可选菜单；失败时回退到对话区提示。"""

        agent = self.agent
        try:
            entries = agent.list_sessions(limit=10)
        except Exception as exc:  # noqa: BLE001
            _LOGGER.debug("读取会话列表失败：%s", exc)
            self._append_message("status", f"会话列表读取失败：{exc}")
            return

        if not entries:
            self._append_message("status", "当前工作区还没有可恢复会话。")
            return

        current_id = agent.current_session_id
        items: list[_SessionMenuItem] = []
        for entry in entries:
            title = entry.title or "未命名会话"
            updated_at = entry.updated_at.astimezone().strftime("%m-%d %H:%M")
            items.append(
                _SessionMenuItem(
                    session_id=entry.session_id,
                    title=title,
                    updated_at=updated_at,
                    message_count=entry.message_count,
                    is_current=entry.session_id == current_id,
                )
            )

        self._session_menu_items = items
        self._session_menu_selection = 0
        self._render_sessions_menu()

    def _render_sessions_menu(self) -> None:
        """渲染当前可见的会话菜单项。"""

        menu = self.query_one("#sessions-menu", Static)
        items = self._session_menu_items
        if not items:
            self._hide_sessions_menu()
            return

        lines = Text(no_wrap=True, overflow="ellipsis")
        visible_limit = self.COMMAND_MENU_VISIBLE_OPTIONS
        visible_start = max(
            0,
            min(
                self._session_menu_selection - visible_limit + 1,
                len(items) - visible_limit,
            ),
        )
        visible_items = items[visible_start : visible_start + visible_limit]
        for offset, item in enumerate(visible_items):
            index = visible_start + offset
            marker = "›" if index == self._session_menu_selection else " "
            style = (
                f"bold {ACCENT_AMBER}"
                if index == self._session_menu_selection
                else TEXT_SECONDARY
            )
            current_mark = "*" if item.is_current else " "
            lines.append(
                f"{marker} {current_mark} {item.session_id[:8]}",
                style=style,
            )
            # 标题和时间紧凑排列在右侧；标题根据终端宽度动态截断，
            # 填满到屏幕边缘再加省略号。
            prefix = f"  {item.updated_at}  {item.message_count}条  "
            term_width = shutil.get_terminal_size().columns
            # CSS padding: 左 1 + 右 1 + composer-wrap 左右各 2 = 约 4 列边距
            available = term_width - 4
            prefix_len = len(prefix) + len(marker) + 1 + 1 + 1 + 8  # 前缀 + marker + 空格x2 + sid
            title_budget = max(4, available - prefix_len)
            title_text = item.title
            if len(title_text) > title_budget:
                title_text = title_text[: title_budget - 3] + "..."
            lines.append(
                f"{prefix}{title_text}",
                style=TEXT_MUTED,
            )
            if offset < len(visible_items) - 1:
                lines.append("\n")
        menu.update(lines)
        menu.display = True
        self._resize_composer_to_text()

    def _hide_sessions_menu(self) -> None:
        """隐藏会话菜单并重置状态。"""

        self._session_menu_items = []
        self._session_menu_selection = 0
        menu = self.query_one("#sessions-menu", Static)
        menu.display = False
        menu.update("")
        self._resize_composer_to_text()

    def _is_sessions_menu_active(self) -> bool:
        """会话菜单是否正在显示。"""

        return bool(self._session_menu_items)

    def _handle_sessions_menu_key(self, event: events.Key) -> bool:
        """菜单打开时消费选择键；Enter 填入 /resume 命令。"""

        if not self._session_menu_items:
            return False

        if event.key in {"up", "down"}:
            offset = -1 if event.key == "up" else 1
            self._session_menu_selection = (
                self._session_menu_selection + offset
            ) % len(self._session_menu_items)
            self._render_sessions_menu()
            return True

        if event.key in {"enter", "tab"}:
            selected = self._session_menu_items[self._session_menu_selection]
            composer = self.query_one("#composer", TextArea)
            resume_cmd = f"/resume {selected.session_id}"
            composer.text = resume_cmd
            composer.cursor_location = (0, len(resume_cmd))
            self._hide_sessions_menu()
            return True

        # 其他键（包括 escape）关闭菜单
        self._hide_sessions_menu()
        return False


__all__ = ["SessionsMenuMixin"]
