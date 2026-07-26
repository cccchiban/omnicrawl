from __future__ import annotations

import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from omnicrawl.documentation import BUNDLED_DOC_URI_PREFIX
from omnicrawl.workspace.monitor import BackgroundMonitorManager
from omnicrawl.workspace.tools import (
    WorkspaceCommandInvocation,
    WorkspaceCommandResult,
    WorkspaceToolError,
    WorkspaceTools,
)


class WorkspaceReadFileTest(unittest.TestCase):
    def test_read_file_preserves_line_range_behavior(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.txt").write_text("one\ntwo\nthree\n", encoding="utf-8")

            output = WorkspaceTools(workspace).read_file(
                {"path": "sample.txt", "start_line": 2, "max_lines": 1}
            )

        self.assertEqual(output, "2: two\n... 已截断，可提高 start_line 继续读取。")

    def test_read_file_locates_python_functions_with_ast(self) -> None:
        source = (
            "@decorator\n"
            "def top_level(value):\n"
            "    return value + 1\n"
            "\n"
            "class Worker:\n"
            "    async def process(self, value):\n"
            "        return value * 2\n"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.py").write_text(source, encoding="utf-8")
            tools = WorkspaceTools(workspace)

            top_level = tools.read_file(
                {"path": "sample.py", "function_name": "top_level", "max_lines": 20}
            )
            method = tools.read_file(
                {"path": "sample.py", "function_name": "Worker.process", "max_lines": 20}
            )

        self.assertIn("定位：函数 top_level（第 1-3 行）", top_level)
        self.assertIn("1: @decorator", top_level)
        self.assertIn("定位：函数 Worker.process（第 6-7 行）", method)
        self.assertIn("6:     async def process", method)

    def test_read_file_rejects_ambiguous_python_function_name(self) -> None:
        source = (
            "class First:\n"
            "    def run(self):\n"
            "        return 1\n"
            "\n"
            "class Second:\n"
            "    def run(self):\n"
            "        return 2\n"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.py").write_text(source, encoding="utf-8")

            with self.assertRaisesRegex(WorkspaceToolError, "存在多个匹配"):
                WorkspaceTools(workspace).read_file(
                    {"path": "sample.py", "function_name": "run"}
                )

    def test_read_file_locates_non_python_braced_function(self) -> None:
        source = (
            "export function render(name: string) {\n"
            "  const message = `hello ${name}`;\n"
            "  return message;\n"
            "}\n"
            "\n"
            "const helper = () => {\n"
            "  return 42;\n"
            "};\n"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.ts").write_text(source, encoding="utf-8")
            output = WorkspaceTools(workspace).read_file(
                {"path": "sample.ts", "function_name": "render", "max_lines": 20}
            )

        self.assertIn("定位：函数 render（第 1-4 行）", output)
        self.assertIn("1: export function render", output)
        self.assertIn("4: }", output)

    def test_read_file_locates_first_text_snippet_with_context(self) -> None:
        source = "before\nneedle first\nafter\nneedle second\nend\n"
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.txt").write_text(source, encoding="utf-8")
            output = WorkspaceTools(workspace).read_file(
                {
                    "path": "sample.txt",
                    "text": "needle",
                    "context_lines": 1,
                    "max_lines": 20,
                }
            )

        self.assertIn("文字片段首次匹配（第 2-2 行，上下文 1 行）", output)
        self.assertIn("1: before", output)
        self.assertIn("3: after", output)
        self.assertNotIn("4: needle second", output)

    def test_read_file_rejects_multiple_selector_kinds(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            workspace = Path(temp_dir)
            (workspace / "sample.py").write_text("def demo():\n    pass\n", encoding="utf-8")

            with self.assertRaisesRegex(WorkspaceToolError, "不能同时指定"):
                WorkspaceTools(workspace).read_file(
                    {"path": "sample.py", "function_name": "demo", "text": "demo"}
                )

    def test_read_file_reads_bundled_omnicrawl_document(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            output = WorkspaceTools(Path(temp_dir)).read_file(
                {"path": f"{BUNDLED_DOC_URI_PREFIX}MCP_USAGE.md", "max_lines": 5}
            )

        self.assertIn("MCP", output)

    def test_read_file_rejects_bundled_document_path_traversal(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with self.assertRaisesRegex(WorkspaceToolError, "内置文档 URI"):
                WorkspaceTools(Path(temp_dir)).read_file(
                    {"path": f"{BUNDLED_DOC_URI_PREFIX}../agent/system_prompt.md"}
                )


class ExplicitShellCommandTest(unittest.TestCase):
    def test_bash_command_uses_git_bash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = WorkspaceTools(Path(temp_dir)).run_shell_command(
                {"command": "printf bash-ok"}, shell="bash"
            )

        self.assertTrue(result.ok)
        self.assertIn("Shell：Bash", result.output)
        self.assertIn("bash-ok", result.output)

    def test_powershell_command_uses_explicit_powershell(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            result = WorkspaceTools(Path(temp_dir)).run_shell_command(
                {"command": "Write-Output powershell-ok"}, shell="powershell"
            )

        self.assertTrue(result.ok)
        self.assertIn("Shell：PowerShell", result.output)
        self.assertIn("powershell-ok", result.output)

    def test_explicit_shell_command_rejects_shell_override(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            with self.assertRaisesRegex(WorkspaceToolError, "不支持参数：shell"):
                WorkspaceTools(Path(temp_dir)).run_shell_command(
                    {"command": "Write-Output blocked", "shell": "bash"},
                    shell="powershell",
                )

    def test_shell_command_forces_utf8_subprocess_environment(self) -> None:
        class CompletedProcess:
            returncode = 0

            def communicate(self, timeout=None):
                return "中文输出", ""

        process = CompletedProcess()
        with tempfile.TemporaryDirectory() as temp_dir:
            tools = WorkspaceTools(Path(temp_dir))
            with (
                patch.object(
                    tools,
                    "command_invocation",
                    return_value=WorkspaceCommandInvocation(
                        args=["shell", "command"],
                        label="Bash",
                    ),
                ),
                patch("omnicrawl.workspace.tools.subprocess.Popen", return_value=process) as popen,
                patch.dict(
                    "omnicrawl.workspace.tools.os.environ",
                    {"PYTHONUTF8": "0"},
                ),
                patch(
                    "omnicrawl.workspace.monitor._assign_process_to_kill_on_close_job",
                    return_value=None,
                ),
            ):
                result = tools.run_shell_command({"command": "command"}, shell="bash")

        environment = popen.call_args.kwargs["env"]
        self.assertEqual(environment["PYTHONIOENCODING"], "utf-8")
        self.assertEqual(environment["PYTHONUTF8"], "0")
        self.assertEqual(environment["LANG"], "C.UTF-8")
        self.assertTrue(result.ok)
        self.assertIn("中文输出", result.output)

    def test_powershell_invocation_forces_utf8_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            invocation = WorkspaceTools(Path(temp_dir)).command_invocation(
                "Write-Output '中文'",
                shell="powershell",
            )

        command = invocation.args[-1]
        self.assertIn("[Console]::OutputEncoding", command)
        self.assertIn("$OutputEncoding", command)
        self.assertTrue(command.endswith("Write-Output '中文'"))

    def test_diagnostic_command_cannot_mask_primary_command_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            tools = WorkspaceTools(Path(temp_dir))
            with patch.object(
                tools,
                "_run_command_invocation",
                side_effect=(
                    WorkspaceCommandResult(ok=False, output="退出码：7\n\nShell：Bash"),
                    WorkspaceCommandResult(ok=True, output="退出码：0\n\nShell：Bash\n\ndiag"),
                ),
            ) as run:
                result = tools.run_shell_command(
                    {
                        "command": "python -m unittest > test.log 2>&1",
                        "diagnostic_command": "tail -n 40 test.log",
                    },
                    shell="bash",
                )

        self.assertFalse(result.ok)
        self.assertEqual(run.call_count, 2)
        self.assertIn("主命令结果", result.output)
        self.assertIn("诊断命令结果", result.output)
        self.assertIn("diag", result.output)

    def test_explicit_shell_timeout_terminates_started_process_tree(self) -> None:
        class TimedOutProcess:
            def communicate(self, timeout=None):
                raise subprocess.TimeoutExpired(["shell"], timeout)

        process = TimedOutProcess()
        with tempfile.TemporaryDirectory() as temp_dir:
            tools = WorkspaceTools(Path(temp_dir))
            with (
                patch.object(tools, "command_invocation") as invocation,
                patch("omnicrawl.workspace.tools.subprocess.Popen", return_value=process),
                patch(
                    "omnicrawl.workspace.monitor.BackgroundMonitorManager._terminate_process_tree"
                ) as terminate,
            ):
                invocation.return_value.args = ["shell"]
                invocation.return_value.label = "PowerShell"
                with self.assertRaisesRegex(WorkspaceToolError, "已终止"):
                    tools.run_shell_command(
                        {"command": "long-running", "timeout_seconds": 1},
                        shell="powershell",
                    )

        terminate.assert_called_once_with(process, job_handle=None)


class BackgroundMonitorTest(unittest.TestCase):
    def test_monitor_start_limit_is_atomic(self) -> None:
        second_start_entered = threading.Event()
        start_count = 0
        start_count_lock = threading.Lock()

        class FakeProcess:
            stdout = None
            stderr = None

            def wait(self, timeout=None):
                return 0

            def poll(self):
                return 0

        def start_process(*_args, **_kwargs):
            nonlocal start_count
            with start_count_lock:
                start_count += 1
                current_count = start_count
            if current_count == 1:
                second_start_entered.wait(timeout=1)
            else:
                second_start_entered.set()
            return FakeProcess()

        with tempfile.TemporaryDirectory() as temp_dir:
            manager = BackgroundMonitorManager(WorkspaceTools(Path(temp_dir)))
            errors: list[Exception] = []

            def start() -> None:
                try:
                    manager.run({"action": "start", "command": "echo ok", "shell": "powershell"})
                except Exception as exc:
                    errors.append(exc)

            with (
                patch("omnicrawl.workspace.monitor.MAX_ACTIVE_MONITORS", 1),
                patch("omnicrawl.workspace.monitor.subprocess.Popen", side_effect=start_process),
                patch("omnicrawl.workspace.monitor._assign_process_to_kill_on_close_job", return_value=None),
            ):
                threads = [threading.Thread(target=start) for _ in range(2)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join(timeout=3)

            self.assertEqual(len(manager._monitors), 1)
            self.assertEqual(len(errors), 1)
            self.assertRegex(str(errors[0]), "最多为 1 个")

    def test_monitor_defaults_to_powershell_and_rejects_default_shell(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manager = BackgroundMonitorManager(WorkspaceTools(Path(temp_dir)))
            try:
                start = manager.run({"action": "start", "command": "Start-Sleep -Seconds 30"})
                self.assertTrue(start.ok)
                self.assertIn("Shell：powershell", start.output)
                with self.assertRaisesRegex(WorkspaceToolError, "仅支持 bash 或 powershell"):
                    manager.run(
                        {
                            "action": "start",
                            "command": "Start-Sleep -Seconds 30",
                            "shell": "default",
                        }
                    )
            finally:
                manager.close()

    def test_monitor_start_poll_stop_and_close(self) -> None:
        command = (
            f"& '{sys.executable}' -u -c "
            "\"import time; print('monitor-ready', flush=True); time.sleep(30)\""
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            manager = BackgroundMonitorManager(WorkspaceTools(Path(temp_dir)))
            try:
                start = manager.run({"action": "start", "command": command, "shell": "powershell"})
                self.assertTrue(start.ok)
                monitor_id = next(
                    line.split("：", 1)[1]
                    for line in start.output.splitlines()
                    if line.startswith("已启动后台任务：")
                )

                output = ""
                for _ in range(40):
                    poll = manager.run(
                        {"action": "poll", "monitor_id": monitor_id, "cursor": 0}
                    )
                    output = poll.output
                    if "monitor-ready" in output:
                        break
                    time.sleep(0.05)
                self.assertIn("monitor-ready", output)

                stopped = manager.run({"action": "stop", "monitor_id": monitor_id})
                self.assertIn("状态：stopped", stopped.output)
            finally:
                manager.close()

    def test_close_terminates_running_monitor(self) -> None:
        command = f"& '{sys.executable}' -u -c \"import time; time.sleep(30)\""
        with tempfile.TemporaryDirectory() as temp_dir:
            manager = BackgroundMonitorManager(WorkspaceTools(Path(temp_dir)))
            start = manager.run({"action": "start", "command": command, "shell": "powershell"})
            monitor_id = next(
                line.split("：", 1)[1]
                for line in start.output.splitlines()
                if line.startswith("已启动后台任务：")
            )
            process = manager._monitors[monitor_id].process
            manager.close()

            for _ in range(40):
                if process.poll() is not None:
                    break
                time.sleep(0.05)

        self.assertIsNotNone(process.poll())


if __name__ == "__main__":
    unittest.main()
