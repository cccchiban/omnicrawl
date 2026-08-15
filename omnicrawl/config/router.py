"""任务思维模式路由器（dsh-routing-suite 移植）配置。

配置段（默认关闭）：

```toml
[router]
enabled = false
mode = "standard"   # standard（RL 接口还原）/ spec（深度思考优先）
```

"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

from .runtime import RuntimeConfigError, get_section, load_config_data


ROUTER_SECTION = "router"
ROUTER_MODES = ("standard", "spec")
DEFAULT_ROUTER_ENABLED = False
DEFAULT_ROUTER_MODE = "standard"


def validate_router_mode(mode: str) -> str:
    """校验 router.mode；非法值抛配置错误。"""

    if not isinstance(mode, str) or mode not in ROUTER_MODES:
        raise RuntimeConfigError(
            f"router.mode 必须是 {' / '.join(ROUTER_MODES)} 之一。"
        )
    return mode


@dataclass(frozen=True)
class RouterConfig:
    """任务路由功能配置。"""

    enabled: bool = DEFAULT_ROUTER_ENABLED
    mode: str = DEFAULT_ROUTER_MODE

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise RuntimeConfigError("router.enabled 必须是布尔值。")
        validate_router_mode(self.mode)

    def as_dict(self) -> dict[str, Any]:
        return {"enabled": self.enabled, "mode": self.mode}


def load_router_config(
    config_path: str | Path | None = None,
) -> RouterConfig:
    """从 ``config.toml`` 的 ``[router]`` 段读取路由配置。

    段缺失或读取失败时返回默认值（关闭 + standard），不阻断启动。
    """

    try:
        data: Mapping[str, Any] = load_config_data(config_path)
        section = get_section(data, ROUTER_SECTION)
    except RuntimeConfigError:
        return RouterConfig()

    enabled = section.get("enabled", DEFAULT_ROUTER_ENABLED)
    mode = section.get("mode", DEFAULT_ROUTER_MODE)
    if not isinstance(enabled, bool):
        raise RuntimeConfigError("router.enabled 必须是布尔值。")
    return RouterConfig(enabled=enabled, mode=validate_router_mode(str(mode)))


def load_router_enabled(config_path: str | Path | None = None) -> bool:
    """便捷读取开关，供 entry/API 构造 AgentConfig 使用。"""

    return load_router_config(config_path).enabled


def load_router_mode(config_path: str | Path | None = None) -> str:
    """便捷读取路由模式。"""

    return load_router_config(config_path).mode


__all__ = [
    "DEFAULT_ROUTER_ENABLED",
    "DEFAULT_ROUTER_MODE",
    "ROUTER_MODES",
    "ROUTER_SECTION",
    "RouterConfig",
    "load_router_config",
    "load_router_enabled",
    "load_router_mode",
    "validate_router_mode",
]
