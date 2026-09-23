#!/usr/bin/env python3
"""生成 Agent 临时工作区的对照数据集，供 `omnicrawl-workspace` 的 parity 测试使用。

单一真相是 `omnicrawl/workspace/temp.py` 的真实现：本脚本把同一批输入喂给真实现
（配置片段、目录配置字符串、子路径、待清理条目、`.last_cleanup` 时间戳），把返回值、
报错文案，以及清理前后的目录树原样记下来。Rust 侧照原样重建后重放，逐字段比对。

两边都有的手写期望（比如「`.last_cleanup` 是目录时会被删掉」）会先跟真实现断言一致，
再写进数据集，避免数据集里混进作者以为的行为。

平台相关的用例（`/outside` 在 Windows 上不是绝对路径、在 POSIX 上是）带 `platform`
标记，测试跳过与当前平台不符的条目；数据集只记录生成时所在平台的形状。

用法：``python rust/tools/gen_workspace_temp_fixture.py``
输出：``rust/crates/omnicrawl-workspace/tests/fixtures/workspace_temp_parity.json``
"""

from __future__ import annotations

import importlib
import json
import os
import shutil
import sys
import tempfile
import types
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[2]
OUTPUT_PATH = (
    ROOT
    / "rust"
    / "crates"
    / "omnicrawl-workspace"
    / "tests"
    / "fixtures"
    / "workspace_temp_parity.json"
)

sys.path.insert(0, str(ROOT))


def import_temp_module():
    """导入 `omnicrawl.workspace.temp`（真实现）。

    `omnicrawl/__init__.py` 会急切导入一批兼容模块，其中 `omnicrawl.commands.slash` 会拉起
    `agent` → `llm.desensitization`；仓库工作区里那份文件当前带着未解决的合并冲突标记
    （`<<<<<<< ours`），语法就不合法，于是任何 `import omnicrawl.*` 都会连带失败。临时工作区
    与这条链路无关，因此先用空模块把它挡住；导入后断言模块文件确实位于仓库内，避免
    「挡住了冲突」变成换了真相源。
    """

    sys.modules.setdefault("omnicrawl.commands.slash", types.ModuleType("omnicrawl.commands.slash"))
    module = importlib.import_module("omnicrawl.workspace.temp")
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")
    return module


T = import_temp_module()

ROOT_TOKEN = "<ROOT>"
CURRENT_PLATFORM = "windows" if os.name == "nt" else "unix"

# 与数据集里 `File` 字段对应的配置文件相对路径：Rust 侧写到自己的临时目录同名位置。
CONFIG_FILE_NAME = "config.toml"

# 固定时间点：清理标记的时间戳与「现在」都从这里推，避免集成真实时钟。
BASE_TIME = datetime(2026, 3, 4, 5, 6, 7)


def norm(path: Path | str, root: Path) -> str:
    """把路径折算成与具体临时目录无关的稳定形状（两侧各自替换自己的根）。"""

    resolved = Path(str(path)).resolve()
    try:
        relative = resolved.relative_to(root.resolve())
    except ValueError:
        return "<OUTSIDE>"
    text = relative.as_posix()
    return ROOT_TOKEN if text == "." else f"{ROOT_TOKEN}/{text}"


def normalize_text(text: str, root: Path) -> str:
    """把文案里出现的本次临时目录根替换成 `<ROOT>`。

    `tempfile.mkdtemp()` 在 Windows 上会给出 8.3 短名（`ADMINI~1`），而 `resolve()` 给的是长名，
    报错文案里用的是调用方传入的那个字符串，因此两种写法都要替换掉。
    """

    resolved = root.resolve()
    for candidate in {str(root), str(resolved), root.as_posix(), resolved.as_posix()}:
        text = text.replace(candidate, ROOT_TOKEN)
    return text


# --- 配置读取 -----------------------------------------------------------------

CONFIG_CASES: list[tuple[str, str | None]] = [
    ("missing_file", None),
    ("defaults", "[agent_temp]\n"),
    ("all_values", (
        "[agent_temp]\n"
        "enabled = false\n"
        'directory = "tmp/agent"\n'
        "cleanup_enabled = false\n"
        "cleanup_interval_hours = 6\n"
    )),
    ("directory_blank", '[agent_temp]\ndirectory = "   "\n'),
    ("directory_not_string", "[agent_temp]\ndirectory = 5\n"),
    ("enabled_not_bool", '[agent_temp]\nenabled = "yes"\n'),
    ("cleanup_enabled_not_bool", "[agent_temp]\ncleanup_enabled = 1\n"),
    ("interval_zero", "[agent_temp]\ncleanup_interval_hours = 0\n"),
    ("interval_negative", "[agent_temp]\ncleanup_interval_hours = -3\n"),
    ("interval_bool", "[agent_temp]\ncleanup_interval_hours = true\n"),
    ("interval_float", "[agent_temp]\ncleanup_interval_hours = 1.5\n"),
    ("no_section", "other = 1\n"),
    ("section_not_table", "agent_temp = 5\n"),
]


