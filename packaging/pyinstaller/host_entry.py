"""PyInstaller 宿主入口：冻结产物里唯一的可执行入口。

源码运行请用 ``omnicrawl`` / ``ocl`` 控制台脚本，本文件只被 ``omnicrawl-host.spec`` 使用。
"""

from __future__ import annotations

import multiprocessing

from omnicrawl.entry import run_application


if __name__ == "__main__":
    # 可选依赖（uvicorn workers、插件 runner）在部分平台走 spawn/fork；冻结产物必须
    # 显式打开这个钩子，否则子进程会重新执行入口脚本、再拉起一个工作台。
    multiprocessing.freeze_support()
    raise SystemExit(run_application())
