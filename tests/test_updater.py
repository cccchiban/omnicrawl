from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import omnicrawl.version_check as version_check_module
from omnicrawl import updater
from omnicrawl.version_check import VersionCheckResult


def _env_without_update_flags() -> dict[str, str]:
    env = dict(os.environ)
    env.pop(updater.SKIP_AUTO_UPDATE_ENV, None)
    env.pop(updater.AUTO_UPDATE_ATTEMPTED_ENV, None)
    return env


class StartupUpdaterSkipTest(unittest.TestCase):
    """验证各种跳过路径都返回 None 且不触碰安装器。"""

    def _driver(self, **kwargs):
        defaults = {
            "latest_checker": lambda: (_ for _ in ()).throw(
                AssertionError("不应发起版本检查")
            ),
            "installer": lambda _latest: (_ for _ in ()).throw(
                AssertionError("不应发起安装")
            ),
            "relauncher": lambda _argv: (_ for _ in ()).throw(
                AssertionError("不应重启")
            ),
        }
        defaults.update(kwargs)
        return updater.run_startup_update_if_due([], **defaults)

    def test_should_skip_in_source_checkout(self) -> None:
        with mock.patch.object(updater, "is_source_checkout", return_value=True):
            self.assertIsNone(self._driver())

    def test_should_skip_when_explicit_env_flag(self) -> None:
        with mock.patch.dict(
            os.environ,
            {updater.SKIP_AUTO_UPDATE_ENV: "1"},
            clear=False,
        ):
            self.assertIsNone(self._driver())

    def test_should_skip_when_already_attempted(self) -> None:
        with mock.patch.dict(
            os.environ,
            {updater.AUTO_UPDATE_ATTEMPTED_ENV: "1"},
            clear=False,
        ):
            self.assertIsNone(self._driver())

    def test_should_skip_when_current_is_up_to_date(self) -> None:
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True):
            result = self._driver(
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", "0.1.35"),
            )
        self.assertIsNone(result)

    def test_should_skip_when_remote_check_failed(self) -> None:
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True):
            result = self._driver(
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", None),
            )
        self.assertIsNone(result)

    def test_should_skip_when_feature_disabled_in_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            config_path.write_text("[update]\nenabled = false\n", encoding="utf-8")
            with mock.patch.object(updater, "is_source_checkout", return_value=False):
                result = updater.run_startup_update_if_due(
                    [],
                    config_path=config_path,
                    latest_checker=lambda: VersionCheckResult("0.1.35", "0.2.0"),
                )
        self.assertIsNone(result)