def build_config_cases(root: Path) -> list[dict[str, Any]]:
    """逐条喂给 `load_agent_temp_workspace_config`，记录配置字段或报错文案。"""

    case_dir = root / "config"
    case_dir.mkdir(parents=True, exist_ok=True)
    config_path = case_dir / CONFIG_FILE_NAME
    cases: list[dict[str, Any]] = []
    for name, content in CONFIG_CASES:
        if content is None:
            config_path.unlink(missing_ok=True)
        else:
            # 写字节而不是 write_text：文本模式会把 \n 折成 \r\n，TOML 解析两侧必须同源。
            config_path.write_bytes(content.encode("utf-8"))
        entry: dict[str, Any] = {"name": name, "file": content}
        try:
            config = T.load_agent_temp_workspace_config(config_path)
        except Exception as exc:  # noqa: BLE001 - 报错文案就是数据的一部分
            entry["error"] = normalize_text(str(exc), root)
        else:
            entry["config"] = {
                "enabled": config.enabled,
                "directory": config.directory,
                # 子目录元组不参与配置读取（Python 侧用默认值），只在常量里记录。
                "cleanup_enabled": config.cleanup_enabled,
                "cleanup_interval_hours": config.cleanup_interval_hours,
            }
        cases.append(entry)
    return cases


# --- 目录解析 -----------------------------------------------------------------

# (配置里的 directory, 平台标记)。生成器只记录当前平台的形状，标记供测试侧过滤：
# `any` 表示两平台行为一致，具体平台名表示换平台结论会变。`/outside` 在 Windows 上不是
# 绝对路径（会走到「不在工作区内」那档报错），在 POSIX 上直接是绝对路径，因此带平台标记。
RESOLVE_CASES: list[tuple[str, str]] = [
    (".omnicrawl/.agent_tmp", "any"),
    ("tmp", "any"),
    ("tmp/agent/work", "any"),
    ("sub/", "any"),
    ("a/./b", "any"),
    ("./x", "any"),
    (".", "any"),
    ("", "any"),
    ("   ", "any"),
    ("..", "any"),
    ("../outside", "any"),
    ("sub/..", "any"),
    ("/outside", CURRENT_PLATFORM),
    ("C:/outside", "windows"),
    ("//server/share", "windows"),
]

CHILD_CASES: list[tuple[str, str]] = [
    ("files", "any"),
    ("images/pic", "any"),
    ("a/./b", "any"),
    (".", "any"),
    ("", "any"),
    ("..", "any"),
    ("../escape", "any"),
    ("/outside", CURRENT_PLATFORM),
    ("files/", "any"),
]


