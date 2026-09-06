from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent.toolkit.knowledge_tools import (
    kb_append_result,
    kb_list_result,
    kb_read_result,
    kb_search_result,
    kb_write_result,
)
from omnicrawl.agent.toolkit.tools import build_agent_tools
from omnicrawl.agent.types import ToolResult
from omnicrawl.knowledge import KnowledgeBase, KnowledgeBaseError


class KnowledgeBaseCoreTest(unittest.TestCase):
    def _make_base(self, temp_dir: str) -> KnowledgeBase:
        return KnowledgeBase(Path(temp_dir))

    def test_ensure_layout_creates_skeleton(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.ensure_layout()

            root = Path(temp_dir)
            self.assertTrue((root / "projects").is_dir())
            self.assertTrue((root / "topics").is_dir())
            self.assertTrue((root / "daily").is_dir())
            self.assertTrue((root / "attachments").is_dir())
            self.assertTrue((root / "README.md").is_file())
            self.assertTrue((root / "INDEX.md").is_file())

    def test_write_creates_note_with_frontmatter_and_index(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            meta = base.write(
                "projects/客户A/2026-06-18-会议纪要",
                "讨论了交付时间线。",
                title="客户A会议纪要",
                project="客户A",
                tags=["会议", "需求"],
                note_type="meeting",
                status="done",
            )

            self.assertEqual(meta.rel_path, "projects/客户A/2026-06-18-会议纪要.md")
            self.assertEqual(meta.title, "客户A会议纪要")
            self.assertEqual(meta.project, "客户A")
            self.assertEqual(meta.type, "meeting")
            self.assertEqual(meta.status, "done")
            self.assertEqual(meta.tags, ("会议", "需求"))
            self.assertTrue(meta.created)
            self.assertTrue(meta.updated)

            text = base.read("projects/客户A/2026-06-18-会议纪要.md")
            self.assertIn("title: 客户A会议纪要", text)
            self.assertIn("type: meeting", text)
            self.assertIn("讨论了交付时间线。", text)
            index = (Path(temp_dir) / "INDEX.md").read_text(encoding="utf-8")
            self.assertIn("客户A会议纪要", index)
            self.assertIn("projects/客户A/2026-06-18-会议纪要.md", index)

    def test_write_overwrite_preserves_created_and_merges_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            first = base.write(
                "topics/规划/roadmap.md",
                "第一版正文",
                title="路线图",
                project="旧项目",
                note_type="note",
            )
            second = base.write(
                "topics/规划/roadmap.md",
                "第二版正文",
                project="新项目",
                status="done",
            )

            self.assertEqual(second.created, first.created)
            self.assertEqual(second.title, "路线图")
            self.assertEqual(second.project, "新项目")
            self.assertEqual(second.status, "done")
            text = base.read("topics/规划/roadmap.md")
            self.assertIn("第二版正文", text)
            self.assertNotIn("第一版正文", text)

    def test_write_create_rejects_existing_note(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write("daily/2026/06/18.md", "第一天")

            with self.assertRaises(KnowledgeBaseError):
                base.write("daily/2026/06/18.md", "重复", mode="create")

    def test_append_keeps_frontmatter_and_appends_body(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write(
                "projects/demo/notes.md",
                "第一段",
                title="Demo 笔记",
                project="demo",
                tags=["demo"],
            )
            meta = base.append("projects/demo/notes.md", "追加段落")

            self.assertEqual(meta.title, "Demo 笔记")
            text = base.read("projects/demo/notes.md")
            self.assertIn("第一段", text)
            self.assertIn("追加段落", text)
            self.assertLess(text.index("第一段"), text.index("追加段落"))

    def test_read_requires_existing_note(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            with self.assertRaises(KnowledgeBaseError):
                base.read("projects/nonexistent.md")

    def test_path_traversal_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            with self.assertRaises(KnowledgeBaseError):
                base.write("../escape.md", "越界")

    def test_search_filters_and_returns_snippets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write(
                "projects/alpha/note-one.md",
                "Alpha 项目讨论数据库索引方案。",
                title="Alpha 数据库方案",
                project="alpha",
                tags=["数据库"],
                note_type="decision",
                status="done",
            )
            base.write(
                "projects/beta/note-two.md",
                "Beta 项目关注前端动画。",
                title="Beta 前端",
                project="beta",
                tags=["前端"],
            )

            by_keyword = base.search("数据库")
            self.assertEqual(len(by_keyword), 1)
            self.assertEqual(by_keyword[0].project, "alpha")
            self.assertIn("数据库索引", by_keyword[0].snippet)

            by_project = base.search("项目", project="beta")
            self.assertEqual(len(by_project), 1)
            self.assertEqual(by_project[0].rel_path, "projects/beta/note-two.md")

            by_type = base.search("项目", note_type="decision")
            self.assertEqual(len(by_type), 1)
            self.assertEqual(by_type[0].project, "alpha")

            by_tag = base.search("项目", tags=["前端"])
            self.assertEqual(len(by_tag), 1)
            self.assertEqual(by_tag[0].project, "beta")

            no_match = base.search("不存在的关键词")
            self.assertEqual(no_match, [])

    def test_list_entries_filters_by_path_and_fields(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write("projects/alpha/a.md", "A", project="alpha", tags=["x"])
            base.write("projects/alpha/b.md", "B", project="alpha", note_type="log")
            base.write("topics/ui/c.md", "C", project="", note_type="note")

            all_notes = base.list_entries()
            self.assertEqual(len(all_notes), 3)

            project_notes = base.list_entries(project="alpha")
            self.assertEqual(len(project_notes), 2)

            dir_notes = base.list_entries(rel_path="projects/alpha")
            self.assertEqual(len(dir_notes), 2)

            type_notes = base.list_entries(note_type="log")
            self.assertEqual(len(type_notes), 1)
            self.assertEqual(type_notes[0].rel_path, "projects/alpha/b.md")


class KnowledgeToolsTest(unittest.TestCase):
    def _make_base(self, temp_dir: str) -> KnowledgeBase:
        return KnowledgeBase(Path(temp_dir))

    def test_search_tool_returns_json_and_validates_query(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write("projects/demo/note.md", "正文包含关键词 Alpha", project="demo")

            missing = kb_search_result(base, {})
            self.assertFalse(missing.ok)
            self.assertEqual(missing.output, "query 不能为空。")

            result = kb_search_result(base, {"query": "Alpha", "project": "demo"})
            self.assertTrue(result.ok)
            payload = json.loads(result.output)
            self.assertEqual(len(payload), 1)
            self.assertEqual(payload[0]["path"], "projects/demo/note.md")

    def test_write_tool_creates_and_validates_mode(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)

            bad_mode = kb_write_result(
                base,
                {"path": "projects/demo/x.md", "content": "正文", "mode": "delete"},
            )
            self.assertFalse(bad_mode.ok)

            result = kb_write_result(
                base,
                {
                    "path": "projects/demo/x.md",
                    "content": "正文",
                    "title": "X",
                    "project": "demo",
                    "tags": ["tag1"],
                    "type": "research",
                    "status": "draft",
                },
            )
            self.assertTrue(result.ok)
            payload = json.loads(result.output)
            self.assertEqual(payload["note"]["type"], "research")

    def test_read_tool_returns_full_text_and_validates_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write("projects/demo/read.md", "可读正文", title="Read")

            missing = kb_read_result(base, {})
            self.assertFalse(missing.ok)

            result = kb_read_result(base, {"path": "projects/demo/read.md"})
            self.assertTrue(result.ok)
            self.assertIn("可读正文", result.output)

    def test_append_and_list_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            base = self._make_base(temp_dir)
            base.write("daily/2026/06/18.md", "开始", title="日志")

            appended = kb_append_result(base, {"path": "daily/2026/06/18.md", "content": "继续"})
            self.assertTrue(appended.ok)

            listed = kb_list_result(base, {"project": "daily"})
            self.assertTrue(listed.ok)
            payload = json.loads(listed.output)
            self.assertEqual(len(payload), 0)  # project 过滤不匹配空 project

            listed_all = kb_list_result(base, {})
            payload = json.loads(listed_all.output)
            self.assertEqual(len(payload), 1)
            self.assertEqual(payload[0]["path"], "daily/2026/06/18.md")


class KnowledgeToolRegistrationTest(unittest.TestCase):
    def test_build_agent_tools_registers_kb_tools(self) -> None:
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        common = {
            "mcp_manager": manager,
            "memory_enabled": False,
            "list": runner,
            "read": runner,
            "grep": runner,
            "edit_file": runner,
            "write_file": runner,
            "bash": runner,
            "powershell": runner,
            "monitor": runner,
            "memory_search": runner,
            "memory_read": runner,
            "memory_expand_related": runner,
            "memory_write": runner,
            "mcp_call": lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            "mcp_read_resource": lambda _uri: ToolResult(ok=True, output="ok"),
            "mcp_get_prompt": lambda _name, _arguments: ToolResult(ok=True, output="ok"),
        }
        kb = {
            "kb_search": runner,
            "kb_read": runner,
            "kb_write": runner,
            "kb_append": runner,
            "kb_list": runner,
        }

        tools = build_agent_tools(**common, **kb)

        self.assertEqual(
            {name for name in tools if name.startswith("kb_")},
            set(kb),
        )
        for name in kb:
            self.assertFalse(tools[name].requires_confirmation)

    def test_build_agent_tools_requires_complete_kb_group(self) -> None:
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        common = {
            "mcp_manager": manager,
            "memory_enabled": False,
            "list": runner,
            "read": runner,
            "grep": runner,
            "edit_file": runner,
            "write_file": runner,
            "bash": runner,
            "powershell": runner,
            "monitor": runner,
            "memory_search": runner,
            "memory_read": runner,
            "memory_expand_related": runner,
            "memory_write": runner,
            "mcp_call": lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            "mcp_read_resource": lambda _uri: ToolResult(ok=True, output="ok"),
            "mcp_get_prompt": lambda _name, _arguments: ToolResult(ok=True, output="ok"),
            "kb_search": runner,
        }

        with self.assertRaisesRegex(ValueError, "完整工具组"):
            build_agent_tools(**common)


if __name__ == "__main__":
    unittest.main()
