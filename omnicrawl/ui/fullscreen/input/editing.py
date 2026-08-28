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

from ..rendering.widgets import TodoPlan


class ConfirmationOption(Static):
    """提问选项行：用终端字符渲染，并把键盘导航交给提问状态机。"""

    can_focus = True

    def __init__(self, owner: Any, index: int, label: str) -> None:
        self._owner = owner
        self.index = index
        self.label_text = label
        # 不使用固定 ID：多个问题会按顺序重建选项行，Textual 异步卸载旧行时
        # 可能与下一组同序号的行短暂冲突；类型查询已足够精确。
        super().__init__()

    def set_option(self, index: int, label: str, selected: bool) -> None:
        self.index = index
        self.label_text = label
        self.set_selected(selected)

    def set_selected(self, selected: bool) -> None:
        marker = "▣" if selected else "▢"
        style = "bold ansi_yellow" if selected else "ansi_yellow"
        self.update(Text(f"{marker}{self.index + 1}.{self.label_text}", style=style))

    def on_key(self, event: events.Key) -> None:
        if event.key in {"left", "right"}:
            self._owner._move_confirmation_group(-1 if event.key == "left" else 1)
            event.prevent_default()
            event.stop()
        elif event.key in {"up", "down"}:
            self._owner._move_confirmation_selection(-1 if event.key == "up" else 1)
            event.prevent_default()
            event.stop()
        elif event.key == "enter":
            self._owner._confirm_confirmation_selection()
            event.prevent_default()
            event.stop()

    def on_click(self, event: events.Click) -> None:
        self._owner._select_confirmation_option(self.index)
        self._owner._confirm_confirmation_selection()
        event.stop()


