from __future__ import annotations

import unittest

from omnicrawl.ui.fullscreen.hud import search_index_status_text
from omnicrawl.workspace.search_index import SearchIndexStatus
from omnicrawl.ui.fullscreen.tool_diff import (
    describe_file_change,
    gutter_diff_text,
    plain_tool_title,
    tool_disclosure_body,
    tool_disclosure_title,
)


class FullscreenToolDiffTest(unittest.TestCase):
    def test_tool_title_uses_semantic_colors_without_bold_font(self) -> None:
        title = tool_disclosure_title(
            tool_name="replace_text",
            arguments={
                "path": "omnicrawl/mcp/server.py",
                "old_text": "a\nb\nc\n",
                "new_text": "a\n",
            },
            status="成功",
            duration_seconds=0.044,
            expanded=False,
        )

        def style_for(fragment: str) -> str:
            offset = title.plain.index(fragment)
            for span in title.spans:
                if span.start <= offset < span.end:
                    return str(span.style)
            self.fail(f"未找到片段样式：{fragment}")

        self.assertEqual(style_for("M"), "yellow")
        self.assertEqual(style_for("omnicrawl/mcp/server.py"), "default")
        self.assertEqual(style_for("-2"), "red")
        self.assertEqual(style_for("✓ 成功"), "green")
        self.assertEqual(style_for("44ms"), "dim")
        self.assertTrue(all("bold" not in str(span.style) for span in title.spans))

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

    def test_read_title_includes_file_name_and_returned_line_range(self) -> None:
        title = plain_tool_title(
            tool_name="read",
            arguments={"path": "omnicrawl/ui/fullscreen/tool_diff.py"},
            status="成功",
            duration_seconds=0.136,
            result_text="18: first\n19: second\n20: third",
        )
        self.assertIn("R  omnicrawl/ui/fullscreen/tool_diff.py", title)
        self.assertIn("tool_diff.py", title)
        self.assertIn("第 18-20 行", title)
        self.assertNotIn("3 行", title)
        self.assertIn("✓ 成功  136ms", title)

    def test_grep_title_includes_search_path_and_target(self) -> None:
        title = plain_tool_title(
            tool_name="grep",
            arguments={"path": "omnicrawl/ui", "pattern": "read_file"},
            status="成功",
            duration_seconds=0.136,
            result_text="omnicrawl/ui/tool_diff.py:1: read_file",
        )
        self.assertIn("G  omnicrawl/ui  |  目标: read_file", title)
        self.assertNotIn("文件: omnicrawl/ui", title)
        self.assertIn("目标: read_file", title)
        self.assertIn("✓ 成功  136ms", title)

    def test_find_title_includes_search_path_and_target(self) -> None:
        title = plain_tool_title(
            tool_name="find",
            arguments={"path": "omnicrawl", "pattern": "agent"},
            status="成功",
            duration_seconds=0.02,
            result_text="omnicrawl/agent/",
        )
        self.assertIn("F  omnicrawl  |  目标: agent", title)

    def test_loading_index_status_uses_thinking_spinner_frames(self) -> None:
        """加载/构建期间显示与状态指示器同款的十帧旋转动画 + “加载索引”。"""

        from omnicrawl.ui.fullscreen.hud import SEARCH_INDEX_SPINNER_FRAMES

        status = SearchIndexStatus(file_state="loading", content_state="loading")
        self.assertEqual(len(SEARCH_INDEX_SPINNER_FRAMES), 10)
        for frame in range(len(SEARCH_INDEX_SPINNER_FRAMES)):
            rendered = search_index_status_text(status, frame)
            self.assertEqual(
                rendered.plain,
                f"{SEARCH_INDEX_SPINNER_FRAMES[frame]} 加载索引",
            )
            self.assertNotEqual(
                search_index_status_text(status, frame).plain,
                search_index_status_text(status, frame + 1).plain,
            )

    def test_index_progress_text_is_visible_only_while_loading_or_building(self) -> None:
        building = search_index_status_text(
            SearchIndexStatus(
                file_state="ready",
                content_state="building",
                content_processed=25,
                content_total=100,
            )
        )
        ready = search_index_status_text(
            SearchIndexStatus(file_state="ready", content_state="ready")
        )
        self.assertEqual(building.plain, "⠋ 加载索引")
        self.assertEqual(ready.plain, "")

    def test_read_title_does_not_claim_lines_before_result(self) -> None:
        title = plain_tool_title(
            tool_name="read",
            arguments={"path": "README.md"},
            status="调用中",
            duration_seconds=0.0,
        )
        self.assertIn("README.md", title)
        self.assertNotIn("行", title)

    def test_list_title_uses_operation_summary(self) -> None:
        title = plain_tool_title(
            tool_name="list",
            arguments={"path": ".", "recursive": False},
            status="成功",
            duration_seconds=1.8,
            result_text="README.md\nomnicrawl/\ntests/",
        )
        self.assertEqual(title, "L  .  |  3 项  ✓ 成功  1.8s")
        self.assertNotIn("列出文件", title)
        self.assertNotIn("≡", title)

    def test_command_tool_title_uses_only_symbol_and_context(self) -> None:
        title = plain_tool_title(
            tool_name="bash",
            arguments={"command": "pytest -q tests/test_fullscreen_tool_diff.py"},
            status="成功",
            duration_seconds=1.8,
        )
        self.assertEqual(
            title,
            "B  pytest -q tests/test_fullscreen_tool_diff.py  ✓ 成功  1.8s",
        )
        self.assertNotIn("Bash", title)

        title = plain_tool_title(
            tool_name="powershell",
            arguments={"command": "Get-ChildItem"},
            status="成功",
            duration_seconds=0.13,
        )
        self.assertEqual(title, "P  Get-ChildItem  ✓ 成功  130ms")
        self.assertNotIn("PowerShell", title)

        title = plain_tool_title(
            tool_name="read",
            arguments={"path": "README.md"},
            status="成功",
            duration_seconds=0.13,
        )
        self.assertNotIn("▸", title)
        self.assertIn("R  README.md", title)
        body = tool_disclosure_body(
            tool_name="read",
            arguments={"path": "README.md"},
            result_text="读取完成",
        ).plain
        mcp_body = tool_disclosure_body(
            tool_name="trusted.read",
            arguments={"path": "README.md"},
            result_text="读取完成",
        ).plain
        self.assertIn("工具：trusted.read", mcp_body)
        self.assertIn("R  (未指定文件)", plain_tool_title(
            tool_name="trusted.read",
            arguments={},
            status="成功",
            duration_seconds=0.001,
        ))
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
