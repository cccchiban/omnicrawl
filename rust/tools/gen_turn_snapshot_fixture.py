#!/usr/bin/env python3
"""生成工作区轮次快照的对照数据集：期望值全部来自 Python 真实现。

每个用例给出一份「动作脚本」（建仓库、写文件、捕获快照、回退、读文件），脚本真跑一遍
Python 的 `WorktreeSnapshotStore`，记录每步结果：快照用补丁的 sha256 + 未跟踪清单表示，
回退记返回值或错误文案。两侧执行同一份脚本，因此比的是行为而不是我对语义的理解。
"""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-session/tests/fixtures/turn_snapshot_parity.json"

if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from omnicrawl.state import turn_snapshot as snapshot_module  # noqa: E402
from omnicrawl.state.turn_snapshot import (  # noqa: E402
    SnapshotError,
    WorktreeSnapshotStore,
)

GIT_IDENTITY = ["-c", "user.email=parity@example.com", "-c", "user.name=parity",
                "-c", "commit.gpgsign=false"]
WORK_ROOT = (
    Path(tempfile.gettempdir()) / f"omnicrawl-snapshot-parity-{__import__('os').getpid()}"
).resolve()


def case(label: str, setup: list, steps: list) -> dict:
    return {"label": label, "setup": setup, "steps": steps}


INIT_REPO = [
    {"op": "git", "args": ["init", "-q"]},
    {"op": "write", "path": "a.txt", "content": "one\n"},
    {"op": "write", "path": "keep.txt", "content": "keep\n"},
    {"op": "git", "args": ["add", "-A"]},
    {"op": "git", "args": [*GIT_IDENTITY, "commit", "-q", "-m", "init"]},
]

CASES = [
    case(
        "roundtrip",
        INIT_REPO,
        [
            {"op": "capture", "save": "start"},
            {"op": "write", "path": "a.txt", "content": "two\n"},
            {"op": "write", "path": "new.txt", "content": "new\n"},
            {"op": "capture", "save": "end"},
            {"op": "transition", "expected": "end", "target": "start"},
            {"op": "read", "path": "a.txt"},
            {"op": "read", "path": "new.txt"},
            {"op": "untracked"},
        ],
    ),
    case(
        "conflict_after_end",
        INIT_REPO,
        [
            {"op": "capture", "save": "start"},
            {"op": "write", "path": "a.txt", "content": "two\n"},
            {"op": "capture", "save": "end"},
            {"op": "write", "path": "a.txt", "content": "three\n"},
            {"op": "transition", "expected": "end", "target": "start"},
            {"op": "read", "path": "a.txt"},
        ],
    ),
    case(
        "not_a_repository",
        [{"op": "write", "path": "plain.txt", "content": "plain\n"}],
        [
            {"op": "capture", "save": "start"},
            {"op": "transition", "expected": "start", "target": "start"},
            {"op": "read", "path": "plain.txt"},
        ],
    ),
    case(
        "deleted_untracked",
        INIT_REPO,
        [
            {"op": "write", "path": "gone.txt", "content": "x\n"},
            {"op": "capture", "save": "start"},
            {"op": "delete", "path": "gone.txt"},
            {"op": "capture", "save": "end"},
            {"op": "transition", "expected": "end", "target": "start"},
            {"op": "untracked"},
            {"op": "read", "path": "gone.txt"},
        ],
    ),
    case(
        "clean_workspace",
        [*INIT_REPO, {"op": "write", "path": "a.txt", "content": "second\n"},
         {"op": "git", "args": ["add", "-A"]},
         {"op": "git", "args": [*GIT_IDENTITY, "commit", "-q", "-m", "second"]}],
        [
            {"op": "capture", "save": "head"},
            {"op": "transition", "expected": "head", "target": "head"},
            {"op": "untracked"},
        ],
    ),
]


def run_git(args: list[str], cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=cwd, stdout=subprocess.PIPE,
                   stderr=subprocess.PIPE, check=True)


def write_file(root: Path, relative: str, content: str) -> None:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def run_case(spec: dict) -> dict:
    root = WORK_ROOT / spec["label"]
    if root.exists():
        shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True, exist_ok=True)

    store = WorktreeSnapshotStore()
    snapshots: dict[str, object] = {}
    results: list[dict] = []

    for step in spec["setup"]:
        apply_setup(root, step)

    for step in spec["steps"]:
        kind = step["op"]
        if kind in ("git", "write", "delete"):
            apply_setup(root, step)
            results.append({"op": kind, "ok": True, "value": None})
            continue
        if kind == "capture":
            try:
                snapshot = store.capture(root)
            except SnapshotError as exc:
                results.append({"op": kind, "ok": False, "error": str(exc)})
                continue
            snapshots[step["save"]] = snapshot
            results.append({
                "op": kind,
                "ok": True,
                "has_head": snapshot.has_head,
                "patch_sha256": hashlib.sha256(snapshot.patch).hexdigest(),
                "patch_len": len(snapshot.patch),
                "untracked": list(snapshot.untracked),
            })
        elif kind == "transition":
            expected = snapshots[step["expected"]]
            target = snapshots[step["target"]]
            try:
                missing = store.transition(root, expected=expected, target=target)
            except SnapshotError as exc:
                results.append({
                    "op": kind,
                    "ok": False,
                    "conflict": isinstance(exc, snapshot_module.SnapshotConflictError),
                    "error": str(exc),
                })
                continue
            results.append({"op": kind, "ok": True, "value": list(missing)})
        elif kind == "read":
            path = root / step["path"]
            results.append({
                "op": kind,
                "path": step["path"],
                "content": path.read_text(encoding="utf-8") if path.exists() else None,
            })
        elif kind == "untracked":
            output = subprocess.run(
                ["git", "ls-files", "--others", "--exclude-standard"],
                cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ).stdout.decode("utf-8", errors="replace")
            results.append({
                "op": kind,
                "value": [line for line in output.splitlines() if line.strip()],
            })
        else:
            raise ValueError(f"未知的 op：{kind}")

    return {"label": spec["label"], "setup": spec["setup"], "steps": spec["steps"],
            "results": results}


def apply_setup(root: Path, step: dict) -> None:
    kind = step["op"]
    if kind == "git":
        run_git(step["args"], root)
    elif kind == "write":
        write_file(root, step["path"], step["content"])
    elif kind == "delete":
        (root / step["path"]).unlink()
    else:
        raise ValueError(f"未知的 setup op：{kind}")


def main() -> None:
    module_path = Path(snapshot_module.__file__).resolve()
    if ROOT not in module_path.parents:
        raise SystemExit(f"对照必须跑在仓库内的真实现上：{module_path}")

    if WORK_ROOT.exists():
        shutil.rmtree(WORK_ROOT, ignore_errors=True)
    payload = {
        "source": "omnicrawl/state/turn_snapshot.py",
        "cases": [run_case(spec) for spec in CASES],
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(payload, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"已写入 {FIXTURE_PATH}")
    shutil.rmtree(WORK_ROOT, ignore_errors=True)


if __name__ == "__main__":
    main()