def build_resolve_cases(root: Path, workspace: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """喂 `resolve_agent_temp_dir` 与 `AgentTempWorkspace._resolve_child`。"""

    resolve_cases: list[dict[str, Any]] = []
    for directory, platform in RESOLVE_CASES:
        entry: dict[str, Any] = {"directory": directory, "platform": CURRENT_PLATFORM if platform == CURRENT_PLATFORM else platform}
        try:
            resolved = T.resolve_agent_temp_dir(workspace, directory)
        except Exception as exc:  # noqa: BLE001 - 报错文案就是数据的一部分
            entry["error"] = normalize_text(str(exc), root)
        else:
            entry["dir"] = norm(resolved, root)
        resolve_cases.append(entry)

    instance = T.AgentTempWorkspace(workspace)
    child_cases: list[dict[str, Any]] = []
    for relative, platform in CHILD_CASES:
        entry = {"relative": relative, "platform": platform}
        try:
            resolved = instance._resolve_child(relative)
        except Exception as exc:  # noqa: BLE001 - 报错文案就是数据的一部分
            entry["error"] = normalize_text(str(exc), root)
        else:
            entry["dir"] = norm(resolved, root)
        child_cases.append(entry)
    return resolve_cases, child_cases


# --- 清理 ---------------------------------------------------------------------

# 清理前铺开的目录树；`files`/`images` 既在配置子目录里、又带内容，用于验证
# 「先删再重建」。`.last_cleanup` 是保留项（不管内容多旧都会留下，重建时被覆写）。
CLEANUP_TREE: dict[str, str] = {
    "files/keep.txt": "keep",
    "images/pic.bin": "data",
    "notes.txt": "hello",
    "sub/deep/nested.txt": "nested",
    ".last_cleanup": "last_cleanup=2000-01-01T00:00:00\n",
    "screen.bin": "screen",
}

# 清理后应当存在的条目：保留的三个根级文件 + 重建的分类子目录（子目录内容已清空）。
CLEANUP_EXPECTED_AFTER = [
    ".gitignore",
    ".last_cleanup",
    "README.md",
    *T.DEFAULT_AGENT_TEMP_SUBDIRECTORIES,
]


def build_cleanup_case(root: Path) -> dict[str, Any]:
    """真跑一次 `clean()`，记录前后目录树、删除清单与标记文件内容。"""

    workspace = root / "cleanup"
    workspace.mkdir(parents=True, exist_ok=True)
    instance = T.AgentTempWorkspace(workspace)
    # 先 ensure() 建出临时目录与分类子目录（`clean()` 自己也会做，这里是为了能把树先铺进去）。
    instance.ensure()
    for relative, content in CLEANUP_TREE.items():
        target = instance.root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content.encode("utf-8"))

    result = instance.clean(now=BASE_TIME)

    after: dict[str, str] = {}
    for entry in sorted(instance.root.rglob("*"), key=lambda item: str(item).lower()):
        kind = "dir" if entry.is_dir() else "symlink" if entry.is_symlink() else "file"
        after[norm(entry, root)] = kind

    marker = (instance.root / T.LAST_CLEANUP_FILENAME).read_text(encoding="utf-8")
    case = {
        "tree": CLEANUP_TREE,
        "now": BASE_TIME.isoformat(timespec="seconds"),
        "root": norm(instance.root, root),
        "deleted_entries": list(result.deleted_entries),
        "failed_entries": list(result.failed_entries),
        "marker_text": marker,
        "after": after,
        "readme": (instance.root / "README.md").read_text(encoding="utf-8"),
        "gitignore": (instance.root / ".gitignore").read_text(encoding="utf-8"),
    }
    expected_after = {f"{case['root']}/{name}" for name in CLEANUP_EXPECTED_AFTER}
    if set(after) != expected_after:
        raise SystemExit(
            "清理后的目录树与预期不符："
            f"多出 {sorted(set(after) - expected_after)}、"
            f"缺少 {sorted(expected_after - set(after))}"
        )
    if set(after.values()) != {"dir", "file"} and "dir" not in after.values():
        raise SystemExit("清理后应当同时存在目录与文件。")
    # clean() 会把根目录下除保留项之外的一切都删掉（含 ensure() 刚建的分类子目录），
    # 再重建子目录并写回标记文件；这里只断言“保留项没被删、临时产物被删”这两条硬约束，
    # 删除清单本身照实记录（顺序由 name.lower() 排序决定，正好是 Rust 侧必须复刻的一环）。
    deleted = set(case["deleted_entries"])
    preserved = {
        name
        for name in T.PRESERVED_ROOT_NAMES
        if name != T.LAST_CLEANUP_FILENAME
    }
    if deleted & preserved:
        raise SystemExit(f"保留项被删除了：{sorted(deleted & preserved)}")
    if not {"notes.txt", "sub", "files"} <= deleted:
        raise SystemExit(f"临时产物未被删除：{sorted(deleted)}")
    if case["failed_entries"]:
        raise SystemExit(f"不应有删除失败：{case['failed_entries']}")
    return case


def build_delete_cases(root: Path, workspace: Path) -> list[dict[str, Any]]:
    """`_delete_entry` 的边界：根目录自身、临时目录外的文件、目录内的普通文件。"""

    instance = T.AgentTempWorkspace(workspace)
    instance.ensure()
    outside = workspace / "outside.txt"
    outside.write_text("outside", encoding="utf-8")
    inside = instance.root / "files" / "inside.txt"
    inside.write_text("inside", encoding="utf-8")

    cases: list[dict[str, Any]] = []
    for entry_path in (instance.root, outside, inside):
        entry: dict[str, Any] = {"entry": norm(entry_path, root)}
        try:
            instance._delete_entry(entry_path)
        except Exception as exc:  # noqa: BLE001 - 报错文案就是数据的一部分
            entry["error"] = normalize_text(str(exc), root)
        else:
            entry["deleted"] = True
        cases.append(entry)
    return cases


