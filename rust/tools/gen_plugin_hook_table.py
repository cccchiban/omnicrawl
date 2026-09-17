#!/usr/bin/env python3
"""生成插件钩子契约表，供 `packages/plugin-sdk` 与 Cordis 宿主读取。

单一真相仍是 `omnicrawl/extensions/plugin_models.py`：本脚本从 Python 真实现抽取 Core Hook、
每个 Hook 允许的 Handler 模式与 JSON Patch 白名单，写成 SDK 可直接读取的 JSON。
Python 侧改了钩子却没重新生成时，`packages/plugin-host` 的契约测试会红。

用法：``python rust/tools/gen_plugin_hook_table.py``
输出：``packages/plugin-sdk/src/hooks.generated.json``
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUTPUT_PATH = ROOT / "packages/plugin-sdk/src/hooks.generated.json"

sys.path.insert(0, str(ROOT))

import omnicrawl.extensions.plugin_models as models  # noqa: E402

if not Path(models.__file__).resolve().is_relative_to(ROOT):
    raise SystemExit(f"加载到的不是仓库源码：{models.__file__}")


def main() -> None:
    hooks = {}
    for hook in sorted(models.CORE_HOOKS):
        modes = models.HOOK_ALLOWED_MODES.get(hook)
        if not modes:
            raise SystemExit(f"{hook} 没有声明允许的 Handler 模式，契约表不完整。")
        hooks[hook] = {
            "modes": sorted(modes),
            "patch": sorted(models.HOOK_PATCH_ALLOWLIST.get(hook, ())),
        }

    table = {
        "api_version": models.HOOK_API_VERSION,
        "handler_modes": sorted(models.HANDLER_MODES),
        "hooks": hooks,
    }

    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text(
        json.dumps(table, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(
        f"已写入 {OUTPUT_PATH.relative_to(ROOT)}："
        f"api_version={table['api_version']}、钩子 {len(hooks)} 个、"
        f"模式 {table['handler_modes']}"
    )


if __name__ == "__main__":
    main()
