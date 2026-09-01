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

from rich.text import Text
from textual import events
from textual.containers import Vertical
from textual.widgets import Static, TextArea

from ....agent import AskUserRequest
from ..rendering.widgets import TodoPlan


class AskUserOption(Static):
    """ask_user 的单选行；键盘导航和提交由所属 App 统一处理。"""

    can_focus = True

    def __init__(self, owner: Any, index: int, label: str) -> None:
        self._owner = owner
        self.index = index
        self.label_text = label
        super().__init__()
        self.set_selected(False)

    def set_option(self, index: int, label: str, selected: bool) -> None:
        self.index = index
        self.label_text = label
        self.set_selected(selected)

    def set_selected(self, selected: bool) -> None:
        marker = "▣" if selected else "▢"
        style = "bold ansi_yellow" if selected else "ansi_yellow"
        self.update(Text(f"{marker}{self.index + 1}.{self.label_text}", style=style))

    def on_key(self, event: events.Key) -> None:
        if event.key in {"up", "down"}:
            self._owner._move_ask_user_selection(-1 if event.key == "up" else 1)
            event.prevent_default()
            event.stop()
        elif event.key == "enter":
            self._owner._submit_ask_user_selection()
            event.prevent_default()
            event.stop()

    def on_click(self, event: events.Click) -> None:
        self._owner._select_ask_user_option(self.index)
        self._owner._submit_ask_user_selection()
        event.stop()

from .composer import Composer, _count_paste_lines, _normalize_pasted_text


_PASTE_COMPACT_LINE_THRESHOLD = 5
_PASTE_PLACEHOLDER_PATTERN = re.compile(r"\[粘贴 #\d+ \+\d+ 行\]")


