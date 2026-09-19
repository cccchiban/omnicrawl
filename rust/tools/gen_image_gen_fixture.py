#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 图像生成工具的对照数据集。

期望值来自 Python 真实现 `omnicrawl/media/image_gen.py` 的 `ImageGenerator`：用注入的桩客户端
（`client_factory`）返回固定的图片数据，因此不访问网络即可对照——结果文本、落盘文件名与文件内容。

文件名与输出文本里的时间戳会规范化成 `{STAMP}`，工作区路径规范化成 `{WORKSPACE}`。

用法（仓库根目录）：

    python rust/tools/gen_image_gen_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test image_gen_parity
"""

from __future__ import annotations

import base64
import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/image_gen_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.config.features.image_gen import ImageGenConfiguration  # noqa: E402
from omnicrawl.media.image_gen import ImageGenError, ImageGenerator  # noqa: E402

if not Path(sys.modules["omnicrawl.media.image_gen"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

STAMP_PATTERN = re.compile(r"image_\d{8}_\d{6}_")
WORKSPACE_PLACEHOLDER = "{WORKSPACE}"
STAMP_PLACEHOLDER = "{STAMP}"

PNG_BYTES = b"png-bytes-1"
JPEG_BYTES = b"jpeg-bytes-22"
REFERENCE_BYTES = b"\x89PNG\r\n\x1a\nreference"


def b64(payload: bytes) -> str:
    return base64.b64encode(payload).decode("ascii")


class _Recorder:
    def __init__(self, items, error: Exception | None = None) -> None:
        self.items = items
        self.error = error
        self.calls: list[dict] = []


class _Images:
    def __init__(self, recorder: _Recorder) -> None:
        self.recorder = recorder

    def generate(self, **kwargs):
        self.recorder.calls.append({"op": "generate", **{k: v for k, v in kwargs.items()}})
        if self.recorder.error is not None:
            raise self.recorder.error
        return SimpleNamespace(data=self.recorder.items)

    def edit(self, **kwargs):
        image = kwargs.pop("image", None)
        recorded = dict(kwargs)
        recorded["image_name"] = getattr(image, "name", None)
        self.recorder.calls.append({"op": "edit", **recorded})
        if self.recorder.error is not None:
            raise self.recorder.error
        return SimpleNamespace(data=self.recorder.items)


class _Client:
    def __init__(self, recorder: _Recorder) -> None:
        self.images = _Images(recorder)
        self.recorder = recorder


def case(
    name: str,
    *,
    arguments: dict,
    items: list[dict],
    configuration: dict | None = None,
    error: Exception | None = None,
    compare: str = "exact",
    prefix: str | None = None,
    status: int = 200,
    response_body: str | None = None,
) -> dict:
    """跑一条用例：构造配置与桩客户端，执行后收集输出与落盘文件。"""

    workdir = Path(tempfile.mkdtemp(prefix="omnicrawl-image-gen-"))
    try:
        root = workdir.resolve()
        (root / "ref.png").write_bytes(REFERENCE_BYTES)
        settings = {
            "enabled": True,
            "base_url": "https://api.example/v1",
            "api_key": "test-key",
            "api_key_env": "OMNICRAWL_TUI_IMAGE_GEN_FIXTURE",
            "model": "gpt-image-2",
            "size": "auto",
            "quality": "auto",
            "output_format": "png",
            "n": 1,
        }
        settings.update(configuration or {})
        configuration_value = ImageGenConfiguration(**settings)

        recorder = _Recorder(
            [SimpleNamespace(**item) for item in items], error=error
        )
        generator = ImageGenerator(
            configuration=configuration_value,
            client_factory=lambda **_kwargs: _Client(recorder),
        )

        resolved_arguments = {
            key: (value.replace(WORKSPACE_PLACEHOLDER, str(root)) if isinstance(value, str) else value)
            for key, value in arguments.items()
        }
        # Python 的默认输出目录 `.omnicrawl/.agent_tmp/images` 是相对进程 cwd 的：
        # 用例执行期间切到临时工作区，避免把图片写进仓库。
        original_cwd = os.getcwd()
        try:
            os.chdir(root)
            try:
                output = generator.run(resolved_arguments)
                ok = True
            except ImageGenError as exc:
                output = str(exc)
                ok = False
            except RuntimeError as exc:
                output = str(exc)
                ok = False
        finally:
            os.chdir(original_cwd)

        files = []
        for path in sorted(root.rglob("*")):
            if not path.is_file() or path == root / "ref.png":
                continue
            relative = path.relative_to(root).as_posix()
            files.append(
                {
                    "path": STAMP_PATTERN.sub(f"image_{STAMP_PLACEHOLDER}_", relative),
                    "base64": base64.b64encode(path.read_bytes()).decode("ascii"),
                }
            )

        return {
            "name": name,
            "arguments": arguments,
            "config": settings,
            "compare": compare,
            "prefix": prefix,
            "status": status,
            "response_body": response_body,
            "data": items,
            "ok": ok,
            "output": STAMP_PATTERN.sub(f"image_{STAMP_PLACEHOLDER}_", output).replace(
                str(root), WORKSPACE_PLACEHOLDER
            ),
            "files": files,
            "calls": recorder.calls,
        }
    finally:
        shutil.rmtree(workdir, ignore_errors=True)


def main() -> int:
    cases = [
        case(
            "generate-single",
            arguments={"prompt": "一只猫"},
            items=[{"b64_json": b64(PNG_BYTES), "output_format": "png", "url": None}],
        ),
        case(
            "generate-two-jpeg",
            arguments={"prompt": "两只猫", "n": 2, "output_format": "jpeg"},
            items=[
                {"b64_json": b64(PNG_BYTES), "output_format": "jpeg", "url": None},
                {"b64_json": b64(JPEG_BYTES), "output_format": "jpeg", "url": None},
            ],
        ),
        case(
            "generate-into-directory",
            arguments={"prompt": "一只猫", "path": "out"},
            items=[{"b64_json": b64(PNG_BYTES), "output_format": "png", "url": None}],
        ),
        case(
            "generate-explicit-file-name",
            arguments={"prompt": "两只猫", "path": "pic.png", "n": 2},
            items=[
                {"b64_json": b64(PNG_BYTES), "output_format": "png", "url": None},
                {"b64_json": b64(JPEG_BYTES), "output_format": "png", "url": None},
            ],
        ),
        case(
            "edit-with-reference",
            arguments={
                "prompt": "把背景涂成蓝色",
                "image": f"{WORKSPACE_PLACEHOLDER}/ref.png",
            },
            items=[{"b64_json": b64(PNG_BYTES), "output_format": "png", "url": None}],
        ),
        case(
            "missing-prompt",
            arguments={"prompt": "   "},
            items=[],
        ),
        case(
            "disabled",
            arguments={"prompt": "一只猫"},
            items=[],
            configuration={"enabled": False},
            compare="prefix",
            prefix="图像生成未启用：",
        ),
        case(
            "missing-api-key",
            arguments={"prompt": "一只猫"},
            items=[],
            configuration={"api_key": "", "api_key_env": "OMNICRAWL_TUI_IMAGE_GEN_FIXTURE"},
            compare="prefix",
            prefix="缺少 API Key：",
        ),
        case(
            "edit-missing-file",
            arguments={"prompt": "编辑", "image": "missing.png"},
            items=[],
        ),
        case(
            "empty-data",
            arguments={"prompt": "一只猫"},
            items=[],
        ),
        case(
            "no-base64-no-url",
            arguments={"prompt": "一只猫"},
            items=[{"b64_json": None, "output_format": "png", "url": None}],
        ),
        case(
            "request-failed",
            arguments={"prompt": "一只猫"},
            items=[],
            error=ImageGenError("boom"),
            compare="prefix",
            prefix="图像生成请求失败：",
            # Rust 侧没有 SDK 异常：同一场景用 HTTP 500 + 响应体模拟。
            status=500,
            response_body="boom",
        ),
    ]

    data = {
        "source": "omnicrawl/media/image_gen.py + omnicrawl/config/features/image_gen.py",
        "workspace_placeholder": WORKSPACE_PLACEHOLDER,
        "stamp_placeholder": STAMP_PLACEHOLDER,
        "reference_base64": base64.b64encode(REFERENCE_BYTES).decode("ascii"),
        "cases": cases,
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写入 {FIXTURE_PATH}（{len(cases)} 例）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
