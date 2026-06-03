from __future__ import annotations

import base64
import os
import queue
import re
import subprocess
import threading
from dataclasses import dataclass
from typing import Any


class TextToSpeechError(RuntimeError):
    """文字转语音初始化或播报失败时抛出。"""


@dataclass
class TTSConfig:
    """本地语音播报配置。

    Windows 默认使用 System.Speech，规避 pyttsx3 在部分 Anaconda/Windows 环境中
    因 SAPI COM 注册异常导致的“没有注册类”问题。
    """

    rate: int = 180
    volume: float = 1.0
    voice_keyword: str = "zh"
    backend: str = "auto"
    init_timeout_seconds: float = 15.0
    stop_timeout_seconds: float = 3.0


@dataclass(frozen=True)
class SpeechTask:
    """一条严格 FIFO 的播报任务。"""

    text: str
    generation: int


class TextToSpeech:
    """单线程 FIFO 文字转语音队列。

    设计目标：
    1. 调用方可以边收到流式文本边 enqueue，不阻塞命令行输出。
    2. 后台线程严格按入队顺序播报，不把任务取出后再塞回队列。
    3. 每条任务无论成功、失败或被跳过，都只调用一次 task_done()，确保 wait_until_done()
       能在队列清空后稳定返回。
    """

    def __init__(self, config: TTSConfig | None = None) -> None:
        self.config = config or TTSConfig()
        self._pyttsx3 = None
        self._backend = self._resolve_backend()
        self._ready = threading.Event()
        self._closed = threading.Event()
        self._init_error: Exception | None = None
        self._queue: queue.Queue[SpeechTask | None] = queue.Queue()
        self._state_lock = threading.Lock()
        self._generation = 0
        self._interrupted_generations: set[int] = set()
        self._current_generation: int | None = None
        self._current_task_done: threading.Event | None = None
        self._current_process: subprocess.Popen[str] | None = None
        self._system_speech_process: subprocess.Popen[str] | None = None
        self._system_speech_lock = threading.Lock()
        self._current_engine: Any | None = None
        self._worker = threading.Thread(target=self._run_worker, daemon=True)
        self._worker.start()

        if not self._ready.wait(timeout=self.config.init_timeout_seconds):
            self._closed.set()
            raise TextToSpeechError("语音播报初始化超时。")
        if self._init_error is not None:
            self._closed.set()
            raise TextToSpeechError(f"语音播报初始化失败：{self._init_error}")

    @property
    def backend_name(self) -> str:
        """返回当前使用的语音后端名称，便于排查本机 TTS 问题。"""

        return self._backend

    def enqueue(self, text: str) -> None:
        """加入一条播报任务。

        空文本、清理后为空的文本会被忽略；如果 TTS 已关闭，则直接丢弃新任务，
        避免程序退出阶段继续堆积语音。
        """

        if self._closed.is_set():
            return

        cleaned = self._clean_text_for_speech(text)
        if cleaned:
            with self._state_lock:
                generation = self._generation
            self._queue.put(SpeechTask(cleaned, generation))

    def wait_until_done(self) -> None:
        """等待当前已入队任务全部处理完成。"""

        self._queue.join()

    def interrupt(self, wait_timeout_seconds: float | None = 5.0) -> bool:
        """立即停止当前朗读，并丢弃本轮尚未播报的片段。

        返回值表示当前播报任务是否已确认结束。Windows System.Speech 通常会很快
        结束；pyttsx3 的驱动不总是可靠支持跨线程停止，因此保留超时返回，避免主
        流程永久卡住。
        """

        if self._closed.is_set():
            return True

        with self._state_lock:
            interrupted_generation = self._generation
            self._generation += 1
            self._interrupted_generations.add(interrupted_generation)
            has_current_task = self._current_generation == interrupted_generation
            current_task_done = self._current_task_done if has_current_task else None
            process = self._current_process if has_current_task else None
            engine = self._current_engine if has_current_task else None

        self._discard_pending_tasks(interrupted_generation)

        if process is not None and process.poll() is None:
            try:
                process.kill()
            except OSError:
                pass

        if engine is not None:
            try:
                engine.stop()
            except Exception:
                pass

        if self._backend == "system_speech":
            self._stop_system_speech_process(force=True)

        if current_task_done is None:
            return True

        return current_task_done.wait(timeout=wait_timeout_seconds)

    def stop(self) -> None:
        """停止后台语音线程。"""

        if self._closed.is_set():
            return

        self._closed.set()
        self._queue.put(None)
        self._worker.join(timeout=self.config.stop_timeout_seconds)
        self._stop_system_speech_process(force=False)

    def speak_sync(self, text: str) -> None:
        """同步播报一段文本，使用独立代次避免和队列播报状态串扰。"""

        cleaned = self._clean_text_for_speech(text)
        if not cleaned:
            return

        with self._state_lock:
            generation = self._generation
            current_task_done = threading.Event()
            self._current_generation = generation
            self._current_task_done = current_task_done

        try:
            self._speak_task(SpeechTask(cleaned, generation))
        finally:
            with self._state_lock:
                if self._current_task_done is current_task_done:
                    self._current_generation = None
                    self._current_task_done = None
                    self._current_process = None
                    self._current_engine = None
                self._interrupted_generations.discard(generation)
            current_task_done.set()

    def _run_worker(self) -> None:
        """后台线程入口：严格按队列顺序消费。"""

        try:
            self._validate_backend()
        except Exception as exc:
            self._init_error = exc
        finally:
            self._ready.set()

        if self._init_error is not None:
            return

        while True:
            task = self._queue.get()
            current_task_done = threading.Event()
            try:
                if task is None:
                    return

                with self._state_lock:
                    self._current_generation = task.generation
                    self._current_task_done = current_task_done

                if not self._is_generation_interrupted(task.generation):
                    try:
                        self._speak_task(task)
                    except Exception as exc:
                        if not self._is_generation_interrupted(task.generation):
                            print(f"\n语音播报失败，已跳过当前片段：{exc}")
            finally:
                with self._state_lock:
                    if self._current_task_done is current_task_done:
                        self._current_generation = None
                        self._current_task_done = None
                        self._current_process = None
                        self._current_engine = None
                    self._interrupted_generations.discard(
                        task.generation if task is not None else -1
                    )
                current_task_done.set()
                self._queue.task_done()

    def _speak_task(self, task: SpeechTask) -> None:
        """播报队列任务，并用 generation 隔离已被打断的旧任务。"""

        cleaned = self._clean_text_for_speech(task.text)
        if not cleaned or self._is_generation_interrupted(task.generation):
            return

        if self._backend == "system_speech":
            self._speak_with_system_speech(cleaned, task.generation)
            return

        self._speak_with_pyttsx3(cleaned, task.generation)

    def _resolve_backend(self) -> str:
        """选择语音后端。"""

        backend = (os.getenv("TTS_BACKEND") or self.config.backend).strip().lower()
        if backend in {"auto", "system_speech", "windows"} and os.name == "nt":
            return "system_speech"

        if backend in {"auto", "pyttsx3"}:
            try:
                import pyttsx3
            except ImportError as exc:
                raise TextToSpeechError(
                    "缺少 pyttsx3 依赖，请先执行：pip install -r requirements.txt"
                ) from exc

            self._pyttsx3 = pyttsx3
            return "pyttsx3"

        raise TextToSpeechError("TTS_BACKEND 仅支持 auto、system_speech、windows、pyttsx3。")

    def _validate_backend(self) -> None:
        """启动时验证语音后端可用，但不发声。"""

        if self._backend == "system_speech":
            completed = subprocess.run(
                [
                    "powershell.exe",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        "Add-Type -AssemblyName System.Speech; "
                        "$speaker=New-Object System.Speech.Synthesis.SpeechSynthesizer; "
                        "$count=$speaker.GetInstalledVoices().Count; "
                        "$speaker.Dispose(); "
                        "if ($count -le 0) { exit 2 }"
                    ),
                ],
                capture_output=True,
                text=True,
                timeout=15,
                creationflags=self._subprocess_creationflags(),
            )
            if completed.returncode != 0:
                message = (completed.stderr or completed.stdout or "").strip()
                raise TextToSpeechError(message or "未检测到可用的 Windows 语音。")
            return

        engine = self._create_pyttsx3_engine()
        engine.stop()

    def _speak_with_system_speech(self, text: str, generation: int) -> None:
        """使用 Windows System.Speech 播报文本。"""

        if self._is_generation_interrupted(generation):
            return

        process = self._ensure_system_speech_process()

        should_stop = False
        with self._state_lock:
            if generation in self._interrupted_generations:
                should_stop = True
            else:
                self._current_process = process

        if should_stop:
            self._stop_system_speech_process(force=True)
            return

        try:
            self._send_system_speech_text(process, text)
        except Exception as exc:
            if self._is_generation_interrupted(generation):
                return
            self._stop_system_speech_process(force=True)
            raise TextToSpeechError(f"Windows System.Speech 播报失败：{exc}") from exc
        finally:
            with self._state_lock:
                if self._current_process is process:
                    self._current_process = None

        if self._is_generation_interrupted(generation):
            return

        if process.poll() is not None:
            self._system_speech_process = None
            raise TextToSpeechError("Windows System.Speech 进程已退出。")

    def _ensure_system_speech_process(self) -> subprocess.Popen[str]:
        """复用长驻 System.Speech 进程，避免每个短句都重新启动 PowerShell。"""

        with self._system_speech_lock:
            process = self._system_speech_process
            if process is not None and process.poll() is None:
                return process

            env = os.environ.copy()
            env["TTS_RATE"] = str(self._system_speech_rate())
            env["TTS_VOLUME"] = str(self._system_speech_volume())
            env["TTS_VOICE_KEYWORD"] = self.config.voice_keyword

            process = subprocess.Popen(
                [
                    "powershell.exe",
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    self._persistent_system_speech_command(),
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                env=env,
                creationflags=self._subprocess_creationflags(),
            )
            self._system_speech_process = process
            return process

    @staticmethod
    def _send_system_speech_text(process: subprocess.Popen[str], text: str) -> None:
        if process.stdin is None or process.stdout is None:
            raise TextToSpeechError("Windows System.Speech 进程管道不可用。")

        encoded_text = base64.b64encode(text.encode("utf-8")).decode("ascii")
        try:
            process.stdin.write(encoded_text + "\n")
            process.stdin.flush()
            result = process.stdout.readline()
        except OSError as exc:
            raise TextToSpeechError(f"写入语音进程失败：{exc}") from exc

        if not result:
            raise TextToSpeechError("语音进程无响应。")

        result = result.strip()
        if result == "OK":
            return
        if result.startswith("ERR "):
            raise TextToSpeechError(result[4:] or "语音进程返回错误。")
        raise TextToSpeechError(f"语音进程返回未知状态：{result}")

    def _stop_system_speech_process(self, *, force: bool) -> None:
        with self._system_speech_lock:
            process = self._system_speech_process
            self._system_speech_process = None

        if process is None:
            return

        if force:
            if process.poll() is None:
                try:
                    process.kill()
                except OSError:
                    pass
            return

        if process.poll() is not None:
            return

        try:
            if process.stdin is not None:
                process.stdin.write("__TTS_EXIT__\n")
                process.stdin.flush()
            process.wait(timeout=2.0)
        except (OSError, subprocess.TimeoutExpired):
            try:
                process.kill()
            except OSError:
                pass

    def _speak_with_pyttsx3(self, text: str, generation: int) -> None:
        """使用短生命周期 pyttsx3 引擎播报。"""

        if self._is_generation_interrupted(generation):
            return

        last_error: Exception | None = None
        for _ in range(2):
            engine = None
            try:
                engine = self._create_pyttsx3_engine()
                should_stop = False
                with self._state_lock:
                    if generation in self._interrupted_generations:
                        should_stop = True
                    else:
                        self._current_engine = engine
                if should_stop:
                    engine.stop()
                    return

                engine.say(text)
                engine.runAndWait()
                if self._is_generation_interrupted(generation):
                    return
                return
            except Exception as exc:
                last_error = exc
            finally:
                with self._state_lock:
                    if self._current_engine is engine:
                        self._current_engine = None
                if engine is not None:
                    try:
                        engine.stop()
                    except Exception:
                        pass

        raise TextToSpeechError(f"pyttsx3 播报失败：{last_error}")

    def _discard_pending_tasks(self, generation: int) -> None:
        """清空指定代次还未被后台线程取走的任务，保持 queue.join() 计数正确。"""

        preserved: list[SpeechTask | None] = []
        while True:
            try:
                task = self._queue.get_nowait()
            except queue.Empty:
                break

            try:
                if task is None or task.generation != generation:
                    preserved.append(task)
            finally:
                self._queue.task_done()

        for task in preserved:
            self._queue.put(task)

    def _is_generation_interrupted(self, generation: int) -> bool:
        """判断某个播报代次是否已被打断。"""

        with self._state_lock:
            return generation in self._interrupted_generations

    def _create_pyttsx3_engine(self):
        """创建短生命周期 pyttsx3 引擎。"""

        if self._pyttsx3 is None:
            raise TextToSpeechError("pyttsx3 后端未初始化。")

        engine = self._pyttsx3.init()
        engine.setProperty("rate", self.config.rate)
        engine.setProperty("volume", self.config.volume)
        self._select_pyttsx3_voice(engine)
        return engine

    def _select_pyttsx3_voice(self, engine) -> None:
        """尽量选择中文语音。"""

        keyword = self.config.voice_keyword.lower()
        for voice in engine.getProperty("voices"):
            voice_text = " ".join(
                str(part).lower()
                for part in (
                    getattr(voice, "id", ""),
                    getattr(voice, "name", ""),
                    getattr(voice, "languages", ""),
                )
            )
            if keyword in voice_text or "chinese" in voice_text or "huihui" in voice_text:
                engine.setProperty("voice", voice.id)
                return

    def _system_speech_rate(self) -> int:
        """将普通语速配置映射到 System.Speech 的 -10 到 10。"""

        return max(-10, min(10, int((self.config.rate - 180) / 20)))

    def _system_speech_volume(self) -> int:
        """将 0.0-1.0 音量映射到 System.Speech 的 0-100。"""

        return max(0, min(100, int(self.config.volume * 100)))

    @staticmethod
    def _persistent_system_speech_command() -> str:
        """返回长驻 System.Speech 播报脚本，逐行接收 Base64 文本并同步确认。"""

        return (
            "[Console]::InputEncoding=[System.Text.Encoding]::UTF8; "
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; "
            "Add-Type -AssemblyName System.Speech; "
            "$speaker=New-Object System.Speech.Synthesis.SpeechSynthesizer; "
            "try { "
            "$speaker.Rate=[int]$env:TTS_RATE; "
            "$speaker.Volume=[int]$env:TTS_VOLUME; "
            "$keyword=($env:TTS_VOICE_KEYWORD + '').ToLower(); "
            "if ($keyword) { "
            "foreach ($voice in $speaker.GetInstalledVoices()) { "
            "$info=$voice.VoiceInfo; "
            "$voiceText=($info.Name + ' ' + $info.Culture.Name).ToLower(); "
            "if ($voiceText.Contains($keyword) -or $voiceText.Contains('huihui') -or "
            "$voiceText.Contains('chinese')) { $speaker.SelectVoice($info.Name); break } "
            "} "
            "} "
            "while ($true) { "
            "$line=[Console]::In.ReadLine(); "
            "if ($null -eq $line -or $line -eq '__TTS_EXIT__') { break } "
            "try { "
            "$bytes=[Convert]::FromBase64String($line); "
            "$text=[System.Text.Encoding]::UTF8.GetString($bytes); "
            "if ($text.Trim()) { $speaker.Speak($text) } "
            "[Console]::Out.WriteLine('OK'); "
            "[Console]::Out.Flush(); "
            "} catch { "
            "[Console]::Out.WriteLine('ERR ' + $_.Exception.Message); "
            "[Console]::Out.Flush(); "
            "} "
            "} "
            "} finally { $speaker.Dispose() }"
        )

    @staticmethod
    def _subprocess_creationflags() -> int:
        """在 Windows 下避免为每次播报额外弹出 PowerShell 子窗口。"""

        return getattr(subprocess, "CREATE_NO_WINDOW", 0)

    @staticmethod
    def _clean_text_for_speech(text: str) -> str:
        """清理不适合朗读的 Markdown 和多余空白。"""

        cleaned = text.strip()
        cleaned = re.sub(r"\*\*(.*?)\*\*", r"\1", cleaned)
        cleaned = re.sub(r"`([^`]*)`", r"\1", cleaned)
        cleaned = cleaned.replace("#", "")
        cleaned = cleaned.replace("*", "")
        cleaned = re.sub(r"\s+", " ", cleaned)
        return cleaned.strip()
