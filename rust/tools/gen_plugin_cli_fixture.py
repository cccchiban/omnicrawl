#!/usr/bin/env python3
"""生成插件 CLI 的对照数据集，供 `omnicrawl-entry` 的 parity 测试使用。

单一真相是 `omnicrawl/cli.py` 的 argparse 参数面：本脚本把同一批 argv 喂给
`build_parser().parse_args()`，记录成功解析出的 namespace（逐字段）或 argparse 的退出码；
再用同一批 argv 跑 `_resolve_scope`，记录三种工作区形态（空 / 有 AGENTS.md / 有 package.json）
下的作用域结论。

只对照「参数面 + 作用域」这两层：安装 / 注册表 / 诊断都要真跑 npm 与网络，本批次不做；
Rust 侧的 `plugin ...` 子命令行为靠 `omnicrawl-extensions` 已有的对照测试覆盖。

用法：``python rust/tools/gen_plugin_cli_fixture.py``
输出：``rust/crates/omnicrawl-entry/tests/fixtures/plugin_cli_parity.json``
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
    ROOT / "rust" / "crates" / "omnicrawl-entry" / "tests" / "fixtures" / "plugin_cli_parity.json"
)

sys.path.insert(0, str(ROOT))


def import_cli_module():
    """导入 `omnicrawl.cli`（真实现）。

    `omnicrawl/__init__.py` 会急切导入一批兼容模块，其中 `omnicrawl.commands.slash` 会拉起
    `agent` → `llm.desensitization`；仓库工作区里那份文件当前带着未解决的合并冲突标记
    （`<<<<<<< ours`），语法就不合法，于是任何 `import omnicrawl.*` 都会连带失败。CLI 参数面
    与这条链路无关，因此先用空模块把它挡住；导入后断言模块文件确实位于仓库内，避免
    「挡住了冲突」变成换了真相源。
    """

    sys.modules.setdefault(
        "omnicrawl.commands.slash", types.ModuleType("omnicrawl.commands.slash")
    )
    module = importlib.import_module("omnicrawl.cli")
    if not Path(module.__file__).resolve().is_relative_to(ROOT):
        raise SystemExit(f"加载到的不是仓库源码：{module.__file__}")
    return module


CLI = import_cli_module()

# 参数解析用例：argv 是剥掉程序名后的完整参数表。
PARSE_CASES: list[list[str]] = [
    # 顶层路由与保留参数
    ["plugin", "list"],
    ["--resume", "abc", "plugin", "list"],
    ["--resume=abc", "plugin", "list"],
    # system
    ["plugin", "system", "enable"],
    ["plugin", "system", "disable"],
    # install
    ["plugin", "install", "demo-plugin"],
    ["plugin", "install", "demo-plugin", "--enable", "--yes"],
    ["plugin", "install", "@scope/demo@1.2.3", "--user"],
    ["plugin", "install", "./local-plugin", "--dev"],
    ["plugin", "install", "C:\\plugins\\demo", "--project"],
    # list
    ["plugin", "list", "--json"],
    ["plugin", "list", "--all", "--json"],
    ["plugin", "list", "--project"],
    ["plugin", "list", "--user"],
    # info
    ["plugin", "info", "demo"],
    ["plugin", "info", "demo", "--json"],
    # enable / disable
    ["plugin", "enable", "demo"],
    ["plugin", "disable", "demo", "--user"],
    # update
    ["plugin", "update", "demo"],
    ["plugin", "update", "demo", "--to", "1.2.3", "--yes", "--no-activate"],
    ["plugin", "update", "demo", "--to=1.2.3"],
    # rollback / uninstall
    ["plugin", "rollback", "demo"],
    ["plugin", "rollback", "demo", "--project"],
    ["plugin", "uninstall", "demo", "--purge", "--yes"],
    ["plugin", "uninstall", "demo"],
    # doctor
    ["plugin", "doctor"],
    ["plugin", "doctor", "demo"],
    ["plugin", "doctor", "--json"],
    # 帮助（argparse 以 0 退出）
    ["--help"],
    ["plugin", "--help"],
    ["plugin", "install", "--help"],
    ["plugin", "list", "-h"],
    # 参数错误（argparse 以 2 退出）
    ["plugin"],
    ["plugin", "bogus"],
    ["plugin", "list", "--purge"],
    ["plugin", "enable", "demo", "--purge"],
    ["plugin", "system"],
    ["plugin", "system", "bogus"],
    ["plugin", "install"],
    ["plugin", "info"],
    ["plugin", "list", "extra"],
    ["plugin", "doctor", "one", "two"],
    ["--bogus", "plugin", "list"],
    ["plugin", "update", "demo", "--no-activate", "--project", "--user"],
]

# 作用域判定用例：(argv, 工作区形态)
SCOPE_CASES: list[tuple[list[str], str]] = [
    (["plugin", "list"], "empty"),
    (["plugin", "list"], "agents"),
    (["plugin", "list"], "package"),
    (["plugin", "install", "demo"], "agents"),
    (["plugin", "install", "demo"], "package"),
    (["plugin", "list", "--project"], "empty"),
    (["plugin", "list", "--user"], "agents"),
    (["plugin", "enable", "demo"], "empty"),
    (["plugin", "uninstall", "demo", "--yes"], "package"),
    (["plugin", "list", "--project", "--user"], "empty"),
]


def namespace_of(argv: list[str]) -> dict[str, Any]:
    try:
        parsed = CLI.build_parser().parse_args(argv)
    except SystemExit as exc:
        return {"exit": int(exc.code or 0)}
    return {"args": {key: value for key, value in vars(parsed).items()}}


def build_workspaces(root: Path) -> dict[str, Path]:
    workspaces = {
        "empty": root / "empty",
        "agents": root / "agents",
        "package": root / "package",
    }
    for path in workspaces.values():
        path.mkdir(parents=True, exist_ok=True)
    (workspaces["agents"] / "AGENTS.md").write_bytes(b"# agents\n")
    (workspaces["package"] / "package.json").write_bytes(b'{"name": "demo"}\n')
    return workspaces


def build_scope_cases(workspaces: dict[str, Path]) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for argv, shape in SCOPE_CASES:
        entry: dict[str, Any] = {"argv": argv, "workspace": shape}
        try:
            parsed = CLI.build_parser().parse_args(argv)
        except SystemExit as exc:
            entry["exit"] = int(exc.code or 0)
            cases.append(entry)
            continue
        try:
            entry["scope"] = CLI._resolve_scope(parsed, workspaces[shape])
        except SystemExit as exc:
            # `--project` 与 `--user` 同时给出：`raise SystemExit(EXIT_USAGE)`。
            entry["exit"] = int(exc.code or 0)
        cases.append(entry)
    return cases


def build_parse_cases() -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    for argv in PARSE_CASES:
        entry: dict[str, Any] = {"argv": argv}
        entry.update(namespace_of(argv))
        cases.append(entry)
    return cases


def main() -> int:
    root = Path(tempfile.mkdtemp(prefix="occli-")).resolve()
    try:
        payload = {
            "exit_codes": {
                "ok": CLI.EXIT_OK,
                "usage": CLI.EXIT_USAGE,
                "node": CLI.EXIT_NODE,
                "registry_net": CLI.EXIT_REGISTRY_NET,
                "manifest": CLI.EXIT_MANIFEST,
                "user_cancel": CLI.EXIT_USER_CANCEL,
                "atomic": CLI.EXIT_ATOMIC,
                "smoke": CLI.EXIT_SMOKE,
            },
            "parse": build_parse_cases(),
            "scope": build_scope_cases(build_workspaces(root)),
        }
        OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        # 与仓库内其他 fixture 一致：直接写字节，避免 Windows 文本模式把 \n 折成 \r\n。
        OUTPUT_PATH.write_bytes(
            (json.dumps(payload, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        )
        print(f"已写入 {OUTPUT_PATH}")
        print("用例统计：" + json.dumps({key: len(value) for key, value in payload.items()}))
        return 0
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
