"""前端 UI 配置加载。"""

from __future__ import annotations

from dataclasses import dataclass

from .runtime_config import RuntimeConfigError, get_section, load_config_data


@dataclass
class FrontendConfig:
    """前端 UI 配置。"""

    type: str = "tui"


def load_frontend_config() -> FrontendConfig:
    """从 config.json 加载前端配置。"""
    data = load_config_data()
    section = get_section(data, "frontend")

    frontend_type = section.get("type", "tui")
    if frontend_type not in ("tui", "qt"):
        raise RuntimeConfigError(
            f"frontend.type 可选值为 tui 或 qt，当前值：{frontend_type!r}"
        )

    return FrontendConfig(type=frontend_type)
