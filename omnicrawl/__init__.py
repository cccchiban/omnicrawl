"""OmniCrawl 的业务模块包。"""

from __future__ import annotations

import importlib
import sys

_COMPAT_MODULES = {
    "approval": ".config.features.approval",
    # llm 已升级为真实包 omnicrawl.llm，并再导出 config.models.llm 的公共配置 API。
    # 不再把 sys.modules["omnicrawl.llm"] 指向 config.llm，避免遮蔽多模型运行时。
    "model_catalog": ".config.models.model_catalog",
    "runtime_config": ".config.core.runtime",
    "workspace_tools": ".workspace.tools",
    "project_context": ".workspace.context",
    "temp_workspace": ".workspace.temp",
    "project": ".state.project",
    "session": ".state.session",
    "memory": ".state.memory",
    "skill": ".extensions.skill",
    "slash_commands": ".commands.slash",
}

for _old_name, _new_name in _COMPAT_MODULES.items():
    _module = importlib.import_module(_new_name, __name__)
    sys.modules[f"{__name__}.{_old_name}"] = _module
    globals()[_old_name] = _module

# 确保真实 llm 包可被 `import omnicrawl.llm` 与兼容路径同时使用。
from . import llm as llm  # noqa: E402
