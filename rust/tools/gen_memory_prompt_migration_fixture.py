#!/usr/bin/env python3
"""生成提示词段落与旧目录迁移的对照数据集。

期望值来自 Python 真实现 `omnicrawl/state/memory.py` 的 `MemoryStore.format_prompt_section`
与 `migrate_legacy_memory`。

迁移会生成记忆 id 与带时间戳的备份目录名，因此两侧统一归一化：
先把 `.migrated-<时间戳>` 里的时间戳换成占位，再把剩下的记忆 id 形状统一换成 `<memory-id>`。

用法：``python rust/tools/gen_memory_prompt_migration_fixture.py``
输出：``rust/crates/omnicrawl-session/tests/fixtures/memory_prompt_migration_parity.json``
"""

from __future__ import annotations

import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = (
    ROOT / "rust/crates/omnicrawl-session/tests/fixtures/memory_prompt_migration_parity.json"
)

sys.path.insert(0, str(ROOT))

from omnicrawl.state import memory as M  # noqa: E402
from omnicrawl.state.memory import MemoryStore, migrate_legacy_memory  # noqa: E402

if not Path(M.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("加载到的不是仓库源码")

TIMESTAMP = "2025-01-02T03:04:05+00:00"
STAMP_PATTERN = re.compile(r"\.migrated-\d{8}-\d{6}(?:-\d{3})?")
ID_PATTERN = re.compile(r"\d{8}-\d{6}(?:-\d{3})?")


def normalize_path(relative: str) -> str:
    return ID_PATTERN.sub("<memory-id>", STAMP_PATTERN.sub(".migrated-<stamp>", relative))


def entry(index: int) -> dict:
    return {
        "id": f"20250102-0304{index:02d}",
        "path": f"task-history/general/20250102-0304{index:02d}.md",
        "storage_directory": "task-history/general",
        "timestamp": TIMESTAMP,
        "touch_count": 0,
        "related_directories": ["task-history/general"],
        "summary": f"第 {index} 条记忆",
    }


def markdown(body: str) -> str:
    return (
        f'---\ntimestamp: "{TIMESTAMP}"\nrelated_directories:\n'
        f'  - "task-history/general"\n---\n\n{body}\n'
    )


def write_tree(root: Path, entries: list[dict], bodies: dict[str, str]) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / "index.json").write_text(
        json.dumps({"memories": entries}, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    for relative, body in bodies.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(markdown(body), encoding="utf-8")


def tree(root: Path) -> dict:
    files = {}
    if not root.exists():
        return files
    for path in sorted(root.rglob("*")):
        if path.is_file():
            files[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
    return files


def prompt_cases() -> list[dict]:
    cases = []
    for name, entries, overrides in [
        ("empty", [], {}),
        ("directories", [entry(5), entry(6), entry(7)], {}),
        ("custom_tools", [entry(5)], {"scope_label": "用户", "search_tool": "s", "read_tool": "r"}),
    ]:
        root = Path(tempfile.mkdtemp(prefix="memory-prompt-"))
        try:
            write_tree(root, [item for item in entries], {})
            text = MemoryStore(root).format_prompt_section(**overrides)
        finally:
            shutil.rmtree(root, ignore_errors=True)
        cases.append({"name": name, "entries": entries, "overrides": overrides, "expected": text})
    return cases


def migration_cases() -> list[dict]:
    cases = []

    # A：目标不存在 -> 直接改名，内容原样保留
    root = Path(tempfile.mkdtemp(prefix="memory-migrate-a-"))
    try:
        source = root / "memory"
        write_tree(source, [entry(1)], {"task-history/general/20250102-030401.md": "旧正文一"})
        source_files = tree(source)
        result = migrate_legacy_memory(source, root / "project" / "memory")
        cases.append(
            {
                "name": "destination_absent",
                "source_files": source_files,
                "destination_files": {},
                "expected": {
                    "migrated": result.migrated,
                    "imported_count": result.imported_count,
                    "has_backup": result.backup_path is not None,
                    "tree": {normalize_path(key): value for key, value in tree(root).items()},
                },
            }
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    # B：目标已存在 -> 导入正文后把源目录改名备份
    root = Path(tempfile.mkdtemp(prefix="memory-migrate-b-"))
    try:
        source = root / "memory"
        destination = root / "project" / "memory"
        write_tree(
            source,
            [entry(1), entry(2), entry(3)],
            {
                "task-history/general/20250102-030401.md": "旧正文一",
                "task-history/general/20250102-030402.md": "旧正文二",
            },
        )
        write_tree(destination, [entry(9)], {})
        source_files = tree(source)
        destination_files = tree(destination)
        result = migrate_legacy_memory(source, destination)
        cases.append(
            {
                "name": "destination_exists",
                "source_files": source_files,
                "destination_files": destination_files,
                "expected": {
                    "migrated": result.migrated,
                    "imported_count": result.imported_count,
                    "has_backup": result.backup_path is not None,
                    "tree": {
                        normalize_path(key): ""
                        for key in sorted(tree(root))
                        if not key.startswith("memory/")
                    },
                },
            }
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    # C：源目录不存在 -> 不迁移
    root = Path(tempfile.mkdtemp(prefix="memory-migrate-c-"))
    try:
        result = migrate_legacy_memory(root / "memory", root / "project" / "memory")
        cases.append(
            {
                "name": "source_absent",
                "source_files": {},
                "destination_files": {},
                "expected": {
                    "migrated": result.migrated,
                    "imported_count": result.imported_count,
                    "has_backup": result.backup_path is not None,
                    "tree": {},
                },
            }
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    # D：源与目标同一路径 -> 不迁移
    root = Path(tempfile.mkdtemp(prefix="memory-migrate-d-"))
    try:
        source = root / "memory"
        write_tree(source, [entry(1)], {"task-history/general/20250102-030401.md": "旧正文一"})
        result = migrate_legacy_memory(source, source)
        cases.append(
            {
                "name": "same_path",
                "same_path": True,
                "source_files": tree(source),
                "destination_files": {},
                "expected": {
                    "migrated": result.migrated,
                    "imported_count": result.imported_count,
                    "has_backup": result.backup_path is not None,
                    "tree": {normalize_path(key): "" for key in sorted(tree(root))},
                },
            }
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    # E：源路径不是目录 -> 报错
    root = Path(tempfile.mkdtemp(prefix="memory-migrate-e-"))
    try:
        source = root / "memory"
        source.write_text("不是目录", encoding="utf-8")
        try:
            migrate_legacy_memory(source, root / "project" / "memory")
            error = None
        except Exception as exc:  # noqa: BLE001 - 对照要记录错误文案
            # 绝对路径里带临时目录名，两侧换成占位才可比对。
            text = str(exc)
            # resolve() 可能把 8.3 短名展开成长名，两种写法都要换掉。
            for variant in {str(root), str(root.resolve())}:
                text = text.replace(variant, "<root>")
            error = text
        cases.append(
            {
                "name": "source_is_file",
                "source_files": {},
                "destination_files": {},
                "source_is_file": True,
                "expected": {"error": error},
            }
        )
    finally:
        shutil.rmtree(root, ignore_errors=True)

    return cases


def main() -> None:
    fixture = {
        "source": ["omnicrawl/state/memory.py"],
        "prompt": prompt_cases(),
        "migration": migration_cases(),
    }
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(
        json.dumps(fixture, ensure_ascii=False, indent=2) + chr(10), encoding="utf-8"
    )
    print(
        f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：提示词 {len(fixture['prompt'])} 例，"
        f"迁移 {len(fixture['migration'])} 例"
    )


if __name__ == "__main__":
    main()
