from __future__ import annotations

import os
import re
from dataclasses import dataclass
from typing import Callable


class SpeechToTextError(RuntimeError):
    """语音识别不可用或识别失败时抛出，主程序会据此降级为键盘输入。"""


@dataclass
class MicrophoneInfo:
    """PyAudio 输入设备信息，用于在命令行中让用户选择真实麦克风。"""

    index: int
    name: str
    input_channels: int
    host_api: str = ""


@dataclass
class SpeechConfig:
    """麦克风录音与识别配置。

    language 使用 Google Web Speech API 的语言代码；中文普通话通常写作 zh-CN。
    timeout 控制等待用户开始说话的最长秒数，phrase_time_limit 控制一次录音的最长秒数。
    """

    language: str = "zh-CN"
    timeout: float = 12.0
    phrase_time_limit: float = 20.0
    pause_threshold: float = 0.8
    adjust_noise_seconds: float = 1.5
    device_index: int | None = None
    device_name_keyword: str | None = None


class SpeechToText:
    """封装 speech_recognition，提供一次录音转文字能力。"""

    def __init__(self, config: SpeechConfig | None = None) -> None:
        self.config = config or SpeechConfig()
        try:
            import speech_recognition as sr
        except ImportError as exc:
            raise SpeechToTextError(
                "缺少 SpeechRecognition 依赖，请先执行：pip install -r requirements.txt"
            ) from exc

        self._sr = sr
        self._recognizer = sr.Recognizer()
        self._recognizer.pause_threshold = self.config.pause_threshold
        self._device_index = self._resolve_device_index()

    @staticmethod
    def list_microphones() -> list[MicrophoneInfo]:
        """列出可录音输入设备，过滤掉纯输出设备。

        speech_recognition 默认只按系统默认设备录音；Windows 上默认设备经常是虚拟摄像头、
        蓝牙免提或降噪虚拟设备，所以提供筛选后的设备列表，便于用户指定真正的麦克风。
        PortAudio 在 Windows 上会通过多个 Host API 暴露同一物理设备；默认优先展示
        WASAPI 输入端点，避免把 MME、DirectSound 和 WDM-KS 的重复/内部端点全列出来。
        """

        try:
            import speech_recognition as sr
        except ImportError as exc:
            raise SpeechToTextError(
                "缺少 SpeechRecognition 依赖，请先执行：pip install -r requirements.txt"
            ) from exc

        try:
            py_audio = sr.Microphone.get_pyaudio().PyAudio()
        except Exception as exc:
            raise SpeechToTextError("无法读取麦克风设备列表，请检查 PyAudio 安装。") from exc

        try:
            microphones: list[MicrophoneInfo] = []
            for index in range(py_audio.get_device_count()):
                device = py_audio.get_device_info_by_index(index)
                input_channels = int(device.get("maxInputChannels", 0) or 0)
                if input_channels <= 0:
                    continue

                name = SpeechToText._clean_device_name(str(device.get("name", "未知麦克风")))
                if SpeechToText._is_hidden_system_input(name):
                    continue

                microphones.append(
                    MicrophoneInfo(
                        index=index,
                        name=name,
                        input_channels=input_channels,
                        host_api=SpeechToText._host_api_name(py_audio, device),
                    )
                )
            return SpeechToText._select_display_microphones(microphones)
        finally:
            py_audio.terminate()

    @staticmethod
    def _host_api_name(py_audio: object, device: dict[str, object]) -> str:
        host_api_index = int(device.get("hostApi", -1) or -1)
        try:
            host_api = py_audio.get_host_api_info_by_index(host_api_index)  # type: ignore[attr-defined]
        except Exception:
            return ""
        return str(host_api.get("name", ""))

    @staticmethod
    def _clean_device_name(name: str) -> str:
        return re.sub(r"\s+", " ", name).strip()

    @staticmethod
    def _normalize_recognized_text(text: str) -> str:
        """清理语音识别结果中常见的中文逐字空格。

        Google 语音识别偶尔会把中文句子转成“目 前 我 的”这种形式。只合并
        CJK 字符之间的空白，保留英文、数字、路径和产品名之间的正常空格。
        """

        normalized = re.sub(r"(?<=[\u3400-\u9fff])\s+(?=[\u3400-\u9fff])", "", text)
        return re.sub(r"\s+", " ", normalized).strip()

    @staticmethod
    def _is_hidden_system_input(name: str) -> bool:
        normalized = name.strip().lower()
        if not normalized or normalized in {"input", "input ()"}:
            return True

        hidden_markers = (
            "microsoft sound mapper",
            "microsoft 声音映射器",
            "primary sound capture driver",
            "主声音捕获驱动程序",
        )
        return any(marker in normalized for marker in hidden_markers)

    @staticmethod
    def _select_display_microphones(microphones: list[MicrophoneInfo]) -> list[MicrophoneInfo]:
        if os.getenv("MIC_SHOW_ALL_INPUTS") == "1":
            return microphones

        if os.name == "nt":
            wasapi_microphones = [
                microphone
                for microphone in microphones
                if "wasapi" in microphone.host_api.lower()
            ]
            if wasapi_microphones:
                return SpeechToText._deduplicate_microphones(wasapi_microphones)

        return SpeechToText._deduplicate_microphones(microphones)

    @staticmethod
    def _deduplicate_microphones(microphones: list[MicrophoneInfo]) -> list[MicrophoneInfo]:
        selected: dict[str, MicrophoneInfo] = {}
        for microphone in microphones:
            key = microphone.name.lower()
            current = selected.get(key)
            if current is None or SpeechToText._host_api_rank(
                microphone.host_api
            ) < SpeechToText._host_api_rank(current.host_api):
                selected[key] = microphone
        return sorted(selected.values(), key=lambda microphone: microphone.index)

    @staticmethod
    def _host_api_rank(host_api: str) -> int:
        normalized = host_api.lower()
        if "wasapi" in normalized:
            return 0
        if "directsound" in normalized:
            return 1
        if "mme" in normalized:
            return 2
        if "wdm-ks" in normalized:
            return 3
        return 4

    @property
    def selected_microphone_label(self) -> str:
        """返回当前录音设备说明，便于排查“未检测到语音”。"""

        if self._device_index is None:
            return "系统默认麦克风"

        for microphone in self.list_microphones():
            if microphone.index == self._device_index:
                return microphone.name

        return "指定麦克风"

    def _resolve_device_index(self) -> int | None:
        """根据显式编号或名称关键字选择麦克风；都未配置时使用系统默认设备。"""

        if self.config.device_index is not None:
            return self.config.device_index

        keyword = (self.config.device_name_keyword or "").strip().lower()
        if not keyword:
            return None

        for microphone in self.list_microphones():
            if keyword in microphone.name.lower():
                return microphone.index

        raise SpeechToTextError(f"未找到名称包含“{self.config.device_name_keyword}”的麦克风。")

    def listen_once(self, status_handler: Callable[[str], None] | None = None) -> str:
        """从默认麦克风录制一句话，并转换为文字。

        返回空字符串表示没有听到可用语音；抛出 SpeechToTextError 表示设备或网络等能力不可用。

        环境变量 DEBUG_SAVE_AUDIO=1 可将录制音频保存为 WAV 文件，便于排查麦克风问题。
        """

        def emit_status(message: str) -> None:
            if status_handler is None:
                print(message)
            else:
                status_handler(message)

        try:
            with self._sr.Microphone(device_index=self._device_index) as source:
                emit_status(f"当前麦克风：{self.selected_microphone_label}")
                emit_status("正在校准环境噪声，请先保持安静...")
                self._recognizer.adjust_for_ambient_noise(
                    source, duration=self.config.adjust_noise_seconds
                )
                # 针对 Realtek 阵列等设备：校准后稍微下调阈值，避免弱信号语音被截断。
                self._recognizer.energy_threshold = max(
                    50, int(self._recognizer.energy_threshold * 0.7)
                )
                emit_status("请开始说话...")
                audio = self._recognizer.listen(
                    source,
                    timeout=self.config.timeout,
                    phrase_time_limit=self.config.phrase_time_limit,
                )
        except self._sr.WaitTimeoutError:
            emit_status("未检测到语音。请确认选择的是正在使用的麦克风，并在提示后再开始说话。")
            return ""
        except OSError as exc:
            raise SpeechToTextError("无法打开麦克风，请检查录音设备或 PyAudio 安装。") from exc

        self._save_debug_audio(audio)

        try:
            recognized_text = self._recognizer.recognize_google(audio, language=self.config.language)
            return self._normalize_recognized_text(recognized_text)
        except self._sr.UnknownValueError:
            emit_status("没有识别出清晰文字，请再说一次。")
            return ""
        except self._sr.RequestError as exc:
            raise SpeechToTextError(
                "语音识别服务请求失败，请检查网络连接，或临时改用键盘输入。"
            ) from exc

    def _save_debug_audio(self, audio: object) -> None:
        """当 DEBUG_SAVE_AUDIO=1 时将录制音频写入 WAV 文件，用于排查麦克风问题。"""

        if os.getenv("DEBUG_SAVE_AUDIO") != "1":
            return

        try:
            import time
            from pathlib import Path

            debug_dir = Path.cwd() / "debug_audio"
            debug_dir.mkdir(exist_ok=True)
            timestamp = time.strftime("%Y%m%d_%H%M%S")
            filename = debug_dir / f"mic_{self._device_index or 'default'}_{timestamp}.wav"
            with open(filename, "wb") as wav_file:
                wav_file.write(audio.get_wav_data())  # type: ignore[union-attr]
            # 不通过 status_handler 输出，避免干扰终端 UI；静默写入。
        except Exception:
            pass  # 调试保存失败不影响主流程
