"""把 Go 搜索核心编译成 CPython 原生扩展（abi3 稳定 ABI）。

用法（构建脚本与 setuptools 的 build_ext 都会调用）：

    python native/build.py --out omnicrawl/_ocsearch.pyd

需要本机具备 Go 工具链与 C 编译器（cgo 要求 gcc/clang，Windows 上是 MinGW-w64，
MSVC 的 cl.exe 不被 cgo 支持）。构建失败时以非零退出码返回，由调用方决定是
回退到「PATH 上的 ripgrep」还是直接报错。
"""

from __future__ import annotations

import argparse
import glob
import os
import shutil
import subprocess
import sys
import sysconfig
from pathlib import Path

# 原生搜索核心的源码目录（本文件所在目录）。
NATIVE_DIR = Path(__file__).resolve().parent

# 扩展模块名：Python 侧 `from omnicrawl import _ocsearch` 依赖它。
MODULE_NAME = "_ocsearch"

# cgo 需要 gcc/clang（不支持 MSVC 的 cl.exe）。Windows 上常见的安装位置不一定
# 在 PATH 中（例如 winget 装到用户目录），这里按顺序兜底探测。
WINDOWS_COMPILER_PATTERNS = (
    r"%LOCALAPPDATA%\Microsoft\WinGet\Packages\*\mingw64\bin\gcc.exe",
    r"%LOCALAPPDATA%\Microsoft\WinGet\Links\*-w64-mingw32-gcc.exe",
    r"%LOCALAPPDATA%\Microsoft\WindowsApps\*-w64-mingw32-gcc.exe",
    r"%ProgramData%\mingw64\bin\gcc.exe",
    r"%ProgramFiles%\mingw64\bin\gcc.exe",
    r"C:\msys64\mingw64\bin\gcc.exe",
    r"C:\msys64\ucrt64\bin\gcc.exe",
    r"C:\mingw64\bin\gcc.exe",
    r"%ProgramData%\Anaconda3\Library\mingw-w64\bin\gcc.exe",
)

# 未找到编译器时给出可执行的安装提示。
COMPILER_HINT = (
    "cgo 需要 gcc 或 clang（MSVC 的 cl.exe 不受支持）："
    "Windows 可执行 winget install BrechtSanders.WinLibs.POSIX.UCRT；"
    "Debian/Ubuntu 安装 build-essential；macOS 安装 Xcode Command Line Tools。"
)


def extension_suffix() -> str:
    """返回 abi3 扩展的文件后缀：Windows 用 .pyd，其余平台用 .so。"""

    return ".pyd" if os.name == "nt" else ".so"


def find_c_compiler() -> Path | None:
    """定位 cgo 可用的 C 编译器：优先 CC 环境变量，其次 PATH，最后已知安装位置。"""

    configured = os.environ.get("CC", "").strip()
    if configured:
        found = shutil.which(configured)
        if found:
            return Path(found)
    for name in ("gcc", "clang", "cc"):
        found = shutil.which(name)
        if found:
            return Path(found)
    if os.name == "nt":
        for pattern in WINDOWS_COMPILER_PATTERNS:
            for match in sorted(glob.glob(os.path.expandvars(pattern))):
                return Path(match)
    return None


def cgo_flags() -> tuple[list[str], list[str]]:
    """返回编译 C 包装层所需的 (CFLAGS, LDFLAGS)。

    Windows 必须链接稳定 ABI 导入库 python3.lib，否则 Py* 符号无法解析；
    Linux/macOS 的扩展模块符号由解释器在加载时提供，无需链接 libpython。
    """

    include_dir = sysconfig.get_paths()["include"]
    cflags = [f"-I{include_dir}", "-DOCSEARCH_WITH_PYTHON"]
    ldflags: list[str] = []
    if os.name == "nt":
        libs_dir = Path(sys.base_prefix) / "libs"
        if not (libs_dir / "python3.lib").is_file():
            raise RuntimeError(f"未找到 abi3 导入库：{libs_dir / 'python3.lib'}")
        ldflags = [f"-L{libs_dir}", "-lpython3"]
    return cflags, ldflags


def build(out_path: Path, *, quiet: bool = False) -> None:
    """编译并写出扩展产物；失败时抛出 RuntimeError。"""

    # go build 的工作目录固定为 native/，相对输出路径必须相对调用方解析，
    # 否则产物会落到 native/ 而不是 wheel 内的包目录。
    out_path = Path(out_path).resolve()
    go = shutil.which("go")
    if go is None:
        raise RuntimeError("未找到 go 可执行文件，无法构建原生搜索扩展。")
    compiler = find_c_compiler()
    if compiler is None:
        raise RuntimeError(COMPILER_HINT)

    cflags, ldflags = cgo_flags()
    environment = dict(os.environ)
    environment["CGO_ENABLED"] = "1"
    # 显式指定编译器：winget/msys2 等安装方式不一定把 gcc 放进 PATH。
    environment["CC"] = str(compiler)
    environment["CGO_CFLAGS"] = " ".join([*cflags, environment.get("CGO_CFLAGS", "")]).strip()
    environment["CGO_LDFLAGS"] = " ".join([*ldflags, environment.get("CGO_LDFLAGS", "")]).strip()

    out_path.parent.mkdir(parents=True, exist_ok=True)
    if out_path.exists():
        out_path.unlink()

    command = [
        go,
        "build",
        "-trimpath",
        "-buildmode=c-shared",
        # 去掉符号表与 DWARF，产物体积更接近"只留必要代码"的目标。
        "-ldflags=-s -w",
        "-o",
        str(out_path),
        "./cmod",
    ]
    result = subprocess.run(
        command,
        cwd=str(NATIVE_DIR),
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        raise RuntimeError(f"go build 失败（退出码 {result.returncode}）：{detail}")
    if not out_path.is_file():
        raise RuntimeError(f"go build 未产出预期文件：{out_path}")

    # c-shared 会顺带生成同名 C 头文件，不是 wheel 需要的产物。
    generated_header = out_path.with_suffix(".h")
    if generated_header.is_file():
        generated_header.unlink()

    if not quiet:
        size_mb = out_path.stat().st_size / (1024 * 1024)
        print(f"已构建原生搜索扩展：{out_path}（{size_mb:.1f}MB）")


def main() -> int:
    parser = argparse.ArgumentParser(description="构建 omnicrawl 原生搜索扩展")
    parser.add_argument(
        "--out",
        required=True,
        help="扩展产物路径，通常为 omnicrawl/_ocsearch.pyd（或 .so）",
    )
    parser.add_argument("--quiet", action="store_true", help="只输出错误信息")
    arguments = parser.parse_args()
    try:
        build(Path(arguments.out).resolve(), quiet=arguments.quiet)
    except RuntimeError as error:
        print(f"构建原生搜索扩展失败：{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
