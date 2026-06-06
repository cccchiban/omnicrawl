from __future__ import annotations

from pathlib import Path

from ai_voice_agent.agent import AgentConfig, AgentError, LocalToolAgent
from ai_voice_agent.audio_setup import create_speech_to_text, create_text_to_speech
from ai_voice_agent.chat_session import run_inline_chat
from ai_voice_agent.llm import LLMError, load_llm_config
from ai_voice_agent.runtime_config import resolve_config_path
from ai_voice_agent.terminal_ui import TerminalUI
from ai_voice_agent.windows_launcher import configure_console_encoding, launch_in_powershell_window


def main() -> None:
    """命令行语音 AI Agent 入口。"""

    configure_console_encoding()
    try:
        config = load_llm_config()
    except LLMError as exc:
        print(f"配置加载失败：{exc}")
        return
    ui = TerminalUI(model_label=config.model)

    config_path = resolve_config_path()

    enabled_label = "已启用" if config.thinking_enabled else "已禁用"
    reasoning_info = f"，推理强度：{config.reasoning_effort}" if config.reasoning_effort else ""
    config_label = str(config_path) if config_path.exists() else f"未找到 {config_path.name}，回退到环境变量"
    ui.print_startup_panel(
        "AI 语音 Agent",
        [
            f"thinking: {enabled_label}{reasoning_info}",
            f"config: {config_label}",
            "输入栏 Ctrl+C 两次或关闭窗口结束会话",
        ],
    )

    transient_output_marked = ui.mark_transient_output_start()
    speech_to_text = create_speech_to_text()
    text_to_speech = create_text_to_speech()
    if transient_output_marked and speech_to_text is not None and text_to_speech is not None:
        ui.clear_transient_output()

    try:
        agent = LocalToolAgent(AgentConfig(llm=config, workspace_root=Path(__file__).resolve().parent))
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
        if text_to_speech is not None:
            text_to_speech.stop()


if __name__ == "__main__":
    if not launch_in_powershell_window(Path(__file__)):
        main()
