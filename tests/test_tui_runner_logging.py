"""全屏 TUI 运行期日志落盘的单元测试。

验证 TUI runner 把 Python logging 收进用户 logs/ 目录（避免 WARNING/ERROR
经 lastResort 直写 stderr 覆盖 Textual 全屏画面），退出时正确卸载 handler。
"""

from __future__ import annotations

import logging
import unittest
from unittest.mock import patch

from omnicrawl.ui.fullscreen.app.runner import (
    _TUI_LOG_FILENAME,
    _attach_tui_log_file,
    _detach_tui_log_file,
)


class TuiLogFileTests(unittest.TestCase):
    def test_attach_writes_warning_to_file_and_removes_on_detach(self) -> None:
        with patch(
            "omnicrawl.ui.fullscreen.app.runner.user_config_dir"
        ) as fake_config_dir:
            import tempfile
            from pathlib import Path

            temp_root = Path(tempfile.mkdtemp(prefix="omnicrawl-tui-log-"))
            fake_config_dir.return_value = temp_root

            handler = _attach_tui_log_file()
            self.assertIsNotNone(handler)
            assert handler is not None
            log_path = Path(handler.baseFilename)
            self.assertEqual(log_path.name, _TUI_LOG_FILENAME)
            self.assertTrue(log_path.parent.is_dir())

            logger = logging.getLogger("omnicrawl.test.tui_runner")
            logger.warning("TUI 运行期警告应落盘")
            self.assertIn(
                "TUI 运行期警告应落盘",
                log_path.read_text(encoding="utf-8"),
            )

            _detach_tui_log_file(handler)
            self.assertNotIn(handler, logging.getLogger().handlers)
            self.assertTrue(handler.stream.closed if handler.stream else True)

    def test_attach_failure_returns_none(self) -> None:
        with patch(
            "omnicrawl.ui.fullscreen.app.runner.logging.FileHandler",
            side_effect=OSError("模拟磁盘失败"),
        ):
            self.assertIsNone(_attach_tui_log_file())

    def test_detach_none_is_noop(self) -> None:
        _detach_tui_log_file(None)  # 不应抛异常


if __name__ == "__main__":
    unittest.main()
