"""Hugging Face 模型下载与模型目录管理（urllib 实现，无 huggingface_hub 依赖）。

部分网络环境 pip 无法安装 huggingface_hub（SSL 受限），但 urllib 可直连
Hugging Face CDN。本模块实现等价下载能力：枚举仓库文件 + 逐个流式下载到磁盘，
并承担模型目录的发现/就绪判断/下载（``ensure_model_dir``）。

本模块只依赖标准库与 :mod:`omnicrawl.tts.config`，**不依赖 numpy、
sentencepiece、onnxruntime**——模型下载与推理引擎解耦，缺失引擎依赖时
设置页仍可下载模型。
"""

from __future__ import annotations

import fnmatch
import json
import logging
import shutil
import urllib.request
from pathlib import Path
from typing import Any, Callable, Sequence

from .config import resolve_model_dir

LOGGER = logging.getLogger(__name__)

HF_API_BASE = "https://huggingface.co/api/models"
HF_RESOLVE_BASE = "https://huggingface.co/{repo}/resolve/main/{path}"

# 官方 ONNX 模型仓库。
TTS_REPO_ID = "OpenMOSS-Team/MOSS-TTS-Nano-100M-ONNX"
CODEC_REPO_ID = "OpenMOSS-Team/MOSS-Audio-Tokenizer-Nano-ONNX"

MANIFEST_CANDIDATE_RELATIVE_PATHS = (
    "browser_poc_manifest.json",
    "MOSS-TTS-Nano-100M-ONNX/browser_poc_manifest.json",
    "MOSS-TTS-Nano-ONNX-CPU/browser_poc_manifest.json",
)
TTS_LAYOUT_REQUIRED_NAMES = (
    "browser_poc_manifest.json",
    "tts_browser_onnx_meta.json",
    "tokenizer.model",
)
CODEC_LAYOUT_REQUIRED_NAMES = ("codec_browser_onnx_meta.json",)

# 下载重试次数（网络抖动重试）。
DOWNLOAD_RETRIES = 3
# 单文件下载进度日志间隔（MB）。
PROGRESS_LOG_INTERVAL_MB = 50.0


class _FollowRedirects(urllib.request.HTTPRedirectHandler):
    """Python 3.9 的 urllib 不自动跟随 308，这里补上。"""

    http_error_308 = urllib.request.HTTPRedirectHandler.http_error_302


def _opener() -> urllib.request.OpenerDirector:
    opener = urllib.request.build_opener(_FollowRedirects)
    opener.addheaders = [("User-Agent", "omnicrawl-tts")]
    return opener


def list_repo_files(repo_id: str) -> list[dict[str, Any]]:
    """列出仓库全部文件（含大小），通过 HF API。"""
    url = f"{HF_API_BASE}/{repo_id}/tree/main?recursive=true&expand=true"
    with _opener().open(url, timeout=60) as response:
        entries = json.load(response)
    files: list[dict[str, Any]] = []
    for entry in entries:
        path = str(entry.get("path") or "")
        if not path or path.endswith("/"):
            continue
        if entry.get("type") == "directory":
            continue
        files.append({"path": path, "size": int(entry.get("size") or 0)})
    return files


def _matches_allow_patterns(path: str, allow_patterns: Sequence[str]) -> bool:
    filename = path.rsplit("/", 1)[-1]
    for pattern in allow_patterns:
        if fnmatch.fnmatch(filename, pattern):
            return True
    return False


