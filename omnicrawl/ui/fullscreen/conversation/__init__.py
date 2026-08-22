"""conversation 类别包：会话视图管理。

本文件只做再导出，不存放业务逻辑：``ConversationViewMixin`` 在
``view.py``；历史事件的重放渲染管线在 ``fullscreen/rendering.py``。
"""

from .view import ConversationViewMixin

__all__ = ["ConversationViewMixin"]
