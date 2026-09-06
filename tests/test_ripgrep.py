import json
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import TestCase
from unittest.mock import patch

from omnicrawl.workspace.ripgrep import (
    RIPGREP_BINARY_NAME,
    batch_paths,
    bundled_ripgrep_name,
    resolve_ripgrep_binary,
)
from omnicrawl.workspace.tools import WorkspaceToolError, WorkspaceTools


class RipgrepBinaryTest(TestCase):
    def test_resolve_ripgrep_binary_prefers_bundled(self) -> None:
        binary = resolve_ripgrep_binary()
        self.assertIsNotNone(binary)
        self.assertTrue(binary.is_file())
        self.assertEqual(binary.name, RIPGREP_BINARY_NAME)

    def test_bundled_ripgrep_name_selects_platform_binary(self) -> None:
        cases = {
            ("win32", "AMD64"): "rg.exe",
            ("win32", "amd64"): "rg.exe",
            ("linux", "x86_64"): "rg-linux-x86_64",
            ("linux", "arm64"): "rg-linux-arm64",
            ("linux", "aarch64"): "rg-linux-arm64",
            ("darwin", "x86_64"): "rg-macos-x86_64",
            ("darwin", "arm64"): "rg-macos-arm64",
            # 未内置平台回退到通用命名（随包路径下不存在时会继续回退 PATH）。
            ("linux", "riscv64"): RIPGREP_BINARY_NAME,
        }
        for (sys_platform, machine), expected in cases.items():
            with self.subTest(platform=sys_platform, machine=machine):
                with patch(
                    "omnicrawl.workspace.ripgrep.sys.platform", sys_platform
                ), patch(
                    "omnicrawl.workspace.ripgrep.platform.machine",
                    return_value=machine,
                ):
                    self.assertEqual(bundled_ripgrep_name(), expected)

    def test_batch_paths_respects_char_budget(self) -> None:
        paths = [Path(f"dir/file_{index:05d}.txt") for index in range(2000)]
        batches = batch_paths(paths)
        self.assertGreater(len(batches), 1)
        self.assertEqual(sum(len(batch) for batch in batches), len(paths))
        for batch in batches:
            self.assertLessEqual(
                sum(len(item) for item in batch) + len(batch),
                16_000,
            )


