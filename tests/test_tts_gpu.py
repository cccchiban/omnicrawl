"""TTS GPU 运行时检测与安装流程测试。"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from omnicrawl.tts import gpu


def test_gpu_runtime_status_properties() -> None:
    ready = gpu.GpuRuntimeStatus(
        "1.20.0",
        ("CUDAExecutionProvider", "CPUExecutionProvider"),
    )
    assert ready.installed is True
    assert ready.cuda_available is True
    assert ready.ready is True

    unavailable = gpu.GpuRuntimeStatus("1.20.0", ("CPUExecutionProvider",))
    assert unavailable.installed is True
    assert unavailable.cuda_available is False
    assert unavailable.ready is False


def test_check_gpu_runtime_reports_package_and_providers(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gpu, "_installed_version", lambda name: "1.20.0" if name == gpu.GPU_RUNTIME_PACKAGE else None)
    monkeypatch.setattr(gpu, "_probe_onnxruntime", lambda *_args: (("CPUExecutionProvider",), None))

    status = gpu.check_gpu_runtime()

    assert status.package_version == "1.20.0"
    assert status.available_providers == ("CPUExecutionProvider",)
    assert status.ready is False


def test_gpu_runtime_is_not_ready_when_session_falls_back_to_cpu() -> None:
    status = gpu.GpuRuntimeStatus(
        "1.21.0",
        ("CUDAExecutionProvider", "CPUExecutionProvider"),
        session_providers=("CPUExecutionProvider",),
    )

    assert status.cuda_available is True
    assert status.session_cuda_available is False
    assert status.ready is False


def test_gpu_runtime_session_error_is_not_ready() -> None:
    status = gpu.GpuRuntimeStatus(
        "1.21.0",
        ("CUDAExecutionProvider", "CPUExecutionProvider"),
        session_error="DLL load failed",
    )

    assert status.ready is False


def test_install_gpu_runtime_removes_cpu_and_checks_result(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[tuple[str, ...]] = []
    versions = {"onnxruntime": "1.19.2", gpu.GPU_RUNTIME_PACKAGE: None}

    monkeypatch.setattr(gpu, "_installed_version", lambda name: versions.get(name))

    def fake_run_pip(arguments, *, timeout=900):
        calls.append(tuple(arguments))
        if arguments[0] == "uninstall":
            versions["onnxruntime"] = None
        else:
            versions[gpu.GPU_RUNTIME_PACKAGE] = "1.20.0"

    monkeypatch.setattr(gpu, "_run_pip", fake_run_pip)
    monkeypatch.setattr(
        gpu,
        "_probe_onnxruntime",
        lambda: (("CUDAExecutionProvider", "CPUExecutionProvider"), None),
    )

    status = gpu.install_gpu_runtime()

    assert calls == [
        ("uninstall", "-y", "onnxruntime"),
        (
            "install",
            "--upgrade",
            gpu.GPU_RUNTIME_REQUIREMENT,
            *gpu.GPU_RUNTIME_PYTHON_DEPENDENCIES,
        ),
    ]
    assert status.ready is True


def test_install_gpu_runtime_restores_cpu_when_gpu_install_fails(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    calls: list[tuple[str, ...]] = []
    versions = {"onnxruntime": "1.19.2", gpu.GPU_RUNTIME_PACKAGE: None}

    monkeypatch.setattr(gpu, "_installed_version", lambda name: versions.get(name))

    def fake_run_pip(arguments, *, timeout=900):
        calls.append(tuple(arguments))
        if arguments[0] == "uninstall":
            versions["onnxruntime"] = None
        elif arguments[0] == "install" and gpu.GPU_RUNTIME_REQUIREMENT in arguments:
            raise RuntimeError("network unavailable")
        else:
            versions["onnxruntime"] = "1.19.2"

    monkeypatch.setattr(gpu, "_run_pip", fake_run_pip)

    with pytest.raises(RuntimeError, match="network unavailable"):
        gpu.install_gpu_runtime()

    assert calls == [
        ("uninstall", "-y", "onnxruntime"),
        (
            "install",
            "--upgrade",
            gpu.GPU_RUNTIME_REQUIREMENT,
            *gpu.GPU_RUNTIME_PYTHON_DEPENDENCIES,
        ),
        ("install", "onnxruntime==1.19.2"),
    ]


def test_probe_failure_is_exposed_in_status(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gpu, "_installed_version", lambda _name: None)
    monkeypatch.setattr(gpu, "_probe_onnxruntime", lambda: ((), "onnxruntime import failed"))

    status = gpu.check_gpu_runtime()

    assert status.available_providers == ()
    assert status.probe_error == "onnxruntime import failed"
    assert status.ready is False


def test_decode_process_output_handles_utf16_and_ansi() -> None:
    raw = "\x1b[31mCUDA DLL error 126\x1b[0m".encode("utf-16")

    assert gpu._decode_process_output(raw) == "CUDA DLL error 126"


def test_probe_diagnostic_is_preserved(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(gpu, "_installed_version", lambda _name: "1.19.2")
    monkeypatch.setattr(
        gpu,
        "_probe_onnxruntime",
        lambda: (
            ("CUDAExecutionProvider", "CPUExecutionProvider"),
            None,
            ("CPUExecutionProvider",),
            None,
            "stderr: LoadLibrary failed with error 126",
        ),
    )

    status = gpu.check_gpu_runtime()

    assert status.diagnostic == "stderr: LoadLibrary failed with error 126"
    assert status.ready is False
