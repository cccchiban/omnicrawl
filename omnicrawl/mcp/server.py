from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any

from ..common.documentation import (
    BUNDLED_DOC_URI_PREFIX,
    BundledDocumentationError,
    bundled_doc_names,
    bundled_doc_uri,
    read_bundled_doc,
)
from ..workspace.tools import (
    MAX_FILE_READ_CHARS,
    WorkspaceToolError,
    WorkspaceTools,
)


class LocalMCPServerError(RuntimeError):
    """Local MCP Server 参数校验或工具执行失败。"""


class LocalMCPServer:
    """提供本地 stdio MCP Server 的 Resource 和 Prompt 能力。"""

    def __init__(self, workspace_root: Path | None = None) -> None:
        self.workspace_root = (workspace_root or Path.cwd()).resolve()
        self._workspace_tools = WorkspaceTools(
            self.workspace_root,
            max_file_read_chars=MAX_FILE_READ_CHARS,
        )
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

    def _build_tools(self) -> dict[str, Any]:
        """Local MCP Server 不再暴露工作区工具。"""

        return {}

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
        document_paths = ["README.md"]
        docs_dir = self.workspace_root / "docs"
        if docs_dir.is_dir() and not self._should_skip_path(docs_dir):
            document_paths.extend(
                f"docs/{path.name}"
                for path in sorted(docs_dir.glob("*.md"), key=lambda value: value.name.lower())
                if path.is_file() and not self._should_skip_path(path)
            )

        for relative in dict.fromkeys(document_paths):
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

        for name in bundled_doc_names():
            resources.append(
                {
                    "uri": bundled_doc_uri(name),
                    "name": f"omnicrawl/docs/{name}",
                    "description": "随 OmniCrawl 安装包提供的只读技术文档。",
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
        elif uri.startswith(BUNDLED_DOC_URI_PREFIX):
            try:
                text = read_bundled_doc(uri)
            except BundledDocumentationError as exc:
                raise LocalMCPServerError(str(exc)) from exc
        else:
            raise LocalMCPServerError(f"不支持的 Resource URI：{uri}")
        return {"contents": [{"uri": uri, "mimeType": "text/markdown", "text": text}]}

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

    def _read_project_text(self, raw_path: str) -> str:
        try:
            return self._workspace_tools.read_project_text(raw_path)
        except WorkspaceToolError as exc:
            raise LocalMCPServerError(str(exc)) from exc

    def _should_skip_path(self, path: Path) -> bool:
        return self._workspace_tools.should_skip_path(path)


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


if __name__ == "__main__":
    main()
