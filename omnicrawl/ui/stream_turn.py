"""终端单轮流式输出的事件编排。"""

from __future__ import annotations

from typing import Any

from .terminal import InputBar, MarkdownStreamState, TerminalUI, WaitingIndicator


class StreamTurnController:
    """串行化一轮 Agent 事件，确保动态尾部区始终先清理再写入历史。

    spinner 和预输入栏属于可覆盖的动态尾部区；模型文本、状态和工具记录属于
    不可覆盖的终端历史。每次要写历史前都调用 ``_settle_dynamic_area``，避免
    多个回调分别移动光标而互相覆盖。
    """

    def __init__(
        self,
        ui: TerminalUI,
        *,
        input_bar: InputBar,
        waiting_indicator: WaitingIndicator,
    ) -> None:
        self._ui = ui
        self._input_bar = input_bar
        self._waiting_indicator = waiting_indicator
        self._markdown_state = MarkdownStreamState()
        self._has_output = False
        self._waiting_active = False
        self._queued_input: str | None = None

    def start(self) -> None:
        """启动首个等待状态。"""

        self._start_waiting()

    def handle_delta(self, delta: str) -> None:
        """接收模型文本分片，并将完整行按追加方式提交。"""

        if not delta:
            return
        self._settle_dynamic_area()
        if not self._has_output:
            self._ui.newline()
            self._ui.print_ai_prefix()
            self._has_output = True
        self._ui.write_markdown_delta(delta, self._markdown_state)

    def handle_status(self, message: str) -> None:
        """展示持久状态；空状态表示继续等待下一次模型事件。"""

        if not message:
            self._start_waiting()
            return
        self._settle_dynamic_area()
        self._flush_output()
        self._ui.status(message, leading_blank=self._has_output)

    def handle_retry_status(self, message: str) -> None:
        """展示可恢复请求错误，并保持后续流式渲染状态。"""

        self._settle_dynamic_area()
        self._flush_output()
        self._ui.status(message, leading_blank=self._has_output, italic=True)

    def handle_tool_start(self, step: int, tool_call: Any) -> None:
        """提交已有回复，再追加工具开始记录。"""

        had_output = self._has_output
        self._settle_dynamic_area()
        self._flush_output()
        self._ui.print_tool_call_start(
            step,
            tool_call.name,
            tool_call.arguments,
            leading_blank=had_output,
        )
        self._reset_output_section()

    def handle_tool_result(self, tool_call: Any, result: Any) -> None:
        """追加工具完成记录。"""

        self._settle_dynamic_area()
        self._flush_output()
        self._ui.print_tool_result_record(
            result.ok,
            result.output,
            tool_name=tool_call.name,
        )
        self._reset_output_section()

    def handle_protocol_wait(self) -> None:
        """模型转入隐藏工具协议阶段时恢复等待区。"""

        self._flush_output()
        self._start_waiting()

    def finish(self) -> str | None:
        """结束本轮，清理动态区并返回用户已确认提交的预输入。"""

        self._settle_dynamic_area()
        self._flush_output()
        if self._has_output:
            self._ui.newline()
        self._input_bar.clear()
        return self._queued_input

    @property
    def draft_input(self) -> str:
        """返回未按 Enter 的草稿，交由下一次行内编辑器继续显示。"""

        if self._queued_input is not None:
            return ""
        return str(getattr(self._input_bar, "pre_input", ""))

    def cancel(self) -> str | None:
        """取消本轮时清理动态尾部区并保留已确认的排队输入。"""

        self._settle_dynamic_area()
        self._flush_output()
        self._input_bar.clear()
        return self._queued_input

    def _start_waiting(self) -> None:
        if self._waiting_active:
            return
        self._waiting_indicator.start()
        self._waiting_active = True

    def _settle_dynamic_area(self) -> None:
        """停止 spinner 并收集一次性提交的预输入，再清除输入栏。"""

        if self._waiting_active:
            submitted = self._waiting_indicator.stop()
            self._waiting_active = False
            if submitted and self._queued_input is None:
                self._queued_input = submitted
        self._input_bar.push_up()

    def _flush_output(self) -> None:
        self._ui.flush_markdown(self._markdown_state)

    def _reset_output_section(self) -> None:
        """工具记录结束一段模型输出；下一段必须重新创建 AI 前缀。"""

        self._markdown_state = MarkdownStreamState()
        self._has_output = False
