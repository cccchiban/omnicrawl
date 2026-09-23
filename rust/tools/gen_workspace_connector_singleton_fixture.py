#!/usr/bin/env python3
"""生成连接器单例锁的对照数据集，供 `omnicrawl-workspace` 的 parity 测试使用。

单一真相是 `omnicrawl/workspace/connector_singleton.py` 的真实现：本脚本把同一批平台名、
锁文件内容喂给真实现（`connector_lock_path` / `_locked_pid` / `ConnectorInstanceLock`），
把锁文件名、PID 解析结果、以及「已有陈旧锁文件时能否拿到单例」记下来。

`user_config_dir` 被换成本次临时目录：这台机器上不该为了跑生成器在用户主目录里留下
`connector-*.lock`（`connector_lock_path` 本身只拼路径，但默认值会落在真实配置目录里）。
数据集只记录文件名，因此替换目录不影响可比性。

不进数据集的部分（见 crate `README.md`）：「另一进程持锁 → 拿不到」与「持有者退出 → 可接管」
要真起进程，放在 Rust 侧的 `connector_lock_process.rs`；「锁被持有期间读 PID」在 Windows 上
本来就不可读（字节区间锁导致共享违例），记录它只会记录平台差异。

用法：``python rust/tools/gen_workspace_connector_singleton_fixture.py``
输出：``rust/crates/omnicrawl-workspace/tests/fixtures/connector_singleton_parity.json``
"""

from __future__ import annotations

import importlib
import json
import shutil
import sys
import tempfile
import types
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
    / "connector_singleton_parity.json"
)

sys.path.insert(0, str(ROOT))


def import_singleton_module():
    """导入 `omnicrawl.workspace.connector_singleton`（真实现）。

    `omnicrawl/__init__.py` 会急切导入一批兼容模块，其中 `omnicrawl.commands.slash` 会拉起
    `agent` → `llm.desensitization`；仓库工作区里那份文件当前带着未解决的合并冲突标记
    （`<<<<<<< ours`），语法就不合法，于是任何 `import omnicrawl.*` 都会连带失败。连接器单例
    与这条链路无关，因此先用空模块把它挡住；导入后断言模块文件确实位于仓库内，避免
    「挡住了冲突」变成换了真相源。
    """

    sys.modules.setdefault("omnicrawl.commands.slash", types.ModuleType("omnicrawl.commands.slash"))
    module = importlib.import_module("omnicrawl.workspace.connector_singleton")
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")
    return module


C = import_singleton_module()

# 平台名用例：覆盖 ASCII、中文、Unicode 字母、需要折成 `-` 的字符、剥掉首尾 `.-`、空名字。
NAMES: list[str] = [
    "Telegram",
    "飞书",
    "weChat",
    "Ünïcode",
    "name.with.dots",
    "-lead-trail-",
    "a/b:c*d?e",
    "  spaced  ",
    "!!!",
    "",
]

# 锁文件内容用例：正数、前导零、零、负数、字母、缺等号、前导空格、多行、CRLF、空文件。
PID_FILE_CASES: list[str] = [
    "pid=1234\n",
    "pid=1234",
    "pid=0012\n",
    "pid=0\n",
    "pid=-5\n",
    "pid=abc\n",
    "pid=\n",
    " pid=5\n",
    "x=1\npid=7\n",
    "pid=1234\r\n",
    "",
]


def build_lock_filenames(scratch: Path) -> list[dict[str, Any]]:
    """记录平台名 → 锁文件名与其中的「净化后平台名」。"""

    # 让默认路径落在临时目录里，避免在真实用户配置目录留下锁文件。
    config_root = scratch / "config"
    config_root.mkdir(parents=True, exist_ok=True)
    C.user_config_dir = lambda: config_root  # type: ignore[assignment]

    cases: list[dict[str, Any]] = []
    prefix = C.LOCK_FILENAME_PREFIX
    suffix = C.LOCK_FILENAME_SUFFIX
    for name in NAMES:
        filename = C.connector_lock_path(name).name
        if not (filename.startswith(prefix) and filename.endswith(suffix)):
            raise SystemExit(f"锁文件名形状不符：{filename}")
        cases.append(
            {
                "name": name,
                "stem": filename[len(prefix) : len(filename) - len(suffix)],
                "filename": filename,
            }
        )
    return cases


def build_pid_line_cases(scratch: Path) -> list[dict[str, Any]]:
    """把每种锁文件内容写盘，记录 `_locked_pid` 的解析结果。"""

    case_dir = scratch / "pid"
    case_dir.mkdir(parents=True, exist_ok=True)
    cases: list[dict[str, Any]] = []
    for index, content in enumerate(PID_FILE_CASES):
        path = case_dir / f"lock-{index}.lock"
        path.write_bytes(content.encode("utf-8"))
        cases.append({"content": content, "pid": C._locked_pid(path)})
    return cases


def build_takeover_case(scratch: Path) -> dict[str, Any]:
    """锁文件里有「已死持有者」的 PID、但没人持锁时，应当直接拿到单例。"""

    lock_path = scratch / "takeover" / f"{C.LOCK_FILENAME_PREFIX}Telegram{C.LOCK_FILENAME_SUFFIX}"
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path.write_bytes(b"pid=0\n")

    lock = C.ConnectorInstanceLock("Telegram", lock_path=lock_path)
    acquired = lock.try_acquire()
    if not acquired:
        raise SystemExit("已有陈旧锁文件时应当能拿到单例。")
    case = {
        "content": "pid=0\n",
        "acquired": acquired,
        "owner_flag": bool(lock._owner),
        "lock_file_exists_while_held": lock_path.exists(),
    }
    lock.release()
    case["lock_file_exists_after_release"] = lock_path.exists()
    if case["lock_file_exists_after_release"]:
        raise SystemExit("释放单例锁后应当删除锁文件。")
    return case


def build_pid_running_cases() -> list[dict[str, Any]]:
    """PID 存活判定的边界：非正 PID 一律视为不可用（与 `os.kill(pid, 0)` 的守卫一致）。"""

    cases = []
    for pid in (0, -1, -9999):
        cases.append({"pid": pid, "running": C.pid_is_running(pid)})
        if cases[-1]["running"]:
            raise SystemExit(f"PID {pid} 不应被判定为存活。")
    return cases


def main() -> None:
    scratch = Path(tempfile.mkdtemp(prefix="omnicrawl-singleton-fixture-"))
    try:
        data: dict[str, Any] = {
            "constants": {
                "prefix": C.LOCK_FILENAME_PREFIX,
                "suffix": C.LOCK_FILENAME_SUFFIX,
                "stale_reclaim_wait_seconds": C.STALE_RECLAIM_WAIT_SECONDS,
            },
            "lock_filenames": build_lock_filenames(scratch),
            "pid_lines": build_pid_line_cases(scratch),
            "takeover": build_takeover_case(scratch),
            "pid_running": build_pid_running_cases(),
        }
        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        OUTPUT_PATH.write_bytes(
            (json.dumps(data, ensure_ascii=False, indent=2, sort_keys=False) + "\n").encode(
                "utf-8"
            )
        )
        print(f"已写出 {OUTPUT_PATH.relative_to(ROOT)}")
        print(
            "用例数："
            f"锁文件名 {len(data['lock_filenames'])}、"
            f"PID 行 {len(data['pid_lines'])}、"
            f"接管 {1 if data['takeover'] else 0}、"
            f"存活 {len(data['pid_running'])}"
        )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
