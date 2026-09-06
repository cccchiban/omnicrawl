from __future__ import annotations

import os
import subprocess
import sys
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

from omnicrawl.connectors import autostart


class _FakeProcess:
    """可控的子进程替身：等待直到测试触发回收。"""

    _next_pid = 1000

    def __init__(self) -> None:
        self.pid = _FakeProcess._next_pid
        _FakeProcess._next_pid += 1
        self.returncode: int | None = None
        self._finished = threading.Event()

    def poll(self) -> int | None:
        return self.returncode

    def wait(self, timeout: float | None = None) -> int:
        if not self._finished.wait(timeout):
            raise subprocess.TimeoutExpired("fake connector", timeout)
        return int(self.returncode or 0)

    def kill(self) -> None:
        self.returncode = -9
        self._finished.set()


class ConnectorProcessManagerTests(unittest.TestCase):
    def _configured(self, *, telegram: bool = True, feishu: bool = True):
        return (
            (autostart._CONNECTOR_SPECS[0], telegram),
            (autostart._CONNECTOR_SPECS[1], feishu),
        )

    def test_starts_only_configured_connectors_with_workspace_environment(self) -> None:
        calls: list[tuple[list[str], dict]] = []
        processes: list[_FakeProcess] = []

        def fake_popen(command, **kwargs):
            calls.append((command, kwargs))
            process = _FakeProcess()
            processes.append(process)
            return process

        workspace = Path.cwd() / "test-workspace"
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop(autostart.AUTO_START_ENV, None)
            with patch.object(
                autostart,
                "_configured_connectors",
                return_value=self._configured(telegram=True, feishu=False),
            ):
                with patch.object(
                    autostart,
                    "assign_process_to_kill_on_close_job",
                    return_value=123,
                ):
                    manager = autostart.ConnectorProcessManager(
                        workspace,
                        popen_factory=fake_popen,
                    )
                    self.assertEqual(manager.start(), ("Telegram",))

        self.assertEqual(len(processes), 1)
        self.assertEqual(manager.started_connectors, ("Telegram",))
        self.assertEqual(
            calls[0][0],
            [sys.executable, "-m", "omnicrawl.connectors.telegram"],
        )
        self.assertEqual(
            Path(calls[0][1]["env"]["AI_WORKSPACE_ROOT"]).resolve(),
            workspace.resolve(),
        )
        # 仅检查模块命令和关键隔离参数；凭证只会通过 env/config 传递，
        # 不会出现在命令行列表中。
        def stop_fake_process(process, *, job_handle, wait):
            self.assertTrue(wait)
            process.returncode = -15
            process._finished.set()

        with patch.object(autostart, "terminate_process_tree", side_effect=stop_fake_process):
            manager.close()

    def test_popen_command_and_environment_do_not_include_credentials(self) -> None:
        calls: list[tuple[list[str], dict]] = []
        processes: list[_FakeProcess] = []

        def fake_popen(command, **kwargs):
            calls.append((command, kwargs))
            process = _FakeProcess()
            processes.append(process)
            return process

        workspace = Path.cwd()
        with patch.dict(
            os.environ,
            {
                autostart.AUTO_START_ENV: "true",
                "TELEGRAM_BOT_TOKEN": "secret-token",
                "FEISHU_APP_SECRET": "secret-app-value",
            },
            clear=False,
        ):
            with patch.object(
                autostart,
                "_configured_connectors",
                return_value=self._configured(telegram=True, feishu=True),
            ):
                with patch.object(
                    autostart,
                    "assign_process_to_kill_on_close_job",
                    return_value=None,
                ):
                    manager = autostart.ConnectorProcessManager(
                        workspace,
                        popen_factory=fake_popen,
                    )
                    manager.start()

        self.assertEqual(len(calls), 2)
        self.assertEqual(
            [call[0] for call in calls],
            [
                [os.sys.executable, "-m", "omnicrawl.connectors.telegram"],
                [os.sys.executable, "-m", "omnicrawl.connectors.fsapp"],
            ],
        )
        for command, kwargs in calls:
            self.assertNotIn("secret-token", command)
            self.assertNotIn("secret-app-value", command)
            self.assertEqual(Path(kwargs["cwd"]).resolve(), workspace.resolve())
            self.assertEqual(
                Path(kwargs["env"]["AI_WORKSPACE_ROOT"]).resolve(),
                workspace.resolve(),
            )
            self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
            self.assertEqual(kwargs["stdout"], subprocess.DEVNULL)
            self.assertEqual(kwargs["stderr"], subprocess.DEVNULL)
            if os.name == "nt":
                self.assertEqual(
                    kwargs["creationflags"],
                    subprocess.CREATE_NEW_PROCESS_GROUP,
                )
            else:
                self.assertTrue(kwargs["start_new_session"])

        def stop_fake_process(process, *, job_handle, wait):
            self.assertTrue(wait)
            process.returncode = -15
            process._finished.set()

        with patch.object(autostart, "terminate_process_tree", side_effect=stop_fake_process) as terminate:
            manager.close()
        self.assertEqual(terminate.call_count, 2)
        for call in terminate.call_args_list:
            self.assertTrue(call.kwargs["wait"])

    def test_close_terminates_every_started_process_and_is_idempotent(self) -> None:
        processes: list[_FakeProcess] = []

        def fake_popen(_command, **_kwargs):
            process = _FakeProcess()
            processes.append(process)
            return process

        with patch.object(
            autostart,
            "_configured_connectors",
            return_value=self._configured(),
        ):
            with patch.object(
                autostart,
                "assign_process_to_kill_on_close_job",
                side_effect=[11, 22],
            ):
                manager = autostart.ConnectorProcessManager(
                    Path.cwd(),
                    popen_factory=fake_popen,
                )
                manager.start()

        def terminate(process, *, job_handle, wait):
            self.assertTrue(wait)
            self.assertIn(process, processes)
            self.assertIn(job_handle, {11, 22})
            process.returncode = -15
            process._finished.set()

        with patch.object(autostart, "terminate_process_tree", side_effect=terminate) as stop:
            manager.close()
            manager.close()

        self.assertEqual(stop.call_count, 2)
        self.assertTrue(all(process.returncode == -15 for process in processes))

    def test_capture_logs_redirects_stdout_stderr_to_user_log_dir(self) -> None:
        calls: list[tuple[list[str], dict]] = []
        processes: list[_FakeProcess] = []

        def fake_popen(command, **kwargs):
            calls.append((command, kwargs))
            process = _FakeProcess()
            processes.append(process)
            return process

        workspace = Path.cwd()
        with patch.dict(os.environ, {autostart.AUTO_START_ENV: "1"}, clear=False):
            with patch.object(
                autostart,
                "_configured_connectors",
                return_value=self._configured(telegram=True, feishu=False),
            ):
                with patch.object(
                    autostart,
                    "assign_process_to_kill_on_close_job",
                    return_value=None,
                ):
                    manager = autostart.ConnectorProcessManager(
                        workspace,
                        popen_factory=fake_popen,
                        capture_logs=True,
                    )
                    self.assertTrue(manager.capture_logs)
                    manager.start()

        self.assertEqual(len(calls), 1)
        kwargs = calls[0][1]
        # stdout 是打开的文件对象而非 DEVNULL；stderr 合并到 stdout
        self.assertIsNot(kwargs["stdout"], subprocess.DEVNULL)
        self.assertEqual(kwargs["stderr"], subprocess.STDOUT)
        self.assertTrue(hasattr(kwargs["stdout"], "write"))
        # 文件落在用户日志目录 logs/ 下
        log_path = kwargs["stdout"].name
        self.assertIn("logs", Path(log_path).parts)

        def stop_fake_process(process, *, job_handle, wait):
            process.returncode = -15
            process._finished.set()

        with patch.object(autostart, "terminate_process_tree", side_effect=stop_fake_process):
            manager.close()

    def test_invalid_connector_config_is_skipped_without_popen(self) -> None:
        popen = Mock()
        telegram_config = {"bot_token": "", "allowed_user_ids": []}
        feishu_config = SimpleNamespace(app_id="cli-test", app_secret="")
        with patch.object(autostart, "load_telegram_config", return_value=telegram_config):
            with patch.object(autostart, "load_feishu_config", return_value=feishu_config):
                with patch.dict(os.environ, {autostart.AUTO_START_ENV: "on"}):
                    manager = autostart.ConnectorProcessManager(
                        Path.cwd(),
                        popen_factory=popen,
                    )
                    self.assertEqual(manager.start(), ())
        popen.assert_not_called()

    def test_disabled_switch_skips_config_loading_and_startup(self) -> None:
        popen = Mock()
        with patch.object(autostart, "_configured_connectors") as detect:
            with patch.dict(os.environ, {autostart.AUTO_START_ENV: "0"}):
                manager = autostart.ConnectorProcessManager(
                    Path.cwd(),
                    popen_factory=popen,
                )
                self.assertEqual(manager.start(), ())
        detect.assert_not_called()
        popen.assert_not_called()

    def test_unexpected_start_failure_is_cleaned_up_by_public_start_helper(self) -> None:
        manager = Mock()
        manager.start.side_effect = RuntimeError("simulated setup failure")
        with patch.object(autostart, "ConnectorProcessManager", return_value=manager):
            with self.assertRaises(RuntimeError):
                autostart.start_configured_connectors(Path.cwd())
        manager.close.assert_called_once_with()

    def test_one_popen_failure_does_not_block_other_connector(self) -> None:
        process = _FakeProcess()
        calls = 0

        def fake_popen(_command, **_kwargs):
            nonlocal calls
            calls += 1
            if calls == 1:
                raise OSError("simulated launch failure")
            return process

        with patch.object(
            autostart,
            "_configured_connectors",
            return_value=self._configured(),
        ):
            with patch.object(
                autostart,
                "assign_process_to_kill_on_close_job",
                return_value=None,
            ):
                manager = autostart.ConnectorProcessManager(
                    Path.cwd(),
                    popen_factory=fake_popen,
                )
                self.assertEqual(manager.start(), ("飞书",))

        def stop_fake_process(process, *, job_handle, wait):
            process.returncode = -15
            process._finished.set()

        with patch.object(autostart, "terminate_process_tree", side_effect=stop_fake_process):
            manager.close()


