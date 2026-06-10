from __future__ import annotations

from pathlib import Path

from ai_voice_agent.agent import AgentConfig, AgentError, LocalToolAgent
from ai_voice_agent.approval import approval_mode_label, load_approval_mode
from ai_voice_agent.audio_setup import (
    VoiceConfigError,
    create_speech_to_text,
    create_text_to_speech,
    load_voice_config,
)
from ai_voice_agent.chat_session import run_inline_chat
from ai_voice_agent.llm import LLMError, load_llm_config
from ai_voice_agent.runtime_config import RuntimeConfigError
from ai_voice_agent.terminal_ui import TerminalUI
from ai_voice_agent.windows_launcher import configure_console_encoding, launch_in_powershell_window


def main() -> None:
    """命令行语音 AI Agent 入口。"""

    configure_console_encoding()
    try:
        config = load_llm_config()
        voice_config = load_voice_config()
        approval_mode = load_approval_mode()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    except VoiceConfigError as exc:
        print(f"配置加载失败：{exc}")
        return
    except RuntimeConfigError as exc:
        print(f"配置加载失败：{exc}")
        return
    ui = TerminalUI(model_label=config.model)

    enabled_label = "已启用" if config.thinking_enabled else "已禁用"
    reasoning_info = f"，推理强度：{config.reasoning_effort}" if config.reasoning_effort else ""
    stt_label = "开启" if voice_config.speech_to_text_enabled else "关闭"
    tts_label = "开启" if voice_config.text_to_speech_enabled else "关闭"
    ui.print_startup_panel(
        "AI 语音 Agent",
        [
            f"thinking: {enabled_label}{reasoning_info}",
            f"approval: {approval_mode_label(approval_mode)}",
            f"voice: 语音转文字 {stt_label}，文字转语音 {tts_label}",
        ],
    )

    transient_output_marked = ui.mark_transient_output_start()
    speech_to_text = None
    text_to_speech = None
    speech_to_text_ready = not voice_config.speech_to_text_enabled
    text_to_speech_ready = not voice_config.text_to_speech_enabled

    if voice_config.speech_to_text_enabled:
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
                workspace_root=Path(__file__).resolve().parent,
                approval_mode=approval_mode,
            )
        )
    except AgentError as exc:
        print(f"Agent 初始化失败：{exc}")
        return

    # 显示 Skill 加载情况
    if agent.skill_manager is not None and agent.skill_manager.count > 0:
        print(ui.muted(f"已加载 {agent.skill_manager.count} 个 Skill，输入 /skills 查看列表。"))

    try:
        run_inline_chat(agent, speech_to_text, text_to_speech, ui)
    except KeyboardInterrupt:
        print("\n对话结束。")
    finally:
        if agent is not None:
            agent.close()
        if text_to_speech is not None:
            text_to_speech.stop()


if __name__ == "__main__":
    if not launch_in_powershell_window(Path(__file__)):
        main()
