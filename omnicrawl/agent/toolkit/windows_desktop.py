"""Windows 桌面自动化的原生 Agent 工具实现。

这里把 Win32 窗口/输入/剪贴板 API 与 Windows 内置 .NET UI Automation
封装为结构化、可审批的 ToolResult。模块在非 Windows 平台可以安全导入，
但所有实际操作都会返回明确的“不支持”错误，避免影响跨平台测试与启动。
"""

from __future__ import annotations

import base64
import ctypes
import json
from contextlib import contextmanager
import os
import shutil
import struct
import subprocess
import time
from ctypes import wintypes
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable
from uuid import uuid4

from ..types import ToolImageAttachment, ToolResult


MAX_WINDOW_RESULTS = 100
MAX_CONTROL_RESULTS = 100
MAX_CONTROL_TEXT_CHARS = 8_192
MAX_INPUT_TEXT_CHARS = 4_096
MAX_CLIPBOARD_TEXT_CHARS = 32_768
CLIPBOARD_OPEN_RETRIES = 3
CLIPBOARD_RETRY_DELAY_SECONDS = 0.05
UI_AUTOMATION_TIMEOUT_SECONDS = 30
DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4
MAX_SCREENSHOT_SOURCE_PIXELS = 100_000_000
DEFAULT_SCREENSHOT_MAX_DIMENSION = 2_048
MAX_SCREENSHOT_DIMENSION = 8_192
MAX_MODEL_IMAGE_BYTES = 5 * 1024 * 1024
MIN_MODEL_IMAGE_DIMENSION = 256

# Win32 常量：仅覆盖本工具实际需要的最小集合，避免把任意窗口消息暴露给模型。
SW_RESTORE = 9
SM_XVIRTUALSCREEN = 76
SM_YVIRTUALSCREEN = 77
SM_CXVIRTUALSCREEN = 78
SM_CYVIRTUALSCREEN = 79
CF_UNICODETEXT = 13
GMEM_MOVEABLE = 0x0002
SRCCOPY = 0x00CC0020
CAPTUREBLT = 0x40000000
HALFTONE = 4

INPUT_MOUSE = 0
INPUT_KEYBOARD = 1
MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_RIGHTDOWN = 0x0008
MOUSEEVENTF_RIGHTUP = 0x0010
MOUSEEVENTF_MIDDLEDOWN = 0x0020
MOUSEEVENTF_MIDDLEUP = 0x0040
MOUSEEVENTF_WHEEL = 0x0800
MOUSEEVENTF_VIRTUALDESK = 0x4000
MOUSEEVENTF_ABSOLUTE = 0x8000
KEYEVENTF_KEYUP = 0x0002
KEYEVENTF_UNICODE = 0x0004

# UIA ControlType 名称与 PowerShell 固定脚本中的映射保持一一对应。模型先 list，
# 再用返回的 name/automation_id/control_type 精确定位，不依赖易漂移的屏幕坐标。
CONTROL_TYPE_NAMES = frozenset(
    {
        "button",
        "checkbox",
        "combobox",
        "edit",
        "hyperlink",
        "list",
        "listitem",
        "menu",
        "menuitem",
        "radiobutton",
        "tab",
        "tabitem",
        "text",
        "tree",
        "treeitem",
        "window",
        "pane",
        "document",
        "custom",
        "group",
    }
)

# 常用虚拟键。输入普通 Unicode 文本应使用 type_text；key/hotkey 只接受明确的
# 键名，禁止把任意整数直接透传给 SendInput，避免不透明的按键注入。
VIRTUAL_KEY_CODES: dict[str, int] = {
    "backspace": 0x08,
    "tab": 0x09,
    "enter": 0x0D,
    "shift": 0x10,
    "ctrl": 0x11,
    "alt": 0x12,
    "pause": 0x13,
    "caps_lock": 0x14,
    "esc": 0x1B,
    "space": 0x20,
    "page_up": 0x21,
    "page_down": 0x22,
    "end": 0x23,
    "home": 0x24,
    "left": 0x25,
    "up": 0x26,
    "right": 0x27,
    "down": 0x28,
    "print_screen": 0x2C,
    "insert": 0x2D,
    "delete": 0x2E,
    "win": 0x5B,
    "rwin": 0x5C,
    "apps": 0x5D,
    "num_lock": 0x90,
    "scroll_lock": 0x91,
}
VIRTUAL_KEY_CODES.update({str(number): 0x30 + number for number in range(10)})
VIRTUAL_KEY_CODES.update({chr(code): code for code in range(ord("A"), ord("Z") + 1)})
VIRTUAL_KEY_CODES.update({f"f{number}": 0x6F + number for number in range(1, 25)})

KEY_NAME_ALIASES = {
    "control": "ctrl",
    "escape": "esc",
    "return": "enter",
    "pgup": "page_up",
    "pgdn": "page_down",
    "del": "delete",
    "ins": "insert",
    "windows": "win",
    "lwin": "win",
}


class WindowsDesktopToolError(RuntimeError):
    """Windows 桌面工具的参数、平台或原生 API 调用失败。"""


class _MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouse_data", wintypes.DWORD),
        ("flags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("extra_info", ctypes.c_size_t),
    ]


