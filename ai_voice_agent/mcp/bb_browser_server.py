from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable


DEFAULT_TIMEOUT_SECONDS = 60


class BBBrowserMCPServerError(RuntimeError):
    """bb-browser MCP 适配器参数校验或命令执行失败。"""


@dataclass(frozen=True)
class _ToolSpec:
    name: str
    description: str
    input_schema: dict[str, Any]
    build_args: Callable[[dict[str, Any]], list[str]]


class BBBrowserMCPServer:
    """把 bb-browser CLI 适配成 stdio MCP Server。

    当前 npm 包的 `bb-browser --mcp` 在本地版本中会输出 CLI 帮助文本，而不是
    MCP 的 Content-Length 帧。这个适配器保留 Host 侧 MCP 协议，由工具调用时再
    转发到本地 bb-browser CLI，避免启动阶段被普通 stdout 污染。
    """

    def __init__(self, workspace_root: Path | None = None) -> None:
        self.workspace_root = (workspace_root or Path.cwd()).resolve()
        self._tools = self._build_tools()

    def run_stdio(self) -> None:
        """运行 stdio JSON-RPC 循环。"""

        while True:
            message = _read_message(sys.stdin.buffer)
            if message is None:
                return
            response = self.handle_message(message)
            if response is not None:
                _write_message(sys.stdout.buffer, response)

    def handle_message(self, message: dict[str, Any]) -> dict[str, Any] | None:
        method = message.get("method")
        request_id = message.get("id")
        try:
            if method == "initialize":
                result = {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {},
                        "resources": {},
                        "prompts": {},
                    },
                    "serverInfo": {"name": "bb-browser-adapter", "version": "0.1"},
                }
            elif method == "notifications/initialized":
                return None
            elif method == "tools/list":
                result = self._list_tools()
            elif method == "tools/call":
                result = self._call_tool(_read_params(message))
            elif method == "resources/list":
                result = {"resources": []}
            elif method == "prompts/list":
                result = {"prompts": []}
            else:
                return _jsonrpc_error(request_id, -32601, f"未知 MCP 方法：{method}")
        except BBBrowserMCPServerError as exc:
            return _jsonrpc_error(request_id, -32000, str(exc))
        except Exception as exc:
            return _jsonrpc_error(request_id, -32603, f"bb-browser MCP 适配器内部错误：{exc}")

        if request_id is None:
            return None
        return {"jsonrpc": "2.0", "id": request_id, "result": result}

    def _build_tools(self) -> dict[str, _ToolSpec]:
        return {
            "browser.status": _ToolSpec(
                name="browser.status",
                description="查看 bb-browser daemon 和受管浏览器状态。",
                input_schema={"type": "object", "properties": {}},
                build_args=lambda _arguments: ["status", "--json"],
            ),
            "browser.tab_list": _ToolSpec(
                name="browser.tab_list",
                description="列出当前可操作的浏览器标签页。",
                input_schema={"type": "object", "properties": {}},
                build_args=lambda _arguments: ["tab", "list", "--json"],
            ),
            "browser.tab_new": _ToolSpec(
                name="browser.tab_new",
                description="新建浏览器标签页，可选 URL。",
                input_schema={
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                },
                build_args=lambda arguments: _compact_args(["tab", "new", _optional_text(arguments, "url"), "--json"]),
            ),
            "browser.open": _ToolSpec(
                name="browser.open",
                description="打开 URL；不传 tab 时新建标签页，传 current 或标签页 id 时复用。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["url"],
                },
                build_args=lambda arguments: _with_optional_flag(
                    ["open", _required_text(arguments, "url"), "--json"],
                    "--tab",
                    _optional_text(arguments, "tab"),
                ),
            ),
            "browser.goto": _ToolSpec(
                name="browser.goto",
                description="让指定标签页导航到新的 URL。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["url", "tab"],
                },
                build_args=lambda arguments: [
                    "goto",
                    _required_text(arguments, "url"),
                    "--tab",
                    _required_text(arguments, "tab"),
                    "--json",
                ],
            ),
            "browser.snapshot": _ToolSpec(
                name="browser.snapshot",
                description="获取指定标签页的可访问性树快照，返回可交互元素 ref。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "tab": {"type": "string"},
                        "interactive": {"type": "boolean"},
                        "compact": {"type": "boolean"},
                        "depth": {"type": "integer"},
                        "selector": {"type": "string"},
                    },
                    "required": ["tab"],
                },
                build_args=_snapshot_args,
            ),
            "browser.get": _ToolSpec(
                name="browser.get",
                description="获取页面或元素的 text、url、title、value 或 html。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "attribute": {"type": "string"},
                        "ref": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["attribute", "tab"],
                },
                build_args=lambda arguments: _compact_args(
                    [
                        "get",
                        _required_text(arguments, "attribute"),
                        _optional_text(arguments, "ref"),
                        "--tab",
                        _required_text(arguments, "tab"),
                        "--json",
                    ]
                ),
            ),
            "browser.click": _ToolSpec(
                name="browser.click",
                description="点击 snapshot 返回的元素 ref。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["ref", "tab"],
                },
                build_args=lambda arguments: [
                    "click",
                    _required_text(arguments, "ref"),
                    "--tab",
                    _required_text(arguments, "tab"),
                    "--json",
                ],
            ),
            "browser.fill": _ToolSpec(
                name="browser.fill",
                description="清空并填充输入框。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string"},
                        "text": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["ref", "text", "tab"],
                },
                build_args=lambda arguments: [
                    "fill",
                    _required_text(arguments, "ref"),
                    _required_text(arguments, "text"),
                    "--tab",
                    _required_text(arguments, "tab"),
                    "--json",
                ],
            ),
            "browser.type_text": _ToolSpec(
                name="browser.type_text",
                description="向输入框追加输入文本，不清空原内容。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "ref": {"type": "string"},
                        "text": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["ref", "text", "tab"],
                },
                build_args=lambda arguments: [
                    "type",
                    _required_text(arguments, "ref"),
                    _required_text(arguments, "text"),
                    "--tab",
                    _required_text(arguments, "tab"),
                    "--json",
                ],
            ),
            "browser.press": _ToolSpec(
                name="browser.press",
                description="向指定标签页发送按键，例如 Enter、Tab、Control+a。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "tab": {"type": "string"},
                    },
                    "required": ["key", "tab"],
                },
                build_args=lambda arguments: [
                    "press",
                    _required_text(arguments, "key"),
                    "--tab",
                    _required_text(arguments, "tab"),
                    "--json",
                ],
            ),
            "browser.evaluate": _ToolSpec(
                name="browser.evaluate",
                description="在标签页或匹配域名的页面上下文中执行 JavaScript。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "script": {"type": "string"},
                        "tab": {"type": "string"},
                        "domain": {"type": "string"},
                        "args": {"type": "string"},
                    },
                    "required": ["script"],
                },
                build_args=_evaluate_args,
            ),
            "browser.site_list": _ToolSpec(
                name="browser.site_list",
                description="列出 bb-browser 可用的 site adapter。",
                input_schema={"type": "object", "properties": {}},
                build_args=lambda _arguments: ["site", "list", "--json"],
            ),
            "browser.site_info": _ToolSpec(
                name="browser.site_info",
                description="查看指定 site adapter 的用法与元信息。",
                input_schema={
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"],
                },
                build_args=lambda arguments: ["site", "info", _required_text(arguments, "name"), "--json"],
            ),
            "browser.site_run": _ToolSpec(
                name="browser.site_run",
                description="运行指定 site adapter；args 会作为 CLI 位置参数依次传入。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "args": {"type": "array", "items": {"type": "string"}},
                        "tab": {"type": "string"},
                    },
                    "required": ["name"],
                },
                build_args=_site_run_args,
            ),
        }

    def _list_tools(self) -> dict[str, Any]:
        return {
            "tools": [
                {
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                }
                for spec in self._tools.values()
            ]
        }

    def _call_tool(self, params: dict[str, Any]) -> dict[str, Any]:
        name = params.get("name")
        arguments = params.get("arguments", {})
        if not isinstance(name, str) or not name:
            raise BBBrowserMCPServerError("tools/call.name 必须是非空字符串。")
        if not isinstance(arguments, dict):
            raise BBBrowserMCPServerError("tools/call.arguments 必须是 JSON 对象。")

        spec = self._tools.get(name)
        if spec is None:
            raise BBBrowserMCPServerError(f"未知工具：{name}")

        try:
            output = self._run_bb_browser(spec.build_args(arguments))
            return {"content": [{"type": "text", "text": output}], "isError": False}
        except BBBrowserMCPServerError as exc:
            return {"content": [{"type": "text", "text": str(exc)}], "isError": True}

    def _run_bb_browser(self, args: list[str]) -> str:
        command = [*_resolve_bb_browser_command(self.workspace_root), *args]
        timeout = _read_timeout()
        try:
            completed = subprocess.run(
                command,
                cwd=str(self.workspace_root),
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
            )
        except FileNotFoundError as exc:
            raise BBBrowserMCPServerError(
                "找不到 bb-browser CLI。请先安装依赖，或设置 BB_BROWSER_COMMAND。"
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise BBBrowserMCPServerError(f"bb-browser 命令超过 {timeout} 秒，已终止。") from exc

        stdout = completed.stdout.strip()
        stderr = completed.stderr.strip()
        if completed.returncode != 0:
            parts = [f"bb-browser 退出码：{completed.returncode}"]
            if stdout:
                parts.append(f"stdout:\n{stdout}")
            if stderr:
                parts.append(f"stderr:\n{stderr}")
            raise BBBrowserMCPServerError("\n\n".join(parts))
        return stdout or stderr or "bb-browser 命令已完成。"


def _resolve_bb_browser_command(workspace_root: Path) -> list[str]:
    raw_command = os.getenv("BB_BROWSER_COMMAND", "").strip()
    if raw_command:
        return [raw_command]

    local_bin = workspace_root / "node_modules" / ".bin" / (
        "bb-browser.cmd" if os.name == "nt" else "bb-browser"
    )
    if local_bin.is_file():
        return [str(local_bin)]

    resolved = shutil.which("bb-browser")
    if resolved:
        return [resolved]

    npx = shutil.which("npx")
    if npx:
        return [npx, "-y", "bb-browser"]

    return ["bb-browser"]


def _snapshot_args(arguments: dict[str, Any]) -> list[str]:
    args = ["snap", "--tab", _required_text(arguments, "tab"), "--json"]
    if bool(arguments.get("interactive")):
        args.append("--interactive")
    if bool(arguments.get("compact")):
        args.append("--compact")
    depth = arguments.get("depth")
    if depth is not None:
        args.extend(["--depth", str(depth)])
    selector = _optional_text(arguments, "selector")
    if selector:
        args.extend(["--selector", selector])
    return args


def _evaluate_args(arguments: dict[str, Any]) -> list[str]:
    args = ["eval", _required_text(arguments, "script"), "--json"]
    tab = _optional_text(arguments, "tab")
    domain = _optional_text(arguments, "domain")
    raw_args = _optional_text(arguments, "args")
    if tab:
        args.extend(["--tab", tab])
    if domain:
        args.extend(["--domain", domain])
    if raw_args:
        args.extend(["--args", raw_args])
    return args


def _site_run_args(arguments: dict[str, Any]) -> list[str]:
    args = ["site", _required_text(arguments, "name")]
    raw_args = arguments.get("args", [])
    if raw_args is None:
        raw_args = []
    if not isinstance(raw_args, list) or not all(isinstance(item, str) for item in raw_args):
        raise BBBrowserMCPServerError("args 必须是字符串数组。")
    args.extend(raw_args)
    tab = _optional_text(arguments, "tab")
    if tab:
        args.extend(["--tab", tab])
    args.append("--json")
    return args


def _required_text(arguments: dict[str, Any], key: str) -> str:
    value = arguments.get(key)
    if not isinstance(value, str) or not value.strip():
        raise BBBrowserMCPServerError(f"缺少必填参数：{key}")
    return value.strip()


def _optional_text(arguments: dict[str, Any], key: str) -> str:
    value = arguments.get(key)
    if not isinstance(value, str):
        return ""
    return value.strip()


def _with_optional_flag(args: list[str], flag: str, value: str) -> list[str]:
    if value:
        return [*args, flag, value]
    return args


def _compact_args(args: list[str]) -> list[str]:
    return [arg for arg in args if arg]


def _read_timeout() -> int:
    raw_value = os.getenv("BB_BROWSER_TIMEOUT_SECONDS", "").strip()
    if not raw_value:
        return DEFAULT_TIMEOUT_SECONDS
    try:
        value = int(raw_value)
    except ValueError:
        return DEFAULT_TIMEOUT_SECONDS
    return max(1, min(300, value))


def _read_params(message: dict[str, Any]) -> dict[str, Any]:
    params = message.get("params", {})
    if params is None:
        return {}
    if not isinstance(params, dict):
        raise BBBrowserMCPServerError("JSON-RPC params 必须是对象。")
    return params


def _read_message(stream: Any) -> dict[str, Any] | None:
    header = bytearray()
    while not header.endswith(b"\r\n\r\n") and not header.endswith(b"\n\n"):
        chunk = stream.read(1)
        if not chunk:
            return None
        header.extend(chunk)
        if len(header) > 8192:
            raise BBBrowserMCPServerError("MCP 请求头超过 8192 字节。")
    length = _parse_content_length(bytes(header))
    body = stream.read(length)
    if len(body) != length:
        raise BBBrowserMCPServerError("MCP 请求体长度不完整。")
    payload = json.loads(body.decode("utf-8"))
    if not isinstance(payload, dict):
        raise BBBrowserMCPServerError("MCP 请求必须是 JSON 对象。")
    return payload


def _write_message(stream: Any, message: dict[str, Any]) -> None:
    body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    stream.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body)
    stream.flush()


def _jsonrpc_error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def _parse_content_length(header: bytes) -> int:
    text = header.decode("ascii", errors="replace")
    for line in text.splitlines():
        key, separator, value = line.partition(":")
        if separator and key.strip().lower() == "content-length":
            length = int(value.strip())
            if length < 0 or length > 50_000_000:
                raise BBBrowserMCPServerError("MCP Content-Length 超出允许范围。")
            return length
    raise BBBrowserMCPServerError("MCP 请求缺少 Content-Length。")


def main() -> None:
    raw_workspace = os.getenv("MCP_WORKSPACE_ROOT", "").strip()
    workspace = Path(raw_workspace).expanduser() if raw_workspace else Path.cwd()
    BBBrowserMCPServer(workspace).run_stdio()


if __name__ == "__main__":
    main()
