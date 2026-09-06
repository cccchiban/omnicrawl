from __future__ import annotations

import random
import unittest

from rich.text import Text

from omnicrawl.ui.fullscreen.status.hud import (
    EROSION_FRACTION,
    GARBLE_CHARS,
    decrypt_frame,
    token_telemetry_text,
)
from omnicrawl.ui.fullscreen.rendering.tool_diff import (
    describe_file_change,
    gutter_diff_text,
    plain_tool_title,
    tool_disclosure_body,
    tool_disclosure_title,
)


class FullscreenToolDiffTest(unittest.TestCase):
    def test_tool_title_uses_semantic_colors_without_bold_font(self) -> None:
        title = tool_disclosure_title(
            tool_name="Edit_file",
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

        # 方案6：只有行首状态色点带语义色；名称亮色，上下文/状态/耗时暗色。
        self.assertEqual(style_for("●"), "green")
        self.assertEqual(style_for("Edit_file"), "default")
        self.assertEqual(style_for("omnicrawl/mcp/server.py"), "dim")
        self.assertEqual(style_for("-2"), "red")
        self.assertEqual(style_for("✓ 成功"), "dim")
        self.assertEqual(style_for("44ms"), "dim")
        self.assertTrue(all("bold" not in str(span.style) for span in title.spans))

    def test_Edit_file_title_keeps_change_summary_and_body_shows_diff(self) -> None:
        """Edit_file 标题保留文件变更摘要；正文展示旁注行号 diff 与结果。"""

        arguments = {
            "path": "omnicrawl/ui/fullscreen/hud.py",
            "old_text": "alpha\nbeta\ngamma\n",
            "new_text": "alpha\nbeta2\ngamma\n",
        }
        title = plain_tool_title(
            tool_name="Edit_file",
            arguments=arguments,
            status="成功",
            duration_seconds=0.13,
        )
        self.assertIn("Edit_file", title)
        self.assertIn("hud.py", title)
        self.assertIn("+1", title)
        self.assertIn("-1", title)
        self.assertIn("成功", title)

        body = tool_disclosure_body(
            tool_name="Edit_file",
            arguments=arguments,
            result_text=(
                "已修改 hud.py，替换 1 处。\n"
                "首个替换位置上下文（第 1-3 行，前后各 2 行）：\n"
                "1: alpha\n2: beta2\n3: gamma"
            ),
        )
        # 与 write_file 一致：正文展示旁注行号 diff 预览；结果区只保留
        # “替换 N 处”摘要，不再整段展示带行号上下文文本；旁注行号从
        # 工具返回的真实文件行号开始。
        self.assertIn("@@ snippet @@", body.plain)
        self.assertIn("+ beta2", body.plain)
        self.assertIn("- beta", body.plain)
        self.assertIn("结果：替换 1 处", body.plain)
        self.assertNotIn("首个替换位置上下文", body.plain)
        self.assertNotIn("1: alpha", body.plain)
        self.assertNotIn("2: beta2", body.plain)

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

    def test_write_file_body_keeps_change_preview_and_full_result(self) -> None:
        """write_file 是唯一豁免工具：正文保留文件变更预览与完整结果文本。"""

        body = tool_disclosure_body(
            tool_name="write_file",
            arguments={
                "path": "notes/demo.txt",
                "content": "line-one\nline-two\n",
            },
            result_text="已写入 notes/demo.txt。",
        ).plain
        self.assertIn("rewrite", body)
        self.assertIn("+ ", body)
        self.assertIn("line-one", body)
        self.assertIn("已写入 notes/demo.txt。", body)

    def test_edit_file_result_area_shows_only_summary_not_line_context(self) -> None:
        """Edit_file 正文不整段展示带行号上下文；write_file 结果区保持完整。"""

        edit_body = tool_disclosure_body(
            tool_name="Edit_file",
            arguments={
                "path": "src/app.py",
                "old_text": "old",
                "new_text": "new",
            },
            result_text=(
                "已修改 src/app.py，替换 2 处。\n"
                "首个替换位置上下文（第 3-7 行，前后各 2 行）：\n"
                "3: alpha\n4: beta\n5: new\n6: gamma\n7: delta"
            ),
        ).plain
        self.assertIn("结果：替换 2 处", edit_body)
        self.assertNotIn("已修改 src/app.py", edit_body)
        self.assertNotIn("首个替换位置上下文", edit_body)
        self.assertNotIn("3: alpha", edit_body)
        # 旁注行号使用文件真实行号（替换位置起始 5），而非 snippet 内计数。
        self.assertIn("   5 │ - old", edit_body)
        self.assertIn("   5 │ + new", edit_body)

        write_body = tool_disclosure_body(
            tool_name="write_file",
            arguments={
                "path": "notes/demo.txt",
                "content": "line-one\nline-two\n",
            },
            result_text="已写入 notes/demo.txt。",
        ).plain
        self.assertIn("结果：已写入 notes/demo.txt。", write_body)

    def test_read_title_includes_file_name_and_returned_line_range(self) -> None:
        title = plain_tool_title(
            tool_name="read",
            arguments={"path": "omnicrawl/ui/fullscreen/tool_diff.py"},
            status="成功",
            duration_seconds=0.136,
            result_text="18: first\n19: second\n20: third",
        )
        self.assertIn("● read omnicrawl/ui/fullscreen/tool_diff.py", title)
        self.assertIn("tool_diff.py", title)
        self.assertIn("第 18-20 行", title)
        self.assertNotIn("3 行", title)
        self.assertIn("✓ 成功 · 136ms", title)

    def test_grep_title_includes_search_path_and_target(self) -> None:
        title = plain_tool_title(
            tool_name="grep",
            arguments={"path": "omnicrawl/ui", "pattern": "read_file"},
            status="成功",
            duration_seconds=0.136,
            result_text="omnicrawl/ui/tool_diff.py:1: read_file",
        )
        self.assertIn("● grep omnicrawl/ui · 目标: read_file", title)
        self.assertNotIn("文件: omnicrawl/ui", title)
        self.assertIn("目标: read_file", title)
        self.assertIn("✓ 成功 · 136ms", title)

    def test_find_title_includes_search_path_and_target(self) -> None:
        title = plain_tool_title(
            tool_name="find",
            arguments={"path": "omnicrawl", "pattern": "agent"},
            status="成功",
            duration_seconds=0.02,
            result_text="omnicrawl/agent/",
        )
        self.assertIn("● find omnicrawl · 目标: agent", title)

    def test_fetcher_title_uses_tool_name_and_body_shows_meta_only(self) -> None:
        """fetcher 标题显示工具名；正文保留汇总/URL/状态/标题，隐藏页面正文。"""

        title = plain_tool_title(
            tool_name="fetcher",
            arguments={"urls": "https://example.com"},
            status="成功",
            duration_seconds=1.24,
        )
        self.assertIn("fetcher", title)
        self.assertIn("✓ 成功 · 1.2s", title)

        body = tool_disclosure_body(
            tool_name="fetcher",
            arguments={"urls": "https://example.com"},
            result_text=(
                "网页抓取完成（1 个 URL，用时 0.50s）\n"
                "1. https://example.com\n"
                "   状态: 200｜最终地址: https://example.com\n"
                "   标题: Example Domain\n"
                "   内容: 这是一个示例页面正文……"
            ),
        ).plain
        self.assertIn("网页抓取完成", body)
        self.assertIn("https://example.com", body)
        self.assertIn("状态: 200", body)
        self.assertIn("Example Domain", body)
        self.assertNotIn("这是一个示例页面正文", body)
        self.assertNotIn("（网页正文内容已隐藏）", body)

    def test_fetcher_body_skips_multiline_content_and_keeps_next_entry(self) -> None:
        """多行页面正文整块隐藏；后续条目的 URL/状态/失败信息原样保留。"""

        body = tool_disclosure_body(
            tool_name="fetcher",
            arguments={"urls": "https://a.example,https://b.example"},
            result_text=(
                "网页抓取完成（2 个 URL，用时 0.80s）\n"
                "1. https://a.example\n"
                "   状态: 200｜最终地址: https://a.example\n"
                "   标题: 页面 A\n"
                "   内容: 第一段正文\n"
                "   第二段正文\n"
                "2. https://b.example\n"
                "   失败: 连接超时"
            ),
        ).plain
        self.assertIn("https://a.example", body)
        self.assertIn("页面 A", body)
        self.assertNotIn("第一段正文", body)
        self.assertNotIn("第二段正文", body)
        self.assertIn("https://b.example", body)
        self.assertIn("失败: 连接超时", body)
        self.assertNotIn("已省略 2 行", body)

    def test_cache_rate_uses_input_tokens_as_denominator(self) -> None:
        """顶部 CH% 是缓存率：缓存命中输入 ÷ 本次总输入，而非上下文窗口。"""

        # 常规：CA 是 IN 的子集，比率不超过 100%。
        telemetry = token_telemetry_text(
            input_tokens=62_500,
            output_tokens=2_400,
            cached_input_tokens=50_000,
            context_limit=100_000,
        )
        self.assertIn("CH80%", telemetry.plain)
        # 无输入时缓存率视为 0%，不因上下文窗口出现虚假比例。
        self.assertIn(
            "CH0%",
            token_telemetry_text(0, 0, 0, 128_000).plain,
        )
        # 异常数据（CA > IN）封顶 100%。
        self.assertIn(
            "CH100%",
            token_telemetry_text(1_000, 0, 2_000, 128_000).plain,
        )

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
        self.assertEqual(title, "● list . · 3 项 · ✓ 成功 · 1.8s")
        self.assertIn("list", title)
        self.assertNotIn("≡", title)

    def test_command_tool_title_uses_name_and_context(self) -> None:
        title = plain_tool_title(
            tool_name="bash",
            arguments={"command": "pytest -q tests/test_fullscreen_tool_diff.py"},
            status="成功",
            duration_seconds=1.8,
        )
        self.assertEqual(
            title,
            "● bash pytest -q tests/test_fullscreen_tool_diff.py · ✓ 成功 · 1.8s",
        )
        self.assertIn("bash", title)

        title = plain_tool_title(
            tool_name="powershell",
            arguments={"command": "Get-ChildItem"},
            status="成功",
            duration_seconds=0.13,
        )
        self.assertEqual(title, "● powershell Get-ChildItem · ✓ 成功 · 130ms")
        self.assertIn("powershell", title)

        title = plain_tool_title(
            tool_name="read",
            arguments={"path": "README.md"},
            status="成功",
            duration_seconds=0.13,
        )
        self.assertNotIn("▸", title)
        self.assertIn("● read README.md", title)
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
        # read 正文不展示给用户：正文为空，不保留任何“已隐藏”提示。
        self.assertEqual(body, "")
        self.assertEqual(mcp_body, "")
        self.assertIn("● trusted.read (未指定文件)", plain_tool_title(
            tool_name="trusted.read",
            arguments={},
            status="成功",
            duration_seconds=0.001,
        ))

    def test_memory_write_tools_hide_empty_result_body(self) -> None:
        """写入类记忆工具的输出（空记录列表）对用户无意义，正文完全隐藏。

        与 read 一致：正文为空，不保留任何“已隐藏”提示行，标题行照常
        保留工具名与状态。
        """

        for tool_name in (
            "memory_write",
            "project_memory_write",
            "session_memory_write",
            "user_memory_write",
        ):
            body = tool_disclosure_body(
                tool_name=tool_name,
                arguments={"memories": [{"content": "示例"}]},
                result_text="[\n  {\n\n  }\n]",
            ).plain
            self.assertEqual(body, "", tool_name)

        title = plain_tool_title(
            tool_name="project_memory_write",
            arguments={"memories": [{"content": "示例"}]},
            status="成功",
            duration_seconds=0.08,
        )
        self.assertIn("project_memory_write", title)
        self.assertIn("✓ 成功 · 80ms", title)

    def test_memory_search_body_hidden_like_all_memory_tools(self) -> None:
        """全部记忆工具（含搜索/读取）正文均不展示，只保留标题行。"""

        body = tool_disclosure_body(
            tool_name="project_memory_search",
            arguments={"query": "TUI"},
            result_text="找到 2 条记忆\n1. 第一条\n2. 第二条",
        ).plain
        self.assertEqual(body, "")

        title = plain_tool_title(
            tool_name="project_memory_search",
            arguments={"query": "TUI"},
            status="成功",
            duration_seconds=0.08,
        )
        self.assertIn("project_memory_search", title)

    def test_gutter_diff_counts_insert_and_delete_lines(self) -> None:
        text, added, removed = gutter_diff_text("a\nb\nc\n", "a\nx\nc\n")
        self.assertEqual(added, 1)
        self.assertEqual(removed, 1)
        plain = text.plain
        self.assertIn("b", plain)
        self.assertIn("x", plain)

    def test_gutter_diff_start_line_uses_real_file_line_numbers(self) -> None:
        """旁注行号从 start_line 开始，而非 snippet 内相对计数。"""

        text, added, removed = gutter_diff_text(
            "old",
            "new",
            start_line=5,
        )
        self.assertEqual((added, removed), (1, 1))
        self.assertIn("   5 │ - old", text.plain)
        self.assertIn("   5 │ + new", text.plain)
        self.assertNotIn("   1 │ ", text.plain)

    def test_gutter_diff_default_start_line_is_one(self) -> None:
        """未指定 start_line 时保持相对计数（从 1 开始）。"""

        text, _, _ = gutter_diff_text("old", "new")
        self.assertIn("   1 │ - old", text.plain)
        self.assertIn("   1 │ + new", text.plain)

    def test_ask_user_title_shows_waiting_then_received_states(self) -> None:
        """ask_user 工具卡：提问期间「↘ 等待回复...」，回答后「↗ 已收到回复」。"""

        waiting = tool_disclosure_title(
            tool_name="ask_user",
            arguments={},
            status="等待回复",
            duration_seconds=3.2,
            expanded=True,
        )
        self.assertIn("↘ 等待回复...", waiting.plain)
        self.assertIn("3.2s", waiting.plain)
        self.assertNotIn("ask_user", waiting.plain)

        received = tool_disclosure_title(
            tool_name="ask_user",
            arguments={},
            status="已收到回复",
            duration_seconds=6.4,
            expanded=True,
        )
        self.assertIn("↗ 已收到回复", received.plain)
        self.assertIn("6.4s", received.plain)

        # 取消等异常状态回退到通用“工具名 + 状态”标题。
        cancelled = tool_disclosure_title(
            tool_name="ask_user",
            arguments={},
            status="已取消",
            duration_seconds=1.0,
            expanded=True,
        )
        self.assertIn("ask_user", cancelled.plain)
        self.assertIn("↷ 已取消", cancelled.plain)


class CarouselDecryptFrameTest(unittest.TestCase):
    """底部轮播切换的解密扫描特效：旧文本被乱码侵蚀、新文本由乱码吐出。"""

    def setUp(self) -> None:
        self.old_text = Text("旧行内容一二三四五")
        self.new_text = Text("甲乙丙丁戊己庚辛壬癸")

    def test_endpoints_show_old_then_new_text(self) -> None:
        """进度 0 完整显示旧文本，进度 1 完整显示新文本。"""

        self.assertEqual(
            decrypt_frame(self.old_text, self.new_text, 0.0).plain,
            self.old_text.plain,
        )
        self.assertEqual(
            decrypt_frame(self.old_text, self.new_text, 1.0).plain,
            self.new_text.plain,
        )

    def test_erosion_replaces_old_text_with_garble(self) -> None:
        """侵蚀阶段结束前：旧文本被乱码从左到右吃光（双宽字替换为两格乱码）。"""

        frame = decrypt_frame(
            self.old_text,
            self.new_text,
            max(0.0, EROSION_FRACTION - 0.001),
            rand_source=random.Random(3),
        )
        # 9 个旧宽字 -> 18 个乱码格，不再残留旧文本字符。
        self.assertTrue(all(ch in GARBLE_CHARS for ch in frame.plain))
        self.assertEqual(len(frame.plain), 18)
        self.assertNotEqual(frame.plain, self.new_text.plain)

    def test_reveal_decodes_new_text_from_left_to_right(self) -> None:
        """解密阶段：扫描波左侧已吐出清晰新文本，右侧仍是乱码。"""

        frame = decrypt_frame(
            self.old_text,
            self.new_text,
            0.9,
            rand_source=random.Random(5),
        )
        # 进度 0.9：front = int(10 * 0.45/0.55) = 8，前 8 个新字符已解密。
        self.assertTrue(frame.plain.startswith("甲乙丙丁戊己庚辛"))
        self.assertNotEqual(frame.plain, self.new_text.plain)

    def test_early_erosion_keeps_tail_old_text_intact(self) -> None:
        """侵蚀刚开始：只有左端少量字符变乱码，其余仍是旧文本。"""

        frame = decrypt_frame(
            self.old_text,
            self.new_text,
            0.02,
            rand_source=random.Random(1),
        )
        # front = ceil(10 * 0.02/0.45) = 1：首个宽字被替换（2 格乱码）。
        self.assertTrue(frame.plain[0] in GARBLE_CHARS)
        self.assertTrue(frame.plain[1] in GARBLE_CHARS)
        self.assertTrue(frame.plain.endswith("行内容一二三四五"))

    def test_mid_reveal_shimmer_eventually_resolves(self) -> None:
        """解密中途持续向新文本收敛：乱码只在波前右侧，且最终全部落定。"""

        mid = decrypt_frame(
            self.old_text,
            self.new_text,
            0.6,
            rand_source=random.Random(9),
        )
        # front = int(10 * 0.15/0.55) = 2：前 2 个新字符已解密。
        self.assertTrue(mid.plain.startswith("甲乙"))
        self.assertTrue(
            all(
                ch in GARBLE_CHARS or ch in self.new_text.plain
                for ch in mid.plain[len("甲乙") :]
            )
        )


if __name__ == "__main__":
    unittest.main()
