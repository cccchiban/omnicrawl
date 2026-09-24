"""OmniCrawl 的 Python 侧业务模块包（**冻结的语义基准**）。

本包不再是产品实现。产品链路已全部迁到 Rust（内核 + 宿主 + TUI + API + MCP + 连接器 + TTS），
由 npm 分发；仓库根的 ``main.py`` / ``python -m omnicrawl`` 只是转发到 Rust 二进制的垫片。

本包现在的唯一职责是充当 **parity 语义基准**：

- ``rust/tools/gen_*.py`` 从这里取期望值，生成 ``crates/*/tests/fixtures/*_parity.json``；
- ``tests/`` 的对照测试直接调用这里，验证 Rust 实现与基准逐字段一致。

因此——

- **不要**在这里新增产品功能；功能缺口一律补齐到 ``rust/crates/``；
- **不要**让 ``rust/crates/**/src/`` 依赖 Python 解释器（由
  ``rust/tools/check_frozen_reference.mjs`` 在 CI 上强制检查）；
- 改动这里的语义实现时，必须重跑对应的 ``rust/tools/gen_*_fixture.py`` 并让 Rust 测试继续通过，
  否则两侧契约会静默分叉。

操作手册见 ``rust/docs/frozen-reference.md``。
"""

from __future__ import annotations

import importlib
import importlib.util
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

# 迁移到子包的模块保留旧路径别名（``omnicrawl.fetcher`` 等）。它们带 curl_cffi/bs4
# 等重依赖，或本就按需加载，故用惰性查找器替代上面的急切导入，避免每次导入本包时
# 都把它们拉起来。
_LAZY_COMPAT_MODULES = {
    f"{__name__}.fetcher": f"{__name__}.net.fetcher",
    f"{__name__}.web_search": f"{__name__}.net.web_search",
    f"{__name__}.http_client": f"{__name__}.net.http_client",
    f"{__name__}.image_gen": f"{__name__}.media.image_gen",
    f"{__name__}.paths": f"{__name__}.common.paths",
    f"{__name__}.redaction": f"{__name__}.common.redaction",
    f"{__name__}.documentation": f"{__name__}.common.documentation",
    f"{__name__}.updater": f"{__name__}.maintenance.updater",
    f"{__name__}.version_check": f"{__name__}.maintenance.version_check",
}


class _CompatAliasLoader:
    """把旧模块名解析为已加载的新模块对象。"""

    def __init__(self, module):
        self._module = module

    def create_module(self, spec):
        return self._module

    def exec_module(self, module):
        pass


class _CompatAliasFinder:
    """让 ``omnicrawl.<旧名>`` 仍可导入，且不产生额外导入开销。"""

    def find_spec(self, fullname, path=None, target=None):
        target_name = _LAZY_COMPAT_MODULES.get(fullname)
        if target_name is None:
            return None
        module = importlib.import_module(target_name)
        return importlib.util.spec_from_loader(fullname, _CompatAliasLoader(module))


sys.meta_path.insert(0, _CompatAliasFinder())
