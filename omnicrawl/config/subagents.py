"""SubAgent 运行配置：默认关闭，环境变量只能关闭能力或收紧超时/并发。"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

from .runtime import (
    RuntimeConfigError,
    get_section,
    load_config_data,
    resolve_subagents_path,
    resolve_subagents_write_path,
    save_config_data,
)


_TRUE_VALUES = {"1", "true", "yes", "on", "enabled", "是", "启用"}
_FALSE_VALUES = {"0", "false", "no", "off", "disabled", "否", "禁用"}


class SubAgentConfigError(RuntimeConfigError):
    """SubAgent 配置读取或安全校验失败。"""


_SUBAGENT_ADVANCED_SETTING_RULES: dict[str, tuple[str, float, float]] = {
    "max_concurrency": ("int", 1, 4),
    "max_tasks_per_batch": ("int", 1, 4),
    "default_timeout_seconds": ("number", 1.0, 3600.0),
    "model_request_concurrency": ("int", 1, 4),
    "verify_command_timeout_seconds": ("int", 1, 360),
    "task_retention_minutes": ("int", 1, 10_080),
}
SUBAGENT_ADVANCED_SETTING_KEYS = tuple(_SUBAGENT_ADVANCED_SETTING_RULES)


def validate_subagent_advanced_setting(name: str, value: Any) -> int | float:
    """校验设置面板允许调整的 SubAgent 资源参数。"""

    rule = _SUBAGENT_ADVANCED_SETTING_RULES.get(name)
    if rule is None:
        raise SubAgentConfigError(f"设置面板不支持配置项 subagents.{name}。")
    value_type, minimum, maximum = rule
    if value_type == "int":
        if isinstance(value, bool) or not isinstance(value, int):
            raise SubAgentConfigError(f"配置项 subagents.{name} 必须是整数。")
        normalized: int | float = value
    else:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise SubAgentConfigError(f"配置项 subagents.{name} 必须是数字。")
        normalized = float(value)
    if normalized < minimum or normalized > maximum:
        raise SubAgentConfigError(
            f"配置项 subagents.{name} 必须在 {minimum:g} 到 {maximum:g} 之间，当前值：{normalized:g}。"
        )
    return normalized


@dataclass(frozen=True)
class SubAgentConfig:
    """SubAgent 全局边界。

    Phase 3 开放默认关闭的 Fork / worktree / standard 写 Agent；
    verify 仍需单独显式启用。高权限能力默认关闭，需配置显式打开。

    ``model_overrides`` 按子代理角色名（AgentDefinition.name）单独指定模型；
    取值与任务级 model 一致：models.toml key/alias、profile/model_id 或裸
    model_id，空字符串或 ``inherit`` 表示沿用父模型。
    """

    enabled: bool = False
    max_depth: int = 1
    max_concurrency: int = 2
    max_tasks_per_batch: int = 4
    default_timeout_seconds: float = 3600.0
    model_request_concurrency: int = 2
    allow_background: bool = False
    allow_fork: bool = False
    allow_shared_workspace_writes: bool = False
    allow_worktree: bool = False
    allow_standard_agent: bool = False
    enable_verify_agent: bool = False
    verify_command_timeout_seconds: int = 120
    task_retention_minutes: int = 60
    result_summary_chars: int = 6000
    model_overrides: Mapping[str, str] = field(default_factory=dict)


def load_subagent_config(
    subagents_path: str | Path | None = None,
) -> SubAgentConfig:
    """读取独立 ``subagents.toml`` 的 ``subagents`` 段并执行安全收紧规则。

    子代理设置已从 ``config.toml`` 完全迁移到独立的 ``subagents.toml``，
    不再回退读取 ``config.toml`` 的 ``[subagents]`` 段；未显式指定路径时
    使用 ``resolve_subagents_path()`` 定位默认位置的子代理设置文件。
    """

    try:
        target = (
            Path(subagents_path).expanduser()
            if subagents_path is not None
            else resolve_subagents_path()
        )
        section = get_section(load_config_data(target), "subagents")
    except RuntimeConfigError as exc:
        raise SubAgentConfigError(str(exc)) from exc

    enabled = _bool_field(section, "enabled", False)
    max_depth = _int_field(section, "max_depth", 1, 1, 1)
    max_concurrency = _int_field(section, "max_concurrency", 2, 1, 4)
    max_tasks_per_batch = _int_field(section, "max_tasks_per_batch", 4, 1, 4)
    default_timeout_seconds = _number_field(
        section,
        "default_timeout_seconds",
        3600.0,
        1.0,
        3600.0,
    )
    model_request_concurrency = _int_field(
        section,
        "model_request_concurrency",
        2,
        1,
        4,
    )
    verify_command_timeout_seconds = _int_field(
        section,
        "verify_command_timeout_seconds",
        120,
        1,
        360,
    )
    task_retention_minutes = _int_field(section, "task_retention_minutes", 60, 1, 10_080)
    result_summary_chars = _int_field(section, "result_summary_chars", 6000, 100, 50_000)

    dangerous_flags = {
        name: _bool_field(section, name, False)
        for name in (
            "allow_background",
            "allow_fork",
            "allow_shared_workspace_writes",
            "allow_worktree",
            "allow_standard_agent",
        )
    }
    # Fork / worktree / standard 写 Agent / 共享写入都是高权限能力，默认关闭；
    # 仅当配置显式打开时才放行，避免配置先于实现扩大权限。
    # allow_shared_workspace_writes 允许 shared isolation 下使用写工具（仍逐工具审批）。
    # allow_worktree 允许 isolation=worktree。
    # allow_standard_agent 允许 permissionMode=standard / general-purpose。
    # verify 是唯一已实现的命令型 profile，但仍默认关闭，且只会调用 Host
    # 固定的 argv 检查，不会把配置字符串当作可执行命令。
    enable_verify_agent = _bool_field(section, "enable_verify_agent", False)

    # 环境变量是部署侧的紧急刹车，只能把功能关掉或把上限调小。
    env_enabled = _bool_env("OMNICRAWL_SUBAGENTS_ENABLED")
    if env_enabled is False:
        enabled = False
    env_verify_enabled = _bool_env("OMNICRAWL_SUBAGENT_VERIFY_AGENT_ENABLED")
    if env_verify_enabled is False:
        enable_verify_agent = False
    env_concurrency = _int_env("OMNICRAWL_SUBAGENT_MAX_CONCURRENCY", 1, 4)
    if env_concurrency is not None:
        max_concurrency = min(max_concurrency, env_concurrency)
    env_timeout = _number_env("OMNICRAWL_SUBAGENT_TIMEOUT_SECONDS", 1.0, 3600.0)
    if env_timeout is not None:
        default_timeout_seconds = min(default_timeout_seconds, env_timeout)
    env_verify_timeout = _int_env("OMNICRAWL_SUBAGENT_VERIFY_TIMEOUT_SECONDS", 1, 360)
    if env_verify_timeout is not None:
        verify_command_timeout_seconds = min(
            verify_command_timeout_seconds,
            env_verify_timeout,
        )

    model_overrides = _parse_model_overrides(section)

    return SubAgentConfig(
        enabled=enabled,
        max_depth=max_depth,
        max_concurrency=max_concurrency,
        max_tasks_per_batch=max_tasks_per_batch,
        default_timeout_seconds=float(default_timeout_seconds),
        model_request_concurrency=model_request_concurrency,
        allow_background=dangerous_flags["allow_background"],
        allow_fork=dangerous_flags["allow_fork"],
        allow_shared_workspace_writes=dangerous_flags[
            "allow_shared_workspace_writes"
        ],
        allow_worktree=dangerous_flags["allow_worktree"],
        allow_standard_agent=dangerous_flags["allow_standard_agent"],
        enable_verify_agent=enable_verify_agent,
        verify_command_timeout_seconds=verify_command_timeout_seconds,
        task_retention_minutes=task_retention_minutes,
        result_summary_chars=result_summary_chars,
        model_overrides=model_overrides,
    )


def _parse_model_overrides(section: Mapping[str, Any]) -> Mapping[str, str]:
    """解析 ``[subagents.models.<角色>]`` 段，按角色名返回模型选择。

    每个子段只允许 ``model`` 字段；值为空或 ``inherit`` 时等价于不覆盖，
    解析结果中直接省略，避免与任务级 ``inherit`` 语义混淆。
    """

    raw_models = section.get("models")
    if raw_models in (None, ""):
        return {}
    if not isinstance(raw_models, Mapping):
        raise SubAgentConfigError("配置项 subagents.models 必须是对象。")

    overrides: dict[str, str] = {}
    for role_name, raw_entry in raw_models.items():
        role = str(role_name).strip().casefold()
        if not role:
            continue
        if not isinstance(raw_entry, Mapping):
            raise SubAgentConfigError(
                f"配置项 subagents.models.{role_name} 必须是对象。"
            )
        model = str(raw_entry.get("model") or "").strip()
        if not model or model.casefold() == "inherit":
            continue
        overrides[role] = model
    return overrides


def _bool_field(section: Mapping[str, Any], name: str, default: bool) -> bool:
    value = section.get(name, default)
    if not isinstance(value, bool):
        raise SubAgentConfigError(f"配置项 subagents.{name} 必须是布尔值。")
    return value


def _int_field(
    section: Mapping[str, Any],
    name: str,
    default: int,
    minimum: int,
    maximum: int,
) -> int:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int):
        raise SubAgentConfigError(f"配置项 subagents.{name} 必须是整数。")
    if value < minimum or value > maximum:
        raise SubAgentConfigError(
            f"配置项 subagents.{name} 必须在 {minimum} 到 {maximum} 之间，当前值：{value}。"
        )
    return value


def _number_field(
    section: Mapping[str, Any],
    name: str,
    default: float,
    minimum: float,
    maximum: float,
) -> float:
    value = section.get(name, default)
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise SubAgentConfigError(f"配置项 subagents.{name} 必须是数字。")
    number = float(value)
    if number < minimum or number > maximum:
        raise SubAgentConfigError(
            f"配置项 subagents.{name} 必须在 {minimum:g} 到 {maximum:g} 之间，当前值：{number:g}。"
        )
    return number


def _bool_env(name: str) -> bool | None:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return None
    normalized = raw.strip().casefold()
    if normalized in _TRUE_VALUES:
        return True
    if normalized in _FALSE_VALUES:
        return False
    raise SubAgentConfigError(f"环境变量 {name} 必须是布尔值，当前值：{raw}。")


def _int_env(name: str, minimum: int, maximum: int) -> int | None:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return None
    try:
        value = int(raw.strip())
    except ValueError as exc:
        raise SubAgentConfigError(f"环境变量 {name} 必须是整数，当前值：{raw}。") from exc
    if value < minimum or value > maximum:
        raise SubAgentConfigError(
            f"环境变量 {name} 必须在 {minimum} 到 {maximum} 之间，当前值：{value}。"
        )
    return value


def _number_env(name: str, minimum: float, maximum: float) -> float | None:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return None
    try:
        value = float(raw.strip())
    except ValueError as exc:
        raise SubAgentConfigError(f"环境变量 {name} 必须是数字，当前值：{raw}。") from exc
    if value < minimum or value > maximum:
        raise SubAgentConfigError(
            f"环境变量 {name} 必须在 {minimum:g} 到 {maximum:g} 之间，当前值：{value:g}。"
        )
    return value
