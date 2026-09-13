"""原生搜索后端与 ripgrep 的同负载基准。

用法：

    python native/benchmark.py                    # 自动探测语料（仓库 + numpy + site-packages）
    python native/benchmark.py --rg C:\\bin\\rg.exe
    python native/benchmark.py --corpus D:\\repo --large D:\\biger

对比对象是同一条 ripgrep 参数（两种后端共用），因此差异只来自实现本身。
`--rg` 不可用时只跑原生后端的绝对耗时，不做倍数对比。
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import sys
import sysconfig
import time
from pathlib import Path

PROJECT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(PROJECT))

from omnicrawl.workspace.search_backend import (  # noqa: E402
    native_backend_available,
    run_search,
)
from omnicrawl.workspace.tools import WorkspaceTools  # noqa: E402

# 两种后端都带上：隐藏文件可见 + 非 git 工作区也读 .gitignore，与项目调用一致。
BASE = ["--hidden", "--no-require-git"]


def build_workloads(project: Path, medium: Path, large: Path) -> list[tuple]:
    """返回 (名称, cwd, argv) 列表；语料不存在时自动跳过。"""

    def workload(name: str, root: Path, extra: list[str], pattern: str | None) -> tuple:
        args = list(extra) + BASE
        args += ["--", pattern, str(root)] if pattern is not None else ["--", str(root)]
        return name, root, args

    loads = [("仓库 json 字面量", project, ["--json", "--line-number"], "workspace")]
    if medium.is_dir():
        loads += [
            ("中量 正则(忽略大小写)", medium, ["--json", "--line-number", "--ignore-case"], r"def\s+test\w*\("),
            ("中量 --count", medium, ["--count", "--with-filename"], "import"),
            ("中量 --files-with-matches", medium, ["--files-with-matches", "-m", "1"], "import"),
        ]
    if large.is_dir():
        loads += [
            ("全站 --files(*.py)", large, ["--files", "--glob", "*.py"], None),
            ("全站 稀有字面量(*.py)", large, ["--json", "--line-number", "--glob", "*.py"], "zzzz_never_matches_zzzz"),
        ]
    return [workload(*load) for load in loads]


def measure(args: list[str], cwd: Path, binary: Path | None, runs: int) -> tuple[float, int]:
    """返回 (最快耗时, 输出字节数)。"""

    timings: list[float] = []
    stdout = ""
    for _ in range(runs):
        start = time.perf_counter()
        stdout, _code = run_search(args, cwd=cwd, timeout=600, ripgrep_binary=binary)
        timings.append(time.perf_counter() - start)
    return min(timings), len(stdout)


def detect_medium_corpus() -> Path:
    """中量语料优先用 numpy 源码目录（README 记录的那套），缺失时退回 site-packages。"""

    spec = importlib.util.find_spec("numpy")
    if spec is not None and spec.origin:
        candidate = Path(spec.origin).parent
        if candidate.is_dir():
            return candidate
    return Path(sysconfig.get_paths()["purelib"])


def main() -> int:
    default_large = Path(sysconfig.get_paths()["purelib"])
    parser = argparse.ArgumentParser(description="原生搜索后端 vs ripgrep 基准")
    parser.add_argument("--corpus", type=Path, default=detect_medium_corpus(), help="中量语料（默认 numpy，缺失时 site-packages）")
    parser.add_argument("--large", type=Path, default=default_large, help="大量语料（默认 site-packages）")
    parser.add_argument("--rg", type=Path, default=None, help="对比用 rg 可执行文件")
    parser.add_argument("--runs", type=int, default=3, help="每个负载的计时次数（取最快）")
    arguments = parser.parse_args()

    if not native_backend_available():
        print("原生扩展不可用：先运行 python native/build.py --out omnicrawl/_ocsearch.pyd")
        return 1
    rg = arguments.rg if arguments.rg and arguments.rg.is_file() else None
    if arguments.rg and rg is None:
        print(f"指定的 rg 不存在：{arguments.rg}，只测原生后端")

    print(f"Python {sys.version.split()[0]} | CPU {os.cpu_count()} 核 | rg: {rg or '未提供'}")
    header = f"{'负载':<26}{'native':>11}{'rg':>11}{'native/rg':>11}{'输出 nat/rg':>16}"
    print(header)
    print("-" * len(header))

    for name, root, args in build_workloads(PROJECT, arguments.corpus, arguments.large):
        cwd = root if root.is_dir() else root.parent
        # 先各跑一次预热文件缓存，再交替计时，避免先跑的一方独占缓存优势。
        run_search(args, cwd=cwd, timeout=600, ripgrep_binary=None)
        if rg is not None:
            run_search(args, cwd=cwd, timeout=600, ripgrep_binary=rg)

        native_time, native_bytes = measure(args, cwd, None, arguments.runs)
        if rg is None:
            print(f"{name:<26}{native_time:>10.3f}s{'—':>11}{'—':>11}{native_bytes:>16}")
            continue
        rg_time, rg_bytes = measure(args, cwd, rg, arguments.runs)
        ratio = f"{native_time / rg_time:.2f}x" if rg_time else "—"
        print(f"{name:<26}{native_time:>10.3f}s{rg_time:>10.3f}s{ratio:>11}{f'{native_bytes}/{rg_bytes}':>16}")

    if rg is not None and arguments.corpus.is_dir():
        print()
        print("=== 工具层（WorkspaceTools.grep，含 Python 解析/排序/过滤）===")
        for label, tools in (
            ("native", WorkspaceTools(arguments.corpus)),
            ("rg", WorkspaceTools(arguments.corpus, ripgrep_binary=rg)),
        ):
            timings = []
            for _ in range(arguments.runs):
                start = time.perf_counter()
                output = tools.grep({"pattern": "import", "count": True, "max_results": 5})
                timings.append(time.perf_counter() - start)
            print(f"{label:<8}count 模式 min={min(timings):.3f}s 输出 {len(output)} 字符")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
