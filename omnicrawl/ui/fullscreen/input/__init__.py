"""input 类别包：输入区控件与应用侧联动。

本文件只做再导出，不存放业务逻辑：
- ``Composer`` 控件与粘贴文本归一化在 ``composer.py``；
- ``InputMixin``（粘贴压缩/编辑器自适应/提交/剪贴板/计划区）在 ``editing.py``；
- ``CommandMenuMixin``（斜杠命令菜单）在 ``menu.py``；
- ``SessionsMenuMixin``（会话列表预选菜单）在 ``sessions_menu.py``。
"""

from .composer import Composer, _count_paste_lines, _normalize_pasted_text
from .editing import InputMixin
from .menu import CommandMenuMixin
from .sessions_menu import SessionsMenuMixin

__all__ = [
    "InputMixin",
    "CommandMenuMixin",
    "SessionsMenuMixin",
    "Composer",
    "_count_paste_lines",
    "_normalize_pasted_text",
]
