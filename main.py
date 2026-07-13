"""项目根启动入口。

开发态：``python main.py`` / ``python main.py plugin ...``
安装后：``omnicrawl`` / ``omnicrawl plugin ...``（见 pyproject.toml console script）

Windows 下普通 TUI 路径仍可弹新 PowerShell 窗口；plugin 子命令必须在弹窗前处理。
"""

from __future__ import annotations

import sys
from pathlib import Path

from omnicrawl.entry import _parse_args, run_application
from omnicrawl.ui.windows_launcher import launch_in_powershell_window


def main(argv: list[str] | None = None) -> int:
    """启动 OmniCrawl 并返回进程退出码；脚本入口负责转换为 SystemExit。"""

    return run_application(argv)


if __name__ == "__main__":
    # plugin 子命令必须在弹新窗口之前处理，否则脚本拿不到真实 stdout/退出码。
    if len(sys.argv) > 1 and sys.argv[1] == "plugin":
        raise SystemExit(main())
    elif not launch_in_powershell_window(Path(__file__)):
        raise SystemExit(main())
