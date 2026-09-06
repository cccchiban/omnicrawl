from __future__ import annotations

import io
import re
import threading
import time
import unicodedata
import unittest
from unittest import mock

from omnicrawl.ui.splash import (
    LOGO_LINES,
    StartupLogSink,
    _draw_box_content,
    _draw_progress_bar,
    _logo_render_lines,
    run_startup_splash,
)


class _TTYBuffer(io.StringIO):
    """模拟交互式终端输出流（isatty=True），同时记录写入内容。"""

    def isatty(self) -> bool:
        return True


class NonInteractiveSplashTest(unittest.TestCase):
    def test_non_tty_runs_prepare_synchronously_and_returns_value(self) -> None:
        marker = {}

        def prepare(sink) -> str:
            marker["called"] = True
            sink.write_line("MCP 初始化完成")
            return "prepared"

        out = io.StringIO()
        result = run_startup_splash(prepare, duration=0.2, stream=out)

        self.assertEqual(result, "prepared")
        self.assertTrue(marker.get("called"))
        # 非交互流不应渲染任何 ANSI 动画
        self.assertEqual(out.getvalue(), "")

    def test_non_tty_propagates_prepare_exception(self) -> None:
        def prepare(_sink) -> None:
            raise RuntimeError("prepare boom")

        with self.assertRaisesRegex(RuntimeError, "prepare boom"):
            run_startup_splash(prepare, duration=0.2, stream=io.StringIO())


class InteractiveSplashTest(unittest.TestCase):
    def test_tty_renders_log_box_and_returns_prepare_value(self) -> None:
        marker = {}

        def prepare(sink) -> str:
            marker["called"] = True
            return "prepared"

        out = _TTYBuffer()
        result = run_startup_splash(prepare, duration=0.2, stream=out)

        self.assertEqual(result, "prepared")
        self.assertTrue(marker.get("called"))
        rendered = out.getvalue()
        # 包含黑底、亮黄与 logo 首行字符
        self.assertIn("\x1b[40m", rendered)
        self.assertIn("\x1b[93m", rendered)
        self.assertIn(LOGO_LINES[0].strip(), rendered)
        # 右侧圆角日志框边框字符
        self.assertIn("╭", rendered)
        self.assertIn("╯", rendered)
        self.assertIn("│", rendered)
        # 底部滚动条轨道背景与滑块背景
        self.assertIn("\x1b[100m", rendered)
        self.assertIn("\x1b[103m", rendered)
        # 结束时恢复光标与颜色
        self.assertIn("\x1b[?25h", rendered)
        self.assertIn("\x1b[0m", rendered)

    def test_tty_renders_log_entries_with_level_colors(self) -> None:
        """启动日志按级别着色：信息「- 」、警告「! 」（黄）、错误「× 」（红）。"""

        out = _TTYBuffer()

        def prepare(sink) -> None:
            sink.write_line("MCP 初始化完成")
            sink.write_line("飞书连接器启动成功")
            sink.write_line("插件配置缺失", level="warning")
            sink.write_line("MCP 能力加载异常", level="error")
            # 停留若干帧，确保日志框在写入后被完整重绘。
            time.sleep(0.2)

        run_startup_splash(prepare, duration=0.3, stream=out)
        rendered = out.getvalue()
        self.assertIn("- MCP 初始化完成", rendered)
        self.assertIn("- 飞书连接器启动成功", rendered)
        self.assertIn("! 插件配置缺失", rendered)
        self.assertIn("\x1b[33m", rendered)  # 警告黄色
        self.assertIn("× MCP 能力加载异常", rendered)
        self.assertIn("\x1b[91m", rendered)  # 错误亮红

    def test_tty_propagates_prepare_exception(self) -> None:
        def prepare(_sink) -> None:
            raise ValueError("worker boom")

        out = _TTYBuffer()
        with self.assertRaisesRegex(ValueError, "worker boom"):
            run_startup_splash(prepare, duration=0.2, stream=out)
        # 画面结束后仍恢复终端
        self.assertTrue(out.getvalue().endswith("\x1b[?25h\x1b[0m\x1b[2J\x1b[H"))

    def test_tty_waits_at_least_duration(self) -> None:
        start = time.monotonic()
        run_startup_splash(lambda sink: None, duration=0.3, stream=_TTYBuffer())
        elapsed = time.monotonic() - start
        self.assertGreaterEqual(elapsed, 0.3)

    def test_tty_waits_for_slow_prepare(self) -> None:
        """prepare 耗时超过 duration 时，画面应继续显示直到 prepare 完成。"""

        def prepare(_sink) -> None:
            # 预留 0.05s 余量，吸收 Windows 计时器精度抖动（sleep 可能略短于设定值）
            time.sleep(0.5)

        start = time.monotonic()
        run_startup_splash(prepare, duration=0.1, stream=_TTYBuffer())
        elapsed = time.monotonic() - start
        self.assertGreaterEqual(elapsed, 0.5)

    def test_tty_prepare_runs_on_background_thread(self) -> None:
        main_thread = threading.current_thread()
        observed: dict[str, bool] = {}

        def prepare(_sink) -> None:
            observed["background"] = threading.current_thread() is not main_thread

        run_startup_splash(prepare, duration=0.1, stream=_TTYBuffer())
        self.assertTrue(observed.get("background"))

    def test_narrow_terminal_log_box_does_not_overlap_logo(self) -> None:
        """窄终端下日志框不得左移到 Logo 区域内（回归守护）。"""
        out = _TTYBuffer()
        with mock.patch(
            "omnicrawl.ui.splash.shutil.get_terminal_size",
            return_value=(80, 24),
        ):
            run_startup_splash(lambda sink: None, duration=0.05, stream=out)
        rendered = out.getvalue()

        # 解析所有 “行;列 + 内容” 的光标定位，找出 Logo 最长行与日志框左边框
        positions = [
            (int(row), int(col), text)
            for row, col, text in re.findall(
                r"\x1b\[(\d+);(\d+)H(?:\x1b\[[0-9;]*m)*([^\x1b]*)",
                rendered,
            )
        ]
        longest = max(_logo_render_lines(), key=len)
        logo_col = next(col for row, col, text in positions if text == longest)
        logo_right = logo_col + len(longest) - 1
        box_border_col = next(col for row, col, text in positions if "│" in text)
        # 日志框左边框必须位于 Logo 右侧，且至少保留 1 列间隙（不能贴住或覆盖）
        self.assertGreaterEqual(box_border_col - logo_right, 2)

    def test_long_log_lines_wrap_inside_box_within_terminal(self) -> None:
        """长日志折行后完整可见，且任何绘制都不越过终端右边界。"""
        out = _TTYBuffer()
        width = 100

        def prepare(sink) -> None:
            sink.write_line("GPU: NVIDIA GeForce RTX 4070 Laptop GPU with 16GB VRAM")
            sink.write_line("飞书连接器初始化完成（全角校验）")
            time.sleep(0.2)

        with mock.patch(
            "omnicrawl.ui.splash.shutil.get_terminal_size",
            return_value=(width, 24),
        ):
            run_startup_splash(prepare, duration=0.3, stream=out)
        rendered = out.getvalue()
        # 折行内容不丢失
        self.assertIn("4070", rendered)
        self.assertIn("Laptop", rendered)
        self.assertIn("飞书连接器初始化完成（全角校验）", rendered)
        # 任何一次光标定位绘制的文本都不能越过终端右边界（CJK 按 2 列计）
        segments = re.findall(
            r"\x1b\[(\d+);(\d+)H((?:[^\x1b]|\x1b\[[0-9;?]*[a-zA-Z])*?)(?=\x1b\[\d+;\d+H|\Z)",
            rendered,
        )
        for row, col, raw in segments:
            text = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw)
            if text:
                display_width = sum(
                    2 if unicodedata.east_asian_width(ch) in {"W", "F"} else 1
                    for ch in text
                )
                self.assertLessEqual(
                    int(col) + display_width - 1,
                    width,
                    f"row {row} 超出终端右边界: {text!r}",
                )


