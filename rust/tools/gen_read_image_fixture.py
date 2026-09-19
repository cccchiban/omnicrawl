#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 图片读取工具的对照数据集。

期望值来自 Python 真实现 `omnicrawl/agent/toolkit/image_tools.py` 的 `read_image_file`：
成功时对照紧凑 JSON 载荷与视觉附件（MIME、文件名、detail、Base64），失败时对照错误文案。

工作区根在两侧不同（临时目录），因此数据集把根路径规范化成 `{WORKSPACE}`；平台相关的
绝对路径用例改用包含比对。

用法（仓库根目录）：

    python rust/tools/gen_read_image_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test read_image_parity
"""

from __future__ import annotations

import base64
import json
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/read_image_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.toolkit.image_tools import read_image_file  # noqa: E402

if not Path(sys.modules["omnicrawl.agent.toolkit.image_tools"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

WORKSPACE_PLACEHOLDER = "{WORKSPACE}"

PNG = b"\x89PNG\r\n\x1a\n0000"
JPEG = b"\xff\xd8\xff\xe0data"
GIF = b"GIF89a...."
WEBP = b"RIFF\x00\x00\x00\x00WEBPVP8 "

FILES = {
    "pic.png": PNG,
    "photo.jpg": JPEG,
    "anim.gif": GIF,
    "shot.webp": WEBP,
    "notes.txt": b"plain text",
}

CASES: list[tuple[dict, str]] = [
    ({"path": "pic.png", "prompt": "描述图片"}, "exact"),
    ({"path": "photo.jpg", "prompt": "描述图片", "detail": "low"}, "exact"),
    ({"path": "anim.gif", "prompt": "x", "detail": "high"}, "exact"),
    ({"path": "shot.webp", "prompt": "x", "detail": "AUTO"}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": "huge"}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": 5}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": False}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": 0}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": ""}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": []}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": {}}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": None}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": [1]}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": {"a": 1}}, "exact"),
    ({"path": "pic.png", "prompt": "x", "detail": "LOW"}, "exact"),
    ({"path": "pic.png"}, "exact"),
    ({"path": "pic.png", "prompt": "   "}, "exact"),
    ({}, "exact"),
    ({"path": 5, "prompt": "x"}, "exact"),
    ({"path": "https://example.com/a.png", "prompt": "x"}, "exact"),
    ({"path": "DATA:image/png;base64,AAAA", "prompt": "x"}, "exact"),
    ({"path": "missing.png", "prompt": "x"}, "exact"),
    ({"path": "../outside.png", "prompt": "x"}, "exact"),
    ({"path": "notes.txt", "prompt": "x"}, "exact"),
]


def main() -> int:
    workdir = Path(tempfile.mkdtemp(prefix="omnicrawl-image-fixture-"))
    try:
        root = workdir.resolve()
        for name, payload in FILES.items():
            (root / name).write_bytes(payload)
        absolute = root / "pic.png"

        cases = []
        for arguments, compare in CASES:
            result = read_image_file(dict(arguments), workspace_root=root)
            entry: dict = {
                "arguments": arguments,
                "ok": bool(result.ok),
                "output": result.output.replace(str(root), WORKSPACE_PLACEHOLDER),
                "compare": compare,
                "images": [
                    {
                        "media_type": image.media_type,
                        "filename": image.filename,
                        "detail": image.detail,
                        "data_base64": image.data_base64,
                    }
                    for image in result.model_images
                ],
            }
            cases.append(entry)

        absolute_arguments = {"path": str(absolute), "prompt": "绝对路径"}
        absolute_result = read_image_file(absolute_arguments, workspace_root=root)
        cases.append(
            {
                # 生成时的临时目录两侧不同：参数里改存占位符，由测试替换成本地工作区路径。
                "arguments": {
                    "path": f"{WORKSPACE_PLACEHOLDER}/pic.png",
                    "prompt": "绝对路径",
                },
                "ok": bool(absolute_result.ok),
                "output": absolute_result.output.replace(str(root), WORKSPACE_PLACEHOLDER),
                "compare": "exact",
                "images": [
                    {
                        "media_type": image.media_type,
                        "filename": image.filename,
                        "detail": image.detail,
                        "data_base64": image.data_base64,
                    }
                    for image in absolute_result.model_images
                ],
            }
        )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    data = {
        "source": "omnicrawl/agent/toolkit/image_tools.py",
        "workspace_placeholder": WORKSPACE_PLACEHOLDER,
        "files": [
            {"path": name, "base64": base64.b64encode(payload).decode("ascii")}
            for name, payload in FILES.items()
        ],
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
