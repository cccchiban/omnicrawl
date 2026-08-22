"""turn 类别包：回合执行与命令分派。

本文件只做再导出，不存放业务逻辑：``TurnExecutionMixin`` 在 ``execution.py``。
"""

from .execution import TurnExecutionMixin

__all__ = ["TurnExecutionMixin"]
