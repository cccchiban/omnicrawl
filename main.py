from __future__ import annotations

import argparse
import threading
from pathlib import Path

from ai_voice_agent.agent import AgentConfig, AgentError, LocalToolAgent
from ai_voice_agent.approval import approval_mode_label, load_approval_mode
from ai_voice_agent.audio_setup import (
    VoiceConfig,
    VoiceConfigError,
    create_speech_to_text,
    create_text_to_speech,
    load_voice_config,
)
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

    parser = argparse.ArgumentParser(description="AI 语音 Agent")
    parser.add_argument(
        "--resume",
        metavar="SESSION_ID",
        default="",
        help="启动时恢复指定会话 ID",
    )
    return parser.parse_args(argv)


def _speech_to_text_status_label(frontend_type: str, voice_config: VoiceConfig) -> str:
    """返回启动面板中的语音输入状态。

    Qt 前端目前只有键盘输入框，还没有麦克风选择和录音交互。如果沿用 TUI 的
    控制台初始化流程，遇到多麦克风设备时会在隐藏控制台里等待 input()，导致
    GUI 启动看起来卡死，所以这里明确展示为未接入并跳过初始化。
    """

    if frontend_type == "qt" and voice_config.speech_to_text_enabled:
        return "未接入 Qt GUI"
    return "开启" if voice_config.speech_to_text_enabled else "关闭"


def _should_initialize_speech_to_text(frontend_type: str, voice_config: VoiceConfig) -> bool:
    """判断当前前端是否需要初始化 SpeechToText。"""

    return frontend_type != "qt" and voice_config.speech_to_text_enabled


def main(argv: list[str] | None = None) -> None:
    """命令行语音 AI Agent 入口。"""

    configure_console_encoding()
    args = _parse_args(argv)
    app_root = Path(__file__).resolve().parent
    try:
        config = load_llm_config()
        voice_config = load_voice_config()
        approval_mode = load_approval_mode()
        temp_workspace_config = load_agent_temp_workspace_config()
        project_context = detect_project_context(app_root=app_root)
        frontend_config = load_frontend_config()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    except VoiceConfigError as exc:
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
    stt_label = _speech_to_text_status_label(frontend_config.type, voice_config)
    tts_label = "开启" if voice_config.text_to_speech_enabled else "关闭"
    frontend_label = "Qt GUI" if frontend_config.type == "qt" else "TUI"
    ui.print_startup_panel(
        "AI 语音 Agent",
        [
            f"frontend: {frontend_label}",
            f"thinking: {enabled_label}{reasoning_info}",
            f"approval: {approval_mode_label(approval_mode)}",
            f"workspace: {project_context_status_label(project_context)}",
            f"voice: 语音转文字 {stt_label}，文字转语音 {tts_label}",
            f"temp: {agent_temp_status_label(temp_workspace_config)}",
        ],
    )

    transient_output_marked = ui.mark_transient_output_start()
    speech_to_text = None
    text_to_speech = None
    should_initialize_speech_to_text = _should_initialize_speech_to_text(
        frontend_config.type,
        voice_config,
    )
    speech_to_text_ready = not should_initialize_speech_to_text
    text_to_speech_ready = not voice_config.text_to_speech_enabled

    if should_initialize_speech_to_text:
        speech_to_text = create_speech_to_text(voice_config)
        speech_to_text_ready = speech_to_text is not None
    if voice_config.text_to_speech_enabled:
        text_to_speech = create_text_to_speech(voice_config)
        text_to_speech_ready = text_to_speech is not None

    if transient_output_marked and speech_to_text_ready and text_to_speech_ready:
        ui.clear_transient_output()

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
                        text_to_speech,
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
            run_inline_chat(agent, speech_to_text, text_to_speech, ui)
    except KeyboardInterrupt:
        print("\n对话结束。")
    finally:
        if is_qt_frontend:
            if qt_stop_event is not None:
                qt_stop_event.set()
            ui.stop()
            if text_to_speech is not None:
                text_to_speech.interrupt(wait_timeout_seconds=0)
            if chat_thread is not None and chat_thread.is_alive():
                chat_thread.join(timeout=2.0)
        if agent is not None:
            agent.close()
        if text_to_speech is not None:
            text_to_speech.stop()


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
