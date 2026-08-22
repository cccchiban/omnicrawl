"""TTS（MOSS-TTS-Nano ONNX CPU）的设置界面。

支持：启用开关、自动播放开关、内置音色选择、模型目录、CPU 线程数，
以及一键下载 ONNX 模型（约 763MB，后台执行并实时显示进度）。
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from textual import work
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Input, Select, Static

from ....config.features.tts import (
    TTSConfigError,
    TTSConfiguration,
    load_tts_configuration,
    save_tts_configuration,
)
from ..terminal.theme import terminal_css, terminal_select_css


def _tts_engine_probe() -> tuple[Any | None, Any | None]:
    """返回 ``(builtin_voice_names, models_ready)`` 探测函数。

    模型探测（omnicrawl.tts.download）只依赖标准库，不加载推理引擎
    （numpy/sentencepiece/onnxruntime）；依赖缺失时返回 ``(None, None)``，
    界面降级为“模型未就绪”并继续运行（合成时仍会给出明确错误）。
    """

    try:
        from ....tts.download import builtin_voice_names, models_ready
    except Exception:  # noqa: BLE001 - 缺失可选依赖不能破坏设置页
        return None, None
    return builtin_voice_names, models_ready

# 模型未下载时音色下拉的兜底候选（与模型 manifest 内置音色一致）。
_FALLBACK_VOICES = (
    "Junhao",
    "Zhiming",
    "Weiguo",
    "Xiaoyu",
    "Yuewen",
    "Lingyu",
    "Trump",
    "Ava",
    "Bella",
    "Adam",
    "Nathan",
    "Soyo",
    "Saki",
    "Mortis",
    "Umiri",
    "Mei",
    "Anon",
    "Arisa",
)
_THREAD_COUNTS = (1, 2, 4, 8)


@dataclass(frozen=True)
class TTSSettingsResult:
    """TTS 设置保存结果。"""

    configuration: TTSConfiguration
    config_path: Path


class TTSSettingsScreen(ModalScreen[Optional[TTSSettingsResult]]):
    """编辑 TTS 开关、音色与模型下载。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css(
        """
    TTSSettingsScreen {
        align: center middle;
        background: $terminal-overlay;
    }
    #tts-dialog {
        width: 86;
        max-width: 96%;
        height: 40;
        max-height: 95%;
        padding: 1 2;
        border: round $terminal-border-strong;
        background: $terminal-surface;
    }
    #tts-title {
        height: 1;
        margin-bottom: 1;
        color: $terminal-white;
        text-style: bold;
    }
    #tts-form {
        height: 1fr;
    }
    .tts-field-label {
        height: 1;
        color: $terminal-text-muted;
    }
    .tts-control {
        height: 3;
        margin-bottom: 1;
    }
    #tts-status {
        height: 2;
        color: $terminal-white;
    }
    #tts-actions {
        height: 3;
        align-horizontal: right;
    }
    """
        + terminal_select_css()
    )

    def __init__(
        self,
        config_path: str | Path,
        *,
        apply_configuration: Callable[[TTSConfiguration], None] | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        self._configuration = load_tts_configuration(self._config_path)
        self._previous_configuration = self._configuration
        self._downloading = False

    def compose(self) -> ComposeResult:
        c = self._configuration
        with Container(id="tts-dialog"):
            yield Static("TTS 语音合成（MOSS-TTS-Nano · 本地 CPU 推理）", id="tts-title")
            with VerticalScroll(id="tts-form"):
                yield Static("启用 TTS（注册 tts_synthesize 工具）", classes="tts-field-label")
                yield Select(
                    [("停用", False), ("启用", True)],
                    value=c.enabled,
                    allow_blank=False,
                    id="tts-enabled",
                    classes="tts-control choice-select",
                )
                yield Static("合成完成后自动播放", classes="tts-field-label")
                yield Select(
                    [("关闭", False), ("开启", True)],
                    value=c.auto_play,
                    allow_blank=False,
                    id="tts-auto-play",
                    classes="tts-control choice-select",
                )
                yield Static("内置音色 voice", classes="tts-field-label")
                yield Select(
                    [(voice, voice) for voice in self._voice_options()],
                    value=c.voice if c.voice in self._voice_options() else "Junhao",
                    allow_blank=False,
                    id="tts-voice",
                    classes="tts-control choice-select",
                )
                yield Static("模型目录 model_dir（留空使用默认 ~/.omnicrawl/tts/models）", classes="tts-field-label")
                yield Input(
                    c.model_dir,
                    placeholder="留空 = 默认目录（OMNICRAWL_TTS_MODEL_DIR 可覆盖）",
                    id="tts-model-dir",
                    classes="tts-control",
                )
                yield Static("CPU 推理线程数 thread_count", classes="tts-field-label")
                yield Select(
                    [(str(item), item) for item in _THREAD_COUNTS],
                    value=c.thread_count if c.thread_count in _THREAD_COUNTS else 4,
                    allow_blank=False,
                    id="tts-thread-count",
                    classes="tts-control choice-select",
                )
                yield Static(self._model_status(), id="tts-status")
                yield Button(
                    "下载 ONNX 模型（约 763MB）" if not self._models_ready()
                    else "重新下载 ONNX 模型",
                    id="tts-download",
                    variant="primary",
                )
            with Horizontal(id="tts-actions"):
                yield Button("取消", id="tts-cancel")
                yield Button("保存", variant="primary", id="tts-save")

    def _models_ready(self) -> bool:
        """模型是否已就绪；TTS 依赖缺失时按未就绪处理，不阻断设置页。"""

        _builtin, models_ready = _tts_engine_probe()
        if models_ready is None:
            return False
        try:
            return bool(models_ready(self._configuration.model_dir or None))
        except Exception:  # noqa: BLE001 - 探测失败按未就绪处理
            return False

    def _voice_options(self) -> tuple[str, ...]:
        builtin_voice_names, _models_ready = _tts_engine_probe()
        voices: list[str] = []
        if builtin_voice_names is not None:
            try:
                voices = builtin_voice_names(self._configuration.model_dir or None)
            except Exception:  # noqa: BLE001 - 读取失败回退兜底音色
                voices = []
        if not voices:
            voices = list(_FALLBACK_VOICES)
        return tuple(voices)

    def _model_status(self) -> str:
        c = self._configuration
        if self._models_ready():
            model_dir = c.resolved_model_dir()
            voices = self._voice_options()
            return (
                f"模型已就绪：{model_dir}（内置音色 {len(voices)} 个）。"
                "状态：已下载 ✓"
            )
        return "模型未下载：点击下方按钮自动下载（约 763MB，首次使用约需数分钟）。"

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "tts-save":
            self.action_save()
        elif event.button.id == "tts-cancel":
            self.action_cancel()
        elif event.button.id == "tts-download":
            self._start_download()

    def action_cancel(self) -> None:
        if not self._downloading:
            self.dismiss(None)

    def action_save(self) -> None:
        if self._downloading:
            self._set_status("模型下载中，请等待完成后再保存。")
            return
        try:
            configuration = TTSConfiguration(
                enabled=bool(self.query_one("#tts-enabled", Select).value),
                model_dir=self._read_input("tts-model-dir", fallback=""),
                voice=str(self.query_one("#tts-voice", Select).value),
                auto_play=bool(self.query_one("#tts-auto-play", Select).value),
                thread_count=int(self.query_one("#tts-thread-count", Select).value),
                output_dir=self._configuration.output_dir,
            )
            path = save_tts_configuration(configuration, self._config_path)
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    save_tts_configuration(self._previous_configuration, self._config_path)
                    raise
        except (TTSConfigError, OSError, ValueError) as exc:
            self._set_status(f"保存失败：{exc}")
            return
        self.dismiss(TTSSettingsResult(configuration, path))

    @work(thread=True, exclusive=True, group="tts-download", exit_on_error=False)
    def _start_download(self) -> None:
        self._downloading = True
        self.app.call_from_thread(
            self._set_status, "正在下载 ONNX 模型（TTS 673MB + Codec 91MB）…"
        )
        try:
            from ....tts.download import ensure_model_dir

            c = self._configuration

            def on_progress(label: str, done_bytes: int, total_bytes: int) -> None:
                self.app.call_from_thread(
                    self._set_download_progress, label, done_bytes, total_bytes
                )

            model_dir = ensure_model_dir(
                c.model_dir or None,
                progress_callback=on_progress,
            )
            self.app.call_from_thread(self._download_finished, str(model_dir), None)
        except Exception as exc:  # noqa: BLE001 - 下载失败显示原因
            self.app.call_from_thread(self._download_finished, None, str(exc))

    def _set_download_progress(
        self,
        label: str,
        done_bytes: int,
        total_bytes: int,
    ) -> None:
        """刷新下载进度（每 1MB 一次）：已下载 MB / 总 MB（百分比）。"""

        done_mb = max(0, int(done_bytes)) / 1e6
        if total_bytes and total_bytes > 0:
            percent = min(100.0, max(0.0, done_bytes * 100.0 / total_bytes))
            text = f"下载中：{label} {done_mb:.0f}/{total_bytes / 1e6:.0f} MB（{percent:.0f}%）"
        else:
            text = f"下载中：{label} 已下载 {done_mb:.0f} MB"
        self._set_status(text)

    def _download_finished(self, model_dir: str | None, error: str | None) -> None:
        self._downloading = False
        if error:
            self._set_status(f"下载失败：{error}")
            return
        self._refresh_voice_options()
        self._set_status(f"模型下载完成：{model_dir}。内置音色已更新 ✓")

    def _refresh_voice_options(self) -> None:
        select = self.query_one("#tts-voice", Select)
        voices = self._voice_options()
        select.set_options([(voice, voice) for voice in voices])
        if self._configuration.voice in voices:
            select.value = self._configuration.voice
        elif voices:
            select.value = voices[0]

    def _read_input(self, widget_id: str, *, fallback: str) -> str:
        value = self.query_one(f"#{widget_id}", Input).value.strip()
        return value or fallback

    def _set_status(self, status: str) -> None:
        self.query_one("#tts-status", Static).update(status)


__all__ = ["TTSSettingsResult", "TTSSettingsScreen"]