_CONFIRMATION_CUSTOM_OPTION = "我有自己的想法"

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
            + self._confirmation_rows()
        )

    def _todo_plan_rows(self) -> int:
        """返回计划区当前占用的紧凑行数。"""

        try:
            return self.query_one("#todo-plan", TodoPlan).row_count
        except Exception:
            return len(self._todo_plan_items)

    def _confirmation_rows(self) -> int:
        """返回当前问题面板占用的行数（含题号、问题和面板边框）。"""

        try:
            panel = self.query_one("#confirmation-panel", Vertical)
            if not panel.display:
                return 0
            header = self.query_one("#confirmation-header", Static)
            question = self.query_one("#confirmation-question", Static)
            options = self.query_one("#confirmation-options", Vertical)
            option_rows = (
                sum(1 for row in options.query(ConfirmationOption) if row.display)
                if options.display
                else 0
            )
            question_rows = max(1, getattr(question.virtual_size, "height", 1))
            # 面板上下边框 2 行，题号 1 行，问题至少 1 行，之后才是选项。
            return (
                2
                + max(1, getattr(header.virtual_size, "height", 1))
                + question_rows
                + option_rows
            )
        except Exception:
            return 4 + len(getattr(self, "_confirmation_options", []))

    def _set_confirmation_required(
        self,
        options: list[dict[str, Any]]
        | list[str]
        | tuple[str, ...]
        | list[list[str]]
        | bool
        | None,
    ) -> None:
        """显示当前问题及其单选答案，并从第一个问题开始等待选择。

        新协议使用 ``{"question": ..., "options": [...]}`` 保存问题与选项
        的配对；同时兼容旧版只传选项列表的调用方，旧调用方的问题正文为空。
        """

        questions: list[str] = []
        groups: list[list[str]] = []
        if options is True:
            questions = [""]
            groups = [[_CONFIRMATION_CUSTOM_OPTION]]
        elif options is False or options is None:
            pass
        elif options and all(isinstance(item, dict) for item in options):
            for item in options:
                question = str(item.get("question", "") or "").strip()
                raw_options = item.get("options", [])
                if not isinstance(raw_options, (list, tuple)):
                    raw_options = []
                group = [
                    str(option).strip()
                    for option in raw_options
                    if str(option).strip()
                ]
                if not group:
                    group = [_CONFIRMATION_CUSTOM_OPTION]
                elif _CONFIRMATION_CUSTOM_OPTION not in group:
                    group.append(_CONFIRMATION_CUSTOM_OPTION)
                questions.append(question)
                groups.append(group)
        elif options and all(isinstance(item, (list, tuple)) for item in options):
            groups = [
                [str(option).strip() for option in group if str(option).strip()]
                for group in options
            ]
            groups = [group for group in groups if group]
            questions = [""] * len(groups)
            for group in groups:
                if _CONFIRMATION_CUSTOM_OPTION not in group:
                    group.append(_CONFIRMATION_CUSTOM_OPTION)
        elif options:
            groups = [[str(item).strip() for item in options if str(item).strip()]]
            questions = [""] if groups[0] else []
            if groups[0] and _CONFIRMATION_CUSTOM_OPTION not in groups[0]:
                groups[0].append(_CONFIRMATION_CUSTOM_OPTION)

        required = bool(groups)
        self._confirmation_required = required
        self._confirmation_questions = questions[: len(groups)]
        if len(self._confirmation_questions) < len(groups):
            self._confirmation_questions.extend(
                [""] * (len(groups) - len(self._confirmation_questions))
            )
        self._confirmation_groups = groups
        self._confirmation_group_index = 0
        self._confirmation_selection = 0
        # 初次显示时全部为空框；焦点只表示当前键盘位置，不等于已选中。
        self._confirmation_has_selection = False
        # 每个问题独立保存已确认答案、当前选项位置和自定义回答。左右切题
        # 时只切换视图，不丢失已确认内容；回到该题后可继续修改再回车确认。
        self._confirmation_answers: list[str | None] = [None] * len(groups)
        self._confirmation_selections: list[int | None] = [None] * len(groups)
        self._confirmation_custom_answers: list[str | None] = [None] * len(groups)
        self._confirmation_custom_mode = False
        self._confirmation_options = groups[0] if groups else []
        panel = self.query_one("#confirmation-panel")
        panel.display = required
        if required:
            panel.add_class("active")
        else:
            panel.remove_class("active")
        # 选项出现时锁定输入框；只有选中“我有自己的想法”后才重新开放。
        composer = self.query_one("#composer", TextArea)
        # 选项阶段只允许导航；清除提问状态后恢复普通输入。
        composer.read_only = required
        self._replace_confirmation_rows()

    def _update_confirmation_question(self) -> None:
        """在输入区面板中渲染当前题号与问题正文。"""

        question = ""
        if self._confirmation_questions:
            question = self._confirmation_questions[self._confirmation_group_index]
        question = question or "请回答以下问题"
        header = Text(
            f"当前问题 {self._confirmation_group_index + 1}/{len(self._confirmation_groups)}",
            style="bold ansi_yellow",
        )
        self.query_one("#confirmation-header", Static).update(header)
        self.query_one("#confirmation-question", Static).update(
            Text(question, style="bold ansi_white")
        )

    def _confirmation_rows_container(self) -> Vertical:
        return self.query_one("#confirmation-options", Vertical)

    def _confirmation_rows_widgets(self) -> list[ConfirmationOption]:
        # 必须限定在当前容器内，并排除为较长问题组保留但当前隐藏的行。
        # 全局 query 或隐藏行参与导航都会让上下键落到不存在的选项。
        return [
            row
            for row in self._confirmation_rows_container().query(ConfirmationOption)
            if row.display
        ]

    def _all_confirmation_rows_widgets(self) -> list[ConfirmationOption]:
        """返回容器中的全部行，供切换问题时复用隐藏行。"""
        return list(self._confirmation_rows_container().query(ConfirmationOption))

    def _replace_confirmation_rows(self) -> None:
        """刷新当前问题行，只隐藏/复用控件，不异步删除 DOM。"""
        container = self._confirmation_rows_container()
        container.display = False
        # Textual 的 remove_children 是异步操作；反复删挂会制造旧行残留、
        # 高度错算和输入区空白。固定容器内的行并复用，切题时只更新文本。
        # 这里直接同步挂载首组，避免选项已显示但子行尚未完成布局的空帧。
        self._sync_confirmation_rows()

    def _sync_confirmation_rows(self) -> None:
        container = self._confirmation_rows_container()
        required_count = len(self._confirmation_options)
        rows = self._all_confirmation_rows_widgets()
        if len(rows) < required_count:
            for index in range(len(rows), required_count):
                container.mount(
                    ConfirmationOption(
                        self,
                        index,
                        self._confirmation_options[index],
                    )
                )
            self.call_after_refresh(self._sync_confirmation_rows)
            return

        if not self._confirmation_required or not required_count:
            for row in rows:
                row.display = False
            container.display = False
            panel = self.query_one("#confirmation-panel")
            panel.display = False
            panel.remove_class("active")
            self._resize_composer_to_text()
            return

        panel = self.query_one("#confirmation-panel")
        panel.display = True
        panel.add_class("active")
        self._update_confirmation_question()

        for index, row in enumerate(rows):
            if index < required_count:
                row.set_option(
                    index,
                    self._confirmation_options[index],
                    self._confirmation_has_selection
                    and index == self._confirmation_selection,
                )
                row.display = True
            else:
                row.display = False
        container.display = True
        self._resize_composer_to_text()
        self.call_after_refresh(self._focus_confirmation_selection)

    def _show_current_confirmation_rows(self) -> None:
        """更新当前问题的行，优先复用已有控件，避免切题空白。"""
        self._sync_confirmation_rows()

    def _focus_confirmation_selection(self) -> None:
        rows = self._confirmation_rows_widgets()
        if rows:
            self._confirmation_selection = min(self._confirmation_selection, len(rows) - 1)
            rows[self._confirmation_selection].focus()

    def _select_confirmation_option(self, index: int) -> None:
        rows = self._confirmation_rows_widgets()
        if not rows or not 0 <= index < len(rows):
            return
        self._confirmation_selection = index
        self._confirmation_selections[self._confirmation_group_index] = index
        self._confirmation_has_selection = True
        # 已确认过的题目再次导航时，新的选项即为对该答案的修改；尚未
        # 确认的题目只保存导航位置，仍由回车触发正式确认。
        if self._confirmation_answers[self._confirmation_group_index] is not None:
            self._confirmation_answers[self._confirmation_group_index] = self._confirmation_options[index]
        for row_index, row in enumerate(rows):
            row.set_selected(row_index == index)
        rows[index].focus()

    def _move_confirmation_selection(self, offset: int) -> None:
        if not self._confirmation_required or self._confirmation_custom_mode:
            return
        rows = self._confirmation_rows_widgets()
        if not rows:
            return
        current = min(max(self._confirmation_selection + offset, 0), len(rows) - 1)
        self._select_confirmation_option(current)

    def _save_confirmation_group_state(self) -> None:
        """保存当前题目的导航草稿，供左右切题后恢复。"""
        group_index = self._confirmation_group_index
        if self._confirmation_custom_mode:
            composer = self.query_one("#composer", TextArea)
            self._confirmation_custom_answers[group_index] = composer.text
        else:
            self._confirmation_selections[group_index] = (
                self._confirmation_selection
                if self._confirmation_has_selection
                else None
            )

    def _load_confirmation_group(self, group_index: int) -> None:
        """切换到问题并恢复其选项选择或自定义回答草稿。"""
        self._confirmation_group_index = group_index
        self._confirmation_options = self._confirmation_groups[group_index]
        saved_custom = self._confirmation_custom_answers[group_index]
        saved_answer = self._confirmation_answers[group_index]
        saved_selection = self._confirmation_selections[group_index]
        # 已确认的自定义答案和左右切题时保存的自定义草稿都进入编辑模式。
        self._confirmation_custom_mode = saved_custom is not None
        self._confirmation_selection = saved_selection if saved_selection is not None else 0
        self._confirmation_has_selection = saved_selection is not None
        composer = self.query_one("#composer", TextArea)
        if self._confirmation_custom_mode:
            composer.read_only = False
            composer.text = saved_custom or ""
            panel = self.query_one("#confirmation-panel")
            panel.display = True
            panel.add_class("active")
            self.query_one("#confirmation-options", Vertical).display = False
            self._update_confirmation_question()
            self._resize_composer_to_text()
            composer.focus()
            return
        else:
            composer.read_only = True
            composer.clear()
            # 如果答案来自旧状态但尚未记录选项位置，按当前选项文字恢复选择。
            if saved_selection is None and saved_answer in self._confirmation_options:
                self._confirmation_selection = self._confirmation_options.index(saved_answer)
                self._confirmation_selections[group_index] = self._confirmation_selection
                self._confirmation_has_selection = True
            self._show_current_confirmation_rows()
        self._resize_composer_to_text()

    def _move_confirmation_group(self, offset: int) -> None:
        """按左右键切换问题；不提交答案，只保留当前题编辑状态。"""
        if not self._confirmation_required or not self._confirmation_groups:
            return
        self._save_confirmation_group_state()
        next_index = min(
            max(self._confirmation_group_index + offset, 0),
            len(self._confirmation_groups) - 1,
        )
        if next_index == self._confirmation_group_index:
            self._load_confirmation_group(next_index)
            return
        self._load_confirmation_group(next_index)

    def _confirm_confirmation_selection(self) -> None:
        if not self._confirmation_required or self._confirmation_custom_mode:
            return
        options = self._confirmation_options
        if not options:
            return
        selected = options[self._confirmation_selection]
        if selected == _CONFIRMATION_CUSTOM_OPTION:
            self._confirmation_custom_mode = True
            # 只有进入自定义回答模式才重新开放输入框；选项阶段始终只读。
            composer = self.query_one("#composer", TextArea)
            composer.read_only = False
            composer.text = self._confirmation_custom_answers[
                self._confirmation_group_index
            ] or ""
            panel = self.query_one("#confirmation-panel")
            panel.display = True
            panel.remove_class("active")
            self.query_one("#confirmation-options", Vertical).display = False
            self._update_confirmation_question()
            self._resize_composer_to_text()
            composer.focus()
            return
        self._accept_confirmation_answer(selected)

    def _accept_confirmation_answer(self, answer: str) -> None:
        group_index = self._confirmation_group_index
        self._confirmation_answers[group_index] = answer
        if self._confirmation_custom_mode:
            self._confirmation_custom_answers[group_index] = answer
        else:
            self._confirmation_custom_answers[group_index] = None
        next_index = group_index + 1
        if next_index < len(self._confirmation_groups):
            # 自定义回答提交后进入下一题，输入框再次锁定，只允许选择该题选项。
            self._load_confirmation_group(next_index)
            return
        if any(answer is None for answer in self._confirmation_answers):
            # 左右键允许跳题；若跳过了尚未回答的问题，完成当前题后回到
            # 第一处空缺，而不是把不完整的答案发送给模型。
            first_unanswered = self._confirmation_answers.index(None)
            self._load_confirmation_group(first_unanswered)
            return
        answer_text = "\n".join(
            f"问题：{self._confirmation_questions[index] or f'第 {index + 1} 个问题'}\n"
            f"答案：{value}"
            for index, value in enumerate(self._confirmation_answers)
        )
        self._set_confirmation_required(None)
        self.query_one("#composer", TextArea).focus()
        self._submit_text_direct(answer_text)

    def _submit_text_direct(self, text: str) -> None:
        """提交已完成的逐题答案，绕过选项状态再次收集。"""

        if not text.strip():
            return
        composer = self.query_one("#composer", TextArea)
        composer.clear()
        self._compact_pastes.clear()
        self._submit(text.strip())

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
        if self._confirmation_required:
            # 在选项列表中直接输入文字，等同于当前问题的自定义回答；
            # 选中“我有自己的想法”后则明确进入同一条路径。
            composer.clear()
            self._compact_pastes.clear()
            self._accept_confirmation_answer(text)
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
