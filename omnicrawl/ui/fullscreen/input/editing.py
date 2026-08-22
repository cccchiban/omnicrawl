"""输入区联动：粘贴压缩、编辑器自适应、提交、复制/清空与自动计划区。

P3 重构从 ``ui/fullscreen/__init__.py`` 拆出（2026-08-21）：``InputMixin``
的方法名、签名与行为与原 ``OmniCrawlApp`` 逐字一致；``Composer`` 控件与
粘贴文本归一化位于同包 ``composer.py``，斜杠命令菜单位于同包 ``menu.py``。
本模块不依赖 Textual 之外的 UI 状态，跨领域方法仍通过 ``self`` 在 MRO 上解析。
"""

from __future__ import annotations

import re
import sys
from typing import Any

from textual import events
from textual.widgets import Static, TextArea

from ..rendering.widgets import TodoPlan
from .composer import Composer, _count_paste_lines, _normalize_pasted_text


_PASTE_COMPACT_LINE_THRESHOLD = 5
_PASTE_PLACEHOLDER_PATTERN = re.compile(r"\[粘贴 #\d+ \+\d+ 行\]")


class InputMixin:
    """原 ``OmniCrawlApp`` 的输入区方法。"""

    def _compact_paste_if_needed(self, text: str) -> str | None:
        pasted_text = _normalize_pasted_text(text)
        line_count = _count_paste_lines(pasted_text)
        if line_count <= _PASTE_COMPACT_LINE_THRESHOLD:
            return None
        self._paste_sequence += 1
        placeholder = f"[粘贴 #{self._paste_sequence} +{line_count} 行]"
        self._compact_pastes[placeholder] = pasted_text
        return placeholder

    def _expand_compact_paste_placeholders(self, text: str) -> str:
        if not self._compact_pastes:
            return text
        return _PASTE_PLACEHOLDER_PATTERN.sub(
            lambda match: self._compact_pastes.get(match.group(0), match.group(0)),
            text,
        )

    def _prune_compact_paste_placeholders(self, text: str) -> None:
        if not self._compact_pastes:
            return
        self._compact_pastes = {
            placeholder: pasted_text
            for placeholder, pasted_text in self._compact_pastes.items()
            if placeholder in text
        }

    def on_text_area_changed(self, event: TextArea.Changed) -> None:
        if event.text_area.id == "composer":
            self._prune_compact_paste_placeholders(event.text_area.text)
            # 历史浏览引起的文本替换不应触发斜杠命令菜单：否则回看以
            # "/" 开头的内容时菜单会抢走上下键，打断连续浏览。
            if not self.query_one("#composer", Composer).is_browsing_history():
                self._refresh_command_menu(event.text_area.text)
            self._resize_composer_to_text()

    def _resize_composer_to_text(self) -> None:
        """让输入区从一行起步，随软折行增长且不挤占整个消息区。"""

        composer = self.query_one("#composer", TextArea)
        explicit_rows = composer.text.count("\n") + 1 if composer.text else 1
        composer_rows = min(
            self.COMPOSER_MAX_ROWS,
            max(
                self.COMPOSER_MIN_ROWS,
                explicit_rows,
                composer.virtual_size.height,
            ),
        )
        composer.styles.height = composer_rows
        menu_rows = min(len(self._command_matches), self.COMMAND_MENU_VISIBLE_OPTIONS)
        self.query_one("#composer-wrap").styles.height = (
            self.COMPOSER_BORDER_ROWS
            + composer_rows
            + menu_rows
            + self._pending_queue_rows()
            + self._todo_plan_rows()
        )

    def _todo_plan_rows(self) -> int:
        """返回计划区当前占用的紧凑行数。"""

        try:
            return self.query_one("#todo-plan", TodoPlan).row_count
        except Exception:
            return len(self._todo_plan_items)

    def _handle_todo_update(self, payload: dict[str, Any]) -> None:
        """在输入框上方替换 Agent 的自动计划，并让其占用布局高度。"""

        items = payload.get("todos") if isinstance(payload, dict) else []
        self._todo_plan_items = list(items) if isinstance(items, list) else []
        plan = self.query_one("#todo-plan", TodoPlan)
        plan.update_items(self._todo_plan_items)
        self._resize_composer_to_text()

    def _clear_todo_plan(self) -> None:
        self._todo_plan_items = []
        try:
            plan = self.query_one("#todo-plan", TodoPlan)
        except Exception:
            return
        plan.update_items([])
        self._resize_composer_to_text()

    def _submit_composer_text(self) -> None:
        """提交编辑器内容，保留内部换行且忽略纯空白输入。"""

        composer = self.query_one("#composer", TextArea)
        text = self._expand_compact_paste_placeholders(composer.text).strip()
        if not text:
            return
        composer.clear()
        self._compact_pastes.clear()
        if self.is_generating:
            if self._command_dispatcher.is_immediate(text):
                # 即时命令（/settings、/skills、只读查询等）不占用回合线程，
                # 生成期间直接执行；有 I/O 或修改回合状态的命令仍按 FIFO 排队。
                self._handle_command(text)
                return
            self._pending_inputs.append(text)
            self._refresh_pending_queue_count()
            return
        self._submit(text)

    def action_copy_or_clear_composer(self) -> None:
        composer = self.query_one("#composer", TextArea)
        if composer.selected_text:
            self.copy_to_clipboard(composer.selected_text)
            return
        selected_text = self.screen.get_selected_text()
        if selected_text:
            self.copy_to_clipboard(selected_text)
            return
        composer.clear()
        self._compact_pastes.clear()

    def copy_to_clipboard(self, text: str) -> None:
        """覆盖 Textual 的 OSC52 剪贴板实现。

        Textual 默认把文本作为 OSC52 转义序列写入终端（\x1b]52;c;<base64>\a），
        该协议仅 Windows Terminal / VS Code 等现代终端支持；传统 conhost
        （旧版 PowerShell / cmd 窗口）会直接忽略，导致 Ctrl+C 后系统剪贴板为空。
        这里在 Windows 上用 Win32 API 直接写入系统剪贴板（CF_UNICODETEXT），
        失败时回退到父类 OSC52 实现（兼容 Windows Terminal）。
        """
        if sys.platform == "win32" and text:
            try:
                import ctypes
                from ctypes import wintypes

                CF_UNICODETEXT = 13
                GMEM_MOVEABLE = 0x0002
                GMEM_ZEROINIT = 0x0040
                user32 = ctypes.windll.user32
                kernel32 = ctypes.windll.kernel32
                # 64 位系统上 HGLOBAL 是指针，必须显式声明 restype/argtypes，
                # 否则 ctypes 默认按 32 位 int 截断句柄导致 GlobalLock 失败
                kernel32.GlobalAlloc.restype = wintypes.HGLOBAL
                kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
                kernel32.GlobalLock.restype = ctypes.c_void_p
                kernel32.GlobalLock.argtypes = [wintypes.HGLOBAL]
                kernel32.GlobalFree.argtypes = [wintypes.HGLOBAL]
                kernel32.GlobalUnlock.argtypes = [wintypes.HGLOBAL]
                user32.SetClipboardData.restype = wintypes.HANDLE
                user32.SetClipboardData.argtypes = [wintypes.UINT, wintypes.HANDLE]
                user32.OpenClipboard.argtypes = [wintypes.HWND]
                # UTF-16LE 并以 NUL 结尾，供 CF_UNICODETEXT 使用
                data = (text + "\0").encode("utf-16-le")
                if not user32.OpenClipboard(None):
                    # 剪贴板被其他进程占用，回退 OSC52
                    return super().copy_to_clipboard(text)
                try:
                    user32.EmptyClipboard()
                    handle = kernel32.GlobalAlloc(
                        GMEM_MOVEABLE | GMEM_ZEROINIT, len(data)
                    )
                    if not handle:
                        return super().copy_to_clipboard(text)
                    locked = kernel32.GlobalLock(handle)
                    if not locked:
                        kernel32.GlobalFree(handle)
                        return super().copy_to_clipboard(text)
                    try:
                        ctypes.memmove(locked, data, len(data))
                    finally:
                        kernel32.GlobalUnlock(handle)
                    # 成功后句柄归系统所有，不可 GlobalFree
                    if not user32.SetClipboardData(CF_UNICODETEXT, handle):
                        kernel32.GlobalFree(handle)
                        return super().copy_to_clipboard(text)
                finally:
                    user32.CloseClipboard()
                return
            except Exception:
                # 任何异常都回退到 OSC52
                return super().copy_to_clipboard(text)
        super().copy_to_clipboard(text)


__all__ = ["InputMixin"]
