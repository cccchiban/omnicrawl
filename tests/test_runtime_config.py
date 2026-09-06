from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

try:
    import tomllib
except ImportError:  # Python < 3.11
    import tomli as tomllib
from omnicrawl.config.core.runtime import dump_toml_text

import omnicrawl.config.core.runtime as runtime_module
from omnicrawl.entry import _parse_args, run_application
from omnicrawl.runtime_config import (
    RuntimeConfigError,
    default_config_path,
    global_agents_path,
    load_config_data,
    migrate_legacy_user_config,
    resolve_config_path,
    resolve_models_path,
    save_config_data,
    user_config_dir,
)
from omnicrawl.ui import UIStartupError
from omnicrawl.ui.windows_launcher import launch_in_powershell_window


class RuntimeConfigTest(unittest.TestCase):
    def test_user_config_directory_is_hidden_directory_in_home(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir) / "home"
            home.mkdir()
            with patch("omnicrawl.config.core.runtime.Path.home", return_value=home):
                self.assertEqual(user_config_dir(), home / ".OmniCrawl")
                self.assertEqual(global_agents_path(), home / ".OmniCrawl" / "AGENTS.md")
                self.assertEqual(default_config_path(), home / ".OmniCrawl" / "config.toml")

    def test_migrate_legacy_user_config_moves_files_and_removes_old_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            appdata = root / "appdata"
            legacy = appdata / "OmniCrawl"
            legacy.mkdir(parents=True)
            (legacy / "config.toml").write_text("legacy = true\n", encoding="utf-8")
            (legacy / "AGENTS.md").write_text("# global\n", encoding="utf-8")

            with patch("omnicrawl.config.core.runtime.Path.home", return_value=home):
                migrated = migrate_legacy_user_config(
                    environ={"APPDATA": str(appdata)},
                    platform_name="win32",
                )

            target = home / ".OmniCrawl"
            self.assertEqual(migrated, target)
            self.assertEqual((target / "config.toml").read_text(encoding="utf-8"), "legacy = true\n")
            self.assertTrue((target / "AGENTS.md").is_file())
            self.assertFalse(legacy.exists())

    def test_migrate_legacy_user_config_preserves_target_conflicts_in_backup(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            home = root / "home"
            appdata = root / "appdata"
            legacy = appdata / "OmniCrawl"
            legacy.mkdir(parents=True)
            target = home / ".OmniCrawl"
            target.mkdir(parents=True)
            (target / "config.toml").write_text("new = true\n", encoding="utf-8")
            (legacy / "config.toml").write_text("old = true\n", encoding="utf-8")

            with patch("omnicrawl.config.core.runtime.Path.home", return_value=home):
                migrate_legacy_user_config(
                    environ={"APPDATA": str(appdata)},
                    platform_name="win32",
                )

            self.assertEqual((target / "config.toml").read_text(encoding="utf-8"), "new = true\n")
            backup = target / "config.toml.migrated.bak"
            self.assertEqual(backup.read_text(encoding="utf-8"), "old = true\n")
            self.assertFalse(legacy.exists())

    def test_should_ignore_legacy_dirs_and_always_use_user_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            user_dir = root / "user"
            legacy_dir = root / "legacy"
            user_dir.mkdir()
            legacy_dir.mkdir()
            legacy_config = legacy_dir / "config.toml"
            legacy_config.write_text("legacy = true\n", encoding="utf-8")

            with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                with patch(
                    "omnicrawl.config.core.runtime.legacy_user_config_dirs",
                    return_value=(legacy_dir,),
                ):
                    with patch.object(
                        runtime_module,
                        "_is_development_environment",
                        return_value=False,
                    ):
                        resolved = resolve_config_path()

        self.assertEqual(resolved, user_dir / "config.toml")

    def test_should_prefer_environment_paths_over_default_candidates(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            source_dir = root / "source"
            env_dir = root / "env"
            cwd.mkdir()
            user_dir.mkdir()
            source_dir.mkdir()
            env_dir.mkdir()
            env_config = env_dir / "config.toml"
            env_models = env_dir / "models.toml"
            env_config.write_text("env = true", encoding="utf-8")
            env_models.write_text("env = true", encoding="utf-8")
            (cwd / "config.toml").write_text("cwd = true", encoding="utf-8")
            (cwd / "models.toml").write_text("cwd = true", encoding="utf-8")
            (user_dir / "config.toml").write_text("user = true", encoding="utf-8")
            (user_dir / "models.toml").write_text("user = true", encoding="utf-8")
            (source_dir / "config.toml").write_text("source = true", encoding="utf-8")
            (source_dir / "models.toml").write_text("source = true", encoding="utf-8")

            with patch.dict(
                os.environ,
                {
                    "AI_CONFIG_FILE": str(env_config),
                    "AI_MODELS_FILE": str(env_models),
                },
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch("omnicrawl.config.core.runtime.project_root", return_value=source_dir):
                            config_path = resolve_config_path()
                            models_path = resolve_models_path()

            self.assertEqual(config_path, env_config)
            self.assertEqual(models_path, env_models)

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            source_dir = root / "source"
            cwd.mkdir()
            user_dir.mkdir()
            source_dir.mkdir()
            cwd_config = cwd / "config.toml"
            cwd_models = cwd / "models.toml"
            cwd_config.write_text("cwd = true", encoding="utf-8")
            cwd_models.write_text("cwd = true", encoding="utf-8")
            (user_dir / "config.toml").write_text("user = true", encoding="utf-8")
            (user_dir / "models.toml").write_text("user = true", encoding="utf-8")
            (source_dir / "config.toml").write_text("source = true", encoding="utf-8")
            (source_dir / "models.toml").write_text("source = true", encoding="utf-8")

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch("omnicrawl.config.core.runtime.project_root", return_value=source_dir):
                            config_path = resolve_config_path()
                            models_path = resolve_models_path()

            self.assertEqual(config_path, user_dir / "config.toml")
            self.assertEqual(models_path, user_dir / "models.toml")

    def test_should_prefer_user_directory_over_project_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            source_dir = root / "source"
            cwd.mkdir()
            user_dir.mkdir()
            source_dir.mkdir()
            cwd_config = cwd / "config.toml"
            user_config = user_dir / "config.toml"
            cwd_config.write_text("cwd = true", encoding="utf-8")
            user_config.write_text("user = true", encoding="utf-8")

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch("omnicrawl.config.core.runtime.project_root", return_value=source_dir):
                            self.assertEqual(resolve_config_path(), user_config)

    def test_should_write_default_config_to_user_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            cwd.mkdir()
            user_dir.mkdir()
            (cwd / "config.toml").write_text("project = true", encoding="utf-8")

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        saved = save_config_data({"user": True})

            self.assertEqual(saved, user_dir / "config.toml")
            self.assertTrue(saved.is_file())
            self.assertEqual(tomllib.loads(saved.read_text(encoding="utf-8")), {"user": True})
            self.assertEqual(
                tomllib.loads((cwd / "config.toml").read_text(encoding="utf-8")),
                {"project": True},
            )

    def test_installed_mode_ignores_working_directory_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            cwd.mkdir()
            user_dir.mkdir()
            cwd_config = cwd / "config.toml"
            cwd_config.write_text("cwd = true", encoding="utf-8")
            user_config = user_dir / "config.toml"
            user_config.write_text("user = true", encoding="utf-8")

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch.object(
                            runtime_module,
                            "_is_development_environment",
                            return_value=False,
                        ):
                            self.assertEqual(resolve_config_path(), user_config)

    def test_should_ignore_working_and_source_dirs_even_in_development(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            source_dir = root / "source"
            cwd.mkdir()
            user_dir.mkdir()
            source_dir.mkdir()
            source_config = source_dir / "config.toml"
            source_models = source_dir / "models.toml"
            source_config.write_text("source = true\n", encoding="utf-8")
            source_models.write_text("source = true\n", encoding="utf-8")

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch("omnicrawl.config.core.runtime.legacy_user_config_dirs", return_value=()):
                            with patch("omnicrawl.config.core.runtime.project_root", return_value=source_dir):
                                with patch.object(
                                    runtime_module,
                                    "_is_development_environment",
                                    return_value=True,
                                    create=True,
                                ):
                                    config_path = resolve_config_path()
                                    models_path = resolve_models_path()

            self.assertEqual(config_path, user_dir / "config.toml")
            self.assertEqual(models_path, user_dir / "models.toml")

    def test_should_keep_user_directory_as_target_when_no_candidate_exists_in_production(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cwd = root / "cwd"
            user_dir = root / "user"
            source_dir = root / "source"
            cwd.mkdir()
            user_dir.mkdir()
            source_dir.mkdir()

            with patch.dict(
                os.environ,
                {"AI_CONFIG_FILE": "", "AI_MODELS_FILE": ""},
                clear=False,
            ):
                with patch("omnicrawl.config.core.runtime.Path.cwd", return_value=cwd):
                    with patch("omnicrawl.config.core.runtime.user_config_dir", return_value=user_dir):
                        with patch("omnicrawl.config.core.runtime.legacy_user_config_dirs", return_value=()):
                            with patch("omnicrawl.config.core.runtime.project_root", return_value=source_dir):
                                with patch.object(
                                    runtime_module,
                                    "_is_development_environment",
                                    return_value=False,
                                    create=True,
                                ):
                                    config_path = resolve_config_path()
                                    models_path = resolve_models_path()

            self.assertEqual(config_path, user_dir / "config.toml")
            self.assertEqual(models_path, user_dir / "models.toml")

    def test_load_config_data_accepts_utf8_bom_toml(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            payload = dump_toml_text(
                {"llm": {"model": "demo"}},
            ).encode("utf-8")
            config_path.write_bytes(b"\xef\xbb\xbf" + payload)

            data = load_config_data(config_path)

        self.assertEqual(data["llm"]["model"], "demo")

    def test_save_config_data_writes_toml_utf8_without_bom(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"

            save_config_data({"approval": {"mode": "auto"}}, config_path)
            raw = config_path.read_bytes()

        self.assertFalse(raw.startswith(b"\xef\xbb\xbf"))
        self.assertEqual(
            tomllib.loads(raw.decode("utf-8"))["approval"]["mode"],
            "auto",
        )

    def test_should_retry_atomic_config_replace_when_windows_temporarily_denies_access(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            original_replace = os.replace
            replace_attempts = 0

            def temporarily_denied(src: str | os.PathLike[str], dst: str | os.PathLike[str]) -> None:
                nonlocal replace_attempts
                replace_attempts += 1
                if replace_attempts < 3:
                    error = PermissionError(13, "Access is denied", str(dst))
                    error.winerror = 5
                    raise error
                original_replace(src, dst)

            with (
                patch.object(runtime_module.os, "replace", side_effect=temporarily_denied),
                patch.object(runtime_module.sys, "platform", "win32"),
                patch.object(runtime_module.time, "sleep"),
            ):
                save_config_data({"approval": {"mode": "manual"}}, config_path)

            self.assertEqual(replace_attempts, 3)
            self.assertEqual(
                tomllib.loads(config_path.read_text(encoding="utf-8"))["approval"]["mode"],
                "manual",
            )

    def test_explicit_json_config_path_is_rejected_for_load_and_save(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            config_path.write_text('{"approval":{"mode":"auto"}}', encoding="utf-8")

            with self.assertRaisesRegex(RuntimeConfigError, "JSON 配置已停止支持"):
                load_config_data(config_path)
            with self.assertRaisesRegex(RuntimeConfigError, "JSON 配置已停止支持"):
                save_config_data({"approval": {"mode": "manual"}}, config_path)

    def test_json_config_path_from_environment_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.json"
            with patch.dict(os.environ, {"AI_CONFIG_FILE": str(config_path)}, clear=False):
                with self.assertRaisesRegex(RuntimeConfigError, "JSON 配置已停止支持"):
                    load_config_data()

    def test_default_loader_reports_legacy_json_without_reading_it(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            yaml_path = Path(temp_dir) / "config.toml"
            json_path = Path(temp_dir) / "config.json"
            json_path.write_text("not valid json and must not be parsed", encoding="utf-8")

            with patch(
                "omnicrawl.config.core.runtime.Path.cwd",
                return_value=Path(temp_dir),
            ):
                with patch(
                    "omnicrawl.config.core.runtime.user_config_dir",
                    return_value=Path(temp_dir),
                ):
                    with patch("omnicrawl.config.core.runtime.legacy_user_config_dirs", return_value=()):
                        with patch(
                            "omnicrawl.config.core.runtime.project_root",
                            return_value=Path(temp_dir),
                        ):
                            with self.assertRaisesRegex(RuntimeConfigError, "不会读取或自动迁移 JSON"):
                                load_config_data()

    def test_parse_args_accepts_resume_session_id(self) -> None:
        args = _parse_args(["--resume", "20260616-201530-a1b2c3"])

        self.assertEqual(args.resume, "20260616-201530-a1b2c3")

    def test_main_stops_before_tui_when_api_key_is_missing(self) -> None:
        setup = SimpleNamespace(api_key_configured=False, errors=())
        with patch("omnicrawl.entry.configure_console_encoding"):
            with patch(
                "omnicrawl.entry.initialize_user_configuration",
                return_value=setup,
            ) as initialize:
                with patch("omnicrawl.entry.format_startup_report", return_value=("missing key",)):
                    with patch("omnicrawl.entry.load_llm_config") as load_llm_config:
                        with patch("builtins.print") as print_mock:
                            code = run_application([])

        self.assertEqual(code, 2)
        initialize.assert_called_once()
        self.assertTrue(callable(initialize.call_args.kwargs["channel_setup"]))
        load_llm_config.assert_not_called()
        print_mock.assert_called_once_with("missing key")

    def test_main_reports_readable_error_when_fullscreen_ui_dependency_is_missing(self) -> None:
        with patch("omnicrawl.entry.configure_console_encoding"):
            with patch("omnicrawl.entry.initialize_user_configuration") as initialize:
                initialize.return_value = SimpleNamespace(
                    api_key_configured=True,
                    errors=(),
                )
                with patch("omnicrawl.entry.format_startup_report", return_value=()):
                    with patch("omnicrawl.entry.load_llm_config", return_value=SimpleNamespace()):
                        with patch("omnicrawl.entry._load_fullscreen_ui", side_effect=UIStartupError("缺少可选终端界面依赖：textual。请执行 pip install -r requirements.txt。")):
                            with patch("builtins.print") as print_mock:
                                code = run_application([])

        self.assertEqual(code, 1)
        print_mock.assert_called_once_with(
            "界面启动失败：缺少可选终端界面依赖：textual。请执行 pip install -r requirements.txt。"
        )

    def test_main_starts_fullscreen_ui_and_closes_agent(self) -> None:
        config = SimpleNamespace(
            thinking_enabled=True,
            reasoning_effort="max",
        )
        project_context = SimpleNamespace(
            workspace_root=Path("D:/workspace"),
            detection_summary="workspace",
            source="fallback_start",
            marker="",
        )
        agent = Mock()

        def fake_load_feature_enabled(section: str, default: bool = True, **_kwargs) -> bool:
            # 测试不得依赖本机 ~/.OmniCrawl/config.toml，只放行已知功能默认值。
            defaults = {
                "memory": True,
            }
            return bool(defaults.get(section, default))

        with patch("omnicrawl.entry.configure_console_encoding"):
            with patch("omnicrawl.entry.load_show_thinking", return_value=True):
                with patch("omnicrawl.entry.load_llm_config", return_value=config):
                    with patch("omnicrawl.entry.load_approval_mode", return_value="manual"):
                        with patch(
                            "omnicrawl.entry.load_feature_enabled",
                            side_effect=fake_load_feature_enabled,
                        ):
                            with patch("omnicrawl.entry.load_agent_temp_workspace_config", return_value="temp-config"):
                                with patch("omnicrawl.entry.load_subagent_config", return_value="subagent-config"):
                                    with patch("omnicrawl.entry.detect_project_context", return_value=project_context):
                                        with patch("omnicrawl.entry.agent_temp_status_label", return_value=".omnicrawl/.agent_tmp"):
                                            with patch("omnicrawl.entry.AgentConfig") as agent_config_class:
                                                with patch("omnicrawl.entry.LocalToolAgent", return_value=agent) as agent_class:
                                                    with patch("omnicrawl.entry._load_fullscreen_ui") as load_fullscreen_ui:
                                                        fullscreen_startup = Mock()
                                                        run_fullscreen_tui = Mock()
                                                        load_fullscreen_ui.return_value = (
                                                            fullscreen_startup,
                                                            run_fullscreen_tui,
                                                        )
                                                        with patch("omnicrawl.entry.initialize_user_configuration") as initialize:
                                                            initialize.return_value = SimpleNamespace(
                                                                api_key_configured=True,
                                                                errors=(),
                                                            )
                                                            with patch("omnicrawl.entry.format_startup_report", return_value=()):
                                                                with patch(
                                                                    "omnicrawl.connectors.autostart.start_configured_connectors",
                                                                    return_value=SimpleNamespace(close=Mock()),
                                                                ):
                                                                    code = run_application(["--resume", "session-demo"])

        self.assertEqual(code, 0)
        agent_config_class.assert_called_once_with(
            llm=config,
            workspace_root=Path("D:/workspace"),
            workspace_detection_summary="workspace",
            approval_mode="manual",
            memory_enabled=True,
            show_thinking=True,
            temp_workspace="temp-config",
            subagents="subagent-config",
            resume_session_id="session-demo",
        )
        agent_class.assert_called_once()
        agent_config = agent_class.call_args.args[0]
        self.assertEqual(agent_config, agent_config_class.return_value)
        fullscreen_startup.assert_called_once()
        self.assertTrue(
            fullscreen_startup.call_args.kwargs["version_check_enabled"]
        )
        run_fullscreen_tui.assert_called_once_with(agent, fullscreen_startup.return_value)
        agent.close.assert_called_once()

    def test_windows_launcher_stays_in_existing_interactive_terminal(self) -> None:
        """从 PowerShell 等交互式终端启动时不应另开窗口。"""

        with patch("omnicrawl.ui.windows_launcher.os.name", "nt"):
            with patch("omnicrawl.ui.windows_launcher._running_in_powershell_child", return_value=False):
                with patch("omnicrawl.ui.windows_launcher._has_interactive_terminal", return_value=True):
                    with patch("omnicrawl.ui.windows_launcher.subprocess.Popen") as popen:
                        launched = launch_in_powershell_window(Path("main.py"), [])

        self.assertFalse(launched)
        popen.assert_not_called()

    def test_windows_launcher_forwards_resume_argument(self) -> None:
        popen_calls: list[dict[str, object]] = []

        def fake_popen(command, **kwargs):
            popen_calls.append({"command": command, **kwargs})
            return object()

        with patch("omnicrawl.ui.windows_launcher.os.name", "nt"):
            with patch("omnicrawl.ui.windows_launcher._running_in_powershell_child", return_value=False):
                with patch("omnicrawl.ui.windows_launcher._has_interactive_terminal", return_value=False):
                    with patch("omnicrawl.ui.windows_launcher.subprocess.Popen", side_effect=fake_popen):
                        with patch("omnicrawl.ui.windows_launcher.subprocess.CREATE_NEW_CONSOLE", 16, create=True):
                            launched = launch_in_powershell_window(
                                Path("main.py"),
                                ["--resume", "20260616-201530-a1b2c3"],
                            )

        self.assertTrue(launched)
        command = popen_calls[0]["command"]
        self.assertIsInstance(command, list)
        power_shell_command = command[-1]
        self.assertIn("'--resume' '20260616-201530-a1b2c3'", power_shell_command)


    def test_windows_launcher_keeps_window_open_after_exit(self) -> None:
        """子进程退出后应由仍存活的 PowerShell 宿主清理协议，并保留窗口回到启动目录。"""

        popen_calls: list[dict[str, object]] = []

        def fake_popen(command, **kwargs):
            popen_calls.append({"command": command, **kwargs})
            return object()

        with patch("omnicrawl.ui.windows_launcher.os.name", "nt"):
            with patch("omnicrawl.ui.windows_launcher._running_in_powershell_child", return_value=False):
                with patch("omnicrawl.ui.windows_launcher.subprocess.Popen", side_effect=fake_popen):
                    with patch("omnicrawl.ui.windows_launcher.subprocess.CREATE_NEW_CONSOLE", 16, create=True):
                        launched = launch_in_powershell_window(Path("main.py"), [])

        self.assertTrue(launched)
        command = popen_calls[0]["command"]
        self.assertIsInstance(command, list)
        self.assertIn("-NoExit", command)
        power_shell_command = command[-1]
        self.assertIn("$appExitCode=$LASTEXITCODE", power_shell_command)
        self.assertIn("$esc=[char]27", power_shell_command)
        for mode in ("1000l", "1003l", "1006l", "1004l", "2004l", "<u", "25h"):
            self.assertIn(mode, power_shell_command)
        self.assertIn("FlushInputBuffer", power_shell_command)
        # 退出后显式回到启动目录，且不直接关闭窗口。
        self.assertIn("Set-Location -LiteralPath", power_shell_command)
        self.assertNotIn("exit $appExitCode", power_shell_command)
        self.assertNotIn("Read-Host", power_shell_command)
        self.assertIn("界面意外退出", power_shell_command)



if __name__ == "__main__":
    unittest.main()
