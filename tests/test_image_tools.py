from __future__ import annotations

import base64
import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from omnicrawl.agent.toolkit.image_tools import read_image_file
from omnicrawl.agent.toolkit.tools import build_agent_tools
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
                    {
                        "path": name,
                        "prompt": "请读取图片中的文字。",
                        "detail": "high",
                    },
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
            {
                "path": str(self.external),
                "prompt": "请读取图片中的文字。",
            },
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
            (
                {"path": "https://example.com/image.png", "prompt": "请读取图片中的文字。"},
                "不支持 URL",
            ),
            (
                {"path": "../external.png", "prompt": "请读取图片中的文字。"},
                "不能越出当前工作区",
            ),
            (
                {"path": "missing.png", "prompt": "请读取图片中的文字。"},
                "不是文件",
            ),
            (
                {"path": "bad.png", "prompt": "请读取图片中的文字。"},
                "不是受支持的图片格式",
            ),
            (
                {"path": "image.png", "prompt": "请读取图片中的文字。", "detail": "medium"},
                "detail",
            ),
            ({"path": "image.png", "prompt": "   "}, "prompt"),
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
            list=runner,
            read=runner,
            read_image=runner,
            grep=runner,
            edit_file=runner,
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
        schema = json.loads(tools["read_image"].argument_schema)
        self.assertEqual(schema["required"], ["path", "prompt"])
        confirmation = format_tool_confirmation(
            "read_image",
            {
                "path": "C:/private/example.png",
                "prompt": "请识别右下角的日期。",
                "detail": "high",
            },
        )
        self.assertIn("C:/private/example.png", confirmation)
        self.assertIn("视觉细节：high", confirmation)
        self.assertIn("请识别右下角的日期", confirmation)


if __name__ == "__main__":
    unittest.main()