# --- 清理间隔 -----------------------------------------------------------------

# (距上次清理的秒数, 是否到期)。刻意避开恰好等于间隔的边界：文件时间的存储精度
# 两侧不完全一致，边界用例放在 Rust 侧的单测里。
DUE_DELTAS: list[float] = [0.0, 3600.0, 86399.0, 86401.0, 200000.0]


def build_due_cases(root: Path) -> dict[str, Any]:
    """把 `.last_cleanup` 的文件时间钉在 BASE_TIME，再问「是否到期 / 还有多久」。"""

    workspace = root / "due"
    workspace.mkdir(parents=True, exist_ok=True)
    instance = T.AgentTempWorkspace(workspace)
    instance.ensure()
    marker = instance.root / T.LAST_CLEANUP_FILENAME
    marker.write_text("last_cleanup=seed\n", encoding="utf-8")
    timestamp = BASE_TIME.timestamp()
    os.utime(marker, (timestamp, timestamp))

    cases: list[dict[str, Any]] = []
    for delta in DUE_DELTAS:
        now = BASE_TIME + timedelta(seconds=delta)
        cases.append(
            {
                "delta_seconds": delta,
                "due": instance.is_cleanup_due(now=now),
                "seconds_until_next": round(instance.seconds_until_next_cleanup(now=now), 3),
            }
        )

    return {
        "base_time": BASE_TIME.isoformat(timespec="seconds"),
        "interval_hours": instance.config.cleanup_interval_hours,
        "cases": cases,
    }


def build_status_labels() -> list[dict[str, Any]]:
    """`agent_temp_status_label` 的三条分支。"""

    configs = [
        T.AgentTempWorkspaceConfig(),
        T.AgentTempWorkspaceConfig(enabled=False),
        T.AgentTempWorkspaceConfig(cleanup_enabled=False),
        T.AgentTempWorkspaceConfig(directory="tmp/agent", cleanup_interval_hours=6),
    ]
    labels: list[dict[str, Any]] = []
    for config in configs:
        labels.append(
            {
                "config": {
                    "enabled": config.enabled,
                    "directory": config.directory,
                    "cleanup_enabled": config.cleanup_enabled,
                    "cleanup_interval_hours": config.cleanup_interval_hours,
                },
                "label": T.agent_temp_status_label(config),
            }
        )
    return labels


def main() -> None:
    scratch = Path(tempfile.mkdtemp(prefix="omnicrawl-temp-fixture-"))
    try:
        workspace = scratch / "workspace"
        workspace.mkdir(parents=True, exist_ok=True)

        defaults = T.AgentTempWorkspaceConfig()
        data: dict[str, Any] = {
            "constants": {
                "default_directory": T.DEFAULT_AGENT_TEMP_DIRECTORY,
                "default_interval_hours": T.DEFAULT_AGENT_TEMP_CLEANUP_INTERVAL_HOURS,
                "subdirectories": list(T.DEFAULT_AGENT_TEMP_SUBDIRECTORIES),
                "last_cleanup_filename": T.LAST_CLEANUP_FILENAME,
                "preserved_root_names": sorted(T.PRESERVED_ROOT_NAMES),
                "readme": T._temp_workspace_readme(),
                "config_defaults": {
                    "enabled": defaults.enabled,
                    "directory": defaults.directory,
                    "cleanup_enabled": defaults.cleanup_enabled,
                    "cleanup_interval_hours": defaults.cleanup_interval_hours,
                },
            },
            "config_cases": build_config_cases(scratch),
            "status_labels": build_status_labels(),
            "cleanup": build_cleanup_case(scratch),
            "due": build_due_cases(scratch),
        }
        resolve_cases, child_cases = build_resolve_cases(scratch, workspace)
        data["resolve_cases"] = resolve_cases
        data["child_cases"] = child_cases
        data["delete_cases"] = build_delete_cases(scratch, scratch / "delete")

        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT_PATH.write_bytes(
            (json.dumps(data, ensure_ascii=False, indent=2, sort_keys=False) + "\n").encode(
                "utf-8"
            )
        )
        print(f"已写出 {OUTPUT_PATH.relative_to(ROOT)}")
        print(
            "用例数："
            f"配置 {len(data['config_cases'])}、"
            f"目录 {len(resolve_cases)}、"
            f"子路径 {len(child_cases)}、"
            f"删除 {len(data['delete_cases'])}、"
            f"状态 {len(data['status_labels'])}、"
            f"间隔 {len(data['due']['cases'])}"
        )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
