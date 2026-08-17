from __future__ import annotations

from pathlib import Path
from typing import Any

from .runtime import RuntimeConfigError, get_section, load_config_data, save_config_data


APPROVAL_MODE_MANUAL = "manual"
APPROVAL_MODE_AUTO = "auto"
APPROVAL_MODE_REVIEW = "review"
VALID_APPROVAL_MODES = {
    APPROVAL_MODE_MANUAL,
    APPROVAL_MODE_AUTO,
    APPROVAL_MODE_REVIEW,
}

_APPROVAL_MODE_ALIASES = {
    "ask": APPROVAL_MODE_MANUAL,
    "confirm": APPROVAL_MODE_MANUAL,
    "manual": APPROVAL_MODE_MANUAL,
    "off": APPROVAL_MODE_MANUAL,
    "auto": APPROVAL_MODE_AUTO,
    "auto_approve": APPROVAL_MODE_AUTO,
    "approve": APPROVAL_MODE_AUTO,
    "always": APPROVAL_MODE_AUTO,
    "review": APPROVAL_MODE_REVIEW,
    "auto_review": APPROVAL_MODE_REVIEW,
    "reviewed": APPROVAL_MODE_REVIEW,
}

_APPROVAL_MODE_LABELS = {
    APPROVAL_MODE_MANUAL: "人工确认",
    APPROVAL_MODE_AUTO: "完全自动批准",
    APPROVAL_MODE_REVIEW: "自动审查",
}


def normalize_approval_mode(value: Any) -> str:
    """把配置或命令里的审批模式规范化为内部枚举字符串。"""

    if not isinstance(value, str):
        raise RuntimeConfigError("配置项 approval.mode 必须是字符串。")

    normalized = value.strip().lower().replace("-", "_")
    mode = _APPROVAL_MODE_ALIASES.get(normalized)
    if mode is None:
        allowed = ", ".join(sorted(VALID_APPROVAL_MODES))
        raise RuntimeConfigError(f"approval.mode 仅支持 {allowed}，当前值：{value}。")
    return mode


def approval_mode_label(mode: str) -> str:
    """返回审批模式的中文显示名。"""

    return _APPROVAL_MODE_LABELS.get(mode, mode)


def load_approval_mode(config_path: str | Path | None = None) -> str:
    """从 config.toml 读取工具审批模式，默认自动审查。"""

    try:
        data = load_config_data(config_path)
        approval_section = get_section(data, "approval")
    except RuntimeConfigError:
        raise

    if "mode" in approval_section:
        return normalize_approval_mode(approval_section["mode"])
    if approval_section.get("auto_review") is True:
        return APPROVAL_MODE_REVIEW
    if approval_section.get("auto_approve") is True:
        return APPROVAL_MODE_AUTO
    return APPROVAL_MODE_REVIEW


def save_approval_mode(mode: str, config_path: str | Path | None = None) -> Path:
    """把工具审批模式写回 config.toml，保留已有配置项。"""

    normalized_mode = normalize_approval_mode(mode)
    data = load_config_data(config_path)
    approval_section = get_section(data, "approval")
    approval_section["mode"] = normalized_mode
    data["approval"] = approval_section
    return save_config_data(data, config_path)