class _KEYBDINPUT(ctypes.Structure):
    _fields_ = [
        ("virtual_key", wintypes.WORD),
        ("scan_code", wintypes.WORD),
        ("flags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("extra_info", ctypes.c_size_t),
    ]


class _HARDWAREINPUT(ctypes.Structure):
    _fields_ = [
        ("message", wintypes.DWORD),
        ("param_low", wintypes.WORD),
        ("param_high", wintypes.WORD),
    ]


class _INPUT_UNION(ctypes.Union):
    _fields_ = [
        ("mouse", _MOUSEINPUT),
        ("keyboard", _KEYBDINPUT),
        ("hardware", _HARDWAREINPUT),
    ]


class _INPUT(ctypes.Structure):
    _fields_ = [("type", wintypes.DWORD), ("data", _INPUT_UNION)]


class _GUID(ctypes.Structure):
    _fields_ = [
        ("data1", wintypes.DWORD),
        ("data2", wintypes.WORD),
        ("data3", wintypes.WORD),
        ("data4", ctypes.c_ubyte * 8),
    ]


class _GDIPLUS_STARTUP_INPUT(ctypes.Structure):
    _fields_ = [
        ("gdiplus_version", wintypes.UINT),
        ("debug_event_callback", ctypes.c_void_p),
        ("suppress_background_thread", wintypes.BOOL),
        ("suppress_external_codecs", wintypes.BOOL),
    ]


PNG_ENCODER_CLSID = _GUID(
    0x557CF406,
    0x1A04,
    0x11D3,
    (ctypes.c_ubyte * 8)(0x9A, 0x73, 0x00, 0x00, 0xF8, 0x1E, 0xF3, 0x2E),
)


# 固定脚本只接收 Python 以 Base64 传入的 JSON，不把任何模型提供的数据拼进
# PowerShell 源码，从而避免控件名称、文本值等内容变成命令注入载体。
_UI_AUTOMATION_SCRIPT = r"""
$ErrorActionPreference = "Stop"
# Windows PowerShell 的非交互 stdout 默认可能使用本地代码页；Python Host 固定按 UTF-8
# 读取，因此在脚本内明确统一输出编码，避免中文控件名被错误解码。
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = $utf8NoBom
$OutputEncoding = $utf8NoBom

function Get-ControlTypeMap {
    return @{
        "button" = [System.Windows.Automation.ControlType]::Button
        "checkbox" = [System.Windows.Automation.ControlType]::CheckBox
        "combobox" = [System.Windows.Automation.ControlType]::ComboBox
        "edit" = [System.Windows.Automation.ControlType]::Edit
        "hyperlink" = [System.Windows.Automation.ControlType]::Hyperlink
        "list" = [System.Windows.Automation.ControlType]::List
        "listitem" = [System.Windows.Automation.ControlType]::ListItem
        "menu" = [System.Windows.Automation.ControlType]::Menu
        "menuitem" = [System.Windows.Automation.ControlType]::MenuItem
        "radiobutton" = [System.Windows.Automation.ControlType]::RadioButton
        "tab" = [System.Windows.Automation.ControlType]::Tab
        "tabitem" = [System.Windows.Automation.ControlType]::TabItem
        "text" = [System.Windows.Automation.ControlType]::Text
        "tree" = [System.Windows.Automation.ControlType]::Tree
        "treeitem" = [System.Windows.Automation.ControlType]::TreeItem
        "window" = [System.Windows.Automation.ControlType]::Window
        "pane" = [System.Windows.Automation.ControlType]::Pane
        "document" = [System.Windows.Automation.ControlType]::Document
        "custom" = [System.Windows.Automation.ControlType]::Custom
        "group" = [System.Windows.Automation.ControlType]::Group
    }
}

function Get-ElementSummary([System.Windows.Automation.AutomationElement]$Element) {
    $rect = $Element.Current.BoundingRectangle
    $controlTypeName = [string]$Element.Current.ControlType.ProgrammaticName
    if ($controlTypeName.StartsWith("ControlType.")) {
        $controlTypeName = $controlTypeName.Substring("ControlType.".Length)
    }

    return [PSCustomObject]@{
        name = [string]$Element.Current.Name
        automation_id = [string]$Element.Current.AutomationId
        class_name = [string]$Element.Current.ClassName
        control_type = $controlTypeName.ToLowerInvariant()
        process_id = [int]$Element.Current.ProcessId
        is_enabled = [bool]$Element.Current.IsEnabled
        is_offscreen = [bool]$Element.Current.IsOffscreen
        bounds = [PSCustomObject]@{
            left = [int][Math]::Round($rect.X)
            top = [int][Math]::Round($rect.Y)
            width = [int][Math]::Round($rect.Width)
            height = [int][Math]::Round($rect.Height)
        }
    }
}

function New-SearchCondition($Request) {
    $conditions = New-Object "System.Collections.Generic.List[System.Windows.Automation.Condition]"
    if ($null -ne $Request.name -and [string]$Request.name -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::NameProperty,
            [string]$Request.name
        ))
    }
    if ($null -ne $Request.automation_id -and [string]$Request.automation_id -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::AutomationIdProperty,
            [string]$Request.automation_id
        ))
    }
    if ($null -ne $Request.class_name -and [string]$Request.class_name -ne "") {
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ClassNameProperty,
            [string]$Request.class_name
        ))
    }
    if ($null -ne $Request.control_type -and [string]$Request.control_type -ne "") {
        $controlTypeMap = Get-ControlTypeMap
        $controlType = $controlTypeMap[[string]$Request.control_type]
        if ($null -eq $controlType) {
            throw "不支持的 control_type：$($Request.control_type)"
        }
        $conditions.Add([System.Windows.Automation.PropertyCondition]::new(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            $controlType
        ))
    }

    $condition = [System.Windows.Automation.Condition]::TrueCondition
    foreach ($item in $conditions) {
        $condition = [System.Windows.Automation.AndCondition]::new($condition, $item)
    }
    return $condition
}

try {
    Add-Type -AssemblyName UIAutomationClient
    $encoded = [string]$env:OMNICRAWL_UIA_REQUEST_B64
    if ([string]::IsNullOrWhiteSpace($encoded)) {
        throw "未收到 UI Automation 请求。"
    }
    $raw = [System.Text.Encoding]::UTF8.GetString([System.Convert]::FromBase64String($encoded))
    $request = $raw | ConvertFrom-Json
    $root = [System.Windows.Automation.AutomationElement]::FromHandle([IntPtr][Int64]$request.window_handle)
    if ($null -eq $root) {
        throw "无法从 window_handle 获取 UI Automation 根元素。"
    }

    $condition = New-SearchCondition $request
    $matches = $root.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        $condition
    )
    $matchCount = [int]$matches.Count

    if ([string]$request.action -eq "list") {
        $limit = [Math]::Min($matchCount, [int]$request.max_results)
        $items = New-Object "System.Collections.Generic.List[object]"
        for ($index = 0; $index -lt $limit; $index++) {
            $items.Add((Get-ElementSummary $matches.Item($index)))
        }
        $result = [PSCustomObject]@{
            action = "list"
            matched_count = $matchCount
            truncated = [bool]($matchCount -gt $limit)
            controls = @($items.ToArray())
        }
    }
    else {
        if ($matchCount -eq 0) {
            throw "未找到匹配的 UI 控件。"
        }
        if ($null -eq $request.index) {
            if ($matchCount -ne 1) {
                throw "定位条件匹配到 $matchCount 个控件；请先 list，或提供 index。"
            }
            $selectedIndex = 0
        }
        else {
            $selectedIndex = [int]$request.index
            if ($selectedIndex -lt 0 -or $selectedIndex -ge $matchCount) {
                throw "index 超出匹配范围：0 到 $($matchCount - 1)。"
            }
        }

        $element = $matches.Item($selectedIndex)
        switch ([string]$request.action) {
            "invoke" {
                $pattern = [System.Windows.Automation.InvokePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.InvokePattern]::Pattern
                )
                $pattern.Invoke()
            }
            "set_value" {
                if ($null -eq $request.value) {
                    throw "set_value 必须提供 value。"
                }
                $pattern = [System.Windows.Automation.ValuePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.ValuePattern]::Pattern
                )
                $pattern.SetValue([string]$request.value)
            }
            "select" {
                $pattern = [System.Windows.Automation.SelectionItemPattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.SelectionItemPattern]::Pattern
                )
                $pattern.Select()
            }
            "toggle" {
                $pattern = [System.Windows.Automation.TogglePattern]$element.GetCurrentPattern(
                    [System.Windows.Automation.TogglePattern]::Pattern
                )
                $pattern.Toggle()
            }
            "focus" {
                $element.SetFocus()
            }
            default {
                throw "不支持的 UI 控件操作：$($request.action)"
            }
        }
        $result = [PSCustomObject]@{
            action = [string]$request.action
            index = $selectedIndex
            target = Get-ElementSummary $element
        }
    }

    [Console]::Out.Write((([PSCustomObject]@{ ok = $true; result = $result }) | ConvertTo-Json -Compress -Depth 8))
}
catch {
    [Console]::Out.Write((([PSCustomObject]@{ ok = $false; error = $_.Exception.Message }) | ConvertTo-Json -Compress -Depth 4))
    exit 1
}
"""


class WindowsDesktopTools:
    """通过固定、结构化的接口提供 Windows 桌面自动化能力。

    工具不接受任意窗口消息、任意 PowerShell 脚本或原始虚拟键整数；这既降低
    误操作面积，也让审批 UI 能展示可理解的动作、坐标与目标窗口。
    """

    def __init__(
        self,
        *,
        screenshot_directory: Path | None = None,
        workspace_root: Path | None = None,
    ) -> None:
        self._user32_api: Any | None = None
        self._kernel32_api: Any | None = None
        self._gdi32_api: Any | None = None
        self._gdiplus_api: Any | None = None
        self._enum_windows_callback_type: Any | None = None
        self._screenshot_directory = (
            screenshot_directory.resolve() if screenshot_directory is not None else None
        )
        self._workspace_root = workspace_root.resolve() if workspace_root is not None else None

    @staticmethod
    def is_supported() -> bool:
        """返回当前 Host 是否可使用 Windows 原生桌面 API。"""

        return os.name == "nt"

    def run_window(self, arguments: dict[str, Any]) -> ToolResult:
        """执行窗口枚举、几何读取或前台激活。"""

        return self._run_operation(self._window_operation, arguments)

    def run_control(self, arguments: dict[str, Any]) -> ToolResult:
        """执行基于 UI Automation 的控件发现与语义化操作。"""

        return self._run_operation(self._control_operation, arguments)

    def run_input(self, arguments: dict[str, Any]) -> ToolResult:
        """执行受限的鼠标与键盘 SendInput 操作。"""

        return self._run_operation(self._input_operation, arguments)

    def run_clipboard(self, arguments: dict[str, Any]) -> ToolResult:
        """执行 Unicode 文本剪贴板读取、写入或清空。"""

        return self._run_operation(self._clipboard_operation, arguments)

    def run_screenshot(self, arguments: dict[str, Any]) -> ToolResult:
        """截取虚拟桌面、屏幕区域或指定窗口并提供视觉模型附件。"""

        try:
            payload, output_path = self._screenshot_operation(arguments)
            image_bytes = output_path.read_bytes()
        except WindowsDesktopToolError as exc:
            return ToolResult(ok=False, output=str(exc))
        except Exception as exc:  # noqa: BLE001 - 原生 API/文件异常统一转成工具结果
            return ToolResult(ok=False, output=f"Windows 截图工具执行失败：{exc}")

        relative_path = str(payload["path"])
        return ToolResult(
            ok=True,
            output=json.dumps(payload, ensure_ascii=False, indent=2),
            ui_artifact={
                "type": "image",
                "title": "Windows 截图",
                "path": relative_path,
                "media_type": "image/png",
                "width": payload["image"]["width"],
                "height": payload["image"]["height"],
            },
            model_images=(
                ToolImageAttachment(
                    media_type="image/png",
                    data_base64=base64.b64encode(image_bytes).decode("ascii"),
                    filename=output_path.name,
                ),
            ),
        )

    def _run_operation(
        self,
        operation: Callable[[dict[str, Any]], dict[str, Any]],
        arguments: dict[str, Any],
    ) -> ToolResult:
        try:
            payload = operation(arguments)
        except WindowsDesktopToolError as exc:
            return ToolResult(ok=False, output=str(exc))
        except Exception as exc:  # noqa: BLE001 - 原生 API 异常必须变为模型可读结果
            return ToolResult(ok=False, output=f"Windows 桌面工具执行失败：{exc}")
        return ToolResult(ok=True, output=json.dumps(payload, ensure_ascii=False, indent=2))

    def _window_operation(self, arguments: dict[str, Any]) -> dict[str, Any]:
        self._ensure_windows()
        _ensure_allowed_keys(
            arguments,
            {
                "action",
                "window_handle",
                "title_contains",
                "class_name_contains",
                "visible_only",
                "include_untitled",
                "max_results",
            },
        )
        action = _read_action(arguments, {"list", "get", "activate"})
        if action == "list":
            title_contains = _read_optional_text(arguments, "title_contains", maximum=512)
            class_name_contains = _read_optional_text(
                arguments,
                "class_name_contains",
                maximum=512,
            )
            visible_only = _read_bool(arguments, "visible_only", default=True)
            include_untitled = _read_bool(arguments, "include_untitled", default=False)
            max_results = _read_bounded_int(
                arguments,
                "max_results",
                default=50,
                minimum=1,
                maximum=MAX_WINDOW_RESULTS,
            )
            matches = []
            for item in self._enumerate_windows():
                title = str(item.get("title") or "")
                class_name = str(item.get("class_name") or "")
                if visible_only and not bool(item.get("is_visible", True)):
                    continue
                if not include_untitled and not title:
                    continue
                if title_contains and title_contains.casefold() not in title.casefold():
                    continue
                if class_name_contains and class_name_contains.casefold() not in class_name.casefold():
                    continue
                matches.append(item)
            return {
                "action": "list",
                "matched_count": len(matches),
                "truncated": len(matches) > max_results,
                "windows": matches[:max_results],
            }

        handle = _read_window_handle(arguments)
        details = self._describe_window(handle)
        if action == "activate":
            self._activate_window(handle)
            details["activated"] = True
        return {"action": action, "window": details}

    def _control_operation(self, arguments: dict[str, Any]) -> dict[str, Any]:
        self._ensure_windows()
        _ensure_allowed_keys(
            arguments,
            {
                "action",
                "window_handle",
                "name",
                "automation_id",
                "class_name",
                "control_type",
                "index",
                "value",
                "max_results",
            },
        )
        action = _read_action(
            arguments,
            {"list", "invoke", "set_value", "select", "toggle", "focus"},
        )
        window_handle = _read_window_handle(arguments)
        name = _read_optional_text(arguments, "name", maximum=512)
        automation_id = _read_optional_text(arguments, "automation_id", maximum=512)
        class_name = _read_optional_text(arguments, "class_name", maximum=512)
        control_type = _read_optional_text(arguments, "control_type", maximum=64)
        if control_type:
            control_type = control_type.casefold().replace("_", "")
            # listitem/radiobutton 等名称不含分隔符；兼容模型常见的 list_item 写法。
            if control_type not in CONTROL_TYPE_NAMES:
                raise WindowsDesktopToolError(
                    "control_type 不支持："
                    f"{arguments.get('control_type')}。可用值：{', '.join(sorted(CONTROL_TYPE_NAMES))}。"
                )

        selectors = (name, automation_id, class_name, control_type)
        if action != "list" and not any(selectors):
            raise WindowsDesktopToolError(
                "控件操作必须至少提供 name、automation_id、class_name 或 control_type 之一作为定位条件。"
            )

        index = _read_optional_bounded_int(
            arguments,
            "index",
            minimum=0,
            maximum=MAX_CONTROL_RESULTS - 1,
        )
        if action == "list":
            max_results = _read_bounded_int(
                arguments,
                "max_results",
                default=30,
                minimum=1,
                maximum=MAX_CONTROL_RESULTS,
            )
        else:
            max_results = 0

        value: str | None = None
        if action == "set_value":
            raw_value = arguments.get("value")
            if not isinstance(raw_value, str):
                raise WindowsDesktopToolError("set_value 的 value 必须是字符串。")
            if len(raw_value) > MAX_CONTROL_TEXT_CHARS:
                raise WindowsDesktopToolError(
                    f"value 不能超过 {MAX_CONTROL_TEXT_CHARS} 个字符。"
                )
            value = raw_value
        elif "value" in arguments:
            raise WindowsDesktopToolError("只有 set_value 操作支持 value 参数。")

        return self._run_ui_automation(
            {
                "action": action,
                "window_handle": window_handle,
                "name": name,
                "automation_id": automation_id,
                "class_name": class_name,
                "control_type": control_type,
                "index": index,
                "value": value,
                "max_results": max_results,
            }
        )

    def _input_operation(self, arguments: dict[str, Any]) -> dict[str, Any]:
        self._ensure_windows()
        _ensure_allowed_keys(
            arguments,
            {
                "action",
                "x",
                "y",
                "button",
                "clicks",
                "wheel_delta",
                "key",
                "keys",
                "presses",
                "text",
            },
        )
        action = _read_action(
            arguments,
            {"move", "click", "scroll", "key", "hotkey", "type_text"},
        )
        if action == "move":
            x, y = _read_coordinates(arguments, required=True)
            assert x is not None and y is not None
            self._send_mouse_move(x, y)
            return {"action": action, "x": x, "y": y}

        if action == "click":
            x, y = _read_coordinates(arguments, required=True)
            assert x is not None and y is not None
            button = str(arguments.get("button") or "left").strip().casefold()
            if button not in {"left", "right", "middle"}:
                raise WindowsDesktopToolError("button 仅支持 left、right 或 middle。")
            clicks = _read_bounded_int(
                arguments,
                "clicks",
                default=1,
                minimum=1,
                maximum=3,
            )
            self._send_mouse_click(x, y, button, clicks)
            return {"action": action, "x": x, "y": y, "button": button, "clicks": clicks}

        if action == "scroll":
            x, y = _read_coordinates(arguments, required=False)
            delta = _read_bounded_int(
                arguments,
                "wheel_delta",
                default=0,
                minimum=-12_000,
                maximum=12_000,
                allow_zero=False,
            )
            self._send_mouse_scroll(x, y, delta)
            payload: dict[str, Any] = {"action": action, "wheel_delta": delta}
            if x is not None and y is not None:
                payload.update({"x": x, "y": y})
            return payload

        if action == "key":
            key = _normalise_virtual_key(arguments.get("key"))
            presses = _read_bounded_int(
                arguments,
                "presses",
                default=1,
                minimum=1,
                maximum=10,
            )
            self._send_virtual_keys([key], presses)
            return {"action": action, "key": key, "presses": presses}

        if action == "hotkey":
            raw_keys = arguments.get("keys")
            if not isinstance(raw_keys, list) or not 2 <= len(raw_keys) <= 5:
                raise WindowsDesktopToolError("hotkey 的 keys 必须是包含 2 到 5 个键名的数组。")
            keys = [_normalise_virtual_key(item) for item in raw_keys]
            if len(set(keys)) != len(keys):
                raise WindowsDesktopToolError("hotkey 的 keys 不能包含重复键名。")
            self._send_virtual_keys(keys)
            return {"action": action, "keys": keys}

        raw_text = arguments.get("text")
        if not isinstance(raw_text, str) or not raw_text:
            raise WindowsDesktopToolError("type_text 的 text 必须是非空字符串。")
        if len(raw_text) > MAX_INPUT_TEXT_CHARS:
            raise WindowsDesktopToolError(
                f"text 不能超过 {MAX_INPUT_TEXT_CHARS} 个字符。"
            )
        self._send_unicode_text(raw_text)
        # 不回显文本，避免输入密码、令牌等内容被工具结果或会话记录持久化。
        return {"action": action, "text_length": len(raw_text)}

    def _clipboard_operation(self, arguments: dict[str, Any]) -> dict[str, Any]:
        self._ensure_windows()
        _ensure_allowed_keys(arguments, {"action", "text", "max_chars"})
        action = _read_action(arguments, {"read_text", "write_text", "clear"})
        if action == "read_text":
            max_chars = _read_bounded_int(
                arguments,
                "max_chars",
                default=8_000,
                minimum=1,
                maximum=MAX_CLIPBOARD_TEXT_CHARS,
            )
            text = self._read_clipboard_text()
            return {
                "action": action,
                "text": text[:max_chars],
                "total_chars": len(text),
                "truncated": len(text) > max_chars,
            }
        if action == "write_text":
            text = arguments.get("text")
            if not isinstance(text, str):
                raise WindowsDesktopToolError("write_text 的 text 必须是字符串。")
            if len(text) > MAX_CLIPBOARD_TEXT_CHARS:
                raise WindowsDesktopToolError(
                    f"text 不能超过 {MAX_CLIPBOARD_TEXT_CHARS} 个字符。"
                )
            self._write_clipboard_text(text)
            return {"action": action, "text_length": len(text)}

        self._clear_clipboard()
        return {"action": action}

    def _screenshot_operation(
        self,
        arguments: dict[str, Any],
    ) -> tuple[dict[str, Any], Path]:
        self._ensure_windows()
        _ensure_allowed_keys(
            arguments,
            {
                "target",
                "window_handle",
                "x",
                "y",
                "width",
                "height",
                "max_dimension",
            },
        )
        target = _read_choice(
            arguments,
            "target",
            allowed={"desktop", "region", "window"},
            default="desktop",
        )
        max_dimension = _read_bounded_int(
            arguments,
            "max_dimension",
            default=DEFAULT_SCREENSHOT_MAX_DIMENSION,
            minimum=MIN_MODEL_IMAGE_DIMENSION,
            maximum=MAX_SCREENSHOT_DIMENSION,
        )
        desktop = self._virtual_desktop_bounds()

        if target == "desktop":
            _reject_present_keys(arguments, {"window_handle", "x", "y", "width", "height"})
            source_bounds = dict(desktop)
        elif target == "region":
            _reject_present_keys(arguments, {"window_handle"})
            x, y = _read_coordinates(arguments, required=True)
            assert x is not None and y is not None
            width = _read_bounded_int(
                arguments,
                "width",
                default=0,
                minimum=1,
                maximum=100_000,
            )
            height = _read_bounded_int(
                arguments,
                "height",
                default=0,
                minimum=1,
                maximum=100_000,
            )
            source_bounds = _bounds_from_xywh(x, y, width, height)
        else:
            _reject_present_keys(arguments, {"x", "y", "width", "height"})
            handle = _read_window_handle(arguments)
            window = self._describe_window(handle)
            if bool(window.get("is_minimized")):
                raise WindowsDesktopToolError("目标窗口已最小化；请先恢复窗口后再截图。")
            raw_bounds = window.get("bounds")
            if not isinstance(raw_bounds, dict):
                raise WindowsDesktopToolError("无法读取目标窗口的有效边界。")
            window_bounds = _bounds_from_xywh(
                int(raw_bounds.get("left", 0)),
                int(raw_bounds.get("top", 0)),
                int(raw_bounds.get("width", 0)),
                int(raw_bounds.get("height", 0)),
            )
            # GetWindowRect 可能包含落在虚拟桌面外的 DWM 边框（最大化窗口常见），
            # 或窗口被用户拖出屏幕一部分。窗口截图取当前可见交集；完全不可见才失败。
            source_bounds = _intersect_bounds(window_bounds, desktop)
            if source_bounds is None:
                raise WindowsDesktopToolError("目标窗口当前完全位于虚拟桌面之外，无法截图。")

        _validate_screenshot_bounds(source_bounds, desktop)
        source_width = int(source_bounds["width"])
        source_height = int(source_bounds["height"])
        if source_width * source_height > MAX_SCREENSHOT_SOURCE_PIXELS:
            raise WindowsDesktopToolError(
                f"截图源区域不能超过 {MAX_SCREENSHOT_SOURCE_PIXELS:,} 像素。"
            )

        output_path = self._new_screenshot_path(target)
        try:
            image_width, image_height, byte_count = self._capture_screenshot_png(
                source_bounds,
                output_path,
                max_dimension=max_dimension,
            )
        except Exception:
            try:
                output_path.unlink(missing_ok=True)
            except OSError:
                pass
            raise

        return (
            {
                "target": target,
                "path": self._display_screenshot_path(output_path),
                "source_bounds": source_bounds,
                "image": {
                    "media_type": "image/png",
                    "width": image_width,
                    "height": image_height,
                    "bytes": byte_count,
                    "scaled": image_width != source_width or image_height != source_height,
                },
            },
            output_path,
        )

    def _virtual_desktop_bounds(self) -> dict[str, int]:
        user32 = self._user32()
        with self._per_monitor_dpi_context():
            left = int(user32.GetSystemMetrics(SM_XVIRTUALSCREEN))
            top = int(user32.GetSystemMetrics(SM_YVIRTUALSCREEN))
            width = int(user32.GetSystemMetrics(SM_CXVIRTUALSCREEN))
            height = int(user32.GetSystemMetrics(SM_CYVIRTUALSCREEN))
        return _bounds_from_xywh(left, top, width, height)

    def _new_screenshot_path(self, target: str) -> Path:
        directory = self._screenshot_directory
        if directory is None:
            raise WindowsDesktopToolError(
                "Agent 临时图片目录未启用，无法保存截图；请启用 agent_temp。"
            )
        try:
            directory.mkdir(parents=True, exist_ok=True)
        except OSError as exc:
            raise WindowsDesktopToolError(f"创建截图目录失败：{exc}") from exc
        timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S_%fZ")
        return directory / f"windows_{target}_{timestamp}_{uuid4().hex[:8]}.png"

    def _display_screenshot_path(self, path: Path) -> str:
        workspace_root = self._workspace_root
        if workspace_root is not None:
            try:
                return str(path.relative_to(workspace_root))
            except ValueError:
                pass
        return str(path)

    def _capture_screenshot_png(
        self,
        source_bounds: dict[str, int],
        output_path: Path,
        *,
        max_dimension: int,
    ) -> tuple[int, int, int]:
        """用 GDI 抓取物理屏幕像素，并用系统 GDI+ 编码为 PNG。

        先按 ``max_dimension`` 缩放；若 PNG 仍超过跨 Provider 较保守的 5 MiB
        内联限制，则继续等比缩小。截图文件与发送给模型的图片保持完全一致。
        """

        source_width = source_bounds["width"]
        source_height = source_bounds["height"]
        with self._per_monitor_dpi_context():
            bitmap = self._capture_screen_bitmap(
                source_bounds["left"],
                source_bounds["top"],
                source_width,
                source_height,
            )
            current_width = source_width
            current_height = source_height
            try:
                target_width, target_height = _fit_dimensions(
                    current_width,
                    current_height,
                    max_dimension=max_dimension,
                )
                if (target_width, target_height) != (current_width, current_height):
                    resized = self._resize_bitmap(
                        bitmap,
                        current_width,
                        current_height,
                        target_width,
                        target_height,
                    )
                    self._delete_gdi_object(bitmap)
                    bitmap = resized
                    current_width, current_height = target_width, target_height

                while True:
                    self._save_bitmap_as_png(bitmap, output_path)
                    byte_count = output_path.stat().st_size
                    if byte_count <= MAX_MODEL_IMAGE_BYTES:
                        return current_width, current_height, byte_count
                    if max(current_width, current_height) <= MIN_MODEL_IMAGE_DIMENSION:
                        raise WindowsDesktopToolError(
                            "截图 PNG 超过视觉模型的安全内联大小限制，且无法继续缩小。"
                        )
                    # 两个维度使用同一缩放因子，避免超宽/超高截图在接近下限时变形。
                    target_width = max(1, round(current_width * 0.75))
                    target_height = max(1, round(current_height * 0.75))
                    if (target_width, target_height) == (current_width, current_height):
                        raise WindowsDesktopToolError("截图无法继续缩小到模型可接受大小。")
                    resized = self._resize_bitmap(
                        bitmap,
                        current_width,
                        current_height,
                        target_width,
                        target_height,
                    )
                    self._delete_gdi_object(bitmap)
                    bitmap = resized
                    current_width, current_height = target_width, target_height
            finally:
                self._delete_gdi_object(bitmap)

    def _enumerate_windows(self) -> list[dict[str, Any]]:
        user32 = self._user32()
        callback_type = self._enum_windows_callback_type
        assert callback_type is not None
        rows: list[dict[str, Any]] = []

        @callback_type
        def visit(hwnd: Any, _lparam: Any) -> bool:
            # 单个窗口可能在枚举期间已销毁；跳过它而不是中止整次可观察操作。
            try:
                handle = _handle_to_int(hwnd)
                if handle:
                    rows.append(self._describe_window(handle))
            except WindowsDesktopToolError:
                pass
            return True

        with self._per_monitor_dpi_context():
            ctypes.set_last_error(0)
            if not user32.EnumWindows(visit, 0):
                _raise_last_error("枚举窗口失败")
        return rows

    def _describe_window(self, handle: int) -> dict[str, Any]:
        user32 = self._user32()
        if not user32.IsWindow(handle):
            raise WindowsDesktopToolError(f"无效或已关闭的 window_handle：{_format_window_handle(handle)}。")

        with self._per_monitor_dpi_context():
            rect = wintypes.RECT()
            if not user32.GetWindowRect(handle, ctypes.byref(rect)):
                _raise_last_error("读取窗口位置失败")
            title_length = max(0, int(user32.GetWindowTextLengthW(handle)))
            title_buffer = ctypes.create_unicode_buffer(title_length + 1)
            user32.GetWindowTextW(handle, title_buffer, len(title_buffer))
            class_buffer = ctypes.create_unicode_buffer(512)
            user32.GetClassNameW(handle, class_buffer, len(class_buffer))
            process_id = wintypes.DWORD()
            user32.GetWindowThreadProcessId(handle, ctypes.byref(process_id))
            foreground = _handle_to_int(user32.GetForegroundWindow())

            return {
                "window_handle": _format_window_handle(handle),
                "title": title_buffer.value,
                "class_name": class_buffer.value,
                "process_id": int(process_id.value),
                "is_visible": bool(user32.IsWindowVisible(handle)),
                "is_minimized": bool(user32.IsIconic(handle)),
                "is_maximized": bool(user32.IsZoomed(handle)),
                "is_foreground": foreground == handle,
                "bounds": {
                    "left": int(rect.left),
                    "top": int(rect.top),
                    "right": int(rect.right),
                    "bottom": int(rect.bottom),
                    "width": int(rect.right - rect.left),
                    "height": int(rect.bottom - rect.top),
                },
            }

    def _activate_window(self, handle: int) -> None:
        user32 = self._user32()
        if not user32.IsWindow(handle):
            raise WindowsDesktopToolError(f"无效或已关闭的 window_handle：{_format_window_handle(handle)}。")
        if user32.IsIconic(handle):
            user32.ShowWindow(handle, SW_RESTORE)
        if not user32.SetForegroundWindow(handle):
            # 不使用 AttachThreadInput 等绕过焦点保护的技巧，保留 Windows 的用户前台控制权。
            raise WindowsDesktopToolError(
                "Windows 拒绝将目标窗口置于前台；请由用户手动切换窗口后再重试。"
            )

    def _run_ui_automation(self, request: dict[str, Any]) -> dict[str, Any]:
        executable = _find_windows_powershell()
        if executable is None:
            raise WindowsDesktopToolError(
                "未找到 Windows PowerShell，无法加载 Windows UI Automation。"
            )
        encoded_request = base64.b64encode(
            json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        ).decode("ascii")
        environment = os.environ.copy()
        environment["OMNICRAWL_UIA_REQUEST_B64"] = encoded_request
        try:
            completed = subprocess.run(
                [
                    executable,
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    _UI_AUTOMATION_SCRIPT,
                ],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=UI_AUTOMATION_TIMEOUT_SECONDS,
                env=environment,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise WindowsDesktopToolError(
                f"UI Automation 操作超过 {UI_AUTOMATION_TIMEOUT_SECONDS} 秒，已终止。"
            ) from exc
        except OSError as exc:
            raise WindowsDesktopToolError(f"启动 Windows UI Automation 失败：{exc}") from exc

        raw_output = completed.stdout.strip().lstrip("\ufeff")
        try:
            payload = json.loads(raw_output)
        except json.JSONDecodeError as exc:
            detail = _output_preview(completed.stderr or raw_output)
            raise WindowsDesktopToolError(
                "UI Automation 未返回有效 JSON。" + (f" 诊断：{detail}" if detail else "")
            ) from exc
        if not isinstance(payload, dict):
            raise WindowsDesktopToolError("UI Automation 返回格式无效。")
        if not payload.get("ok"):
            message = str(payload.get("error") or "未知 UI Automation 错误。")
            raise WindowsDesktopToolError(f"UI 控件操作失败：{message}")
        if completed.returncode != 0:
            detail = _output_preview(completed.stderr)
            raise WindowsDesktopToolError(
                "UI Automation 进程异常退出。" + (f" 诊断：{detail}" if detail else "")
            )
        result = payload.get("result")
        if not isinstance(result, dict):
            raise WindowsDesktopToolError("UI Automation 未返回结构化结果。")
        return result

    def _send_mouse_move(self, x: int, y: int) -> None:
        self._send_inputs([self._absolute_mouse_input(x, y, MOUSEEVENTF_MOVE)])

    def _send_mouse_click(self, x: int, y: int, button: str, clicks: int) -> None:
        flags = {
            "left": (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            "right": (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            "middle": (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }[button]
        inputs = [self._absolute_mouse_input(x, y, MOUSEEVENTF_MOVE)]
        for _ in range(clicks):
            inputs.extend([_mouse_input(flags=flags[0]), _mouse_input(flags=flags[1])])
        self._send_inputs(inputs)

    def _send_mouse_scroll(self, x: int | None, y: int | None, delta: int) -> None:
        inputs: list[_INPUT] = []
        if x is not None and y is not None:
            inputs.append(self._absolute_mouse_input(x, y, MOUSEEVENTF_MOVE))
        inputs.append(_mouse_input(flags=MOUSEEVENTF_WHEEL, mouse_data=delta))
        self._send_inputs(inputs)

    def _send_virtual_keys(self, keys: list[str], presses: int = 1) -> None:
        codes = [VIRTUAL_KEY_CODES[key] for key in keys]
        inputs: list[_INPUT] = []
        if len(codes) == 1:
            for _ in range(presses):
                inputs.extend([_keyboard_input(codes[0]), _keyboard_input(codes[0], key_up=True)])
        else:
            inputs.extend(_keyboard_input(code) for code in codes)
            inputs.extend(_keyboard_input(code, key_up=True) for code in reversed(codes))
        self._send_inputs(inputs)

    def _send_unicode_text(self, text: str) -> None:
        encoded = text.encode("utf-16-le")
        code_units = struct.unpack(f"<{len(encoded) // 2}H", encoded)
        inputs: list[_INPUT] = []
        for code_unit in code_units:
            inputs.extend(
                [
                    _keyboard_input(0, scan_code=code_unit, unicode=True),
                    _keyboard_input(0, scan_code=code_unit, key_up=True, unicode=True),
                ]
            )
        self._send_inputs(inputs)

    def _absolute_mouse_input(self, x: int, y: int, flags: int) -> _INPUT:
        user32 = self._user32()
        with self._per_monitor_dpi_context():
            left = int(user32.GetSystemMetrics(SM_XVIRTUALSCREEN))
            top = int(user32.GetSystemMetrics(SM_YVIRTUALSCREEN))
            width = int(user32.GetSystemMetrics(SM_CXVIRTUALSCREEN))
            height = int(user32.GetSystemMetrics(SM_CYVIRTUALSCREEN))
        if width <= 1 or height <= 1:
            raise WindowsDesktopToolError("无法读取有效的虚拟桌面尺寸。")
        if not (left <= x < left + width and top <= y < top + height):
            raise WindowsDesktopToolError(
                "坐标超出虚拟桌面范围："
                f"x={x}、y={y}，范围为 [{left}, {left + width - 1}] × [{top}, {top + height - 1}]。"
            )
        normalized_x = round((x - left) * 65_535 / (width - 1))
        normalized_y = round((y - top) * 65_535 / (height - 1))
        return _mouse_input(
            dx=normalized_x,
            dy=normalized_y,
            flags=flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )

    def _send_inputs(self, inputs: list[_INPUT]) -> None:
        if not inputs:
            return
        user32 = self._user32()
        array_type = _INPUT * len(inputs)
        array = array_type(*inputs)
        ctypes.set_last_error(0)
        sent = int(user32.SendInput(len(inputs), array, ctypes.sizeof(_INPUT)))
        if sent != len(inputs):
            _raise_last_error(f"SendInput 仅提交了 {sent}/{len(inputs)} 个输入事件")

    def _capture_screen_bitmap(self, x: int, y: int, width: int, height: int) -> int:
        user32 = self._user32()
        gdi32 = self._gdi32()
        screen_dc = user32.GetDC(None)
        if not screen_dc:
            _raise_last_error("获取屏幕设备上下文失败")
        memory_dc = None
        bitmap = None
        previous_object = None
        try:
            memory_dc = gdi32.CreateCompatibleDC(screen_dc)
            if not memory_dc:
                _raise_last_error("创建截图内存设备上下文失败")
            bitmap = gdi32.CreateCompatibleBitmap(screen_dc, width, height)
            if not bitmap:
                _raise_last_error("创建截图位图失败")
            previous_object = gdi32.SelectObject(memory_dc, bitmap)
            if not previous_object:
                _raise_last_error("选择截图位图失败")
            if not gdi32.BitBlt(
                memory_dc,
                0,
                0,
                width,
                height,
                screen_dc,
                x,
                y,
                SRCCOPY | CAPTUREBLT,
            ):
                _raise_last_error("复制屏幕像素失败")
            return int(bitmap)
        except Exception:
            if bitmap:
                gdi32.DeleteObject(bitmap)
            raise
        finally:
            if memory_dc and previous_object:
                gdi32.SelectObject(memory_dc, previous_object)
            if memory_dc:
                gdi32.DeleteDC(memory_dc)
            user32.ReleaseDC(None, screen_dc)

    def _resize_bitmap(
        self,
        bitmap: int,
        source_width: int,
        source_height: int,
        target_width: int,
        target_height: int,
    ) -> int:
        user32 = self._user32()
        gdi32 = self._gdi32()
        screen_dc = user32.GetDC(None)
        if not screen_dc:
            _raise_last_error("获取缩放设备上下文失败")
        source_dc = None
        target_dc = None
        target_bitmap = None
        previous_source = None
        previous_target = None
        try:
            source_dc = gdi32.CreateCompatibleDC(screen_dc)
            target_dc = gdi32.CreateCompatibleDC(screen_dc)
            if not source_dc or not target_dc:
                _raise_last_error("创建缩放内存设备上下文失败")
            target_bitmap = gdi32.CreateCompatibleBitmap(screen_dc, target_width, target_height)
            if not target_bitmap:
                _raise_last_error("创建缩放目标位图失败")
            previous_source = gdi32.SelectObject(source_dc, bitmap)
            previous_target = gdi32.SelectObject(target_dc, target_bitmap)
            if not previous_source or not previous_target:
                _raise_last_error("选择缩放位图失败")
            gdi32.SetStretchBltMode(target_dc, HALFTONE)
            if not gdi32.StretchBlt(
                target_dc,
                0,
                0,
                target_width,
                target_height,
                source_dc,
                0,
                0,
                source_width,
                source_height,
                SRCCOPY,
            ):
                _raise_last_error("缩放截图位图失败")
            return int(target_bitmap)
        except Exception:
            if target_bitmap:
                gdi32.DeleteObject(target_bitmap)
            raise
        finally:
            if source_dc and previous_source:
                gdi32.SelectObject(source_dc, previous_source)
            if target_dc and previous_target:
                gdi32.SelectObject(target_dc, previous_target)
            if source_dc:
                gdi32.DeleteDC(source_dc)
            if target_dc:
                gdi32.DeleteDC(target_dc)
            user32.ReleaseDC(None, screen_dc)

    def _save_bitmap_as_png(self, bitmap: int, output_path: Path) -> None:
        gdiplus = self._gdiplus()
        startup_input = _GDIPLUS_STARTUP_INPUT(1, None, False, False)
        token = ctypes.c_size_t()
        status = int(gdiplus.GdiplusStartup(ctypes.byref(token), ctypes.byref(startup_input), None))
        if status != 0:
            raise WindowsDesktopToolError(f"启动 GDI+ 失败（状态码 {status}）。")
        image = ctypes.c_void_p()
        try:
            status = int(gdiplus.GdipCreateBitmapFromHBITMAP(bitmap, None, ctypes.byref(image)))
            if status != 0 or not image.value:
                raise WindowsDesktopToolError(f"从截图位图创建 GDI+ 图像失败（状态码 {status}）。")
            status = int(
                gdiplus.GdipSaveImageToFile(
                    image,
                    str(output_path),
                    ctypes.byref(PNG_ENCODER_CLSID),
                    None,
                )
            )
            if status != 0:
                raise WindowsDesktopToolError(f"保存 PNG 截图失败（状态码 {status}）。")
        finally:
            if image.value:
                gdiplus.GdipDisposeImage(image)
            gdiplus.GdiplusShutdown(token)

    def _delete_gdi_object(self, handle: int | None) -> None:
        if handle:
            self._gdi32().DeleteObject(handle)

    def _read_clipboard_text(self) -> str:
        user32 = self._user32()
        kernel32 = self._kernel32()
        self._open_clipboard()
        try:
            handle = user32.GetClipboardData(CF_UNICODETEXT)
            if not handle:
                return ""
            pointer = kernel32.GlobalLock(handle)
            if not pointer:
                _raise_last_error("锁定剪贴板文本失败")
            try:
                return ctypes.wstring_at(pointer)
            finally:
                kernel32.GlobalUnlock(handle)
        finally:
            user32.CloseClipboard()

    def _write_clipboard_text(self, text: str) -> None:
        user32 = self._user32()
        kernel32 = self._kernel32()
        data = (text + "\0").encode("utf-16-le")
        handle = kernel32.GlobalAlloc(GMEM_MOVEABLE, len(data))
        if not handle:
            _raise_last_error("分配剪贴板内存失败")
        transfer_ownership = False
        try:
            pointer = kernel32.GlobalLock(handle)
            if not pointer:
                _raise_last_error("锁定剪贴板内存失败")
            try:
                ctypes.memmove(pointer, data, len(data))
            finally:
                kernel32.GlobalUnlock(handle)

            self._open_clipboard()
            try:
                if not user32.EmptyClipboard():
                    _raise_last_error("清空剪贴板失败")
                if not user32.SetClipboardData(CF_UNICODETEXT, handle):
                    _raise_last_error("写入剪贴板失败")
                # SetClipboardData 成功后由系统负责释放 GlobalAlloc 内存。
                transfer_ownership = True
            finally:
                user32.CloseClipboard()
        finally:
            if not transfer_ownership:
                kernel32.GlobalFree(handle)

    def _clear_clipboard(self) -> None:
        user32 = self._user32()
        self._open_clipboard()
        try:
            if not user32.EmptyClipboard():
                _raise_last_error("清空剪贴板失败")
        finally:
            user32.CloseClipboard()

    def _open_clipboard(self) -> None:
        user32 = self._user32()
        for attempt in range(CLIPBOARD_OPEN_RETRIES):
            ctypes.set_last_error(0)
            if user32.OpenClipboard(None):
                return
            if attempt + 1 < CLIPBOARD_OPEN_RETRIES:
                time.sleep(CLIPBOARD_RETRY_DELAY_SECONDS)
        _raise_last_error("剪贴板正被其他应用占用，无法打开")

    @contextmanager
    def _per_monitor_dpi_context(self):
        """在当前线程临时使用物理像素坐标，随后恢复 Host 原有 DPI 上下文。

        UI Automation 的 BoundingRectangle 以物理像素给出；若 Python 进程保持
        DPI-unaware，上层窗口矩形和 GetSystemMetrics 则可能被缩放虚拟化，二者
        混用会让“根据控件位置点击”落在错误坐标。Win10 1607 之前没有该 API，
        此时保持系统默认行为并让调用按既有坐标空间工作。
        """

        user32 = self._user32()
        set_context = getattr(user32, "SetThreadDpiAwarenessContext", None)
        if set_context is None:
            yield
            return
        try:
            ctypes.set_last_error(0)
            previous_context = set_context(
                ctypes.c_void_p(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            )
        except OSError:
            yield
            return
        if not previous_context:
            yield
            return
        try:
            yield
        finally:
            # 恢复本线程进入工具前的上下文，不改变 TUI、模型调用或其他线程的 DPI 语义。
            set_context(previous_context)

    def _user32(self) -> Any:
        self._ensure_windows()
        if self._user32_api is not None:
            return self._user32_api
        try:
            user32 = ctypes.WinDLL("user32", use_last_error=True)
        except OSError as exc:
            raise WindowsDesktopToolError(f"加载 user32.dll 失败：{exc}") from exc

        callback_type = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
        self._enum_windows_callback_type = callback_type
        user32.EnumWindows.argtypes = [callback_type, wintypes.LPARAM]
        user32.EnumWindows.restype = wintypes.BOOL
        user32.IsWindow.argtypes = [wintypes.HWND]
        user32.IsWindow.restype = wintypes.BOOL
        user32.IsWindowVisible.argtypes = [wintypes.HWND]
        user32.IsWindowVisible.restype = wintypes.BOOL
        user32.IsIconic.argtypes = [wintypes.HWND]
        user32.IsIconic.restype = wintypes.BOOL
        user32.IsZoomed.argtypes = [wintypes.HWND]
        user32.IsZoomed.restype = wintypes.BOOL
        user32.GetWindowRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
        user32.GetWindowRect.restype = wintypes.BOOL
        user32.GetWindowTextLengthW.argtypes = [wintypes.HWND]
        user32.GetWindowTextLengthW.restype = ctypes.c_int
        user32.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
        user32.GetWindowTextW.restype = ctypes.c_int
        user32.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
        user32.GetClassNameW.restype = ctypes.c_int
        user32.GetWindowThreadProcessId.argtypes = [
            wintypes.HWND,
            ctypes.POINTER(wintypes.DWORD),
        ]
        user32.GetWindowThreadProcessId.restype = wintypes.DWORD
        user32.GetForegroundWindow.argtypes = []
        user32.GetForegroundWindow.restype = wintypes.HWND
        user32.ShowWindow.argtypes = [wintypes.HWND, ctypes.c_int]
        user32.ShowWindow.restype = wintypes.BOOL
        user32.SetForegroundWindow.argtypes = [wintypes.HWND]
        user32.SetForegroundWindow.restype = wintypes.BOOL
        try:
            user32.SetThreadDpiAwarenessContext.argtypes = [ctypes.c_void_p]
            user32.SetThreadDpiAwarenessContext.restype = ctypes.c_void_p
        except AttributeError:
            # Windows 10 1607 前没有线程级 DPI 上下文 API；保留兼容回退。
            pass
        user32.GetSystemMetrics.argtypes = [ctypes.c_int]
        user32.GetSystemMetrics.restype = ctypes.c_int
        user32.GetDC.argtypes = [wintypes.HWND]
        user32.GetDC.restype = ctypes.c_void_p
        user32.ReleaseDC.argtypes = [wintypes.HWND, ctypes.c_void_p]
        user32.ReleaseDC.restype = ctypes.c_int
        user32.SendInput.argtypes = [wintypes.UINT, ctypes.POINTER(_INPUT), ctypes.c_int]
        user32.SendInput.restype = wintypes.UINT
        user32.OpenClipboard.argtypes = [wintypes.HWND]
        user32.OpenClipboard.restype = wintypes.BOOL
        user32.CloseClipboard.argtypes = []
        user32.CloseClipboard.restype = wintypes.BOOL
        user32.EmptyClipboard.argtypes = []
        user32.EmptyClipboard.restype = wintypes.BOOL
        user32.GetClipboardData.argtypes = [wintypes.UINT]
        user32.GetClipboardData.restype = ctypes.c_void_p
        user32.SetClipboardData.argtypes = [wintypes.UINT, ctypes.c_void_p]
        user32.SetClipboardData.restype = ctypes.c_void_p
        self._user32_api = user32
        return user32

    def _gdi32(self) -> Any:
        self._ensure_windows()
        if self._gdi32_api is not None:
            return self._gdi32_api
        try:
            gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
        except OSError as exc:
            raise WindowsDesktopToolError(f"加载 gdi32.dll 失败：{exc}") from exc
        gdi32.CreateCompatibleDC.argtypes = [ctypes.c_void_p]
        gdi32.CreateCompatibleDC.restype = ctypes.c_void_p
        gdi32.DeleteDC.argtypes = [ctypes.c_void_p]
        gdi32.DeleteDC.restype = wintypes.BOOL
        gdi32.CreateCompatibleBitmap.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
        gdi32.CreateCompatibleBitmap.restype = ctypes.c_void_p
        gdi32.SelectObject.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        gdi32.SelectObject.restype = ctypes.c_void_p
        gdi32.DeleteObject.argtypes = [ctypes.c_void_p]
        gdi32.DeleteObject.restype = wintypes.BOOL
        gdi32.BitBlt.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            wintypes.DWORD,
        ]
        gdi32.BitBlt.restype = wintypes.BOOL
        gdi32.SetStretchBltMode.argtypes = [ctypes.c_void_p, ctypes.c_int]
        gdi32.SetStretchBltMode.restype = ctypes.c_int
        gdi32.StretchBlt.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            wintypes.DWORD,
        ]
        gdi32.StretchBlt.restype = wintypes.BOOL
        self._gdi32_api = gdi32
        return gdi32

    def _gdiplus(self) -> Any:
        self._ensure_windows()
        if self._gdiplus_api is not None:
            return self._gdiplus_api
        try:
            gdiplus = ctypes.WinDLL("gdiplus", use_last_error=True)
        except OSError as exc:
            raise WindowsDesktopToolError(f"加载 gdiplus.dll 失败：{exc}") from exc
        gdiplus.GdiplusStartup.argtypes = [
            ctypes.POINTER(ctypes.c_size_t),
            ctypes.POINTER(_GDIPLUS_STARTUP_INPUT),
            ctypes.c_void_p,
        ]
        gdiplus.GdiplusStartup.restype = ctypes.c_int
        gdiplus.GdiplusShutdown.argtypes = [ctypes.c_size_t]
        gdiplus.GdiplusShutdown.restype = None
        gdiplus.GdipCreateBitmapFromHBITMAP.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        gdiplus.GdipCreateBitmapFromHBITMAP.restype = ctypes.c_int
        gdiplus.GdipSaveImageToFile.argtypes = [
            ctypes.c_void_p,
            wintypes.LPCWSTR,
            ctypes.POINTER(_GUID),
            ctypes.c_void_p,
        ]
        gdiplus.GdipSaveImageToFile.restype = ctypes.c_int
        gdiplus.GdipDisposeImage.argtypes = [ctypes.c_void_p]
        gdiplus.GdipDisposeImage.restype = ctypes.c_int
        self._gdiplus_api = gdiplus
        return gdiplus

    def _kernel32(self) -> Any:
        self._ensure_windows()
        if self._kernel32_api is not None:
            return self._kernel32_api
        try:
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        except OSError as exc:
            raise WindowsDesktopToolError(f"加载 kernel32.dll 失败：{exc}") from exc
        kernel32.GlobalAlloc.argtypes = [wintypes.UINT, ctypes.c_size_t]
        kernel32.GlobalAlloc.restype = ctypes.c_void_p
        kernel32.GlobalLock.argtypes = [ctypes.c_void_p]
        kernel32.GlobalLock.restype = ctypes.c_void_p
        kernel32.GlobalUnlock.argtypes = [ctypes.c_void_p]
        kernel32.GlobalUnlock.restype = wintypes.BOOL
        kernel32.GlobalFree.argtypes = [ctypes.c_void_p]
        kernel32.GlobalFree.restype = ctypes.c_void_p
        self._kernel32_api = kernel32
        return kernel32

    @staticmethod
    def _ensure_windows() -> None:
        if os.name != "nt":
            raise WindowsDesktopToolError("Windows 桌面原生工具仅支持 Windows。")


def _ensure_allowed_keys(arguments: dict[str, Any], allowed: set[str]) -> None:
    if not isinstance(arguments, dict):
        raise WindowsDesktopToolError("工具参数必须是 JSON 对象。")
    unsupported = set(arguments) - allowed
    if unsupported:
        raise WindowsDesktopToolError("不支持的参数：" + "、".join(sorted(unsupported)) + "。")


def _read_action(arguments: dict[str, Any], allowed: set[str]) -> str:
    raw_action = arguments.get("action")
    if not isinstance(raw_action, str):
        raise WindowsDesktopToolError("action 必须是字符串。")
    action = raw_action.strip().casefold()
    if action not in allowed:
        raise WindowsDesktopToolError("action 不支持：" + raw_action + "。可用值：" + "、".join(sorted(allowed)) + "。")
    return action


def _read_optional_text(arguments: dict[str, Any], key: str, *, maximum: int) -> str | None:
    value = arguments.get(key)
    if value is None:
        return None
    if not isinstance(value, str):
        raise WindowsDesktopToolError(f"{key} 必须是字符串。")
    normalized = value.strip()
    if not normalized:
        return None
    if len(normalized) > maximum:
        raise WindowsDesktopToolError(f"{key} 不能超过 {maximum} 个字符。")
    return normalized


def _read_bool(arguments: dict[str, Any], key: str, *, default: bool) -> bool:
    value = arguments.get(key, default)
    if not isinstance(value, bool):
        raise WindowsDesktopToolError(f"{key} 必须是布尔值。")
    return value


def _read_bounded_int(
    arguments: dict[str, Any],
    key: str,
    *,
    default: int,
    minimum: int,
    maximum: int,
    allow_zero: bool = True,
) -> int:
    raw_value = arguments.get(key, default)
    if isinstance(raw_value, bool):
        raise WindowsDesktopToolError(f"{key} 必须是整数。")
    try:
        value = int(raw_value)
    except (TypeError, ValueError) as exc:
        raise WindowsDesktopToolError(f"{key} 必须是整数。") from exc
    if value < minimum or value > maximum or (not allow_zero and value == 0):
        range_text = f"{minimum} 到 {maximum}"
        if not allow_zero:
            range_text += "，且不能为 0"
        raise WindowsDesktopToolError(f"{key} 必须在 {range_text} 之间。")
    return value


def _read_optional_bounded_int(
    arguments: dict[str, Any],
    key: str,
    *,
    minimum: int,
    maximum: int,
) -> int | None:
    if key not in arguments or arguments.get(key) is None:
        return None
    return _read_bounded_int(
        arguments,
        key,
        default=minimum,
        minimum=minimum,
        maximum=maximum,
    )


def _read_window_handle(arguments: dict[str, Any]) -> int:
    value = arguments.get("window_handle")
    if isinstance(value, bool) or value is None:
        raise WindowsDesktopToolError("window_handle 必须是十进制整数或 0x 开头的十六进制字符串。")
    try:
        if isinstance(value, str):
            handle = int(value.strip(), 0)
        else:
            handle = int(value)
    except (TypeError, ValueError) as exc:
        raise WindowsDesktopToolError(
            "window_handle 必须是十进制整数或 0x 开头的十六进制字符串。"
        ) from exc
    if handle <= 0:
        raise WindowsDesktopToolError("window_handle 必须是正整数。")
    return handle


def _read_coordinates(arguments: dict[str, Any], *, required: bool) -> tuple[int | None, int | None]:
    has_x = arguments.get("x") is not None
    has_y = arguments.get("y") is not None
    if has_x != has_y:
        raise WindowsDesktopToolError("x 与 y 必须同时提供。")
    if not has_x:
        if required:
            raise WindowsDesktopToolError("该操作必须同时提供 x 与 y。")
        return None, None
    x = _read_bounded_int(arguments, "x", default=0, minimum=-100_000, maximum=100_000)
    y = _read_bounded_int(arguments, "y", default=0, minimum=-100_000, maximum=100_000)
    return x, y


def _read_choice(
    arguments: dict[str, Any],
    key: str,
    *,
    allowed: set[str],
    default: str,
) -> str:
    raw_value = arguments.get(key, default)
    if not isinstance(raw_value, str):
        raise WindowsDesktopToolError(f"{key} 必须是字符串。")
    value = raw_value.strip().casefold()
    if value not in allowed:
        raise WindowsDesktopToolError(
            f"{key} 不支持：{raw_value}。可用值：{'、'.join(sorted(allowed))}。"
        )
    return value


def _reject_present_keys(arguments: dict[str, Any], forbidden: set[str]) -> None:
    present = sorted(key for key in forbidden if key in arguments and arguments.get(key) is not None)
    if present:
        raise WindowsDesktopToolError("当前截图目标不支持参数：" + "、".join(present) + "。")


def _bounds_from_xywh(x: int, y: int, width: int, height: int) -> dict[str, int]:
    if width <= 0 or height <= 0:
        raise WindowsDesktopToolError("截图宽度和高度必须为正整数。")
    return {
        "left": int(x),
        "top": int(y),
        "right": int(x + width),
        "bottom": int(y + height),
        "width": int(width),
        "height": int(height),
    }


def _intersect_bounds(
    first: dict[str, int],
    second: dict[str, int],
) -> dict[str, int] | None:
    left = max(first["left"], second["left"])
    top = max(first["top"], second["top"])
    right = min(first["right"], second["right"])
    bottom = min(first["bottom"], second["bottom"])
    if right <= left or bottom <= top:
        return None
    return _bounds_from_xywh(left, top, right - left, bottom - top)


def _validate_screenshot_bounds(
    source: dict[str, int],
    desktop: dict[str, int],
) -> None:
    if source["width"] <= 0 or source["height"] <= 0:
        raise WindowsDesktopToolError("截图区域没有有效尺寸。")
    if (
        source["left"] < desktop["left"]
        or source["top"] < desktop["top"]
        or source["right"] > desktop["right"]
        or source["bottom"] > desktop["bottom"]
    ):
        raise WindowsDesktopToolError(
            "截图区域必须完全位于虚拟桌面内；"
            f"请求区域为 [{source['left']}, {source['top']}, {source['right']}, {source['bottom']}]，"
            f"虚拟桌面为 [{desktop['left']}, {desktop['top']}, {desktop['right']}, {desktop['bottom']}]。"
        )


def _fit_dimensions(width: int, height: int, *, max_dimension: int) -> tuple[int, int]:
    largest = max(width, height)
    if largest <= max_dimension:
        return width, height
    scale = max_dimension / largest
    return max(1, round(width * scale)), max(1, round(height * scale))


def _normalise_virtual_key(value: Any) -> str:
    if not isinstance(value, str):
        raise WindowsDesktopToolError("键名必须是字符串。")
    key = value.strip().casefold().replace("-", "_").replace(" ", "_")
    key = KEY_NAME_ALIASES.get(key, key)
    if len(key) == 1 and key.isalpha():
        key = key.upper()
    if key not in VIRTUAL_KEY_CODES:
        raise WindowsDesktopToolError(f"不支持的键名：{value}。")
    return key


def _mouse_input(
    *,
    dx: int = 0,
    dy: int = 0,
    flags: int = 0,
    mouse_data: int = 0,
) -> _INPUT:
    input_item = _INPUT()
    input_item.type = INPUT_MOUSE
    input_item.data.mouse = _MOUSEINPUT(
        dx=dx,
        dy=dy,
        mouse_data=ctypes.c_uint32(mouse_data).value,
        flags=flags,
        time=0,
        extra_info=0,
    )
    return input_item


def _keyboard_input(
    virtual_key: int,
    *,
    scan_code: int = 0,
    key_up: bool = False,
    unicode: bool = False,
) -> _INPUT:
    flags = (KEYEVENTF_UNICODE if unicode else 0) | (KEYEVENTF_KEYUP if key_up else 0)
    input_item = _INPUT()
    input_item.type = INPUT_KEYBOARD
    input_item.data.keyboard = _KEYBDINPUT(
        virtual_key=virtual_key,
        scan_code=scan_code,
        flags=flags,
        time=0,
        extra_info=0,
    )
    return input_item


def _handle_to_int(value: Any) -> int:
    if isinstance(value, ctypes.c_void_p):
        return int(value.value or 0)
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _format_window_handle(handle: int) -> str:
    return f"0x{handle:016X}"


def _raise_last_error(prefix: str) -> None:
    error_code = ctypes.get_last_error()
    if error_code:
        message = ctypes.FormatError(error_code).strip()
        raise WindowsDesktopToolError(f"{prefix}：{message}（错误码 {error_code}）。")
    raise WindowsDesktopToolError(prefix + "。")


def _find_windows_powershell() -> str | None:
    # UIAutomationClient 在 Windows PowerShell 5.1 中长期随系统提供；优先使用它，
    # PowerShell 7 仅作为安装精简系统上的回退。
    return shutil.which("powershell") or shutil.which("pwsh")


def _output_preview(value: str, *, maximum: int = 1_000) -> str:
    text = str(value or "").strip()
    if len(text) <= maximum:
        return text
    return text[: maximum - 3] + "..."
