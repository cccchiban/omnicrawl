"""OmniCrawl 的业务模块包。"""

from __future__ import annotations

import importlib
import sys

_COMPAT_MODULES = {
    "approval": ".config.approval",
    "llm": ".config.llm",
    "model_catalog": ".config.model_catalog",
    "runtime_config": ".config.runtime",
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
