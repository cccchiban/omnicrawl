from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

from .runtime_config import RuntimeConfigError, get_section, load_config_data
from .speech_to_text import MicrophoneInfo, SpeechConfig, SpeechToText, SpeechToTextError
from .text_to_speech import TextToSpeech, TextToSpeechError


class VoiceConfigError(RuntimeError):
    """语音功能配置读取或校验失败时抛出。"""


@dataclass(frozen=True)
class VoiceConfig:
    """语音功能开关配置。

    两个开关分别控制输入侧的语音转文字和输出侧的文字转语音。默认都保持开启，
    这样老配置文件没有 voice 段时仍沿用原有语音体验。
    """

    speech_to_text_enabled: bool = True
    text_to_speech_enabled: bool = True


def _read_bool_config(section: dict[str, Any], key: str, default: bool) -> bool:
    """读取布尔配置，避免把字符串 "false" 误当作开启。"""

    value = section.get(key, default)
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    raise VoiceConfigError(f"配置项 voice.{key} 必须是布尔值 true 或 false。")


def load_voice_config() -> VoiceConfig:
    """从 config.json 的 voice 段读取语音功能开关。"""

    try:
        data = load_config_data()
        voice_section = get_section(data, "voice")
    except RuntimeConfigError as exc:
        raise VoiceConfigError(str(exc)) from exc

    return VoiceConfig(
        speech_to_text_enabled=_read_bool_config(
            voice_section, "speech_to_text_enabled", True
        ),
        text_to_speech_enabled=_read_bool_config(
            voice_section, "text_to_speech_enabled", True
        ),
    )


def _read_mic_device_index_from_env() -> int | None:
    """读取环境变量中的底层麦克风编号，适合高级排查时固定使用同一设备。"""

    raw_index = os.getenv("MIC_DEVICE_INDEX", "").strip()
    if not raw_index:
        return None

    try:
        return int(raw_index)
    except ValueError as exc:
        raise SpeechToTextError("MIC_DEVICE_INDEX 必须是底层麦克风设备编号，例如 22。") from exc


def _print_microphones(microphones: list[MicrophoneInfo]) -> None:
    """展示可录音输入设备，帮助用户避开虚拟设备和错误默认设备。"""

    print("\n检测到多个录音输入设备：")
    for display_index, microphone in enumerate(microphones, start=1):
        print(f"  {display_index}. {microphone.name}")


def _choose_microphone_index() -> int | None:
    """首次运行时让用户选择麦克风；直接回车则沿用系统默认设备。"""

    env_index = _read_mic_device_index_from_env()
    if env_index is not None:
        return env_index

    if os.getenv("MIC_DEVICE_KEYWORD", "").strip():
        return None

    microphones = SpeechToText.list_microphones()
    if len(microphones) <= 1:
        return None

    _print_microphones(microphones)
    choice_to_device_index = {
        display_index: microphone.index
        for display_index, microphone in enumerate(microphones, start=1)
    }

    while True:
        choice = input("请输入要使用的麦克风序号；直接回车使用系统默认麦克风：").strip()
        if not choice:
            return None

        try:
            selected_choice = int(choice)
        except ValueError:
            print("请输入列表中的数字序号。")
            continue

        if selected_choice in choice_to_device_index:
            return choice_to_device_index[selected_choice]

        print("该序号不是可录音输入设备，请重新选择。")


def create_speech_to_text(voice_config: VoiceConfig | None = None) -> SpeechToText | None:
    """初始化语音识别；失败时返回 None，让主流程继续支持键盘输入。"""

    voice_config = voice_config or load_voice_config()
    if not voice_config.speech_to_text_enabled:
        print("语音转文字已在 config.json 中关闭，本次会话将使用键盘输入。")
        return None

    try:
        config = SpeechConfig(
            device_index=_choose_microphone_index(),
            device_name_keyword=os.getenv("MIC_DEVICE_KEYWORD") or None,
        )
        speech_to_text = SpeechToText(config)
        print(f"录音设备：{speech_to_text.selected_microphone_label}")
        return speech_to_text
    except SpeechToTextError as exc:
        print(f"语音识别初始化失败：{exc}")
        print("本次会话将改用键盘输入。")
        return None


def create_text_to_speech(voice_config: VoiceConfig | None = None) -> TextToSpeech | None:
    """初始化语音播报；失败时返回 None，不影响命令行文字对话。"""

    voice_config = voice_config or load_voice_config()
    if not voice_config.text_to_speech_enabled:
        print("文字转语音已在 config.json 中关闭，AI 回复仅显示在命令行。")
        return None

    try:
        text_to_speech = TextToSpeech()
        print(f"语音后端：{text_to_speech.backend_name}")
        return text_to_speech
    except TextToSpeechError as exc:
        print(f"语音播报初始化失败：{exc}")
        print("AI 回复仍会显示在命令行。")
        return None
