from __future__ import annotations

import argparse
import threading
from pathlib import Path

from ai_voice_agent.agent import AgentConfig, AgentError, LocalToolAgent
from ai_voice_agent.approval import approval_mode_label, load_approval_mode
from ai_voice_agent.chat_session import run_inline_chat
from ai_voice_agent.llm import LLMError, load_llm_config
from ai_voice_agent.project_context import (
    ProjectContextError,
    detect_project_context,
    project_context_status_label,
)
from ai_voice_agent.runtime_config import RuntimeConfigError
from ai_voice_agent.temp_workspace import (
    AgentTempWorkspaceError,
    agent_temp_status_label,
    load_agent_temp_workspace_config,
)
from ai_voice_agent.frontend_config import load_frontend_config
from ai_voice_agent.ui import UIStartupError, create_ui
from ai_voice_agent.windows_launcher import configure_console_encoding, launch_in_powershell_window


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """解析启动参数；当前只暴露会话恢复入口。"""

    parser = argparse.ArgumentParser(description="AI Agent")
    parser.add_argument(
        "--resume",
        metavar="SESSION_ID",
        default="",
        help="启动时恢复指定会话 ID",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> None:
    """命令行 AI Agent 入口。"""

    configure_console_encoding()
    args = _parse_args(argv)
    app_root = Path(__file__).resolve().parent
    try:
        config = load_llm_config()
        approval_mode = load_approval_mode()
        temp_workspace_config = load_agent_temp_workspace_config()
        project_context = detect_project_context(app_root=app_root)
        frontend_config = load_frontend_config()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    except RuntimeConfigError as exc:
        print(f"配置加载失败：{exc}")
        return
    except AgentTempWorkspaceError as exc:
        print(f"配置加载失败：{exc}")
        return
    except ProjectContextError as exc:
        print(f"项目路径检测失败：{exc}")
        return
    try:
        ui = create_ui(
            model_label=config.model,
            frontend_type=frontend_config.type,
        )
        is_qt_frontend = frontend_config.type == "qt"
        if is_qt_frontend:
            ui.start()
            ui.set_model_label(config.model)
    except UIStartupError as exc:
        print(f"界面启动失败：{exc}")
        return

    enabled_label = "已启用" if config.thinking_enabled else "已禁用"
    reasoning_info = f"，推理强度：{config.reasoning_effort}" if config.reasoning_effort else ""
    frontend_label = "Qt GUI" if frontend_config.type == "qt" else "TUI"
    ui.print_startup_panel(
        "AI Agent",
        [
            f"frontend: {frontend_label}",
            f"thinking: {enabled_label}{reasoning_info}",
            f"approval: {approval_mode_label(approval_mode)}",
            f"workspace: {project_context_status_label(project_context)}",
            f"temp: {agent_temp_status_label(temp_workspace_config)}",
        ],
    )

    if is_qt_frontend:
        # Qt 首屏不依赖 Agent、MCP 初始化；这些工作放到后台线程，
        # 让 QWebEngine 尽快进入事件循环并渲染可见窗口。后台初始化完成后
        # 再启动完整对话循环，功能路径保持和原来一致。
        ui.status("正在初始化")
        ui.set_input_enabled(False)
        ui.set_input_placeholder("正在初始化 Agent...")

        agent: LocalToolAgent | None = None
        qt_stop_event = threading.Event()
        qt_cancel_event = ui.get_cancel_event()

        def _qt_chat_thread() -> None:
            nonlocal agent
            try:
                agent = LocalToolAgent(
                    AgentConfig(
                        llm=config,
                        workspace_root=project_context.workspace_root,
                        workspace_detection_summary=project_context.detection_summary,
                        approval_mode=approval_mode,
                        temp_workspace=temp_workspace_config,
                        resume_session_id=args.resume,
                    )
                )
                if agent.skill_manager is not None and agent.skill_manager.count > 0:
                    ui.notice(
                        f"已加载 {agent.skill_manager.count} 个 Skill，输入 /skills 查看列表。"
                    )

                # 在后台初始化阶段提前完成 MCP 能力发现，避免首次对话时出现
                # "正在加载 MCP 能力" 的加载框打断用户体验。_ensure_mcp_tools_ready
                # 内部会检查 manager.discovered 标记，重复调用时为无操作。
                if agent._mcp_manager is not None and agent._mcp_manager.enabled:
                    ui.status("正在加载 MCP 能力")
                    agent._ensure_mcp_tools_ready()
                    ui.status("正在初始化")

                from ai_voice_agent.qt_chat_session import run_qt_chat

                run_qt_chat(
                    agent,
                    ui,
                    stop_event=qt_stop_event,
                    cancel_event=qt_cancel_event,
                )
            except AgentError as exc:
                ui.status("")
                ui.set_input_enabled(False)
                ui.set_input_placeholder("Agent 初始化失败")
                ui.notice(f"Agent 初始化失败：{exc}")
            finally:
                # 对话结束或初始化失败后关闭窗口，让 exec_and_wait 返回。
                ui.stop()

        chat_thread = threading.Thread(target=_qt_chat_thread, daemon=True)
        chat_thread.start()
        try:
            ui.exec_and_wait()
        except KeyboardInterrupt:
            print("\n对话结束。")
        finally:
            qt_stop_event.set()
            ui.stop()
            if chat_thread.is_alive():
                chat_thread.join(timeout=2.0)
            if agent is not None:
                agent.close()
        return

    agent: LocalToolAgent | None = None
    try:
        agent = LocalToolAgent(
            AgentConfig(
                llm=config,
                workspace_root=project_context.workspace_root,
                workspace_detection_summary=project_context.detection_summary,
                approval_mode=approval_mode,
                temp_workspace=temp_workspace_config,
                resume_session_id=args.resume,
            )
        )
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}")
        return

    # 显示 Skill 加载情况
    if agent.skill_manager is not None and agent.skill_manager.count > 0:
        skill_status = f"已加载 {agent.skill_manager.count} 个 Skill，输入 /skills 查看列表。"
        if is_qt_frontend:
            ui.notice(skill_status)
        else:
            print(ui.muted(skill_status))

    chat_thread: threading.Thread | None = None
    qt_stop_event: threading.Event | None = None
    try:
        if is_qt_frontend:
            # Qt GUI：对话循环在后台线程，主线程留给 Qt 事件循环
            from ai_voice_agent.qt_chat_session import run_qt_chat

            qt_stop_event = threading.Event()
            qt_cancel_event = ui.get_cancel_event()

            def _chat_thread() -> None:
                try:
                    run_qt_chat(
                        agent,
                        ui,
                        stop_event=qt_stop_event,
                        cancel_event=qt_cancel_event,
                    )
                finally:
                    # 对话结束后关闭窗口，让 exec_and_wait 返回
                    ui.stop()

            chat_thread = threading.Thread(target=_chat_thread, daemon=True)
            chat_thread.start()
            ui.exec_and_wait()
        else:
            run_inline_chat(agent, ui)
    except KeyboardInterrupt:
        print("\n对话结束。")
    finally:
        if is_qt_frontend:
            if qt_stop_event is not None:
                qt_stop_event.set()
            ui.stop()
            if chat_thread is not None and chat_thread.is_alive():
                chat_thread.join(timeout=2.0)
        if agent is not None:
            agent.close()


if __name__ == "__main__":
    # Qt 模式：直接启动，不弹 PowerShell 窗口
    # TUI 模式：从 IDE 启动时弹出独立 PowerShell 窗口
    try:
        from ai_voice_agent.frontend_config import load_frontend_config
        _frontend = load_frontend_config()
    except Exception:
        _frontend = None

    if _frontend and _frontend.type == "qt":
        main()
    else:
        if not launch_in_powershell_window(Path(__file__)):
            main()
