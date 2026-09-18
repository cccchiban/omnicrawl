#!/usr/bin/env python3
"""生成会话目录扫描的对照数据集，供 Rust 侧 `omnicrawl-session` 的 parity 测试使用。

期望值来自 Python 真实现 `omnicrawl/state/session_consistency.py` 的 `discover_transcripts`
与 `discover_artifact_session_ids`：只在 `sessions/`、`archive/` 一级目录里认符合会话 id 命名
规则的文件，artifact 只认像 session_id 的一级子目录。

用法：``python rust/tools/gen_session_scan_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/session_scan_parity.json``
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/session_scan_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.state import session_consistency as C  # noqa: E402

if not Path(C.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{C.__file__}")

GOOD_ID = "20260918-030529-abcdef"
ARCHIVED_ID = "20260917-010101-012345"
SECOND_ID = "20260916-030529-abcdef"
# 大写十六进制单独用一天：Windows 文件系统大小写不敏感，同一天的大小写两份会被当成同一个文件。
UPPER_ID = "20260915-030529-ABCDEF"

# 目录树：以 "/" 结尾的是目录；其余是文件（内容与扫描无关，写空文件即可）。
TREE = [
    f"sessions/{GOOD_ID}.jsonl",
    f"sessions/{SECOND_ID}.jsonl",
    f"sessions/{UPPER_ID}.jsonl",
    f"archive/{ARCHIVED_ID}.jsonl",
    "sessions/20260918-030529-abcde.jsonl",
    "sessions/2026-09-18-030529-abcdef.jsonl",
    f"sessions/export-{GOOD_ID}.jsonl",
    "sessions/notes.txt",
    f"archive/nested/{GOOD_ID}.jsonl",
    f"artifacts/{GOOD_ID}/",
    f"artifacts/{UPPER_ID}/",
    "artifacts/20260918-030529-abcde/",
    "artifacts/not-a-session-id/",
    f"artifacts/{GOOD_ID}.txt",
    "artifacts/nested/deeper/",
]


def build_tree(root: Path) -> None:
    for item in TREE:
        target = root / item.rstrip("/")
        if item.endswith("/"):
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("", encoding="utf-8")


def main() -> None:
    with tempfile.TemporaryDirectory() as workspace:
        sandbox = Path(workspace)
        build_tree(sandbox)

        transcripts = C.discover_transcripts(sandbox)
        artifacts = C.discover_artifact_session_ids(sandbox / "artifacts")

    payload = {
        "source": "omnicrawl/state/session_consistency.py",
        "tree": TREE,
        "expected": {
            # 绝对路径不进数据集：Python 的 Path.resolve() 与 Rust canonicalize() 在
            # Windows 上前缀写法不同，两边只对「会话 id + 相对路径」这个契约负责。
            "transcripts": [
                {"session_id": item.session_id, "relative_path": item.relative_path}
                for item in transcripts
            ],
            "artifact_session_ids": artifacts,
        },
    }
    FIXTURE_PATH.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"已写出 {FIXTURE_PATH.relative_to(ROOT)}")
    print(f"转录 {len(transcripts)} 条，artifact 会话 {len(artifacts)} 个")


if __name__ == "__main__":
    main()