class WorkspaceGrepRipgrepTest(TestCase):
    def _make_workspace(self, files: dict[str, str]) -> tuple[Path, WorkspaceTools]:
        temp_dir = TemporaryDirectory()
        workspace = Path(temp_dir.name)
        for relative, content in files.items():
            path = workspace / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        self.addCleanup(temp_dir.cleanup)
        return workspace, WorkspaceTools(workspace)

    def test_find_star_matches_all_file_names(self) -> None:
        _, tools = self._make_workspace(
            {
                "a.txt": "one\n",
                "nested/b.py": "two\n",
            }
        )
        output = tools.find_files({"pattern": "*", "kind": "file"})
        self.assertIn("a.txt", output)
        self.assertIn("nested", output)
        self.assertIn("b.py", output)
        self.assertNotIn("未找到匹配结果", output)

    def test_find_glob_matches_file_names(self) -> None:
        _, tools = self._make_workspace(
            {
                "a.txt": "one\n",
                "b.py": "two\n",
            }
        )
        output = tools.find_files({"pattern": "*.py", "kind": "file"})
        self.assertIn("b.py", output)
        self.assertNotIn("a.txt", output)

    def test_grep_uses_bundled_ripgrep_binary(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "hello world\n"})
        self.assertEqual(tools._ripgrep_binary(), resolve_ripgrep_binary())
        output = tools.grep({"pattern": "hello"})
        self.assertIn("sample.txt:1: hello world", output)

    def test_grep_expands_absolute_and_relative_windows_globs_with_chinese_paths(self) -> None:
        workspace, tools = self._make_workspace(
            {
                "工程程序/DNS监控/public/index.js": "const marker = '首页';\n",
                "工程程序/DNS监控/public/dashboard.js": "const marker = '仪表盘';\n",
                "工程程序/DNS监控/public/readme.txt": "const marker = 'not-js';\n",
                "工程程序/DNS监控/private/secret.js": "const marker = 'not-public';\n",
            }
        )
        public_glob = workspace / "工程程序" / "DNS监控" / "public" / "*.js"

        absolute_output = tools.grep(
            {"pattern": "marker", "path": str(public_glob)}
        )
        self.assertIn("工程程序\\DNS监控\\public\\index.js:1: const marker", absolute_output)
        self.assertIn(
            "工程程序\\DNS监控\\public\\dashboard.js:1: const marker",
            absolute_output,
        )
        self.assertNotIn("readme.txt", absolute_output)
        self.assertNotIn("private", absolute_output)

        relative_output = tools.grep(
            {
                "pattern": "marker",
                "path": r"工程程序\DNS监控\public\*.js",
                "files_with_matches": True,
            }
        )
        self.assertEqual(
            relative_output.splitlines(),
            [
                r"工程程序\DNS监控\public\dashboard.js",
                r"工程程序\DNS监控\public\index.js",
            ],
        )

    def test_grep_supports_multiple_alternatives(self) -> None:
        _, tools = self._make_workspace(
            {
                "messages.txt": "messages\n",
                "context.txt": "context\n",
                "calls.txt": "tool_calls\n",
                "other.txt": "unrelated\n",
            }
        )
        output = tools.grep({"pattern": "messages|context|tool_calls"})
        self.assertIn("messages.txt:1: messages", output)
        self.assertIn("context.txt:1: context", output)
        self.assertIn("calls.txt:1: tool_calls", output)
        self.assertNotIn("other.txt", output)

    def test_grep_regex_default(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "axb\na.*b\n"})
        output = tools.grep({"pattern": "a.b"})
        self.assertIn("sample.txt:1: axb", output)
        self.assertNotIn("sample.txt:2", output)

    def test_grep_literal_mode(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "axb\na.*b\n"})
        output = tools.grep({"pattern": "a.*b", "use_regex": False})
        self.assertIn("sample.txt:2: a.*b", output)
        self.assertNotIn("sample.txt:1: axb", output)

    def test_grep_case_sensitive(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "Hello\nhello\n"})
        insensitive = tools.grep({"pattern": "hello"})
        self.assertIn("sample.txt:1: Hello", insensitive)
        self.assertIn("sample.txt:2: hello", insensitive)
        sensitive = tools.grep({"pattern": "hello", "case_sensitive": True})
        self.assertNotIn("sample.txt:1", sensitive)
        self.assertIn("sample.txt:2: hello", sensitive)

    def test_grep_count(self) -> None:
        _, tools = self._make_workspace(
            {"a.txt": "x\nx\n", "b.txt": "y\n"}
        )
        output = tools.grep({"pattern": "x", "count": True})
        self.assertIn("a.txt: 2", output)
        self.assertNotIn("b.txt", output)

    def test_grep_files_with_matches(self) -> None:
        _, tools = self._make_workspace(
            {"a.txt": "x\n", "b.txt": "y\n"}
        )
        output = tools.grep({"pattern": "x", "files_with_matches": True})
        self.assertEqual(output, "a.txt")

    def test_grep_context_lines(self) -> None:
        _, tools = self._make_workspace(
            {"sample.txt": "one\nneedle\nthree\n"}
        )
        output = tools.grep({"pattern": "needle", "context_lines": 1})
        self.assertIn("sample.txt-1- one", output)
        self.assertIn("sample.txt:2: needle", output)
        self.assertIn("sample.txt-3- three", output)

    def test_grep_include_exclude(self) -> None:
        _, tools = self._make_workspace(
            {
                "a.py": "keyword\n",
                "b.txt": "keyword\n",
                "skip.py": "keyword\n",
            }
        )
        output = tools.grep(
            {"pattern": "keyword", "include": "*.py", "exclude": "skip*"}
        )
        self.assertIn("a.py", output)
        self.assertNotIn("b.txt", output)
        self.assertNotIn("skip.py", output)

    def test_grep_max_results_truncates(self) -> None:
        _, tools = self._make_workspace(
            {"sample.txt": "\n".join(f"line {index}" for index in range(10))}
        )
        output = tools.grep({"pattern": "line", "max_results": 3})
        self.assertIn("已达到 max_results", output)
        self.assertEqual(output.count("sample.txt:"), 3)

    def test_grep_missing_binary_reports_clear_error(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "x\n"})
        with patch(
            "omnicrawl.workspace.tools.resolve_ripgrep_binary",
            return_value=None,
        ):
            with self.assertRaisesRegex(WorkspaceToolError, "未找到 ripgrep"):
                tools.grep({"pattern": "x"})

    def test_grep_invalid_override_binary_reports_error(self) -> None:
        temp_dir = TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        workspace = Path(temp_dir.name)
        tools = WorkspaceTools(
            workspace, ripgrep_binary=workspace / "missing-rg.exe"
        )
        with self.assertRaisesRegex(WorkspaceToolError, "ripgrep 二进制不存在"):
            tools.grep({"pattern": "x"})

    def test_find_skips_gitignored_dirs(self) -> None:
        _, tools = self._make_workspace(
            {
                ".gitignore": "node_modules/\nbuild/\n",
                "src/main.py": "def main(): pass\n",
                "node_modules/big.js": "const x = 1;\n",
                "build/out.txt": "artifact\n",
            }
        )
        output = tools.find_files({"pattern": "*"})
        self.assertIn("src", output)
        self.assertIn("main.py", output)
        self.assertNotIn("node_modules", output)
        self.assertNotIn("big.js", output)
        self.assertNotIn("build", output)
        self.assertNotIn("out.txt", output)

    def test_find_includes_hidden_files(self) -> None:
        _, tools = self._make_workspace(
            {".github/workflow.yml": "on: push\n", "main.py": "x\n"}
        )
        output = tools.find_files({"pattern": "*"})
        self.assertIn(".github", output)
        self.assertIn("workflow.yml", output)

    def test_grep_json_parses_colon_paths(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "x\n"})
        record = json.dumps(
            {
                "type": "match",
                "data": {
                    "path": {"text": r"C:\proj\weird:name.txt"},
                    "lines": {"text": "needle here\r\n"},
                    "line_number": 7,
                },
            }
        )
        self.assertEqual(
            tools._parse_ripgrep_json_record(record),
            (r"C:\proj\weird:name.txt", 7, "needle here"),
        )
        # 非 match 记录跳过
        self.assertIsNone(
            tools._parse_ripgrep_json_record(
                json.dumps({"type": "begin", "data": {}})
            )
        )

    def test_grep_json_non_utf8_line_placeholder(self) -> None:
        _, tools = self._make_workspace({"sample.txt": "x\n"})
        record = json.dumps(
            {
                "type": "match",
                "data": {
                    "path": {"text": "x.bin"},
                    "lines": {"bytes": "AP4A"},
                    "line_number": 3,
                },
            }
        )
        relative, line_no, text = tools._parse_ripgrep_json_record(record)
        self.assertEqual((relative, line_no), ("x.bin", 3))
        self.assertIn("not valid UTF-8", text)

    def test_grep_long_line_capped(self) -> None:
        _, tools = self._make_workspace(
            {"long.txt": "x" * 5000 + "\n"}
        )
        output = tools.grep({"pattern": "x{100}"})
        self.assertIn("(line truncated)", output)
        body = output.split(": ", 1)[1]
        self.assertLessEqual(len(body.split(" (line truncated)")[0]), 2000)

    def test_grep_truncated_spills_complete_result(self) -> None:
        _, tools = self._make_workspace(
            {"sample.txt": "\n".join(f"line {index}" for index in range(10))}
        )
        output = tools.grep({"pattern": "line", "max_results": 3})
        self.assertIn("已达到 max_results", output)
        self.assertEqual(output.count("sample.txt:"), 3)
        self.assertIn("完整结果已保存至", output)
        spill = Path(output.split("已保存至：")[-1])
        self.assertTrue(spill.is_file())
        self.assertIn("line 9", spill.read_text(encoding="utf-8"))
        # 落盘文件不在搜索结果里（.omnicrawl 从搜索中排除）
        again = tools.find_files({"pattern": "*"})
        self.assertNotIn("grep_matches", again)

    def test_find_truncated_spills_complete_result(self) -> None:
        _, tools = self._make_workspace(
            {"a.txt": "\n", "b.txt": "\n", "c.txt": "\n"}
        )
        output = tools.find_files({"pattern": "*.txt", "max_results": 2})
        self.assertIn("已达到 max_results", output)
        self.assertIn("完整结果已保存至", output)
        spill = Path(output.split("已保存至：")[-1])
        self.assertTrue(spill.is_file())
        content = spill.read_text(encoding="utf-8")
        self.assertIn("a.txt", content)
        self.assertIn("c.txt", content)

    def test_grep_count_and_list_spill_when_capped(self) -> None:
        _, tools = self._make_workspace(
            {"a.txt": "match\n", "b.txt": "match\n", "c.txt": "match\n"}
        )
        count_output = tools.grep(
            {"pattern": "match", "count": True, "max_results": 1}
        )
        self.assertIn("完整结果已保存至", count_output)
        list_output = tools.grep(
            {"pattern": "match", "files_with_matches": True, "max_results": 1}
        )
        self.assertIn("完整结果已保存至", list_output)