def _download_file(
    url: str,
    destination: Path,
    *,
    expected_size: int = 0,
    progress_callback: Callable[[int, int], None] | None = None,
) -> None:
    """流式下载单个文件到磁盘，带重试、进度日志与可选进度回调。

    ``progress_callback(done_bytes, total_bytes)`` 每写一个 1MB 块触发一次；
    ``total_bytes`` 未知时为 0，调用方据此决定是否显示百分比。
    """
    last_error: Exception | None = None
    for attempt in range(1, DOWNLOAD_RETRIES + 1):
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "omnicrawl-tts"})
            with _opener().open(request, timeout=300) as response:
                destination.parent.mkdir(parents=True, exist_ok=True)
                partial = destination.with_suffix(destination.suffix + ".part")
                received = 0
                last_log_mb = 0.0
                with open(str(partial), "wb") as out_file:
                    while True:
                        chunk = response.read(1 << 20)
                        if not chunk:
                            break
                        out_file.write(chunk)
                        received += len(chunk)
                        if progress_callback is not None:
                            try:
                                progress_callback(received, expected_size)
                            except Exception:  # noqa: BLE001 - 进度回调失败不影响下载
                                pass
                        received_mb = received / 1e6
                        if received_mb - last_log_mb >= PROGRESS_LOG_INTERVAL_MB:
                            last_log_mb = received_mb
                            LOGGER.info(
                                "  %s 已下载 %.0f MB%s",
                                destination.name,
                                received_mb,
                                f"/{expected_size / 1e6:.0f} MB" if expected_size else "",
                            )
                if partial.stat().st_size != received:
                    raise RuntimeError("下载字节数与写入不一致")
                partial.replace(destination)
                LOGGER.info(
                    "  %s 完成（%.1f MB）",
                    destination.name,
                    received / 1e6,
                )
                return
        except Exception as exc:  # noqa: BLE001 - 网络错误需重试
            last_error = exc
            LOGGER.warning("下载失败（第 %d/%d 次）：%s", attempt, DOWNLOAD_RETRIES, exc)
    raise RuntimeError(f"下载失败：{url}（{last_error}）")


def download_repo(
    repo_id: str,
    local_dir: str | Path,
    *,
    allow_patterns: Sequence[str],
    progress_callback: Callable[[int, int], None] | None = None,
) -> None:
    """下载仓库中匹配 allow_patterns 的全部文件到 local_dir。

    ``progress_callback(done_bytes, total_bytes)`` 按仓库累计进度触发：
    done 为已下载字节数（含先前文件），total 为该仓库匹配文件总字节数。
    """
    local_dir = Path(local_dir)
    files = list_repo_files(repo_id)
    matched = [entry for entry in files if _matches_allow_patterns(entry["path"], allow_patterns)]
    if not matched:
        raise RuntimeError(f"仓库 {repo_id} 中没有匹配 {allow_patterns} 的文件。")
    total_bytes = sum(int(entry["size"]) for entry in matched)
    LOGGER.info(
        "开始下载 %s：%d 个文件，共 %.0f MB",
        repo_id,
        len(matched),
        total_bytes / 1e6,
    )
    base_done = 0
    for entry in matched:
        path = str(entry["path"])
        url = HF_RESOLVE_BASE.format(repo=repo_id, path=path.replace(" ", "%20"))
        destination = local_dir / path
        file_size = int(entry["size"])

        def _report_progress(done_in_file: int, _total: int) -> None:
            if progress_callback is not None:
                try:
                    progress_callback(base_done + done_in_file, total_bytes)
                except Exception:  # noqa: BLE001 - 进度回调失败不影响下载
                    pass

        _download_file(
            url,
            destination,
            expected_size=file_size,
            progress_callback=_report_progress,
        )
        base_done += file_size


# ---------------------------------------------------------------------------
# 模型目录发现 / 布局归一化 / 就绪判断 / 下载
# ---------------------------------------------------------------------------


def _directory_contains_all(parent: Path, required_names: Sequence[str]) -> bool:
    return all((parent / name).exists() for name in required_names)


def _find_directory_with_required_names(
    root_dir: Path, required_names: Sequence[str]
) -> Path | None:
    if not root_dir.exists():
        return None
    if _directory_contains_all(root_dir, required_names):
        return root_dir
    sentinel_name = str(required_names[0])
    for candidate in root_dir.rglob(sentinel_name):
        parent = candidate.parent
        if _directory_contains_all(parent, required_names):
            return parent
    return None


def _promote_directory_contents(source_dir: Path, target_dir: Path) -> None:
    """把子目录内容提升到目标目录（HF 快照下载可能多套一层目录）。"""
    if source_dir.resolve() == target_dir.resolve():
        return
    target_dir.mkdir(parents=True, exist_ok=True)
    for child in source_dir.iterdir():
        destination = target_dir / child.name
        if destination.exists():
            continue
        shutil.move(str(child), str(destination))


def _normalize_download_layout(target_dir: Path, required_names: Sequence[str]) -> None:
    candidate_dir = _find_directory_with_required_names(target_dir, required_names)
    if candidate_dir is None:
        return
    _promote_directory_contents(candidate_dir, target_dir)


