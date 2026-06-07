from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable


MAX_FILE_READ_CHARS = 200_000
MAX_SEARCH_RESULTS = 200
MAX_LIST_ENTRIES = 500
DEFAULT_COMMAND_TIMEOUT_SECONDS = 120

PROTECTED_NAMES = {
    ".git",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".codex-ref",
    ".env",
    "config.json",
}


class LocalMCPServerError(RuntimeError):
    """Local MCP Server 参数校验或工具执行失败。"""


@dataclass(frozen=True)
class _ToolSpec:
    name: str
    description: str
    input_schema: dict[str, Any]
    handler: Callable[[dict[str, Any]], str]


class LocalMCPServer:
    """把当前项目的安全文件工具暴露为本地 stdio MCP Server。"""

    def __init__(self, workspace_root: Path | None = None) -> None:
        self.workspace_root = (workspace_root or Path.cwd()).resolve()
        self._tools = self._build_tools()
        self._prompts = self._build_prompts()

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
                    "serverInfo": {"name": "ai-voice-agent-local", "version": "0.1"},
                }
            elif method == "notifications/initialized":
                return None
            elif method == "tools/list":
                result = self._list_tools()
            elif method == "tools/call":
                result = self._call_tool(_read_params(message))
            elif method == "resources/list":
                result = self._list_resources()
            elif method == "resources/read":
                result = self._read_resource(_read_params(message))
            elif method == "prompts/list":
                result = self._list_prompts()
            elif method == "prompts/get":
                result = self._get_prompt(_read_params(message))
            else:
                return _jsonrpc_error(request_id, -32601, f"未知 MCP 方法：{method}")
        except LocalMCPServerError as exc:
            return _jsonrpc_error(request_id, -32000, str(exc))
        except Exception as exc:
            return _jsonrpc_error(request_id, -32603, f"Local MCP Server 内部错误：{exc}")

        if request_id is None:
            return None
        return {"jsonrpc": "2.0", "id": request_id, "result": result}

    def _build_tools(self) -> dict[str, _ToolSpec]:
        return {
            "workspace.list_files": _ToolSpec(
                name="workspace.list_files",
                description="列出工作区内文件和目录，自动跳过受保护路径。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "default": "."},
                        "recursive": {"type": "boolean", "default": False},
                    },
                },
                handler=self._tool_list_files,
            ),
            "workspace.read_file": _ToolSpec(
                name="workspace.read_file",
                description="读取工作区内 UTF-8 文本文件，禁止读取受保护路径。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "start_line": {"type": "integer", "default": 1},
                        "max_lines": {"type": "integer", "default": 200},
                    },
                    "required": ["path"],
                },
                handler=self._tool_read_file,
            ),
            "workspace.search_text": _ToolSpec(
                name="workspace.search_text",
                description="在工作区文本文件中搜索正则或普通文本。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "default": "."},
                        "case_sensitive": {"type": "boolean", "default": False},
                        "max_results": {"type": "integer", "default": 50},
                    },
                    "required": ["pattern"],
                },
                handler=self._tool_search_text,
            ),
            "workspace.replace_text": _ToolSpec(
                name="workspace.replace_text",
                description="在工作区单个 UTF-8 文本文件中替换指定文本。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_text": {"type": "string", "minLength": 1},
                        "new_text": {"type": "string"},
                        "count": {"type": "integer", "default": 1, "minimum": 0},
                    },
                    "required": ["path", "old_text", "new_text"],
                },
                handler=self._tool_replace_text,
            ),
            "workspace.write_file": _ToolSpec(
                name="workspace.write_file",
                description="写入或追加工作区内 UTF-8 文本文件。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"},
                        "mode": {"type": "string", "default": "overwrite"},
                    },
                    "required": ["path", "content"],
                },
                handler=self._tool_write_file,
            ),
            "workspace.run_command": _ToolSpec(
                name="workspace.run_command",
                description="在工作区执行本地命令，设置超时并截断输出。",
                input_schema={
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_seconds": {"type": "integer", "default": DEFAULT_COMMAND_TIMEOUT_SECONDS},
                    },
                    "required": ["command"],
                },
                handler=self._tool_run_command,
            ),
        }

    def _build_prompts(self) -> dict[str, dict[str, Any]]:
        return {
            "project_doc_writer": {
                "name": "project_doc_writer",
                "description": "编写项目技术文档的任务模板。",
                "arguments": [
                    {"name": "target", "description": "文档目标", "required": True},
                    {"name": "scope", "description": "覆盖范围", "required": False},
                ],
            },
            "code_review": {
                "name": "code_review",
                "description": "代码审查任务模板。",
                "arguments": [
                    {"name": "path", "description": "审查文件路径", "required": True},
                    {"name": "focus", "description": "关注点", "required": False},
                ],
            },
            "debug_triage": {
                "name": "debug_triage",
                "description": "排障分析任务模板。",
                "arguments": [
                    {"name": "error", "description": "错误日志或现象", "required": True},
                    {"name": "expected", "description": "期望行为", "required": False},
                ],
            },
            "safe_change_plan": {
                "name": "safe_change_plan",
                "description": "高风险改动前的安全方案模板。",
                "arguments": [
                    {"name": "goal", "description": "变更目标", "required": True},
                    {"name": "constraints", "description": "约束和回滚要求", "required": False},
                ],
            },
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
            raise LocalMCPServerError("tools/call.name 必须是非空字符串。")
        if not isinstance(arguments, dict):
            raise LocalMCPServerError("tools/call.arguments 必须是 JSON 对象。")
        spec = self._tools.get(name)
        if spec is None:
            raise LocalMCPServerError(f"未知工具：{name}")
        try:
            output = spec.handler(arguments)
            return {"content": [{"type": "text", "text": output}], "isError": False}
        except LocalMCPServerError as exc:
            return {"content": [{"type": "text", "text": str(exc)}], "isError": True}

    def _list_resources(self) -> dict[str, Any]:
        resources = [
            {
                "uri": "project://agents-instructions",
                "name": "AGENTS.md",
                "description": "项目协作规范。",
                "mimeType": "text/markdown",
            },
            {
                "uri": "server://local_project/health",
                "name": "Local MCP Server Health",
                "description": "本地 MCP Server 健康状态。",
                "mimeType": "text/plain",
            },
        ]
        for relative in ("README.md", "docs/TERMINAL_UI.md", "docs/MCP_DESIGN_TECHNICAL.md"):
            path = self.workspace_root / relative
            if path.is_file() and not self._should_skip_path(path):
                resources.append(
                    {
                        "uri": f"project://{relative}",
                        "name": relative,
                        "description": "项目只读文档。",
                        "mimeType": "text/markdown",
                    }
                )
        return {"resources": resources}

    def _read_resource(self, params: dict[str, Any]) -> dict[str, Any]:
        uri = params.get("uri")
        if not isinstance(uri, str) or not uri.strip():
            raise LocalMCPServerError("resources/read.uri 必须是非空字符串。")
        uri = uri.strip()
        if uri == "server://local_project/health":
            text = f"ok\nworkspace_root={self.workspace_root}\n"
        elif uri == "project://agents-instructions":
            text = self._read_project_text("AGENTS.md")
        elif uri.startswith("project://"):
            text = self._read_project_text(uri[len("project://") :])
        else:
            raise LocalMCPServerError(f"不支持的 Resource URI：{uri}")
        return {"contents": [{"uri": uri, "mimeType": "text/plain", "text": text}]}

    def _list_prompts(self) -> dict[str, Any]:
        return {"prompts": list(self._prompts.values())}

    def _get_prompt(self, params: dict[str, Any]) -> dict[str, Any]:
        name = params.get("name")
        arguments = params.get("arguments", {})
        if not isinstance(name, str) or not name:
            raise LocalMCPServerError("prompts/get.name 必须是非空字符串。")
        if not isinstance(arguments, dict):
            raise LocalMCPServerError("prompts/get.arguments 必须是 JSON 对象。")
        if name not in self._prompts:
            raise LocalMCPServerError(f"未知 Prompt：{name}")

        text = _render_prompt(name, arguments)
        return {
            "description": self._prompts[name]["description"],
            "messages": [
                {
                    "role": "user",
                    "content": {"type": "text", "text": text},
                }
            ],
        }

    def _tool_list_files(self, arguments: dict[str, Any]) -> str:
        path = self._safe_path(str(arguments.get("path") or "."))
        recursive = bool(arguments.get("recursive", False))
        if not path.exists():
            raise LocalMCPServerError(f"路径不存在：{self._relative_path(path)}")
        if path.is_file():
            return self._relative_path(path)

        entries: list[str] = []
        iterator = path.rglob("*") if recursive else path.iterdir()
        for entry in sorted(iterator, key=lambda item: str(item).lower()):
            if self._should_skip_path(entry):
                continue
            suffix = "/" if entry.is_dir() else ""
            entries.append(f"{self._relative_path(entry)}{suffix}")
            if len(entries) >= MAX_LIST_ENTRIES:
                entries.append(f"... 已截断，结果超过 {MAX_LIST_ENTRIES} 项。")
                break
        return "\n".join(entries) or "目录为空。"

    def _tool_read_file(self, arguments: dict[str, Any]) -> str:
        path = self._safe_path(str(arguments.get("path") or ""))
        start_line = _read_limited_int(arguments, "start_line", default=1, minimum=1, maximum=100_000)
        max_lines = _read_limited_int(arguments, "max_lines", default=200, minimum=1, maximum=500)
        if not path.is_file():
            raise LocalMCPServerError(f"不是文件：{self._relative_path(path)}")
        text = self._read_text(path)
        lines = text.splitlines()
        start_index = start_line - 1
        selected = lines[start_index : start_index + max_lines]
        numbered = [f"{line_no}: {line}" for line_no, line in enumerate(selected, start=start_line)]
        if start_index + max_lines < len(lines):
            numbered.append("... 已截断，可提高 start_line 继续读取。")
        return "\n".join(numbered)

    def _tool_search_text(self, arguments: dict[str, Any]) -> str:
        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            raise LocalMCPServerError("pattern 不能为空。")
        root = self._safe_path(str(arguments.get("path") or "."))
        case_sensitive = bool(arguments.get("case_sensitive", False))
        max_results = _read_limited_int(
            arguments,
            "max_results",
            default=50,
            minimum=1,
            maximum=MAX_SEARCH_RESULTS,
        )
        flags = 0 if case_sensitive else re.IGNORECASE
        try:
            regex = re.compile(pattern, flags)
        except re.error:
            regex = re.compile(re.escape(pattern), flags)

        files = [root] if root.is_file() else self._iter_search_files(root)
        results: list[str] = []
        for file_path in files:
            try:
                lines = self._read_text(file_path).splitlines()
            except LocalMCPServerError:
                continue
            for line_no, line in enumerate(lines, start=1):
                if regex.search(line):
                    results.append(f"{self._relative_path(file_path)}:{line_no}: {line}")
                    if len(results) >= max_results:
                        return "\n".join(results) + "\n... 已达到 max_results。"
        return "\n".join(results) or "未找到匹配结果。"

    def _tool_replace_text(self, arguments: dict[str, Any]) -> str:
        path = self._safe_path(str(arguments.get("path") or ""))
        old_text = str(arguments.get("old_text") or "")
        new_text = str(arguments.get("new_text") or "")
        count = _read_limited_int(arguments, "count", default=1, minimum=0, maximum=10_000)
        if not path.is_file():
            raise LocalMCPServerError(f"不是文件：{self._relative_path(path)}")
        if not old_text:
            raise LocalMCPServerError("old_text 不能为空。")

        original = self._read_text(path)
        occurrences = original.count(old_text)
        if occurrences == 0:
            raise LocalMCPServerError("未找到 old_text，文件未修改。")
        replace_count = occurrences if count <= 0 else min(count, occurrences)
        path.write_text(original.replace(old_text, new_text, replace_count), encoding="utf-8")
        return f"已修改 {self._relative_path(path)}，替换 {replace_count} 处。"

    def _tool_write_file(self, arguments: dict[str, Any]) -> str:
        path = self._safe_path(str(arguments.get("path") or ""))
        content = str(arguments.get("content") or "")
        mode = str(arguments.get("mode") or "overwrite").lower()
        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == "append":
            with path.open("a", encoding="utf-8") as file:
                file.write(content)
            action = "追加"
        elif mode in {"overwrite", "write"}:
            path.write_text(content, encoding="utf-8")
            action = "写入"
        else:
            raise LocalMCPServerError("mode 仅支持 overwrite 或 append。")
        return f"已{action} {self._relative_path(path)}，字符数：{len(content)}。"

    def _tool_run_command(self, arguments: dict[str, Any]) -> str:
        command = str(arguments.get("command") or "").strip()
        if not command:
            raise LocalMCPServerError("command 不能为空。")
        timeout = _read_limited_int(
            arguments,
            "timeout_seconds",
            default=DEFAULT_COMMAND_TIMEOUT_SECONDS,
            minimum=1,
            maximum=300,
        )
        try:
            completed = subprocess.run(
                command,
                cwd=str(self.workspace_root),
                shell=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            raise LocalMCPServerError(f"命令执行超过 {timeout} 秒，已终止。")

        output_parts = [f"退出码：{completed.returncode}"]
        if completed.stdout.strip():
            output_parts.append(f"stdout:\n{completed.stdout.strip()}")
        if completed.stderr.strip():
            output_parts.append(f"stderr:\n{completed.stderr.strip()}")
        if completed.returncode != 0:
            raise LocalMCPServerError("\n\n".join(output_parts))
        return "\n\n".join(output_parts)

    def _safe_path(self, raw_path: str) -> Path:
        raw_path = raw_path.strip()
        if not raw_path:
            raise LocalMCPServerError("路径不能为空。")
        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not _is_relative_to(resolved, self.workspace_root):
            raise LocalMCPServerError(f"拒绝访问工作区外路径：{raw_path}")
        if self._should_skip_path(resolved):
            raise LocalMCPServerError(f"拒绝访问受保护路径：{self._relative_path(resolved)}")
        return resolved

    def _read_project_text(self, raw_path: str) -> str:
        path = self._safe_path(raw_path)
        if not path.is_file():
            raise LocalMCPServerError(f"不是文件：{self._relative_path(path)}")
        return self._read_text(path)

    def _read_text(self, path: Path) -> str:
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise LocalMCPServerError(f"文件不是 UTF-8 文本：{self._relative_path(path)}") from exc
        except OSError as exc:
            raise LocalMCPServerError(f"读取文件失败：{self._relative_path(path)}，{exc}") from exc
        if len(text) > MAX_FILE_READ_CHARS:
            return text[:MAX_FILE_READ_CHARS] + "\n... 文件内容已截断。"
        return text

    def _iter_search_files(self, root: Path) -> list[Path]:
        files: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(root):
            current_dir = Path(dirpath)
            dirnames[:] = [
                dirname
                for dirname in sorted(dirnames, key=lambda value: value.lower())
                if not self._should_skip_path(current_dir / dirname)
            ]
            for filename in sorted(filenames, key=lambda value: value.lower()):
                file_path = current_dir / filename
                if not self._should_skip_path(file_path):
                    files.append(file_path)
        return files

    def _should_skip_path(self, path: Path) -> bool:
        return any(part in PROTECTED_NAMES or part.startswith(".env.") for part in path.parts)

    def _relative_path(self, path: Path) -> str:
        try:
            return str(path.resolve().relative_to(self.workspace_root))
        except ValueError:
            return str(path)


def main() -> None:
    raw_workspace = os.getenv("MCP_WORKSPACE_ROOT", "").strip()
    workspace = Path(raw_workspace).expanduser() if raw_workspace else Path.cwd()
    LocalMCPServer(workspace).run_stdio()


def _render_prompt(name: str, arguments: dict[str, Any]) -> str:
    if name == "project_doc_writer":
        return (
            "请编写项目技术文档。\n"
            f"目标：{arguments.get('target', '')}\n"
            f"范围：{arguments.get('scope', '')}\n"
            "要求：先说明结论，再覆盖关键模块、数据流、边界条件和验证方式。"
        )
    if name == "code_review":
        return (
            "请进行代码审查，优先指出 bug、回归风险和缺失测试。\n"
            f"文件：{arguments.get('path', '')}\n"
            f"关注点：{arguments.get('focus', '')}"
        )
    if name == "debug_triage":
        return (
            "请进行排障分析。\n"
            f"错误现象：{arguments.get('error', '')}\n"
            f"期望行为：{arguments.get('expected', '')}\n"
            "要求：给出最可能原因、验证步骤和最小修复路径。"
        )
    if name == "safe_change_plan":
        return (
            "请为高风险改动制定安全方案。\n"
            f"目标：{arguments.get('goal', '')}\n"
            f"约束：{arguments.get('constraints', '')}\n"
            "要求：覆盖影响面、执行步骤、验证方式和回滚思路。"
        )
    raise LocalMCPServerError(f"未知 Prompt：{name}")


def _read_params(message: dict[str, Any]) -> dict[str, Any]:
    params = message.get("params", {})
    if params is None:
        return {}
    if not isinstance(params, dict):
        raise LocalMCPServerError("JSON-RPC params 必须是对象。")
    return params


def _read_message(stream: Any) -> dict[str, Any] | None:
    header = bytearray()
    while not header.endswith(b"\r\n\r\n") and not header.endswith(b"\n\n"):
        chunk = stream.read(1)
        if not chunk:
            return None
        header.extend(chunk)
        if len(header) > 8192:
            raise LocalMCPServerError("MCP 请求头超过 8192 字节。")
    length = _parse_content_length(bytes(header))
    body = stream.read(length)
    if len(body) != length:
        raise LocalMCPServerError("MCP 请求体长度不完整。")
    payload = json.loads(body.decode("utf-8"))
    if not isinstance(payload, dict):
        raise LocalMCPServerError("MCP 请求必须是 JSON 对象。")
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
                raise LocalMCPServerError("MCP Content-Length 超出允许范围。")
            return length
    raise LocalMCPServerError("MCP 请求缺少 Content-Length。")


def _read_limited_int(
    arguments: dict[str, Any],
    key: str,
    *,
    default: int,
    minimum: int,
    maximum: int,
) -> int:
    value = arguments.get(key, default)
    if isinstance(value, bool):
        return default
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(maximum, parsed))


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


if __name__ == "__main__":
    main()
