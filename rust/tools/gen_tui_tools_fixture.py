#!/usr/bin/env python3
"""生成 `omnicrawl-tui` 工作区工具的对照数据集。

期望值来自 Python 真实现：`omnicrawl/workspace/tools.py` 的 `WorkspaceTools`
（read/write_file/edit_file）与 `_sample_command_output`，以及
`omnicrawl/agent/toolkit/host_tools.py` 的工具声明构造。

用法（仓库根目录）：

    python rust/tools/gen_tui_tools_fixture.py
    cd rust && cargo test -p omnicrawl-tui --test workspace_tools_parity
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = ROOT / "rust/crates/omnicrawl-tui/tests/fixtures/workspace_tools_parity.json"

sys.path.insert(0, str(ROOT))

from omnicrawl.agent.toolkit.host_tools import build_tool_declaration  # noqa: E402
from omnicrawl.agent.toolkit.git_tools import git_result  # noqa: E402
from omnicrawl.agent.types import ToolDefinition  # noqa: E402
from omnicrawl.workspace.tools import (  # noqa: E402
    WorkspaceToolError,
    WorkspaceTools,
    _sample_command_output,
)

if not Path(sys.modules["omnicrawl.workspace.tools"].__file__).resolve().is_relative_to(ROOT):
    raise SystemExit("导入到的 omnicrawl 不在本仓库内，先确认运行目录")

# 声明用例：本宿主声明进表的工具（read/write_file/Edit_file/bash/powershell/monitor + 内核自持三件）。
CORE_TOOLS = ["read", "write_file", "Edit_file", "bash", "powershell", "monitor"]
META_TOOLS = ["update_todos", "ask_user", "pause_work"]


def catalog() -> dict:
    data = json.loads(
        (ROOT / "rust/crates/omnicrawl-controllers/data/agent_tools.json").read_text(encoding="utf-8")
    )
    entries = {}
    for group in data["groups"].values():
        for entry in group:
            entries[entry["name"]] = entry
    return entries


def declarations() -> list[dict]:
    entries = catalog()
    result = []
    for name in CORE_TOOLS + META_TOOLS:
        entry = entries[name]
        tool = ToolDefinition(
            name=entry["name"],
            description=entry["description"],
            argument_schema=entry["argument_schema"],
            requires_confirmation=entry["requires_confirmation"],
            run=lambda _arguments: None,
        )
        result.append(
            {
                "name": name,
                "declaration": build_tool_declaration(tool),
            }
        )
    return result


def file_cases(workspace: Path) -> dict:
    tools = WorkspaceTools(workspace)

    def capture(operation, arguments):
        try:
            return {"ok": True, "output": operation(arguments), "code": None, "retryable": False}
        except WorkspaceToolError as exc:
            return {
                "ok": False,
                "output": exc.formatted_message(),
                "code": exc.code,
                "retryable": exc.retryable,
            }

    cases: dict[str, list[dict]] = {"read": [], "write_file": [], "edit_file": []}

    (workspace / "window.txt").write_text("第一行\n第二行\n第三行\n", encoding="utf-8")
    (workspace / "long.txt").write_text("x" * 2100 + "\n", encoding="utf-8")
    (workspace / "empty.txt").write_text("", encoding="utf-8")
    (workspace / "many.txt").write_text("".join(f"行{i}\n" for i in range(1, 51)), encoding="utf-8")
    # CRLF 文件必须按二进制写：Python 文本模式会把 "\n" 再翻译一次，变成 "\r\r\n"。
    (workspace / "crlf.txt").write_bytes(b"one\r\ntwo\r\n")
    (workspace / "snippet.txt").write_text("".join(f"行{i}\n" for i in range(1, 21)), encoding="utf-8")

    read_cases = [
        {"path": "window.txt"},
        {"path": "window.txt", "start_line": 2, "max_lines": 2},
        {"path": "window.txt", "start_line": 9},
        {"path": "long.txt"},
        {"path": "empty.txt"},
        {"path": "many.txt", "start_line": 48, "max_lines": 500},
        {"path": "missing.txt"},
        {"path": ""},
        {"path": "snippet.txt", "text": "行5", "context_lines": 1, "max_lines": 10},
        {"path": "snippet.txt", "text": "不存在"},
        {"path": "snippet.txt", "function_name": "f", "text": "行5"},
    ]
    for arguments in read_cases:
        cases["read"].append({"arguments": arguments, **capture(tools.read_file, arguments)})

    write_cases = [
        {"path": "out/new.txt", "content": "你好世界"},
        {"path": "out/new.txt", "content": "追加", "mode": "append"},
        {"path": "out/new.txt", "content": "覆盖", "mode": ""},
        {"path": "out/new.txt", "content": "x", "mode": "patch"},
        {"path": "config.toml", "content": "x"},
    ]
    for arguments in write_cases:
        cases["write_file"].append({"arguments": arguments, **capture(tools.write_file, arguments)})

    def edit_case(name: str, arguments: dict) -> None:
        cases["edit_file"].append({"arguments": arguments, **capture(tools.edit_file, arguments)})

    (workspace / "edit.txt").write_text("第一行\n第二行\n第三行\n", encoding="utf-8")
    edit_case("unique", {"path": "edit.txt", "old_text": "第二行", "new_text": "改过的行"})

    (workspace / "edit2.txt").write_text("x\nx\n", encoding="utf-8")
    edit_case("missing", {"path": "edit2.txt", "old_text": "y", "new_text": "z"})
    edit_case("ambiguous", {"path": "edit2.txt", "old_text": "x", "new_text": "z"})
    edit_case("count_one", {"path": "edit2.txt", "old_text": "x", "new_text": "z", "count": 1})
    edit_case("count_all", {"path": "edit2.txt", "old_text": "z", "new_text": "w", "count": 0})

    (workspace / "crlf-edit.txt").write_bytes(b"one\r\ntwo\r\n")
    edit_case(
        "crlf_restore",
        {"path": "crlf-edit.txt", "old_text": "one\ntwo", "new_text": "three\nfour"},
    )
    edit_case("not_a_file", {"path": "nope.txt", "old_text": "a", "new_text": "b"})
    edit_case("empty_old", {"path": "crlf-edit.txt", "old_text": "", "new_text": "b"})

    return cases


def unsupported_locator_cases(workspace: Path) -> list[dict]:
    """Python 支持、当前 Rust 宿主尚未实现的定位方式（只记录期望，不做逐字对照）。"""

    tools = WorkspaceTools(workspace)
    cases = []
    (workspace / "locate.txt").write_text("def alpha():\n    return 1\n", encoding="utf-8")
    for arguments in ({"path": "locate.txt", "function_name": "alpha"},):
        try:
            cases.append({"arguments": arguments, "python_output": tools.read_file(arguments)})
        except WorkspaceToolError as exc:
            cases.append({"arguments": arguments, "python_output": exc.formatted_message()})
    return cases


# find 的候选顺序按修改时间倒序，必须把 mtime 钉死才能跨语言对照。
FIXED_MTIME = 1_700_000_000


def join_lines(*lines: str) -> str:
    return "\n".join([*lines, ""])


def _freeze_mtimes(workspace: Path) -> None:
    for path in sorted(workspace.rglob("*")):
        os.utime(path, (FIXED_MTIME, FIXED_MTIME))
    os.utime(workspace, (FIXED_MTIME, FIXED_MTIME))


def search_cases(workspace: Path) -> dict:
    """list / find / grep 用例；workspace 里放好结构、忽略规则与受保护目录。"""

    tools = WorkspaceTools(workspace)
    (workspace / "src" / "deep").mkdir(parents=True, exist_ok=True)
    (workspace / "node_modules" / "pkg").mkdir(parents=True, exist_ok=True)
    (workspace / ".git").mkdir(exist_ok=True)
    (workspace / "src" / "agent.py").write_text(
        join_lines("class Agent:", "    def run(self):", "        return self.tools"),
        encoding="utf-8",
    )
    (workspace / "src" / "helper.py").write_text(
        join_lines("def helper():", "    return 1"), encoding="utf-8"
    )
    (workspace / "src" / "deep" / "tool.py").write_text(
        join_lines("TOOL = 'agent'"), encoding="utf-8"
    )
    (workspace / "notes.md").write_text(
        join_lines("Agent 说明", "无关内容"), encoding="utf-8"
    )
    (workspace / "README.md").write_text(join_lines("项目说明"), encoding="utf-8")
    (workspace / "node_modules" / "pkg" / "index.js").write_text(
        join_lines("// Agent in node_modules"), encoding="utf-8"
    )
    (workspace / ".gitignore").write_text(join_lines("node_modules/"), encoding="utf-8")
    (workspace / ".git" / "HEAD").write_text(
        join_lines("ref: refs/heads/main"), encoding="utf-8"
    )
    _freeze_mtimes(workspace)

    def capture(operation, arguments):
        try:
            return {"ok": True, "output": operation(arguments), "code": None, "retryable": False}
        except WorkspaceToolError as exc:
            return {
                "ok": False,
                "output": exc.formatted_message(),
                "code": exc.code,
                "retryable": exc.retryable,
            }

    cases: dict[str, list[dict]] = {"list": [], "find": [], "grep": []}
    for arguments in (
        {"path": "."},
        {"path": "src"},
        {"path": "src", "recursive": True},
        {"path": "README.md"},
        {"path": "nope"},
        {"path": "config.toml"},
    ):
        cases["list"].append({"arguments": arguments, **capture(tools.list_files, arguments)})

    for arguments in (
        {"pattern": "agent"},
        {"pattern": "*.py"},
        {"pattern": "tool"},
        {"pattern": "AGENT", "case_sensitive": True},
        {"pattern": "src", "kind": "directory"},
        {"pattern": "*.py", "max_results": 1},
        {"pattern": "x", "kind": "symlink"},
        {"pattern": ""},
    ):
        case = {"arguments": arguments, **capture(tools.find_files, arguments)}
        if arguments.get("kind") == "directory":
            # 目录条目的修改时间在两侧无法取到同一值，只对照条目集合。
            case["compare"] = "lines_sorted"
        cases["find"].append(case)

    for arguments in (
        {"pattern": "Agent"},
        {"pattern": "Agent", "count": True},
        {"pattern": "Agent", "files_with_matches": True},
        {"pattern": "def run", "context_lines": 1},
        {"pattern": "Agent", "include": "*.py"},
        {"pattern": "Agent", "exclude": "*.py"},
        {"pattern": "self.tools", "use_regex": False},
        {"pattern": "AGENT", "case_sensitive": True},
        {"pattern": "agent", "path": "src"},
        {"pattern": "agent", "path": "src/*.py"},
        {"pattern": "Agent", "max_results": 1},
        {"pattern": "Agent", "path": "nope"},
        {"pattern": "Agent", "path": "*.nope"},
        {"pattern": ""},
    ):
        cases["grep"].append({"arguments": arguments, **capture(tools.grep, arguments)})
    return cases


def git_cases(workspace: Path) -> list[dict]:
    """git 用例：校验失败文案 + 真仓库里的只读 status（先 init）。"""

    cases: list[dict] = []
    for arguments in (
        {"action": "frobnicate"},
        {"action": "status", "args": ["--git-dir=/tmp/x"]},
        {"action": "config", "args": ["--global", "user.name", "x"]},
        {"action": "archive", "args": ["-o", "out.zip"]},
        {"action": "commit"},
        {"action": "commit", "message": 5},
        {"action": "add", "paths": ["../outside.txt"]},
    ):
        result = git_result(workspace, arguments)
        cases.append({"arguments": arguments, "ok": result.ok, "output": result.output})

    subprocess.run(["git", "init", "-q"], cwd=str(workspace), check=True)
    result = git_result(workspace, {"action": "status"})
    cases.append({"arguments": {"action": "status"}, "ok": result.ok, "output": result.output})
    return cases


def sampling_cases() -> list[dict]:
    cases = []
    short = "ok\n12 passed\n"
    cases.append({"text": short, "expected": _sample_command_output(short)})
    long_text = "".join(f"行{i}\n" for i in range(2000))
    cases.append({"text": long_text, "expected": _sample_command_output(long_text)})
    cases.append({"text": "", "expected": _sample_command_output("")})
    return cases


def main() -> None:
    workspace = Path(tempfile.mkdtemp(prefix="omnicrawl-tui-fixture-")).resolve()
    try:
        search_workspace = Path(tempfile.mkdtemp(prefix="omnicrawl-tui-search-")).resolve()
        git_workspace = Path(tempfile.mkdtemp(prefix="omnicrawl-tui-git-")).resolve()
        try:
            search = search_cases(search_workspace)
            git = git_cases(git_workspace)
        finally:
            shutil.rmtree(search_workspace, ignore_errors=True)
            shutil.rmtree(git_workspace, ignore_errors=True)
        temp_roots = [str(search_workspace), str(git_workspace)]
        payload = {
            "source": "omnicrawl/workspace/tools.py + omnicrawl/agent/toolkit/host_tools.py",
            "declarations": declarations(),
            "files": file_cases(workspace),
            "unsupported_locators": unsupported_locator_cases(workspace),
            "sampling": sampling_cases(),
            "search": search,
            "git": git,
        }
    finally:
        shutil.rmtree(workspace, ignore_errors=True)

    serialized = json.dumps(payload, ensure_ascii=False, indent=2)
    for prefix in (str(workspace), *temp_roots, str(Path(FIXTURE_PATH.parent)), str(ROOT)):
        serialized = serialized.replace(prefix.replace("\\", "\\\\"), "<WORKSPACE>")
        serialized = serialized.replace(prefix, "<WORKSPACE>")
    # 落盘文件名的随机后缀与运行无关，统一成占位符
    # 落盘文件名的随机后缀与运行无关，统一成占位符（用 lambda 避免反斜杠转义歧义）
    serialized = re.sub(
        r"(find_results|grep_matches|grep_counts|grep_files)_[0-9a-f]+\.txt",
        lambda match: f"{match.group(1)}_<ID>.txt",
        serialized,
    )
    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    FIXTURE_PATH.write_text(serialized + "\n", encoding="utf-8")
    counts = {
        "declarations": len(payload["declarations"]),
        **{name: len(cases) for name, cases in payload["files"].items()},
        "unsupported_locators": len(payload["unsupported_locators"]),
        **{f"search.{name}": len(cases) for name, cases in payload["search"].items()},
        "git": len(payload["git"]),
        "sampling": len(payload["sampling"]),
    }
    print(f"已写入 {FIXTURE_PATH.relative_to(ROOT)}：{counts}")


if __name__ == "__main__":
    main()