class InputMixin:
    """原 ``OmniCrawlApp`` 的输入区方法。"""

    # 选中即复制后，输入框上方的复制状态提示显示时长（秒）。
    COPY_STATUS_DISPLAY_SECONDS = 2.0
    # 「已复制「…」」内最多展示的预览字符数，超出用省略号截断。
    COPY_STATUS_PREVIEW_LIMIT = 24

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
            composer = self.query_one("#composer", Composer)
            if composer.is_browsing_history():
                if not composer.text_matches_browsed_entry():
                    # 用户手动编辑使文本偏离正在浏览的历史条目时，浏览态已
                    # 结束：必须复位，否则命令菜单会一直抑制到下一次提交。
                    composer.exit_browse_mode()
                else:
                    # 历史浏览引起的程序化文本替换：不刷新命令菜单，同时
                    # 隐藏旧候选，避免残留菜单与 Enter 误补全旧命令。
                    self._hide_command_menu()
                    return
            self._refresh_command_menu(event.text_area.text)
            self._resize_composer_to_text()

    def _resize_composer_to_text(self) -> None:
        """让输入区从一行起步，随软折行增长且不挤占整个消息区。"""

        composer = self.query_one("#composer", TextArea)
        explicit_rows = composer.text.count("\n") + 1 if composer.text else 1
        is_compact_paste_placeholder = bool(
            _PASTE_PLACEHOLDER_PATTERN.fullmatch(composer.text.strip())
        )
        measured_rows = (
            1 if is_compact_paste_placeholder else composer.virtual_size.height
        )
        composer_rows = min(
            self.COMPOSER_MAX_ROWS,
            max(self.COMPOSER_MIN_ROWS, explicit_rows, measured_rows),
        )
        composer.styles.height = composer_rows
        # 命令/会话菜单每项渲染一行（说明文本同行尾随、横向省略），
        # 高度预算按可见项数计算。
        menu_rows = min(len(self._command_matches), self.COMMAND_MENU_VISIBLE_OPTIONS)
        sessions_menu_rows = min(
            len(self._session_menu_items), self.COMMAND_MENU_VISIBLE_OPTIONS
        )
        self.query_one("#composer-wrap").styles.height = (
            self.COMPOSER_BORDER_ROWS
            + composer_rows
            + menu_rows
            + sessions_menu_rows
            + self._pending_queue_rows()
            + self._todo_plan_rows()
            + self._ask_user_rows()
            + self._copy_status_rows()
        )

    def _todo_plan_rows(self) -> int:
        """返回计划区当前占用的紧凑行数。"""

        try:
            return self.query_one("#todo-plan", TodoPlan).row_count
        except Exception:
            return len(self._todo_plan_items)

    def _ask_user_rows(self) -> int:
        """返回 ask_user 面板占用的行数，供输入区布局使用。"""

        try:
            panel = self.query_one("#ask-user-panel", Vertical)
            if not panel.display:
                return 0
            question = self.query_one("#ask-user-question", Static)
            options = self._ask_user_options()
            question_rows = max(1, getattr(question.virtual_size, "height", 1))
            return 3 + question_rows + len(options)
        except Exception:
            return 0

    def _set_ask_user_request(self, request: AskUserRequest | None) -> None:
        """在 Textual 主线程显示或关闭一次 ask_user 请求。"""

        self._ask_user_request = request
        self._ask_user_answer = None
        panel = self.query_one("#ask-user-panel", Vertical)
        options = self.query_one("#ask-user-options", Vertical)
        composer = self.query_one("#composer", TextArea)
        rows = list(options.query(AskUserOption))
        if request is None:
            panel.display = False
            options.display = False
            composer.read_only = False
            self._ask_user_selection = 0
            for row in rows:
                row.display = False
            self._resize_composer_to_text()
            return
        panel.display = True
        header = {"question": "需要补充信息", "select": "请选择一项", "confirm": "请确认"}.get(request.kind, "需要回答")
        self.query_one("#ask-user-header", Static).update(Text(header, style="bold ansi_yellow"))
        self.query_one("#ask-user-question", Static).update(Text(request.question, style="bold ansi_white"))
        self._ask_user_selection = 0
        # 所有 kind 都必须携带 options；select 只允许从选项中选择，
        # question/confirm 默认从选项中选择，同时保留自由文本输入兜底。
        options.display = True
        for index, label in enumerate(request.options):
            if index < len(rows):
                row = rows[index]
                row.set_option(index, label, False)
            else:
                row = AskUserOption(self, index, label)
                options.mount(row)
            row.display = True
        for row in rows[len(request.options) :]:
            row.display = False
        if request.kind == "select":
            composer.read_only = True
            self.call_after_refresh(self._focus_ask_user_selection)
        else:
            composer.read_only = False
            composer.clear()
            composer.focus()
        self._resize_composer_to_text()

    def _ask_user(self, request: AskUserRequest) -> str | None:
        """在 UI 线程展示问题，并阻塞 Agent 工具线程等待答案。"""

        self._ask_user_event.clear()
        self._ask_user_answer = None
        self.call_from_thread(self._set_ask_user_request, request)
        while not self._ask_user_event.wait(0.05):
            if self._cancel_requested.is_set():
                self.call_from_thread(self._set_ask_user_request, None)
                return None
        answer = self._ask_user_answer
        self.call_from_thread(self._set_ask_user_request, None)
        return answer

    def _ask_user_options(self) -> list[AskUserOption]:
        return [row for row in self.query_one("#ask-user-options", Vertical).query(AskUserOption) if row.display]

    def _focus_ask_user_selection(self) -> None:
        rows = self._ask_user_options()
        if rows:
            self._ask_user_selection = min(self._ask_user_selection, len(rows) - 1)
            rows[self._ask_user_selection].focus()

    def _select_ask_user_option(self, index: int) -> None:
        rows = self._ask_user_options()
        if not rows or not 0 <= index < len(rows):
            return
        self._ask_user_selection = index
        for row_index, row in enumerate(rows):
            row.set_selected(row_index == index)
        rows[index].focus()

    def _move_ask_user_selection(self, offset: int) -> None:
        rows = self._ask_user_options()
        if not rows:
            return
        self._select_ask_user_option(
            (self._ask_user_selection + offset) % len(rows)
        )

    def _submit_ask_user_selection(self) -> None:
        request = self._ask_user_request
        if request is None or not request.options:
            return
        index = min(self._ask_user_selection, len(request.options) - 1)
        self._ask_user_selection = index
        self._ask_user_answer = request.options[index]
        self._ask_user_event.set()

    def _submit_ask_user_text(self, text: str) -> bool:
        if self._ask_user_request is None or self._ask_user_request.kind == "select":
            return False
        answer = text.strip()
        if not answer:
            return True
        self._ask_user_answer = answer
        self._ask_user_event.set()
        return True

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
        """提交输入框内容，兼容提问状态下的自定义答案。"""

        composer = self.query_one("#composer", TextArea)
        text = self._expand_compact_paste_placeholders(composer.text).strip()
        if not text:
            return
        if self._ask_user_request is not None:
            # question/confirm 由普通输入框提交；select 必须通过单选项完成，
            # 避免把任意文本误当成未声明的选项。
            if self._submit_ask_user_text(text):
                composer.clear()
                self._compact_pastes.clear()
            return
        composer.clear()
        self._compact_pastes.clear()
        if self.is_generating:
            if self._command_dispatcher.is_immediate(text):
                self._handle_command(text)
                return
            self._pending_inputs.append(text)
            self._refresh_pending_queue_count()
            return
        self._submit(text)

    def action_copy_or_clear_composer(self) -> None:
        composer = self.query_one("#composer", TextArea)
        if composer.selected_text:
            self._copy_text_and_notify(composer.selected_text)
            return
        selected_text = self.screen.get_selected_text()
        if selected_text:
            self._copy_text_and_notify(selected_text)
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

    def on_text_selected(self, event: events.TextSelected) -> None:
        """选中即复制：鼠标拖选结束即把选中文本写入系统剪贴板。

        Textual 在每次鼠标抬起后向 Screen 投递 TextSelected，事件投递前
        选区状态已落定；普通点击会先清空选区，此时 get_selected_text
        返回空值，静默跳过。只有实现了 get_selection 的消息区组件
        （AI 回复 / 思考块）参与屏幕级选区，输入框内部选区由 TextArea
        自己管理，不触发本路径。
        """

        del event  # 状态已在事件投递前落定，无需读取事件字段
        selected = self.screen.get_selected_text()
        if selected:
            self._copy_text_and_notify(selected)

    def _copy_text_and_notify(self, text: str) -> None:
        """写入剪贴板并在输入框上方显示两秒的复制状态提示。"""

        if not text:
            return
        self.copy_to_clipboard(text)
        self._show_copy_status(text)

    def _show_copy_status(self, text: str) -> None:
        """在输入框上方显示“已复制「预览…」”，两秒后自动隐藏。

        重复复制会先停掉上一个计时器再重新计时，避免提示提前消失。
        """

        preview = " ".join(str(text).split())
        if len(preview) > self.COPY_STATUS_PREVIEW_LIMIT:
            preview = preview[: self.COPY_STATUS_PREVIEW_LIMIT] + "…"
        try:
            status = self.query_one("#copy-status", Static)
        except Exception:  # noqa: BLE001 - 组件尚未挂载的测试替身
            return
        status.update(f"已复制「{preview}」")
        status.display = True
        self._copy_status_displaying = True
        self._resize_composer_to_text()
        timer = getattr(self, "_copy_status_timer", None)
        if timer is not None:
            try:
                timer.stop()
            except Exception:  # noqa: BLE001
                pass
        self._copy_status_timer = self.set_timer(
            self.COPY_STATUS_DISPLAY_SECONDS,
            self._hide_copy_status,
        )

    def _hide_copy_status(self) -> None:
        """两秒到点后隐藏复制状态提示并回收输入区高度。"""

        self._copy_status_timer = None
        self._copy_status_displaying = False
        try:
            self.query_one("#copy-status", Static).display = False
        except Exception:  # noqa: BLE001 - 组件尚未挂载的测试替身
            pass
        self._resize_composer_to_text()

    def _copy_status_rows(self) -> int:
        """复制状态提示占用的行数：显示时 1 行，其余时间 0 行。"""

        return 1 if getattr(self, "_copy_status_displaying", False) else 0


__all__ = ["AskUserOption", "InputMixin"]
