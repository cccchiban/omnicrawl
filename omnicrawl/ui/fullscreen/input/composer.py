"""多行输入编辑器与粘贴文本归一化。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）。
"""

from __future__ import annotations

from typing import Any, Callable

from textual import events
from textual.widgets import TextArea


def _normalize_pasted_text(text: str) -> str:
    """把终端粘贴中的 CRLF/CR 统一为 TextArea 使用的 LF。"""

    return text.replace("\r\n", "\n").replace("\r", "\n")


def _count_paste_lines(text: str) -> int:
    """按编辑器语义统计粘贴行数，保留末尾空行。"""

    if not text:
        return 0
    return text.count("\n") + 1


class Composer(TextArea):
    """多行编辑器：Enter 由应用提交，Shift+Enter 插入真实换行。"""

    def __init__(
        self,
        *,
        submit_handler: Callable[[], None],
        command_key_handler: Callable[[events.Key], bool],
        sessions_menu_key_handler: Callable[[events.Key], bool],
        copy_or_clear_handler: Callable[[], None],
        paste_handler: Callable[[str], str | None],
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._submit_handler = submit_handler
        self._command_key_handler = command_key_handler
        self._sessions_menu_key_handler = sessions_menu_key_handler
        self._copy_or_clear_handler = copy_or_clear_handler
        self._paste_handler = paste_handler
        # 已发送消息的历史浏览状态（bash 式上下键回看）。
        self._history: list[str] = []
        # -1 表示编辑器显示的是用户自己的草稿；>= 0 表示正在浏览第 N 条历史。
        self._history_index = -1
        # 进入历史浏览前编辑器里的内容，浏览结束按一次下键时恢复。
        self._history_draft = ""
        # 程序化替换文本期间置位，供应用层跳过命令菜单刷新。
        self._history_navigating = False

    def _insert_paste_text(self, text: str) -> None:
        replacement = self._paste_handler(text)
        insert_text = replacement if replacement is not None else _normalize_pasted_text(text)
        if result := self._replace_via_keyboard(insert_text, *self.selection):
            self.move_cursor(result.end_location)
            self.focus()

    async def _on_paste(self, event: events.Paste) -> None:
        self._insert_paste_text(event.text)
        event.prevent_default()
        event.stop()

    def action_paste(self) -> None:
        if self.read_only:
            return
        self._insert_paste_text(self.app.clipboard)

    def history_record(self, text: str) -> None:
        """记录一条已发送消息供上下键回看；与最近一条重复时不重复入列。

        提交即退出浏览态：无论是否新入列，下一条上键都从最新一条开始。
        """

        if text and (not self._history or self._history[-1] != text):
            self._history.append(text)
        self._history_index = -1
        self._history_draft = ""

    def is_browsing_history(self) -> bool:
        """是否处于历史浏览（含程序化替换文本的瞬间）。"""

        return self._history_index >= 0 or self._history_navigating

    def text_matches_browsed_entry(self) -> bool:
        """当前文本是否仍是正在浏览的历史条目（程序化替换后的瞬时状态）。

        浏览历史时，程序化替换文本与历史条目完全一致；用户一旦手动编辑
        （输入/粘贴/删除）就会偏离，调用方据此判定浏览态已结束。
        """

        if 0 <= self._history_index < len(self._history):
            return self.text == self._history[self._history_index]
        return False

    def exit_browse_mode(self) -> None:
        """退出历史浏览态（用户开始手动编辑时调用）。

        浏览态会抑制斜杠命令菜单刷新；用户手动编辑后必须复位，否则菜单
        会一直不显示直到下一次提交。
        """

        self._history_index = -1
        self._history_draft = ""
        self._history_navigating = False

    def navigate_history(self, direction: int) -> bool:
        """按上下键浏览已发送消息；返回 True 表示按键已被历史浏览消费。

        ``direction`` 为 -1（上键，向更早翻）或 +1（下键，向更新翻）。
        进入浏览前先把编辑器当前内容保存为草稿；下键翻到最新一条之后再
        按一次即恢复草稿并退出浏览，与 bash readline 行为一致。没有历史
        记录、或下键处于非浏览态时返回 False，让调用方回退到会话滚动。
        """

        if not self._history:
            return False
        if direction < 0:
            if self._history_index < 0:
                self._history_draft = self.text
                self._history_index = len(self._history) - 1
            elif self._history_index > 0:
                self._history_index -= 1
            else:
                return True
        else:
            if self._history_index < 0:
                return False
            if self._history_index >= len(self._history) - 1:
                # 已翻到最新一条：恢复进入浏览前的草稿并结束浏览。
                self._history_index = -1
                self._replace_history_text(self._history_draft)
                return True
            self._history_index += 1
        self._replace_history_text(self._history[self._history_index])
        return True

    def _replace_history_text(self, text: str) -> None:
        """替换编辑器文本并把光标移到末尾；期间抑制命令菜单刷新。"""

        self._history_navigating = True
        try:
            self.text = text
            lines = text.split("\n")
            self.cursor_location = (len(lines) - 1, len(lines[-1]))
        finally:
            self._history_navigating = False

    def on_key(self, event: events.Key) -> None:
        # ask_user 提问状态下，选项由单选行呈现：select 始终由选项状态机
        # 消费上下键和回车；question/confirm 在输入框为空时同样导航/确认
        # 选项，输入了自定义文本后回车则提交该文本。
        app = self.app
        request = getattr(app, "_ask_user_request", None)
        if request is not None:
            is_select = getattr(request, "kind", "") == "select"
            custom_mode = getattr(app, "_ask_user_custom_mode", False)
            composer_empty = not self.text.strip()
            # select 的选项模式（未进入自定义输入）下按键始终由选项状态机
            # 消费；其余情况（含「but I Think...」自定义输入模式）输入框为空
            # 时上下键/回车导航选项，输入了内容则保持编辑器语义（移动光标/
            # 提交文本）。
            option_navigation = composer_empty or (is_select and not custom_mode)
            if event.key in {"up", "down"} and option_navigation:
                app._move_ask_user_selection(-1 if event.key == "up" else 1)
                event.prevent_default()
                event.stop()
                return
            if event.key == "enter" and option_navigation:
                app._submit_ask_user_selection()
                event.prevent_default()
                event.stop()
                return
        if event.key == "escape":
            self.app.action_cancel_or_focus()
            event.prevent_default()
            event.stop()
            return
        # 某些 Windows Terminal / VS Code 组合会把 Shift+Enter 归一为
        # Key("enter", "\n")；先判断换行事件，避免被普通 Enter 分支提交。
        is_newline_key = event.key in {
            "shift+enter",
            "shift+\r",
            "shift+j",
        } or (event.key == "enter" and event.character == "\n")
        if event.key == "ctrl+c":
            self._copy_or_clear_handler()
            event.prevent_default()
            event.stop()
        elif is_newline_key:
            self.insert("\n")
            event.prevent_default()
            event.stop()
        elif self._sessions_menu_key_handler(event):
            event.prevent_default()
            event.stop()
        elif self._command_key_handler(event):
            event.prevent_default()
            event.stop()
        elif event.key in {"up", "down"}:
            direction = -1 if event.key == "up" else 1
            if not self.navigate_history(direction):
                # 没有可回看的历史时，保留原有的会话滚动行为。
                scroll_action = getattr(
                    self.app,
                    f"action_scroll_conversation_{event.key}",
                )
                scroll_action()
            event.prevent_default()
            event.stop()
        elif event.key == "enter":
            self._submit_handler()
            event.prevent_default()
            event.stop()


__all__ = ["Composer", "_count_paste_lines", "_normalize_pasted_text"]
