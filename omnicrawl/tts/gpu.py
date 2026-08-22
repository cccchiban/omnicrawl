"""TTS 的 ONNX Runtime CUDA 依赖检测与安装。

这里管理 Python 包 ``onnxruntime-gpu`` 及其 CUDA/cuDNN Python 运行时依赖，不检测、不下载、也不安装显卡驱动或 CUDA Toolkit。所有导入和 pip 操作都在调用时进行，避免设置界面启动时强依赖 TTS 推理环境。
"""

from __future__ import annotations

import importlib.metadata
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

GPU_RUNTIME_PACKAGE = "onnxruntime-gpu"
# onnxruntime-gpu 1.20/1.19 使用 CUDA 12 + cuDNN 9。不能直接要求 1.21+：
# Python 3.9 环境通常只能安装到 1.19.2，而较新的 wheel 还可能切换到 CUDA 13。
# CUDA/cuDNN wheel 作为单独的 Python 依赖安装；不安装显卡驱动或 CUDA Toolkit。
GPU_RUNTIME_REQUIREMENT = "onnxruntime-gpu<1.21.0"
# 12.6.x 可由 CUDA 12.7 驱动（例如 NVIDIA 566 系列）加载；不直接拉取
# 最新 12.9.x，避免运行时要求比现有驱动更新的 CUDA 版本。
GPU_RUNTIME_PYTHON_DEPENDENCIES = (
    "nvidia-cuda-nvrtc-cu12<12.7",
    "nvidia-cuda-runtime-cu12<12.7",
    "nvidia-cublas-cu12<12.7",
    "nvidia-cufft-cu12<12.7",
    "nvidia-curand-cu12<10.4",
    "nvidia-cudnn-cu12<9.7",
)
_PROVIDER_NAME = "CUDAExecutionProvider"
_PROBE_CODE = r'''
import json
import os
import sys
from pathlib import Path


def add_python_cuda_dll_dirs():
    """让旧版 onnxruntime 也能找到 NVIDIA wheel 提供的 DLL。"""
    try:
        import site
        roots = list(site.getsitepackages())
        user_site = site.getusersitepackages()
        if user_site:
            roots.append(user_site)
    except Exception:
        roots = []
    relative_dirs = (
        "nvidia/cuda_nvrtc/bin",
        "nvidia/cuda_runtime/bin",
        "nvidia/cublas/bin",
        "nvidia/cufft/bin",
        "nvidia/curand/bin",
        "nvidia/cudnn/bin",
    )
    added = []
    handles = []
    for root in roots:
        for relative in relative_dirs:
            candidate = Path(root) / relative
            if not candidate.is_dir():
                continue
            candidate_text = str(candidate)
            if candidate_text in added:
                continue
            added.append(candidate_text)
            os.environ["PATH"] = candidate_text + os.pathsep + os.environ.get("PATH", "")
            add_dll_directory = getattr(os, "add_dll_directory", None)
            if callable(add_dll_directory):
                handles.append(add_dll_directory(candidate_text))
    # 保持 Windows DLL 搜索目录句柄存活到本子进程结束。
    return added, handles


dll_dirs, _dll_handles = add_python_cuda_dll_dirs()
result = {"python": sys.executable, "dll_dirs": dll_dirs}
try:
    import onnxruntime as ort

    preload = getattr(ort, "preload_dlls", None)
    if callable(preload):
        preload()
    result["onnxruntime"] = getattr(ort, "__version__", "unknown")
    result["providers"] = ort.get_available_providers()
    if len(sys.argv) > 1 and sys.argv[1]:
        try:
            session = ort.InferenceSession(
                sys.argv[1],
                providers=["CUDAExecutionProvider", "CPUExecutionProvider"],
            )
            result["session_providers"] = session.get_providers()
        except Exception as exc:
            result["session_error"] = str(exc)
except Exception as exc:
    result["import_error"] = repr(exc)
print(json.dumps(result, ensure_ascii=False))
'''


