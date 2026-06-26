from __future__ import annotations

import os

from .agent import AgentError, LocalToolAgent
from .inline_input import read_line_autocomplete
from .slash_commands import (
    build_slash_commands,
    format_tool_confirmation,
    handle_model_command,
    handle_approval_command,
    handle_reasoning_command,
    handle_session_command,
    print_memory_clean_result,
    print_mcp_status,
    print_skills_list,
)
from .terminal_ui import USER_PREFIX, InputBar, StatusLine, TerminalUI, WaitingIndicator


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


def _get_user_text(
    ui: TerminalUI,
    slash_commands: list[str] | None = None,
    history: list[str] | None = None,
) -> str:
    """读取一轮用户输入，支持斜杠命令 Tab 补全（Windows 下）。"""

    if os.name == "nt" and slash_commands:
        try:
            import msvcrt
        except ImportError:
            pass
        else:
            return read_line_autocomplete(
                ui.prompt(),
                slash_commands,
                ui,
                history=history,
            ).strip()
    return input(ui.prompt()).strip()


def run_inline_chat(
    agent: LocalToolAgent,
    ui: TerminalUI,
) -> None:
    """运行默认的普通终端内联 UI。"""

    agent.set_confirm_handler(
        lambda tool_name, arguments: ui.prompt_yes_no(
            format_tool_confirmation(tool_name, arguments),
            confirmed_label="",
        )
    )
    pending_user_text: str | None = None
    input_interrupt_count = 0
    try:
        input_history = agent.prompt_history_texts(limit=100)
    except AgentError:
        input_history = []
    while True:
        try:
            if pending_user_text is not None:
                user_text = pending_user_text
                pending_user_text = None
            else:
                user_text = _get_user_text(
                    ui,
                    build_slash_commands(agent),
                    history=input_history,
                )
        except KeyboardInterrupt:
            input_interrupt_count += 1
            if input_interrupt_count >= 2:
                print("\n对话结束。")
                break
            ui.notice("\n已取消输入，再按一次 Ctrl+C 退出。")
            continue

        input_interrupt_count = 0
        if not user_text:
            continue

        if user_text.strip().lower() in EXIT_WORDS:
            print("对话结束。")
            break

        if user_text.strip() == NEW_CHAT_COMMAND:
            agent.reset_conversation()
            ui.notice("已开启新对话。")
            continue

        if user_text.strip() == "/skills":
            print_skills_list(agent)
            continue

        if user_text.strip() == "/memory:clean":
            print_memory_clean_result(agent)
            continue

        if user_text.strip() == "/workspace" or user_text.strip().startswith("/workspace "):
            parts = user_text.strip().split(None, 1)
            if len(parts) == 1 or not parts[1].strip():
                print(f"当前工作区：{agent.workspace_root}")
                print("用法：/workspace <新工作区路径>")
                continue
            try:
                new_root = agent.switch_workspace(parts[1].strip())
                print(f"已切换到工作区：{new_root}")
            except AgentError as exc:
                print(f"工作区切换失败：{exc}")
            continue
        if user_text.strip() == "/mcp":
            print_mcp_status(agent)
            continue

        session_message = handle_session_command(agent, user_text)
        if session_message is not None:
            print(session_message)
            continue

        before_model = agent.current_model
        model_message = handle_model_command(agent, user_text)
        if model_message is not None:
            if agent.current_model != before_model:
                ui.set_model_label(agent.current_model)
            ui.notice(model_message)
            continue

        approval_message = handle_approval_command(agent, user_text)
        if approval_message is not None:
            ui.notice(approval_message)
            continue

        reasoning_message = handle_reasoning_command(agent, user_text)
        if reasoning_message is not None:
            ui.notice(reasoning_message)
            continue

        ui.inline_turn_base(user_text)
        status_line = StatusLine(ui)
        input_bar = InputBar(ui)
        waiting_indicator = WaitingIndicator(status_line, input_bar=input_bar)

        # 流式输出状态：管理 markdown 增量渲染和首次输出标记
        _has_display_output = False
        from .terminal_ui import MarkdownStreamState
        _markdown_state = MarkdownStreamState()

        def _collect_pre_input() -> None:
            """停止 spinner 并收集预输入到 pending_user_text。"""
            nonlocal pending_user_text
            pre = waiting_indicator.stop()
            if pre and pending_user_text is None:
                pending_user_text = pre

        tool_display_state = None

        def handle_delta(delta: str) -> None:
            """流式增量文本渲染回调，直接写入终端 Markdown。"""
            nonlocal _has_display_output
            if not _has_display_output:
                _collect_pre_input()
                status_line.clear()
                input_bar.push_up()
                ui.newline()
                ui.print_ai_prefix()
                _has_display_output = True
            input_bar.push_up()
            ui.write_markdown_delta(delta, _markdown_state)
            input_bar.pop_down()

        def _flush_display() -> None:
            """提交当前 Markdown 预览到终端，不触发额外操作。"""
            input_bar.push_up()
            ui.flush_markdown(_markdown_state)
            input_bar.pop_down()

        def handle_agent_status(message: str) -> None:
            if message:
                _flush_display()
                _collect_pre_input()
                input_bar.push_up()
                ui.status(message, leading_blank=_has_display_output)
                input_bar.pop_down()
            else:
                if _has_display_output:
                    _flush_display()
                    _markdown_state = MarkdownStreamState()
                    _has_display_output = False
                waiting_indicator.start()

        def handle_retry_status(message: str) -> None:
            """流式连接可恢复中断时，用弱提示说明自动重试。"""
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.status(message, leading_blank=_has_display_output, italic=True)
            input_bar.pop_down()

        def handle_tool_start(step: int, tool_call) -> None:
            """工具开始执行时立即展示调用详情和运行态标记。"""
            nonlocal tool_display_state
            had_output = _has_display_output
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            tool_display_state = ui.print_tool_call_start(
                step,
                tool_call.name,
                tool_call.arguments,
                leading_blank=had_output,
            )
            input_bar.pop_down()

        def handle_tool_result(_tool_call, result) -> None:
            """工具执行完成时先收起等待动画，再输出执行摘要。"""
            nonlocal tool_display_state
            _flush_display()
            _collect_pre_input()
            input_bar.push_up()
            ui.print_tool_result_record(
                result.ok,
                result.output,
                tool_name=_tool_call.name,
                display_state=tool_display_state,
            )
            input_bar.pop_down()
            tool_display_state = None

        def handle_protocol_wait() -> None:
            """模型已显示进度、正在继续输出隐藏工具协议时恢复等待动画。"""
            _flush_display()
            waiting_indicator.start()

        try:
            waiting_indicator.start()
            agent.run_stream(
                user_text,
                handle_delta,
                on_status=handle_agent_status,
                on_tool_start=handle_tool_start,
                on_tool_result=handle_tool_result,
                on_token_usage=ui.update_token_usage,
                on_protocol_wait=handle_protocol_wait,
                on_retry_status=handle_retry_status,
            )
            _collect_pre_input()
            input_bar.push_up()
            _flush_display()
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
        except KeyboardInterrupt:
            _collect_pre_input()
            input_bar.push_up()
            ui.newline()
            input_bar.pop_down()
            input_bar.clear()
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            _collect_pre_input()
            input_bar.push_up()
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            print(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            input_bar.pop_down()
            input_bar.clear()
            continue