class StartupUpdaterUpdateTest(unittest.TestCase):
    """验证升级 → 校验 → 重启主流程。"""

    def test_should_install_probe_and_relaunch_when_behind(self) -> None:
        lines: list[str] = []
        relaunched: list[list[str]] = []
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True), \
             mock.patch.dict(os.environ, _env_without_update_flags(), clear=True):
            code = updater.run_startup_update_if_due(
                ["--resume", "abc"],
                print_fn=lines.append,
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", "0.2.0"),
                installer=lambda latest: lines.append(f"install:{latest}") or True,
                version_prober=lambda: "0.2.0",
                relauncher=lambda argv: relaunched.append(list(argv)) or 7,
            )
        self.assertEqual(code, 7)
        self.assertTrue(
            any(line.startswith("发现新版本 0.2.0") for line in lines),
            lines,
        )
        self.assertTrue(any("正在重新启动 TUI" in line for line in lines), lines)
        self.assertEqual(relaunched, [["--resume", "abc"]])

    def test_install_failure_should_continue_current_version(self) -> None:
        lines: list[str] = []
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True), \
             mock.patch.dict(os.environ, _env_without_update_flags(), clear=True):
            code = updater.run_startup_update_if_due(
                [],
                print_fn=lines.append,
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", "0.2.0"),
                installer=lambda _latest: False,
                relauncher=lambda _argv: (_ for _ in ()).throw(
                    AssertionError("失败后不应重启")
                ),
            )
        self.assertIsNone(code)
        self.assertTrue(any("自动升级失败" in line for line in lines), lines)
        self.assertTrue(any("手动执行升级" in line for line in lines), lines)

    def test_probe_mismatch_should_not_relaunch(self) -> None:
        lines: list[str] = []
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True), \
             mock.patch.dict(os.environ, _env_without_update_flags(), clear=True):
            code = updater.run_startup_update_if_due(
                [],
                print_fn=lines.append,
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", "0.2.0"),
                installer=lambda _latest: True,
                version_prober=lambda: "0.1.35",
                relauncher=lambda _argv: (_ for _ in ()).throw(
                    AssertionError("版本未变不应重启")
                ),
            )
        self.assertIsNone(code)
        self.assertTrue(any("版本校验未通过" in line for line in lines), lines)

    def test_should_mark_attempt_env_before_installing(self) -> None:
        with mock.patch.object(updater, "is_source_checkout", return_value=False), \
             mock.patch.object(updater, "_load_update_enabled", return_value=True), \
             mock.patch.dict(os.environ, _env_without_update_flags(), clear=True):
            updater.run_startup_update_if_due(
                [],
                installed_version="0.1.35",
                latest_checker=lambda: VersionCheckResult("0.1.35", "0.2.0"),
                installer=lambda _latest: True,
                version_prober=lambda: "0.2.0",
                relauncher=lambda _argv: 0,
            )
            self.assertEqual(os.environ.get(updater.AUTO_UPDATE_ATTEMPTED_ENV), "1")


class StartupUpdaterHelpersTest(unittest.TestCase):
    def test_is_source_checkout_detects_current_repo(self) -> None:
        # 测试在源码目录内运行：version_check.py 所在包父目录应含 pyproject.toml + .git。
        package_dir = Path(version_check_module.__file__).resolve().parent
        self.assertTrue(updater.is_source_checkout(package_dir))

    def test_is_source_checkout_false_without_repo_markers(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            site_packages = Path(temp_dir) / "site-packages" / "omnicrawl"
            site_packages.mkdir(parents=True)
            self.assertFalse(updater.is_source_checkout(site_packages))

    def test_install_latest_version_builds_pinned_pip_command(self) -> None:
        captured: list[list[str]] = []

        def fake_runner(command):
            captured.append(list(command))
            return 0

        self.assertTrue(updater.install_latest_version("0.2.0", process_runner=fake_runner))
        self.assertEqual(captured[0][:4], [sys.executable, "-m", "pip", "install"])
        self.assertEqual(captured[0][-1], "omnicrawl-agent==0.2.0")

    def test_install_latest_version_failure_maps_to_false(self) -> None:
        self.assertFalse(updater.install_latest_version("0.2.0", process_runner=lambda _cmd: 1))

        def raises(_cmd):
            raise OSError("pip 不可用")

        self.assertFalse(updater.install_latest_version("0.2.0", process_runner=raises))

    def test_probe_installed_version_reads_probe_output(self) -> None:
        completed = subprocess.CompletedProcess([], 0, stdout="0.2.0", stderr="")
        self.assertEqual(
            updater.probe_installed_version(probe_runner=lambda _cmd: completed),
            "0.2.0",
        )
        self.assertIsNone(
            updater.probe_installed_version(probe_runner=lambda _cmd: 0)
        )

    def test_restart_tui_uses_python_module_and_returns_child_code(self) -> None:
        captured: list[list[str]] = []

        def fake_runner(command):
            captured.append(list(command))
            return 9

        self.assertEqual(updater.restart_tui(["--resume", "s1"], process_runner=fake_runner), 9)
        self.assertEqual(captured[0], [sys.executable, "-m", "omnicrawl", "--resume", "s1"])

    def test_restart_tui_launch_error_returns_none(self) -> None:
        def raises(_cmd):
            raise OSError("无法启动")

        self.assertIsNone(updater.restart_tui([], process_runner=raises))


if __name__ == "__main__":
    unittest.main()