class StartupLogBoxTest(unittest.TestCase):
    """圆角日志框内容：级别着色与超出后滚动保留最新行。"""

    def test_box_content_scrolls_to_latest_lines(self) -> None:
        out = io.StringIO()
        entries = tuple(
            ("info", f"第 {index} 行")
            for index in range(1, 21)
        )
        _draw_box_content(
            out,
            top=1,
            left=1,
            inner_width=20,
            inner_rows=6,
            entries=entries,
        )
        rendered = out.getvalue()
        self.assertNotIn("第 1 行", rendered)
        self.assertNotIn("第 8 行", rendered)
        self.assertIn("第 15 行", rendered)
        self.assertIn("第 20 行", rendered)

    def test_box_content_colors_warning_and_error(self) -> None:
        out = io.StringIO()
        entries = (
            ("info", "普通信息"),
            ("warning", "注意告警"),
            ("error", "严重错误"),
        )
        _draw_box_content(
            out,
            top=1,
            left=1,
            inner_width=20,
            inner_rows=6,
            entries=entries,
        )
        rendered = out.getvalue()
        self.assertIn("- 普通信息", rendered)
        self.assertIn("\x1b[33m! 注意告警", rendered)
        self.assertIn("\x1b[91m× 严重错误", rendered)

    def test_sink_ignores_unknown_level_and_blank_lines(self) -> None:
        sink = StartupLogSink()
        sink.write_line("", level="info")
        sink.write_line("  ", level="warning")
        sink.write_line("正常日志", level="debug")
        self.assertEqual(sink.snapshot(), (("info", "正常日志"),))


class LogoContentTest(unittest.TestCase):
    def test_logo_has_expected_shape(self) -> None:
        self.assertEqual(len(LOGO_LINES), 14)
        widths = {len(line) for line in LOGO_LINES}
        self.assertGreaterEqual(max(widths), 40)
        self.assertTrue(LOGO_LINES[0].startswith(" "))

    def test_logo_render_lines_removes_common_leading_spaces(self) -> None:
        lines = _logo_render_lines()
        self.assertEqual(len(lines), len(LOGO_LINES))
        # 至少有一行不再以空格开头（公共前导空白已去除）
        self.assertTrue(any(not line.startswith(" ") for line in lines))
        # 去除公共前导空格后 Logo 最大宽度应小于原始 LOGO_LINES 最大宽度
        self.assertLess(
            max(len(line) for line in lines),
            max(len(line) for line in LOGO_LINES),
        )


class ProgressBarColorRegressionTest(unittest.TestCase):
    """回归守护：滑块右侧轨道必须保持灰色，不能沿用滑块黄色背景。"""

    def test_slider_right_side_track_remains_gray(self) -> None:
        buf = io.StringIO()
        # f=6：滑块完整出现在轨道左端 [0,6)，右侧 [6,30) 必须是灰色轨道
        _draw_progress_bar(buf, 1, 0, 30, 6, 6)
        rendered = buf.getvalue()
        esc = chr(27)
        # 滑块（黄）与轨道（灰）都应存在
        self.assertIn(esc + "[103m", rendered)
        self.assertIn(esc + "[100m", rendered)
        # 灰色必须出现 2 次（滑块左、右两侧轨道），滑块黄只出现 1 次
        self.assertGreater(
            rendered.count(esc + "[100m"),
            rendered.count(esc + "[103m"),
        )


if __name__ == "__main__":
    unittest.main()