"""app 类别包：全屏工作台的装配、启动与进程入口。

本文件只做再导出，不存放业务逻辑：
- ``OmniCrawlApp`` 组合类在 ``core.py``；
- ``FullscreenStartup`` 启动参数在 ``startup.py``；
- ``run_fullscreen_tui`` 进程入口在 ``runner.py``。
"""

from .core import OmniCrawlApp
from .runner import run_fullscreen_tui
from .startup import FullscreenStartup

__all__ = ["OmniCrawlApp", "FullscreenStartup", "run_fullscreen_tui"]
