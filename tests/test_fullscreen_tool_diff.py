from __future__ import annotations

import unittest

from omnicrawl.ui.fullscreen.tool_diff import (
    describe_file_change,
    gutter_diff_text,
    plain_tool_title,
    tool_disclosure_body,
)


class FullscreenToolDiffTest(unittest.TestCase):
    def test_replace_text_title_and_gutter_diff(self) -> None:
        arguments = {
            "path": "omnicrawl/ui/fullscreen/hud.py",
            "old_text": "alpha\nbeta\ngamma\n",
            "new_text": "alpha\nbeta2\ngamma\n",
        }
        title = plain_tool_title(
            tool_name="replace_text",
            arguments=arguments,
            status="成功",
            duration_seconds=0.13,
        )
        self.assertIn("M", title)
        self.assertIn("hud.py", title)
        self.assertIn("+1", title)
        self.assertIn("-1", title)
        self.assertIn("成功", title)

        body = tool_disclosure_body(
            tool_name="replace_text",
            arguments=arguments,
            result_text="已修改 hud.py，替换 1 处。",
        ).plain
        self.assertIn("│", body)
        self.assertIn("- ", body)
        self.assertIn("+ ", body)
        self.assertIn("beta", body)
        self.assertIn("beta2", body)
        self.assertIn("结果：已修改", body)

    def test_write_file_overwrite_uses_rewrite_stats_without_fake_deletes(self) -> None:
        arguments = {
            "path": "notes/demo.txt",
            "mode": "overwrite",
            "content": "line-one\nline-two\n",
        }
        change = describe_file_change("write_file", arguments)
        self.assertEqual(change.status_code, "M")
        self.assertEqual(change.stats_label, "rewrite +2 lines")
        body = change.body.plain
        self.assertIn("rewrite", body)
        self.assertIn("+ ", body)
        self.assertIn("line-one", body)
        # 3A：没有旧内容时不得伪造删除行。
        self.assertNotIn("\n- ", "\n" + body.replace("+ ", "PLUS "))

        title = plain_tool_title(
            tool_name="write_file",
            arguments=arguments,
            status="成功",
            duration_seconds=0.02,
        )
        self.assertIn("rewrite +2 lines", title)

    def test_write_file_append_label(self) -> None:
        arguments = {
            "path": "log.txt",
            "mode": "append",
            "content": "a\nb\n",
        }
        change = describe_file_change("write_file", arguments)
        self.assertEqual(change.stats_label, "append +2 lines")
        self.assertIn("append", change.body.plain)

    def test_non_file_tools_keep_legacy_title_and_body(self) -> None:
        title = plain_tool_title(
            tool_name="read_file",
            arguments={"path": "README.md"},
            status="成功",
            duration_seconds=0.13,
        )
        self.assertEqual(title, "▸ ⌁ read_file · 成功 · 0.13s")
        body = tool_disclosure_body(
            tool_name="read_file",
            arguments={"path": "README.md"},
            result_text="读取完成",
        ).plain
        self.assertIn("参数：", body)
        self.assertIn("README.md", body)
        self.assertIn("结果：", body)
        self.assertIn("读取完成", body)

    def test_gutter_diff_counts_insert_and_delete_lines(self) -> None:
        text, added, removed = gutter_diff_text("a\nb\nc\n", "a\nx\nc\n")
        self.assertEqual(added, 1)
        self.assertEqual(removed, 1)
        plain = text.plain
        self.assertIn("b", plain)
        self.assertIn("x", plain)


if __name__ == "__main__":
    unittest.main()