class ApplicationConnectorIntegrationTests(unittest.TestCase):
    def test_run_application_closes_connectors_before_main_agent(self) -> None:
        events: list[str] = []
        workspace = Path.cwd()
        agent = SimpleNamespace(close=lambda: events.append("agent.close"))
        manager = SimpleNamespace(close=lambda: events.append("connectors.close"))
        config = SimpleNamespace(thinking_enabled=False, reasoning_effort="")
        project_context = SimpleNamespace(
            workspace_root=workspace,
            source="fallback_start",
            marker=None,
        )
        setup = SimpleNamespace(errors=(), api_key_configured=True)
        prepared = {
            "agent": agent,
            "config": config,
            "project_context": project_context,
            "approval_mode": "review",
            "temp_workspace_config": SimpleNamespace(enabled=False),
            "fullscreen_startup": lambda **_kwargs: SimpleNamespace(),
            "run_fullscreen_tui": lambda *_args: 0,
            "plugin_runtime": None,
            "plugin_lines": (),
            "startup_messages": (),
            "connector_manager": manager,
        }

        with patch("omnicrawl.entry.initialize_user_configuration", return_value=setup):
            with patch("omnicrawl.entry.format_startup_report", return_value=()):
                with patch("omnicrawl.entry.run_startup_splash", return_value=prepared):
                    with patch("omnicrawl.entry.configure_console_encoding"):
                        result = __import__("omnicrawl.entry", fromlist=["run_application"]).run_application([])

        self.assertEqual(result, 0)
        self.assertEqual(events, ["connectors.close", "agent.close"])


if __name__ == "__main__":
    unittest.main()
