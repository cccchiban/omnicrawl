"""兼容旧版 pip/setuptools 的安装入口。

现代元数据以 pyproject.toml 为准；本文件只做两件事：调用 setuptools.setup()，
并把随包分发的原生搜索扩展（Go + abi3）挂到 build_ext 上就地编译。
"""

from __future__ import annotations

import importlib.util
import os
import sys
from pathlib import Path
from types import ModuleType

from setuptools import Extension, setup
from setuptools.command.build_ext import build_ext

# 原生扩展的构建脚本（native/build.py），由 build_ext 直接调用。
NATIVE_BUILD_SCRIPT = Path(__file__).resolve().parent / "native" / "build.py"

# 设为 1 时构建失败直接中断安装；默认降级为"不带原生扩展"，运行时回退到
# PATH 上的 ripgrep。发布 wheel 时应在有 Go 与 C 编译器的环境里设为 1。
REQUIRE_NATIVE_ENV = "OMNICRAWL_REQUIRE_NATIVE_SEARCH"


def _load_native_build_module() -> ModuleType:
    """加载 native/build.py；它不在包内，因此用文件路径导入。"""

    spec = importlib.util.spec_from_file_location(
        "omnicrawl_native_build", NATIVE_BUILD_SCRIPT
    )
    if spec is None or spec.loader is None:
        raise RuntimeError(f"无法加载原生扩展构建脚本：{NATIVE_BUILD_SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class NativeSearchBuildExt(build_ext):
    """把 Go 搜索核心编译成包内的 _ocsearch.pyd/_ocsearch.so。

    产物使用 abi3 稳定 ABI（Py_LIMITED_API），一份文件即可覆盖 Python 3.9+，
    不需要按解释器版本各出一份。
    """

    def get_ext_filename(self, ext_name: str) -> str:
        """返回不带解释器标签的文件名，保证 abi3 产物能被 importlib 直接加载。"""

        return os.path.join(*ext_name.split(".")) + (
            ".pyd" if os.name == "nt" else ".so"
        )

    def build_extension(self, ext: Extension) -> None:
        """用 Go 工具链构建扩展（不经过常规 C 编译流程）。"""

        target = Path(self.get_ext_fullpath(ext.name))
        try:
            _load_native_build_module().build(target, quiet=True)
        except Exception as error:  # noqa: BLE001 - 缺失工具链时降级而不是中断
            message = f"跳过原生搜索扩展构建（{ext.name}）：{error}"
            if os.environ.get(REQUIRE_NATIVE_ENV) == "1":
                raise RuntimeError(message) from error
            print(f"warning: {message}", file=sys.stderr)
            return
        print(f"已构建原生搜索扩展：{target}")


setup(
    ext_modules=[
        Extension(
            "omnicrawl._ocsearch",
            sources=["native/cmod/module.c"],
            # 声明稳定 ABI：wheel 平台标签会变成 cp39-abi3-<platform>，
            # 一份产物即可覆盖 Python 3.9+，不必按解释器版本各出一份。
            py_limited_api=True,
        )
    ],
    cmdclass={"build_ext": NativeSearchBuildExt},
    # 显式声明 abi3 版本：与 module.c 的 Py_LIMITED_API 0x03090000 对应。
    options={"bdist_wheel": {"py_limited_api": "cp39"}},
)
