
from __future__ import annotations

import os

from ..agent import AgentError, LocalToolAgent
from .inline_input import read_line_autocomplete
from ..commands.slash import (
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
from .stream_turn import StreamTurnController
from .terminal import InputBar, StatusLine, TerminalUI, WaitingIndicator


EXIT_WORDS = {"退出", "结束", "再见"}
NEW_CHAT_COMMAND = "/new"


def _get_user_text(
    ui: TerminalUI,
    slash_commands: list[str] | None = None,
    history: list[str] | None = None,
    initial_text: str = "",
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
                initial_text=initial_text,
            ).strip()
    prompt = ui.prompt()
    if initial_text:
        return input(f"{prompt}{initial_text}").strip()
    return input(prompt).strip()


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
    pending_draft_text = ""
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
                # 预输入在上一轮动态输入栏中已被显示过；下一轮开始时将其固化为
                # 普通用户消息，避免直接跳过输入编辑器而让终端历史缺失该问题。
                print(ui.prompt(), end="", flush=True)
                print(user_text, flush=True)
            else:
                user_text = _get_user_text(
                    ui,
                    build_slash_commands(agent),
                    history=input_history,
                    initial_text=pending_draft_text,
                )
                pending_draft_text = ""
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

        turn = StreamTurnController(
            ui,
            input_bar=input_bar,
            waiting_indicator=waiting_indicator,
        )

        try:
            turn.start()
            agent.run_stream(
                user_text,
                turn.handle_delta,
                on_status=turn.handle_status,
                on_tool_start=turn.handle_tool_start,
                on_tool_result=turn.handle_tool_result,
                on_token_usage=ui.update_token_usage,
                on_protocol_wait=turn.handle_protocol_wait,
                on_retry_status=turn.handle_retry_status,
            )
            queued_input = turn.finish()
            if queued_input and pending_user_text is None:
                pending_user_text = queued_input
            elif not queued_input:
                pending_draft_text = turn.draft_input
        except KeyboardInterrupt:
            queued_input = turn.cancel()
            if queued_input and pending_user_text is None:
                pending_user_text = queued_input
            elif not queued_input:
                pending_draft_text = turn.draft_input
            ui.notice("已取消当前操作。")
            continue
        except AgentError as exc:
            queued_input = turn.cancel()
            if queued_input and pending_user_text is None:
                pending_user_text = queued_input
            elif not queued_input:
                pending_draft_text = turn.draft_input
            message = str(exc).strip() or "Agent 请求失败，请检查配置或稍后重试。"
            print(message if message.startswith("Agent ") else f"Agent 请求失败：{message}")
            continue
