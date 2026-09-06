"""Slug 安全验证单元测试。

覆盖：安全名称放行、路径遍历/分隔符/点/空白/控制字符拒绝、长度上限、
规范化（去首尾空白）与错误消息携带字段名。
"""
from __future__ import annotations

import unittest

from omnicrawl.workspace.slug import (
    SlugSafetyError,
    is_safe_slug,
    validate_slug,
)

SAFE_NAMES = (
    "abc",
    "ABC_123",
    "a-b_c",
    "w1a2b3-f00",
    "single",
    "x" * 64,
)

UNSAFE_NAMES = (
    "",
    "   ",
    "../x",
    "..",
    "a..b",
    "a/b",
    "a\\b",
    "a b",
    "a\tb",
    "a\nb",
    "a.b",
    ".hidden",
    "x" * 65,
    "aw-../../evil",
)


class SlugSafetyTests(unittest.TestCase):
    def test_safe_names_accepted(self) -> None:
        for name in SAFE_NAMES:
            with self.subTest(name=name):
                self.assertTrue(is_safe_slug(name), name)
                self.assertEqual(validate_slug(name), name)

    def test_unsafe_names_rejected(self) -> None:
        for name in UNSAFE_NAMES:
            with self.subTest(name=name):
                self.assertFalse(is_safe_slug(name), name)
                with self.assertRaises(SlugSafetyError):
                    validate_slug(name)

    def test_validate_slug_strips_outer_whitespace(self) -> None:
        self.assertEqual(validate_slug("  my-id  "), "my-id")

    def test_validate_slug_reports_field_in_error(self) -> None:
        with self.assertRaises(SlugSafetyError) as ctx:
            validate_slug("../evil", field="隔离区实例 ID")
        self.assertIn("隔离区实例 ID", str(ctx.exception))

    def test_validate_slug_rejects_too_long(self) -> None:
        with self.assertRaises(SlugSafetyError) as ctx:
            validate_slug("x" * 65)
        self.assertIn("长度超过", str(ctx.exception))

    def test_validate_slug_rejects_empty(self) -> None:
        with self.assertRaises(SlugSafetyError) as ctx:
            validate_slug("")
        self.assertIn("不能为空", str(ctx.exception))

    def test_max_length_is_configurable(self) -> None:
        self.assertTrue(is_safe_slug("x" * 128, max_length=128))
        self.assertFalse(is_safe_slug("x" * 129, max_length=128))


if __name__ == "__main__":
    unittest.main()