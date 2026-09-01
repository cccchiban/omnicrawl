"""Agent 运行节奏护栏与自动续跑配置。"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

from ..core.runtime import (
    RuntimeConfigError,
    get_section,
    load_config_data,
    save_config_data,
)


class RunGuardConfigError(RuntimeConfigError):
    """运行节奏护栏配置缺失、类型错误或超出安全边界。"""


@dataclass(frozen=True)
class ReasoningGuardConfig:
    """单次模型调用的 reasoning 死循环检测参数。"""

    enabled: bool = True
    window_chars: int = 2_000
    substr_len: int = 32
    repeat_ratio: float = 0.7
    check_every: int = 50
    max_blocks: int = 10_000
    max_chars: int = 500_000
    max_guard_retries: int = 2
    # 白名单使用 provider-neutral 错误码（见 omnicrawl/llm/errors.py::ModelErrorCode）；
    # 网关等上游业务错误码/文本可追加到该列表，configured_retry_code 支持
    # 从异常属性或异常文本中匹配用户配置的任意字符串。
    auto_retry_errors: tuple[str, ...] = ("SERVICE_UNAVAILABLE",)

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise RunGuardConfigError("run_guard.guard.enabled 必须是布尔值。")
        _require_int_range(
            "window_chars",
            self.window_chars,
            minimum=64,
            maximum=1_000_000,
        )
        _require_int_range("substr_len", self.substr_len, minimum=8, maximum=128)
        _require_float_range(
            "repeat_ratio",
            self.repeat_ratio,
            minimum=0.0,
            maximum=1.0,
        )
        _require_int_range("check_every", self.check_every, minimum=1, maximum=100_000)
        _require_int_range("max_blocks", self.max_blocks, minimum=100, maximum=10_000_000)
        _require_int_range("max_chars", self.max_chars, minimum=1_000, maximum=100_000_000)
        _require_int_range("max_guard_retries", self.max_guard_retries, minimum=0, maximum=10)
        if not isinstance(self.auto_retry_errors, (tuple, list)):
            raise RunGuardConfigError(
                "run_guard.guard.auto_retry_errors 必须是字符串数组。"
            )
        normalized: list[str] = []
        for raw_code in self.auto_retry_errors:
            if not isinstance(raw_code, str) or not raw_code.strip():
                raise RunGuardConfigError(
                    "run_guard.guard.auto_retry_errors 的每项必须是非空字符串。"
                )
            code = raw_code.strip()
            if code not in normalized:
                normalized.append(code)
        if len(normalized) > 32:
            raise RunGuardConfigError(
                "run_guard.guard.auto_retry_errors 最多允许 32 个错误码。"
            )
        object.__setattr__(self, "auto_retry_errors", tuple(normalized))


@dataclass(frozen=True)
class ContinueConfig:
    """Todo 未完成和 reasoning-only 停止时的自动续跑参数。"""

    enabled: bool = True
    max_auto_followups: int = 3

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise RunGuardConfigError("run_guard.continue.enabled 必须是布尔值。")
        _require_int_range(
            "max_auto_followups",
            self.max_auto_followups,
            minimum=1,
            maximum=20,
        )


@dataclass(frozen=True)
class RunGuardConfig:
    """运行节奏功能总配置，对应 ``config.toml`` 的 ``[run_guard]`` 段。"""

    enabled: bool = True
    # 使用 default_factory，避免在本模块底部的校验辅助函数定义前
    # 提前实例化嵌套配置；同时保留每个 RunGuardConfig 独立的不可变默认对象。
    guard: ReasoningGuardConfig = field(default_factory=ReasoningGuardConfig)
    continuation: ContinueConfig = field(default_factory=ContinueConfig)

    def __post_init__(self) -> None:
        if not isinstance(self.enabled, bool):
            raise RunGuardConfigError("run_guard.enabled 必须是布尔值。")
        if not isinstance(self.guard, ReasoningGuardConfig):
            raise RunGuardConfigError("run_guard.guard 配置对象无效。")
        if not isinstance(self.continuation, ContinueConfig):
            raise RunGuardConfigError("run_guard.continue 配置对象无效。")


def save_run_guard_config(
    config: RunGuardConfig,
    config_path: str | Path | None = None,
) -> Path:
    """保留其他配置段，只写回完整的 ``run_guard`` 配置。"""

    if not isinstance(config, RunGuardConfig):
        raise RunGuardConfigError("运行节奏护栏配置对象无效。")
    try:
        data = load_config_data(config_path)
        data["run_guard"] = {
            "enabled": config.enabled,
            "guard": {
                "enabled": config.guard.enabled,
                "window_chars": config.guard.window_chars,
                "substr_len": config.guard.substr_len,
                "repeat_ratio": config.guard.repeat_ratio,
                "check_every": config.guard.check_every,
                "max_blocks": config.guard.max_blocks,
                "max_chars": config.guard.max_chars,
                "max_guard_retries": config.guard.max_guard_retries,
                "auto_retry_errors": list(config.guard.auto_retry_errors),
            },
            "continue": {
                "enabled": config.continuation.enabled,
                "max_auto_followups": config.continuation.max_auto_followups,
            },
        }
        return save_config_data(data, config_path)
    except RunGuardConfigError:
        raise
    except RuntimeConfigError as exc:
        raise RunGuardConfigError(str(exc)) from exc


def load_run_guard_config(
    config_path: str | Path | None = None,
) -> RunGuardConfig:
    """读取并严格校验 ``config.toml`` 的 ``run_guard`` 段。"""

    try:
        section = get_section(load_config_data(config_path), "run_guard")
        guard_section = _nested_section(section, "guard")
        continue_section = _nested_section(section, "continue")
        _reject_unknown_fields(
            section,
            allowed={"enabled", "guard", "continue"},
            prefix="run_guard",
        )
        _reject_unknown_fields(
            guard_section,
            allowed={
                "enabled",
                "window_chars",
                "substr_len",
                "repeat_ratio",
                "check_every",
                "max_blocks",
                "max_chars",
                "max_guard_retries",
                "auto_retry_errors",
            },
            prefix="run_guard.guard",
        )
        _reject_unknown_fields(
            continue_section,
            allowed={"enabled", "max_auto_followups"},
            prefix="run_guard.continue",
        )
        defaults = RunGuardConfig()
        guard_defaults = defaults.guard
        continue_defaults = defaults.continuation
        return RunGuardConfig(
            enabled=section.get("enabled", defaults.enabled),
            guard=ReasoningGuardConfig(
                enabled=guard_section.get("enabled", guard_defaults.enabled),
                window_chars=guard_section.get("window_chars", guard_defaults.window_chars),
                substr_len=guard_section.get("substr_len", guard_defaults.substr_len),
                repeat_ratio=guard_section.get("repeat_ratio", guard_defaults.repeat_ratio),
                check_every=guard_section.get("check_every", guard_defaults.check_every),
                max_blocks=guard_section.get("max_blocks", guard_defaults.max_blocks),
                max_chars=guard_section.get("max_chars", guard_defaults.max_chars),
                max_guard_retries=guard_section.get(
                    "max_guard_retries",
                    guard_defaults.max_guard_retries,
                ),
                auto_retry_errors=guard_section.get(
                    "auto_retry_errors",
                    guard_defaults.auto_retry_errors,
                ),
            ),
            continuation=ContinueConfig(
                enabled=continue_section.get("enabled", continue_defaults.enabled),
                max_auto_followups=continue_section.get(
                    "max_auto_followups",
                    continue_defaults.max_auto_followups,
                ),
            ),
        )
    except RunGuardConfigError:
        raise
    except RuntimeConfigError as exc:
        raise RunGuardConfigError(str(exc)) from exc
    except (TypeError, ValueError) as exc:
        raise RunGuardConfigError(f"run_guard 配置字段类型无效：{exc}") from exc


def _nested_section(section: Mapping[str, Any], name: str) -> dict[str, Any]:
    value = section.get(name, {})
    if value in (None, ""):
        return {}
    if not isinstance(value, dict):
        raise RunGuardConfigError(f"配置项 run_guard.{name} 必须是对象。")
    return dict(value)


def _reject_unknown_fields(
    section: Mapping[str, Any],
    *,
    allowed: set[str],
    prefix: str,
) -> None:
    unknown = sorted(set(section) - allowed)
    if unknown:
        raise RunGuardConfigError(f"{prefix} 包含未知配置项：{', '.join(unknown)}")


def _require_int_range(name: str, value: Any, *, minimum: int, maximum: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise RunGuardConfigError(
            f"run_guard.{name} 必须是 {minimum} 到 {maximum} 之间的整数。"
        )


def _require_float_range(
    name: str,
    value: Any,
    *,
    minimum: float,
    maximum: float,
) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RunGuardConfigError(f"run_guard.guard.{name} 必须是数字。")
    numeric = float(value)
    if numeric < minimum or numeric > maximum:
        raise RunGuardConfigError(
            f"run_guard.guard.{name} 必须满足 {minimum} <= value <= {maximum}。"
        )


__all__ = [
    "ContinueConfig",
    "ReasoningGuardConfig",
    "RunGuardConfig",
    "RunGuardConfigError",
    "load_run_guard_config",
    "save_run_guard_config",
]