@dataclass(frozen=True)
class GpuRuntimeStatus:
    """当前 Python 环境中 CUDA EP 的可用状态。

    ``get_available_providers()`` 只代表 provider 被编译进 wheel；Windows 上缺少
    CUDA/cuDNN DLL 时仍可能在真正创建 session 时退回 CPU，因此可选地记录真实
    session 的 provider 和错误。
    """

    package_version: str | None
    available_providers: tuple[str, ...]
    probe_error: str | None = None
    session_providers: tuple[str, ...] = ()
    session_error: str | None = None
    diagnostic: str | None = None

    @property
    def installed(self) -> bool:
        return self.package_version is not None

    @property
    def cuda_available(self) -> bool:
        return _PROVIDER_NAME in self.available_providers

    @property
    def session_cuda_available(self) -> bool:
        return _PROVIDER_NAME in self.session_providers

    @property
    def ready(self) -> bool:
        session_ready = (
            not self.session_error
            and (not self.session_providers or self.session_cuda_available)
        )
        return self.installed and self.cuda_available and session_ready


def _installed_version(package_name: str) -> str | None:
    try:
        return importlib.metadata.version(package_name)
    except importlib.metadata.PackageNotFoundError:
        return None


_PROBE_MODEL_NAMES = (
    "MOSS-TTS-Nano-100M-ONNX/moss_tts_prefill.onnx",
    "moss_tts_prefill.onnx",
)


def _resolve_probe_model(model_path: str | Path | None) -> Path | None:
    if not model_path:
        return None
    candidate = Path(model_path).expanduser()
    if candidate.is_file():
        return candidate.resolve()
    if candidate.is_dir():
        for relative in _PROBE_MODEL_NAMES:
            nested = candidate / relative
            if nested.is_file():
                return nested.resolve()
    return None


def _decode_process_output(raw: bytes | str | None) -> str:
    """解码 Windows 子进程输出；某些 Anaconda/ORT 日志会以 UTF-16LE 写入 stderr。"""

    if not raw:
        return ""
    if isinstance(raw, str):
        text = raw
    else:
        if b"\x00" in raw:
            try:
                text = raw.decode("utf-16")
            except UnicodeDecodeError:
                text = raw.decode("utf-8", errors="replace")
        else:
            text = raw.decode("utf-8", errors="replace")
    # ONNX Runtime 的 Windows 日志可能带 ANSI 颜色码和 NUL；去掉后诊断才可读。
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", text)
    return text.replace("\x00", "").strip()


def _diagnostic_detail(stdout: str, stderr: str, *, limit: int = 1800) -> str:
    parts = []
    if stdout:
        parts.append(f"stdout: {stdout}")
    if stderr:
        parts.append(f"stderr: {stderr}")
    return "；".join(parts)[-limit:] or "无输出"


