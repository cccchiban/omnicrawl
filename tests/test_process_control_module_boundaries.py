from __future__ import annotations

import importlib
import unittest
from pathlib import Path

from omnicrawl.workspace import monitor, process_control


class ProcessControlModuleBoundaryTests(unittest.TestCase):
    """锁定 Windows 进程树控制实现的真实模块边界。"""

    def test_process_control_is_real_module(self) -> None:
        package_path = Path(monitor.__file__).resolve().parent
        module = importlib.import_module("omnicrawl.workspace.process_control")
        self.assertEqual(module.__name__, "omnicrawl.workspace.process_control")
        self.assertEqual(Path(module.__file__).resolve(), package_path / "process_control.py")

    def test_monitor_reexports_private_process_helpers(self) -> None:
        self.assertIs(
            monitor._assign_process_to_kill_on_close_job,
            process_control.assign_process_to_kill_on_close_job,
        )
        self.assertIs(
            monitor._close_windows_handle,
            process_control.close_windows_handle,
        )

    def test_monitor_no_longer_embeds_job_object_structures(self) -> None:
        source = Path(monitor.__file__).read_text(encoding="utf-8")
        self.assertNotIn("class _JobObjectExtendedLimitInformation", source)
        self.assertNotIn("CreateJobObjectW", source)
        self.assertNotIn("from ctypes import wintypes", source)