def _snapshot_download_repo(
    *,
    repo_id: str,
    local_dir: Path,
    allow_patterns: Sequence[str],
    progress_callback: Callable[[int, int], None] | None = None,
) -> None:
    """下载仓库：优先 huggingface_hub（若可用），否则用内置 urllib 下载器。"""
    local_dir.mkdir(parents=True, exist_ok=True)
    try:
        from huggingface_hub import snapshot_download
    except ModuleNotFoundError:
        LOGGER.info(
            "huggingface_hub 不可用，使用内置 urllib 下载器（%s）",
            repo_id,
        )
        download_repo(
            repo_id,
            local_dir,
            allow_patterns=allow_patterns,
            progress_callback=progress_callback,
        )
        return
    snapshot_download(
        repo_id=repo_id,
        local_dir=str(local_dir),
        local_dir_use_symlinks=False,
        allow_patterns=list(allow_patterns),
    )


def _find_manifest_path(model_dir: Path) -> Path | None:
    for relative_path in MANIFEST_CANDIDATE_RELATIVE_PATHS:
        candidate = (model_dir / relative_path).resolve()
        if candidate.is_file():
            return candidate
    return None


def builtin_voice_names(model_dir: str | Path | None = None) -> list[str]:
    """读取模型 manifest 中的内置音色名（不加载 ONNX session，轻量）。

    模型缺失时返回空列表。
    """

    resolved = resolve_model_dir(model_dir)
    manifest_path = _find_manifest_path(resolved)
    if manifest_path is None:
        return []
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        voices = manifest.get("builtin_voices") or []
        return [str(row.get("voice", "")) for row in voices if row.get("voice")]
    except Exception:  # noqa: BLE001 - 读取失败视为模型不可用
        return []


def models_ready(model_dir: str | Path | None = None) -> bool:
    """模型目录是否已就绪（存在 manifest）。"""

    return _find_manifest_path(resolve_model_dir(model_dir)) is not None


def ensure_model_dir(
    model_dir: str | Path | None = None,
    *,
    progress_callback: Callable[[str, int, int], None] | None = None,
) -> Path:
    """确保模型目录就绪：已有 manifest 直接返回；缺失时自动下载。

    - `model_dir` 为 None 时使用默认目录（`~/.omnicrawl/tts/models`），缺失则自动
      从 Hugging Face 下载两个 ONNX 仓库。
    - 显式传入的 `model_dir` 缺失时抛 FileNotFoundError，不做自动下载。
    - ``progress_callback(label, done_bytes, total_bytes)`` 可选：两个仓库依次
      下载时按仓库上报进度（label 为仓库名，total 为 0 表示大小未知）。
    """
    resolved = resolve_model_dir(model_dir)
    manifest_path = _find_manifest_path(resolved)
    if manifest_path is not None:
        return resolved

    if model_dir is not None:
        tried_paths = [str((resolved / item).resolve()) for item in MANIFEST_CANDIDATE_RELATIVE_PATHS]
        raise FileNotFoundError(
            "指定的模型目录中未找到 browser_poc_manifest.json。已尝试："
            + ", ".join(tried_paths)
            + "。可省略 model_dir 使用默认目录自动下载，或手动放置模型。"
        )

    LOGGER.info("模型目录 %s 缺失，从 Hugging Face 自动下载。", resolved)
    LOGGER.info("TTS 仓库：%s", TTS_REPO_ID)
    LOGGER.info("Codec 仓库：%s", CODEC_REPO_ID)

    def _repo_progress(label: str) -> Callable[[int, int], None] | None:
        if progress_callback is None:
            return None
        return lambda done, total: progress_callback(label, done, total)

    _snapshot_download_repo(
        repo_id=TTS_REPO_ID,
        local_dir=resolved / "MOSS-TTS-Nano-100M-ONNX",
        allow_patterns=("*.onnx", "*.data", "*.json", "tokenizer.model"),
        progress_callback=_repo_progress("TTS 模型（673MB）"),
    )
    _snapshot_download_repo(
        repo_id=CODEC_REPO_ID,
        local_dir=resolved / "MOSS-Audio-Tokenizer-Nano-ONNX",
        allow_patterns=("*.onnx", "*.data", "*.json"),
        progress_callback=_repo_progress("Codec 模型（91MB）"),
    )
    _normalize_download_layout(resolved / "MOSS-TTS-Nano-100M-ONNX", TTS_LAYOUT_REQUIRED_NAMES)
    _normalize_download_layout(resolved / "MOSS-Audio-Tokenizer-Nano-ONNX", CODEC_LAYOUT_REQUIRED_NAMES)

    manifest_path = _find_manifest_path(resolved)
    if manifest_path is None:
        raise FileNotFoundError(
            "模型已下载但未找到 browser_poc_manifest.json。下载目录：%s" % resolved
        )
    return resolved
