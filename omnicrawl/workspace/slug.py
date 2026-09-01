"""Slug 安全验证：校验名称能否安全用作单一路径段，避免路径遍历攻击。

任何把外部/不可信名称拼进文件系统路径的地方（目录名、元数据文件名、
git 分支名等）都应先通过 ``validate_slug`` 或 ``is_safe_slug`` 校验。

安全规则（fail-closed）：
- 非空；
- 只允许 ASCII 字母、数字、下划线、连字符（``[A-Za-z0-9_-]``）——
  不含 ``.``（因此不可能出现 ``..``）、路径分隔符（``/`` ``\\``）、
  空白与控制字符，保证拼接后仍是单个路径段；
- 长度 1..64（默认），避免超长名称造成的平台路径问题。

项目内已有按模块的类似校验（如 ``validate_skill_name``），本模块提供
与具体业务无关的通用规则；业务模块应依据自身命名空间决定宽松程度，
例如隔离工作区的实例 ID 可以含连字符/下划线。
"""

from __future__ import annotations

import re


class SlugSafetyError(ValueError):
    """名称不是安全 slug 时抛出，消息包含具体原因，便于上层包装成业务错误。"""


DEFAULT_MAX_LENGTH = 64
# 单一路径段：字母/数字/下划线/连字符，无点、无分隔符、无空白/控制字符。
SAFE_SLUG_PATTERN = re.compile(r"^[A-Za-z0-9_-]+$")


def is_safe_slug(name: str, *, max_length: int = DEFAULT_MAX_LENGTH) -> bool:
    """判断名称是否为安全 slug（可用于拼接单一路径段）。"""

    if not isinstance(name, str) or not name.strip():
        return False
    if len(name) > max_length:
        return False
    return SAFE_SLUG_PATTERN.fullmatch(name) is not None


def validate_slug(
    name: str,
    *,
    field: str = "名称",
    max_length: int = DEFAULT_MAX_LENGTH,
) -> str:
    """校验并返回规范化后的 slug；不安全时抛出 ``SlugSafetyError``。

    只做去除首尾空白与校验，不做替换/截断：调用方传入不可信名称时应
    校验失败而不是静默改写，避免「净化后仍被拼错」的歧义。
    """

    raw = str(name or "").strip()
    if not raw:
        raise SlugSafetyError(f"{field}不能为空。")
    if len(raw) > max_length:
        raise SlugSafetyError(f"{field}长度超过 {max_length} 字符（当前 {len(raw)} 字符）。")
    if SAFE_SLUG_PATTERN.fullmatch(raw) is None:
        raise SlugSafetyError(
            f"{field}只能包含字母、数字、下划线与连字符，且不能包含点、路径"
            f"分隔符、空格等字符。实际值：{raw!r}"
        )
    return raw


__all__ = [
    "DEFAULT_MAX_LENGTH",
    "SAFE_SLUG_PATTERN",
    "SlugSafetyError",
    "is_safe_slug",
    "validate_slug",
]