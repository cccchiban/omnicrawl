from __future__ import annotations

import base64
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent.image_tools import read_image_file
from omnicrawl.agent.tools import build_agent_tools
from omnicrawl.agent.types import ToolResult
from omnicrawl.commands.slash import format_tool_confirmation


class ReadImageFileTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name) / "workspace"
        self.root.mkdir()
        self.external = Path(self.temp_dir.name) / "external.png"

    def test_reads_supported_formats_and_returns_model_attachment(self) -> None:
        fixtures = {
            "image.png": (b"\x89PNG\r\n\x1a\nfixture", "image/png"),
            "image.jpg": (b"\xff\xd8\xff\xe0fixture", "image/jpeg"),
            "image.webp": (b"RIFF\x04\x00\x00\x00WEBPfixture", "image/webp"),
            "image.gif": (b"GIF89afixture", "image/gif"),
        }

        for name, (content, media_type) in fixtures.items():
            with self.subTest(name=name):
                path = self.root / name
                path.write_bytes(content)
                result = read_image_file(
                    {"path": name, "detail": "high"},
                    workspace_root=self.root,
                )

                self.assertTrue(result.ok, result.output)
                self.assertEqual(len(result.model_images), 1)
                attachment = result.model_images[0]
                self.assertEqual(attachment.media_type, media_type)
                self.assertEqual(attachment.detail, "high")
                self.assertEqual(
                    base64.b64decode(attachment.data_base64),
                    content,
                )
                payload = json.loads(result.output)
                self.assertEqual(payload["media_type"], media_type)
                self.assertEqual(payload["bytes"], len(content))
                self.assertNotIn(attachment.data_base64, result.output)

    def test_reads_absolute_path_outside_workspace(self) -> None:
        content = b"\x89PNG\r\n\x1a\nexternal"
        self.external.write_bytes(content)

        result = read_image_file(
            {"path": str(self.external)},
            workspace_root=self.root,
        )

        self.assertTrue(result.ok, result.output)
        self.assertEqual(
            base64.b64decode(result.model_images[0].data_base64),
            content,
        )
        self.assertEqual(json.loads(result.output)["path"], str(self.external.resolve()))

    def test_rejects_urls_invalid_formats_and_invalid_detail(self) -> None:
        cases = (
            ({"path": "https://example.com/image.png"}, "不支持 URL"),
            ({"path": "../external.png"}, "不能越出当前工作区"),
            ({"path": "missing.png"}, "不是文件"),
            ({"path": "bad.png"}, "不是受支持的图片格式"),
            ({"path": "image.png", "detail": "medium"}, "detail"),
        )
        (self.root / "bad.png").write_bytes(b"not an image")
        (self.root / "image.png").write_bytes(b"\x89PNG\r\n\x1a\nvalid")

        for arguments, expected in cases:
            with self.subTest(arguments=arguments):
                result = read_image_file(arguments, workspace_root=self.root)
                self.assertFalse(result.ok)
                self.assertIn(expected, result.output)


class ReadImageToolRegistrationTest(unittest.TestCase):
    def test_agent_registers_read_image_as_confirmed_tool(self) -> None:
        runner = lambda _arguments: ToolResult(ok=True, output="ok")
        manager = SimpleNamespace(
            registry=SimpleNamespace(tools={}, resources={}, prompts={})
        )
        tools = build_agent_tools(
            mcp_manager=manager,
            memory_enabled=False,
            list_files=runner,
            read_file=runner,
            read_image=runner,
            search_text=runner,
            replace_text=runner,
            write_file=runner,
            bash=runner,
            powershell=runner,
            monitor=runner,
            memory_search=runner,
            memory_read=runner,
            memory_expand_related=runner,
            memory_write=runner,
            mcp_call=lambda _meta, _arguments: ToolResult(ok=True, output="ok"),
            mcp_read_resource=lambda _uri: ToolResult(ok=True, output="ok"),
            mcp_get_prompt=lambda _name, _arguments: ToolResult(ok=True, output="ok"),
        )

        self.assertIn("read_image", tools)
        self.assertTrue(tools["read_image"].requires_confirmation)
        self.assertIn("绝对路径", tools["read_image"].description)
        confirmation = format_tool_confirmation(
            "read_image",
            {"path": "C:/private/example.png", "detail": "high"},
        )
        self.assertIn("C:/private/example.png", confirmation)
        self.assertIn("视觉细节：high", confirmation)


if __name__ == "__main__":
    unittest.main()
