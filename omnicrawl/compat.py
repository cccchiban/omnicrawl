"""Python 兼容入口（已弃用）：把命令行转发给 Rust 宿主 / 内核二进制。

产品入口由 npm 分发的 Rust 二进制承担（`omnicrawl-cli` 启动器 → `omnicrawl-host` 宿主，
`omnicrawl` 内核）。本模块只为旧脚本保留三个历史入口，让它们继续可用但不再承载逻辑：

- ``python main.py``（项目根脚本）
- ``python -m omnicrawl``（模块入口）
- 控制台脚本 ``ocl`` / ``omnicrawl``（`pyproject.toml` 的 ``[project.scripts]``）

转发规则：按同样的参数启动 Rust 二进制并把它的退出码作为本次退出码。二进制按以下顺序查找：

1. ``OMNICRAWL_HOST`` / ``OMNICRAWL_BINARY``（与 npm 启动器同名的逃生口，协议调试与开发用）；
2. ``PATH`` 里的 ``omnicrawl-host``（刻意不查 ``omnicrawl``：控制台脚本本身就叫这个名字）；
3. 仓库内的构建产物 ``rust/target/release/omnicrawl-host[.exe]``。

都找不到时打印迁移提示并以 ``127`` 退出。

开发逃生口：``OMNICRAWL_LEGACY_PYTHON_UI=1`` 或 ``--legacy-python`` 会回退到仓库内保留的
Textual UI（``omnicrawl.entry.run_application``，同样已弃用），仅供开发对照视觉 parity 使用。
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Sequence

#: 回退到仓库内 Textual UI 的开关（环境变量 / 命令行两种写法）。
LEGACY_UI_ENV = "OMNICRAWL_LEGACY_PYTHON_UI"
LEGACY_UI_FLAG = "--legacy-python"
#: 找不到 Rust 二进制时的退出码（沿用 shell 的「命令不可执行」约定）。
EXIT_BINARY_MISSING = 127

DEPRECATION_NOTICE = (
    "提示：Python 启动入口已弃用，正式入口是 npm 安装的 Rust 二进制（omnicrawl-cli）。"
)

MIGRATION_HINT = (
    "未找到 Rust 宿主可执行文件，无法启动 OmniCrawl。\n"
    "  正式安装：npm install -g omnicrawl-cli\n"
    "  本地构建：cargo build --release -p omnicrawl-host\n"
    "  也可用 OMNICRAWL_HOST=<可执行文件> 直接指定路径。\n"
    "  （开发对照 Textual UI：OMNICRAWL_LEGACY_PYTHON_UI=1 或 --legacy-python）"
)

_HOST_FILENAME = "omnicrawl-host.exe" if os.name == "nt" else "omnicrawl-host"


def _repo_root() -> Path | None:
    """仓库根目录：源码检出时本文件位于 ``<root>/omnicrawl/compat.py``。"""

    candidate = Path(__file__).resolve().parent.parent
    if (candidate / "rust" / "Cargo.toml").is_file():
        return candidate
    return None


def rust_binary_candidates() -> list[Path]:
    """按优先级列出候选二进制路径（环境变量 > PATH > 仓库构建产物）。"""

    candidates: list[Path] = []
    for key in ("OMNICRAWL_HOST", "OMNICRAWL_BINARY"):
        value = os.environ.get(key, "").strip()
        if value:
            candidates.append(Path(value))
    found = shutil.which("omnicrawl-host")
    if found:
        candidates.append(Path(found))
    root = _repo_root()
    if root is not None:
        candidates.append(root / "rust" / "target" / "release" / _HOST_FILENAME)
    return candidates


def resolve_rust_binary() -> Path | None:
    """返回第一个存在的候选二进制；都没有则返回 ``None``。"""

    for candidate in rust_binary_candidates():
        try:
            if candidate.is_file():
                return candidate
        except OSError:  # 例如路径过长或权限不足：当作不存在继续找下一个
            continue
    return None


def should_use_legacy_ui(argv: Sequence[str], environ: dict[str, str] | None = None) -> bool:
    """是否走仓库内保留的 Textual UI（环境变量或显式开关）。"""

    env = os.environ if environ is None else environ
    if str(env.get(LEGACY_UI_ENV, "")).strip() not in {"", "0", "false", "False"}:
        return True
    return any(argument == LEGACY_UI_FLAG for argument in argv)


def _run_legacy_ui(argv: Sequence[str]) -> int:
    print(DEPRECATION_NOTICE + " 已按 OMNICRAWL_LEGACY_PYTHON_UI / --legacy-python 使用 Textual UI。", file=sys.stderr)
    from omnicrawl.entry import run_application

    forwarded = [argument for argument in argv if argument != LEGACY_UI_FLAG]
    return int(run_application(forwarded))


def forward_to_rust(argv: Sequence[str] | None = None) -> int:
    """把命令行转发给 Rust 宿主；返回进程退出码。"""

    arguments = list(sys.argv[1:] if argv is None else argv)
    if should_use_legacy_ui(arguments):
        return _run_legacy_ui(arguments)

    binary = resolve_rust_binary()
    if binary is None:
        print(DEPRECATION_NOTICE, file=sys.stderr)
        print(MIGRATION_HINT, file=sys.stderr)
        return EXIT_BINARY_MISSING

    try:
        completed = subprocess.run([str(binary), *arguments], check=False)
    except OSError as exc:
        print(f"启动 {binary} 失败：{exc}", file=sys.stderr)
        print(MIGRATION_HINT, file=sys.stderr)
        return EXIT_BINARY_MISSING
    return int(completed.returncode)


__all__ = [
    "DEPRECATION_NOTICE",
    "EXIT_BINARY_MISSING",
    "LEGACY_UI_ENV",
    "LEGACY_UI_FLAG",
    "MIGRATION_HINT",
    "forward_to_rust",
    "resolve_rust_binary",
    "rust_binary_candidates",
    "should_use_legacy_ui",
]
