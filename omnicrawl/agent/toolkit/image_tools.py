"""本地图片读取工具。

该模块只负责从本机文件系统读取常见栅格图片，并把图片封装为 Agent 当前
工具循环使用的 ``ToolImageAttachment``。它不执行网络请求，也不把图片内容
写入 Session；路径权限由调用方根据 Agent 当前工作区和用户配置决定。
"""

from __future__ import annotations

import base64
import json
from pathlib import Path
from typing import Any

from ..types import ToolImageAttachment, ToolResult


_IMAGE_SIGNATURES: tuple[tuple[bytes, str], ...] = (
    (b"\x89PNG\r\n\x1a\n", "image/png"),
    (b"GIF87a", "image/gif"),
    (b"GIF89a", "image/gif"),
    (b"\xff\xd8\xff", "image/jpeg"),
)
_WEBP_RIFF_PREFIX = b"RIFF"
_WEBP_FORMAT_MARKER = b"WEBP"
_SUPPORTED_MEDIA_TYPES = frozenset({"image/png", "image/jpeg", "image/webp", "image/gif"})


class ImageToolError(RuntimeError):
    """本地图片读取、路径或格式校验失败。"""


def read_image_file(
    arguments: dict[str, Any],
    *,
    workspace_root: Path,
) -> ToolResult:
    """读取本地图片并返回视觉模型附件。

    ``path`` 可以是当前工作区相对路径，也可以是本机绝对路径；``prompt`` 是
    本次图片分析的显式提示词，由视觉结果路由与图片一起交给视觉模型。按照用户
    确认的策略，这里不人为限制文件大小或像素尺寸；图片会完整读入内存并 Base64
    编码，因此调用方应意识到超大文件可能造成明显内存占用和模型请求延迟。
    """

    try:
        raw_path = arguments.get("path")
        if not isinstance(raw_path, str) or not raw_path.strip():
            raise ImageToolError("path 必须是非空的本地图片路径。")
        prompt = arguments.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise ImageToolError("prompt 必须是非空的图片分析提示词。")
        detail = _read_detail(arguments.get("detail", "auto"))
        path = _resolve_image_path(raw_path, workspace_root=workspace_root)
        image_bytes = path.read_bytes()
        media_type = _detect_media_type(path, image_bytes)
    except ImageToolError as exc:
        return ToolResult(ok=False, output=str(exc))
    except FileNotFoundError:
        return ToolResult(ok=False, output=f"图片文件不存在：{raw_path}")
    except PermissionError:
        return ToolResult(ok=False, output=f"没有权限读取图片：{raw_path}")
    except OSError as exc:
        return ToolResult(ok=False, output=f"读取图片失败：{raw_path}，{exc}")

    display_path = _display_image_path(path, workspace_root)
    payload = {
        "path": display_path,
        "media_type": media_type,
        "bytes": len(image_bytes),
        "detail": detail,
        "vision_attachment": True,
    }
    return ToolResult(
        ok=True,
        output=json.dumps(payload, ensure_ascii=False, separators=(",", ":")),
        ui_artifact={
            "type": "image",
            "title": "读取图片",
            "path": display_path,
            "media_type": media_type,
            "bytes": len(image_bytes),
        },
        model_images=(
            ToolImageAttachment(
                media_type=media_type,
                data_base64=base64.b64encode(image_bytes).decode("ascii"),
                filename=path.name,
                detail=detail,
            ),
        ),
    )


def _resolve_image_path(raw_path: str, *, workspace_root: Path) -> Path:
    """将相对路径绑定到当前工作区，同时保留绝对本地路径能力。"""

    candidate_text = raw_path.strip()
    lowered = candidate_text.casefold()
    if "://" in lowered or lowered.startswith("data:"):
        raise ImageToolError("read_image 只支持本机文件路径，不支持 URL 或 data URI。")

    candidate = Path(candidate_text).expanduser()
    was_absolute = candidate.is_absolute()
    if not was_absolute:
        candidate = workspace_root / candidate
    try:
        resolved = candidate.resolve(strict=False)
    except OSError as exc:
        raise ImageToolError(f"图片路径无法解析：{candidate_text}，{exc}") from exc
    if not was_absolute:
        try:
            resolved.relative_to(workspace_root.resolve())
        except ValueError as exc:
            raise ImageToolError("相对图片路径不能越出当前工作区；请使用明确的本机绝对路径。") from exc
    if not resolved.is_file():
        raise ImageToolError(f"图片路径不是文件：{candidate_text}")
    return resolved


def _display_image_path(path: Path, workspace_root: Path) -> str:
    try:
        return path.relative_to(workspace_root.resolve()).as_posix()
    except ValueError:
        return str(path)


def _detect_media_type(path: Path, image_bytes: bytes) -> str:
    for signature, media_type in _IMAGE_SIGNATURES:
        if image_bytes.startswith(signature):
            return media_type
    if (
        len(image_bytes) >= 12
        and image_bytes[:4] == _WEBP_RIFF_PREFIX
        and image_bytes[8:12] == _WEBP_FORMAT_MARKER
    ):
        return "image/webp"
    supported = "、".join(sorted(_SUPPORTED_MEDIA_TYPES))
    raise ImageToolError(
        f"文件不是受支持的图片格式：{path}。支持的 MIME 类型：{supported}。"
    )


def _read_detail(value: Any) -> str:
    detail = str(value or "auto").strip().casefold()
    if detail not in {"auto", "low", "high"}:
        raise ImageToolError("detail 仅支持 auto、low 或 high。")
    return detail


__all__ = ["ImageToolError", "read_image_file"]