def _probe_onnxruntime(
    model_path: str | Path | None = None,
) -> tuple[tuple[str, ...], str | None, tuple[str, ...], str | None, str | None]:
    """在独立进程中同时探测 provider 和真实 ONNX session。

    ``get_available_providers()`` 不能证明 CUDA DLL 已正确加载；只有实际创建
    session 并检查 ``session.get_providers()`` 才能识别当前报错这种“声明 CUDA、
    实际回退 CPU”的情况。
    """

    probe_model = _resolve_probe_model(model_path)
    command = [sys.executable, "-c", _PROBE_CODE]
    if probe_model is not None:
        command.append(str(probe_model))
    try:
        # 必须保留原始 bytes：Anaconda/ONNX Runtime 的 stderr 可能是 UTF-16LE，
        # 如果交给 subprocess 先按 UTF-8 解码，会变成带 NUL 的乱码。
        completed = subprocess.run(
            command,
            capture_output=True,
            timeout=120,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return (), f"无法启动 ONNX Runtime 探测进程：{exc}", (), None, None
    output = _decode_process_output(completed.stdout)
    error_output = _decode_process_output(completed.stderr)
    diagnostic = _diagnostic_detail(output, error_output)
    if completed.returncode != 0:
        return (), f"ONNX Runtime 探测进程失败（退出码 {completed.returncode}）：{diagnostic}", (), None, diagnostic
    try:
        payload = json.loads(output)
        if not isinstance(payload, dict):
            raise ValueError("探测结果不是对象")
        providers = payload.get("providers", [])
        session_providers = payload.get("session_providers", [])
        if not isinstance(providers, list) or not isinstance(session_providers, list):
            raise ValueError("providers 不是列表")
        import_error = str(payload["import_error"]) if payload.get("import_error") else None
        session_error = str(payload["session_error"]) if payload.get("session_error") else None
        if import_error and not error_output:
            error_output = import_error
        diagnostic = _diagnostic_detail(output, error_output)
        return (
            tuple(str(item) for item in providers),
            import_error,
            tuple(str(item) for item in session_providers),
            session_error,
            diagnostic,
        )
    except (ValueError, TypeError, json.JSONDecodeError) as exc:
        detail = _diagnostic_detail(output, error_output)
        return (), f"ONNX Runtime 探测结果无效：{exc}；{detail}", (), None, detail


def check_gpu_runtime(model_path: str | Path | None = None) -> GpuRuntimeStatus:
    """检查 GPU 包，并在模型存在时验证真实 session 是否使用 CUDA。"""

    package_version = _installed_version(GPU_RUNTIME_PACKAGE)
    # 没有模型路径时保留无参调用，兼容旧的扩展/测试 monkeypatch。
    probe_result = (
        _probe_onnxruntime()
        if model_path is None
        else _probe_onnxruntime(model_path)
    )
    # 兼容旧的测试/扩展 monkeypatch：旧探测器只返回 providers + error，
    # 或上一版返回四元组；新版本额外保留 stdout/stderr 诊断。
    diagnostic = None
    if len(probe_result) == 2:
        providers, probe_error = probe_result  # type: ignore[misc]
        session_providers, session_error = (), None
    elif len(probe_result) == 4:
        providers, probe_error, session_providers, session_error = probe_result  # type: ignore[misc]
    else:
        providers, probe_error, session_providers, session_error, diagnostic = probe_result
    return GpuRuntimeStatus(
        package_version,
        providers,
        probe_error,
        session_providers,
        session_error,
        diagnostic,
    )


def _run_pip(arguments: Sequence[str], *, timeout: int = 900) -> None:
    command = [sys.executable, "-m", "pip", *arguments]
    try:
        completed = subprocess.run(
            command,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise RuntimeError(f"无法启动 pip：{exc}") from exc
    if completed.returncode == 0:
        return
    detail = (completed.stderr or completed.stdout or "未知错误").strip()
    raise RuntimeError(f"pip 命令失败（退出码 {completed.returncode}）：{detail[-1500:]}")


def install_gpu_runtime(model_path: str | Path | None = None) -> GpuRuntimeStatus:
    """下载并安装 ``onnxruntime-gpu``，不处理显卡驱动。

    CPU 版和 GPU 版 ONNX Runtime 不能共存：安装前移除 CPU 版；若安装失败且之前
    存在 CPU 版，则尽力恢复原 CPU 版本，避免 TTS 完全不可用。
    """

    cpu_version = _installed_version("onnxruntime")
    if cpu_version is not None:
        _run_pip(["uninstall", "-y", "onnxruntime"])
    try:
        # 旧版 onnxruntime-gpu 没有 pip extras，显式安装 CUDA/cuDNN Python wheel，
        # 这样 Python 3.9 的 Anaconda 环境也能获得实际 session 所需的 DLL。
        _run_pip(
            [
                "install",
                "--upgrade",
                GPU_RUNTIME_REQUIREMENT,
                *GPU_RUNTIME_PYTHON_DEPENDENCIES,
            ]
        )
    except Exception as exc:
        if cpu_version is not None:
            try:
                _run_pip(["install", f"onnxruntime=={cpu_version}"], timeout=600)
            except Exception as restore_exc:
                raise RuntimeError(
                    f"onnxruntime-gpu 安装失败：{exc}；恢复 CPU 版也失败：{restore_exc}"
                ) from exc
        raise
    return check_gpu_runtime(model_path)


__all__ = [
    "GPU_RUNTIME_PACKAGE",
    "GPU_RUNTIME_REQUIREMENT",
    "GPU_RUNTIME_PYTHON_DEPENDENCIES",
    "GpuRuntimeStatus",
    "check_gpu_runtime",
    "install_gpu_runtime",
]
