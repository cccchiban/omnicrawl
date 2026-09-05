"""TTS（MOSS-TTS-Nano ONNX CPU）设置（可内嵌右侧的 Pane + 整屏薄壳）。

支持：启用开关、自动播放开关、内置音色选择、模型目录、CPU 线程数，
以及一键下载 ONNX 模型（约 763MB，后台执行并实时显示进度）。打开设置时会检查
onnxruntime-gpu；仅在用户点击按钮后下载并自动安装该 Python 包，不处理显卡驱动。
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
from .panes import SettingsPane


def _tts_engine_probe() -> tuple[Any | None, Any | None]:
    """返回 ``(builtin_voice_names, models_ready)`` 探测函数。"""

    try:
        from ....tts.download import builtin_voice_names, models_ready
    except Exception:  # noqa: BLE001 - 缺失可选依赖不能破坏设置页
        return None, None
    return builtin_voice_names, models_ready


def _models_ready(configuration: "TTSConfiguration") -> bool:
    """模型是否已就绪；TTS 依赖缺失时按未就绪处理，不阻断设置页。"""

    _builtin, models_ready = _tts_engine_probe()
    if models_ready is None:
        return False
    try:
        return bool(models_ready(configuration.model_dir or None))
    except Exception:  # noqa: BLE001 - 探测失败按未就绪处理
        return False


def _voice_options(configuration: "TTSConfiguration") -> tuple[str, ...]:
    builtin_voice_names, _models_ready_fn = _tts_engine_probe()
    voices: list[str] = []
    if builtin_voice_names is not None:
        try:
            voices = builtin_voice_names(configuration.model_dir or None)
        except Exception:  # noqa: BLE001 - 读取失败回退兜底音色
            voices = []
    if not voices:
        voices = list(_FALLBACK_VOICES)
    return tuple(voices)


def _model_status_text(configuration: "TTSConfiguration") -> str:
    if _models_ready(configuration):
        model_dir = configuration.resolved_model_dir()
        voices = _voice_options(configuration)
        return (
            f"模型已就绪：{model_dir}（内置音色 {len(voices)} 个）。"
            "状态：已下载 ✓"
        )
    return "模型未下载：点击下方按钮自动下载（约 763MB，首次使用约需数分钟）。"


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
_DEVICE_OPTIONS = (
    ("自动（优先 CUDA，不可用时回退 CPU）", "auto"),
    ("CPU", "cpu"),
    ("CUDA（不可用时报错）", "cuda"),
)


@dataclass(frozen=True)
class TTSSettingsResult:
    """TTS 设置保存结果。"""

    configuration: TTSConfiguration
    config_path: Path


_PANE_CSS = """
#tts-form { height: 1fr; }
.tts-field-label { height: 1; color: $terminal-text-muted; }
.tts-control { height: 3; margin-bottom: 1; }
.tts-status { height: 2; color: $terminal-white; }
.tts-download-button { margin-bottom: 1; }
#tts-actions { height: 3; align-horizontal: right; }
"""


class TTSSettingsPane(SettingsPane):
    """TTS 二级面板：编辑 TTS 开关、音色与模型下载。"""

    BINDINGS = [
        Binding("escape", "cancel", "返回", priority=True),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    DEFAULT_CSS = terminal_css(_PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path | None = None,
        *,
        agent: Any | None = None,
        apply_configuration: Callable[[TTSConfiguration], None] | None = None,
        configuration: TTSConfiguration | None = None,
    ) -> None:
        super().__init__(agent=agent)
        self._config_path = Path(config_path) if config_path is not None else None
        self._apply_configuration = apply_configuration
        self._configuration = configuration or (
            load_tts_configuration(self._config_path)
            if self._config_path is not None
            else TTSConfiguration()
        )
        self._previous_configuration = self._configuration
        self._downloading = False
        self._gpu_busy = False
        # onnxruntime 可能已在当前进程加载；安装新 wheel 后必须完整重启，
        # 才能让 tts_synthesize 使用新的 CUDA/cuDNN DLL。
        self._gpu_restart_required = False

    def on_mount(self) -> None:
        """打开 TTS 设置时自动检查一次 GPU 运行时。"""
        self._start_gpu_check()
        self.call_after_refresh(self.refresh_pane)

    def compose_pane(self) -> ComposeResult:
        c = self._configuration
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
            yield Static("推理设备 device", classes="tts-field-label")
            yield Select(
                list(_DEVICE_OPTIONS),
                value=c.device,
                allow_blank=False,
                id="tts-device",
                classes="tts-control choice-select",
            )
            yield Static("CPU 推理线程数 thread_count", classes="tts-field-label")
            yield Select(
                [(str(item), item) for item in _THREAD_COUNTS],
                value=c.thread_count if c.thread_count in _THREAD_COUNTS else 4,
                allow_blank=False,
                id="tts-thread-count",
                classes="tts-control choice-select",
            )
            yield Static(self._model_status(), id="tts-status", classes="tts-status")
            yield Button(
                "下载 ONNX 模型（约 763MB）" if not self._models_ready()
                else "重新下载 ONNX 模型",
                id="tts-download",
                classes="tts-download-button",
                variant="primary",
            )
            yield Static("GPU 运行时：正在检查…", id="tts-gpu-status", classes="tts-status")
            yield Button(
                "下载并安装 onnxruntime-gpu",
                id="tts-gpu-download",
                classes="tts-download-button",
                variant="primary",
            )
        with Horizontal(id="tts-actions"):
            yield Static("", classes="pane-save-hint")
            yield Button("取消", id="tts-cancel")
            yield Button("保存", variant="primary", id="tts-save")

    def _models_ready(self) -> bool:
        """模型是否已就绪；TTS 依赖缺失时按未就绪处理，不阻断设置页。"""
        return _models_ready(self._configuration)

    def _voice_options(self) -> tuple[str, ...]:
        return _voice_options(self._configuration)

    def _model_status(self) -> str:
        return _model_status_text(self._configuration)

    def on_button_pressed(self, event: Button.Pressed) -> None:
        if event.button.id == "tts-save":
            self.action_save()
        elif event.button.id == "tts-cancel":
            self.action_cancel()
        elif event.button.id == "tts-download":
            self._start_download()
        elif event.button.id == "tts-gpu-download":
            self._start_gpu_install()

    def activate(self) -> None:
        self.query_one("#tts-enabled", Select).focus()

    def action_cancel(self) -> None:
        if not self._downloading and not self._gpu_busy:
            self.request_back()

    def action_save(self) -> None:
        if self._downloading or self._gpu_busy:
            self._set_status("TTS 依赖安装或模型下载中，请等待完成后再保存。")
            return
        try:
            configuration = TTSConfiguration(
                enabled=bool(self.query_one("#tts-enabled", Select).value),
                model_dir=self._read_input("tts-model-dir", fallback=""),
                voice=str(self.query_one("#tts-voice", Select).value),
                auto_play=bool(self.query_one("#tts-auto-play", Select).value),
                thread_count=int(self.query_one("#tts-thread-count", Select).value),
                device=str(self.query_one("#tts-device", Select).value),
                output_dir=self._configuration.output_dir,
            )
            if self._config_path is not None:
                path = save_tts_configuration(configuration, self._config_path)
            else:
                path = self._config_path
            if self._apply_configuration is not None:
                try:
                    self._apply_configuration(configuration)
                except Exception:
                    if self._config_path is not None:
                        save_tts_configuration(self._previous_configuration, self._config_path)
                    raise
        except (TTSConfigError, OSError, ValueError) as exc:
            self._set_status(f"保存失败：{exc}")
            return
        self.flash_save_hint()
        self.commit(TTSSettingsResult(configuration, path))

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

    def _set_gpu_status(self, status: str) -> None:
        self.query_one("#tts-gpu-status", Static).update(status)

    def _set_gpu_busy(self, busy: bool) -> None:
        self._gpu_busy = busy
        if self.is_mounted:
            self.query_one("#tts-gpu-download", Button).disabled = busy

    @staticmethod
    def _format_gpu_status(status: Any) -> str:
        package_version = getattr(status, "package_version", None)
        providers = tuple(getattr(status, "available_providers", ()) or ())
        probe_error = str(getattr(status, "probe_error", "") or "")
        if bool(getattr(status, "ready", False)):
            return f"GPU 运行时：CUDA 可用（onnxruntime-gpu {package_version}）✓"
        session_providers = tuple(getattr(status, "session_providers", ()) or ())
        session_error = str(getattr(status, "session_error", "") or "")
        if package_version:
            if session_error:
                message = (
                    f"GPU 运行时：已安装 onnxruntime-gpu {package_version}，"
                    "但 CUDA 会话创建失败。"
                )
            elif session_providers and "CUDAExecutionProvider" not in session_providers:
                message = (
                    f"GPU 运行时：已安装 onnxruntime-gpu {package_version}，"
                    "但实际会话回退到了 CPU。"
                )
            else:
                message = (
                    f"GPU 运行时：已安装 onnxruntime-gpu {package_version}，"
                    "当前未提供 CUDAExecutionProvider。"
                )
        else:
            message = "GPU 运行时：未安装 onnxruntime-gpu。"
        if providers:
            message += f" 可用 provider：{', '.join(providers)}。"
        if session_providers:
            message += f" 实际会话 provider：{', '.join(session_providers)}。"
        if session_error:
            message += f" 会话检查错误：{session_error}"
        if probe_error:
            message += f" 检查错误：{probe_error}"
        diagnostic = str(getattr(status, "diagnostic", "") or "")
        if diagnostic and diagnostic not in message:
            message += f" 详细诊断：{diagnostic[-1200:]}"
        return message

    @work(thread=True, exclusive=True, group="tts-gpu", exit_on_error=False)
    def _start_gpu_check(self) -> None:
        self.app.call_from_thread(self._set_gpu_busy, True)
        try:
            from ....tts.gpu import check_gpu_runtime

            status = check_gpu_runtime(self._configuration.resolved_model_dir())
            self.app.call_from_thread(self._gpu_check_finished, status, None)
        except Exception as exc:  # noqa: BLE001 - 设置页只显示检查失败
            self.app.call_from_thread(self._gpu_check_finished, None, str(exc))

    def _gpu_check_finished(self, status: Any | None, error: str | None) -> None:
        self._set_gpu_busy(False)
        if error:
            self._set_gpu_status(f"GPU 运行时检查失败：{error}")
            return
        self._set_gpu_status(self._format_gpu_status(status))
        button = self.query_one("#tts-gpu-download", Button)
        button.label = (
            "重新安装 onnxruntime-gpu"
            if bool(getattr(status, "installed", False))
            else "下载并安装 onnxruntime-gpu"
        )

    @work(thread=True, exclusive=True, group="tts-gpu", exit_on_error=False)
    def _start_gpu_install(self) -> None:
        if self._gpu_busy:
            return
        self.app.call_from_thread(self._set_gpu_busy, True)
        self.app.call_from_thread(
            self._set_gpu_status,
            "正在下载并安装 onnxruntime-gpu；不会处理显卡驱动…",
        )
        try:
            from ....tts.gpu import install_gpu_runtime

            status = install_gpu_runtime(self._configuration.resolved_model_dir())
            self.app.call_from_thread(self._gpu_install_finished, status, None)
        except Exception as exc:  # noqa: BLE001 - 安装失败显示 pip 错误
            self.app.call_from_thread(self._gpu_install_finished, None, str(exc))

    def _gpu_install_finished(self, status: Any | None, error: str | None) -> None:
        self._set_gpu_busy(False)
        if error:
            self._set_gpu_status(f"onnxruntime-gpu 安装失败：{error}")
            return
        self._gpu_restart_required = True
        formatted = self._format_gpu_status(status)
        if status is not None and not bool(getattr(status, "ready", False)):
            formatted += " 请确认 NVIDIA 驱动、CUDA/cuDNN 与该版本兼容。"
        self._set_gpu_status(
            formatted
            + " 安装已完成；请完全退出并重启 OmniCrawl 后再调用 tts_synthesize。"
        )


class TTSSettingsScreen(ModalScreen[Optional[TTSSettingsResult]]):
    """整屏薄壳：内嵌 TTSSettingsPane。"""

    BINDINGS = [
        Binding("escape", "cancel", "取消"),
        Binding("ctrl+s", "save", "保存", priority=True),
    ]

    CSS = terminal_css("""
    TTSSettingsScreen { align: center middle; background: $terminal-overlay; }
    #tts-dialog { width: 86; max-width: 96%; height: 40; max-height: 95%; padding: 1 2; border: round $terminal-border-strong; background: $terminal-surface; }
    #tts-title { height: 1; margin-bottom: 1; color: $terminal-white; text-style: bold; }
    """ + _PANE_CSS + terminal_select_css())

    def __init__(
        self,
        config_path: str | Path,
        *,
        apply_configuration: Callable[[TTSConfiguration], None] | None = None,
    ) -> None:
        super().__init__()
        self._config_path = Path(config_path)
        self._apply_configuration = apply_configuration
        self._pane: Optional[TTSSettingsPane] = None
        self._configuration = load_tts_configuration(self._config_path)

    # 查询转发：既有测试在未挂载时直接读取模型状态。
    def _models_ready(self) -> bool:
        return _models_ready(self._configuration)

    def _voice_options(self) -> tuple[str, ...]:
        return _voice_options(self._configuration)

    def _model_status(self) -> str:
        return _model_status_text(self._configuration)

    def compose(self) -> ComposeResult:
        with Container(id="tts-dialog"):
            yield Static("TTS 语音合成（MOSS-TTS-Nano · ONNX 推理）", id="tts-title")
            self._pane = TTSSettingsPane(
                self._config_path,
                apply_configuration=self._apply_configuration,
            )
            self._pane.bind_pane_events(
                on_back=lambda: self.dismiss(None),
                on_commit=lambda result: self.dismiss(result),
            )
            yield self._pane

    def on_mount(self) -> None:
        if self._pane is not None:
            self._pane.activate()

    def action_cancel(self) -> None:
        if self._pane is not None:
            self._pane.action_cancel()

    def action_save(self) -> None:
        if self._pane is not None:
            self._pane.action_save()


__all__ = ["TTSSettingsResult", "TTSSettingsPane", "TTSSettingsScreen"]
