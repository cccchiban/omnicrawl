from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable


MAX_FILE_READ_CHARS = 200_000
MAX_SEARCH_RESULTS = 200
MAX_LIST_ENTRIES = 500
DEFAULT_COMMAND_TIMEOUT_SECONDS = 360
MAX_COMMAND_TIMEOUT_SECONDS = 360

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


class WorkspaceToolError(RuntimeError):
    """工作区工具参数校验或执行失败。"""


@dataclass(frozen=True)
class WorkspaceCommandResult:
    ok: bool
    output: str


class WorkspaceTools:
    """工作区文件、搜索和命令工具的共享实现。

    Agent 内置工具和本地 MCP Server 都需要同一组受保护路径、UTF-8 文本处理、
    搜索剪枝和命令输出规则。这里集中实现真实业务逻辑，两端只负责把结果适配
    成各自协议的返回形态，避免安全边界和细节行为逐渐分叉。
    """

    def __init__(
        self,
        workspace_root: Path,
        *,
        command_timeout_seconds: int = DEFAULT_COMMAND_TIMEOUT_SECONDS,
        max_file_read_chars: int | None = None,
        extra_protection_message: Callable[[Path], str | None] | None = None,
    ) -> None:
        self.workspace_root = workspace_root.resolve()
        self.command_timeout_seconds = max(
            1,
            min(command_timeout_seconds, MAX_COMMAND_TIMEOUT_SECONDS),
        )
        self.max_file_read_chars = max_file_read_chars
        self._extra_protection_message = extra_protection_message

    def list_files(self, arguments: dict[str, Any]) -> str:
        path = self.safe_path(str(arguments.get("path") or "."))
        recursive = bool(arguments.get("recursive", False))
        if not path.exists():
            raise WorkspaceToolError(f"路径不存在：{self.relative_path(path)}")
        if path.is_file():
            return self.relative_path(path)

        entries: list[str] = []
        iterator = path.rglob("*") if recursive else path.iterdir()
        for entry in sorted(iterator, key=lambda item: str(item).lower()):
            if self.should_skip_path(entry):
                continue
            suffix = "/" if entry.is_dir() else ""
            entries.append(f"{self.relative_path(entry)}{suffix}")
            if len(entries) >= MAX_LIST_ENTRIES:
                entries.append(f"... 已截断，结果超过 {MAX_LIST_ENTRIES} 项。")
                break
        return "\n".join(entries) or "目录为空。"

    def read_file(self, arguments: dict[str, Any]) -> str:
        path = self.safe_path(str(arguments.get("path") or ""))
        start_line = _read_limited_int(arguments, "start_line", default=1, minimum=1, maximum=100_000)
        max_lines = _read_limited_int(arguments, "max_lines", default=200, minimum=1, maximum=500)
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")

        text = self.read_text(path)
        lines = text.splitlines()
        start_index = start_line - 1
        selected = lines[start_index : start_index + max_lines]
        numbered = [f"{line_no}: {line}" for line_no, line in enumerate(selected, start=start_line)]
        if start_index + max_lines < len(lines):
            numbered.append("... 已截断，可提高 start_line 继续读取。")
        return "\n".join(numbered)

    def search_text(self, arguments: dict[str, Any]) -> str:
        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            raise WorkspaceToolError("pattern 不能为空。")

        root = self.safe_path(str(arguments.get("path") or "."))
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

        files = [root] if root.is_file() else self.iter_search_files(root)
        results: list[str] = []
        for file_path in files:
            if self.should_skip_path(file_path):
                continue
            try:
                lines = self.read_text(file_path).splitlines()
            except WorkspaceToolError:
                continue
            for line_no, line in enumerate(lines, start=1):
                if regex.search(line):
                    results.append(f"{self.relative_path(file_path)}:{line_no}: {line}")
                    if len(results) >= max_results:
                        return "\n".join(results) + "\n... 已达到 max_results。"
        return "\n".join(results) or "未找到匹配结果。"

    def replace_text(self, arguments: dict[str, Any]) -> str:
        path = self.safe_path(str(arguments.get("path") or ""))
        old_text = str(arguments.get("old_text") or "")
        new_text = str(arguments.get("new_text") or "")
        count = _read_limited_int(arguments, "count", default=1, minimum=0, maximum=10_000)
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")
        if not old_text:
            raise WorkspaceToolError("old_text 不能为空。")

        original = self.read_text(path)
        occurrences = original.count(old_text)
        if occurrences == 0:
            raise WorkspaceToolError("未找到 old_text，文件未修改。")

        replace_count = occurrences if count <= 0 else min(count, occurrences)
        path.write_text(original.replace(old_text, new_text, replace_count), encoding="utf-8")
        return f"已修改 {self.relative_path(path)}，替换 {replace_count} 处。"

    def write_file(self, arguments: dict[str, Any]) -> str:
        path = self.safe_path(str(arguments.get("path") or ""))
        content = str(arguments.get("content") or "")
        mode = str(arguments.get("mode") or "overwrite").lower()
        if mode not in {"append", "overwrite", "write"}:
            raise WorkspaceToolError("mode 仅支持 overwrite 或 append。")

        path.parent.mkdir(parents=True, exist_ok=True)
        if mode == "append":
            with path.open("a", encoding="utf-8") as file:
                file.write(content)
            action = "追加"
        else:
            path.write_text(content, encoding="utf-8")
            action = "写入"
        return f"已{action} {self.relative_path(path)}，字符数：{len(content)}。"

    def run_command(self, arguments: dict[str, Any]) -> WorkspaceCommandResult:
        command = str(arguments.get("command") or "").strip()
        if not command:
            raise WorkspaceToolError("command 不能为空。")

        timeout = _read_limited_int(
            arguments,
            "timeout_seconds",
            default=self.command_timeout_seconds,
            minimum=1,
            maximum=MAX_COMMAND_TIMEOUT_SECONDS,
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
            raise WorkspaceToolError(f"命令执行超过 {timeout} 秒，已终止。")
        except OSError as exc:
            raise WorkspaceToolError(f"命令执行失败：{exc}") from exc

        output_parts = [f"退出码：{completed.returncode}"]
        if completed.stdout.strip():
            output_parts.append(f"stdout:\n{completed.stdout.strip()}")
        if completed.stderr.strip():
            output_parts.append(f"stderr:\n{completed.stderr.strip()}")
        return WorkspaceCommandResult(
            ok=completed.returncode == 0,
            output="\n\n".join(output_parts),
        )

    def read_project_text(self, raw_path: str) -> str:
        path = self.safe_path(raw_path)
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")
        return self.read_text(path)

    def safe_path(self, raw_path: str) -> Path:
        raw_path = raw_path.strip()
        if not raw_path:
            raise WorkspaceToolError("路径不能为空。")

        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if not _is_relative_to(resolved, self.workspace_root):
            raise WorkspaceToolError(f"拒绝访问工作区外路径：{raw_path}")
        if self.is_protected_path(resolved):
            raise WorkspaceToolError(f"拒绝访问受保护路径：{self.relative_path(resolved)}")
        extra_message = self._extra_protection_message(resolved) if self._extra_protection_message else None
        if extra_message:
            raise WorkspaceToolError(extra_message)
        return resolved

    def read_text(self, path: Path) -> str:
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise WorkspaceToolError(f"文件不是 UTF-8 文本或包含二进制内容：{self.relative_path(path)}") from exc
        except OSError as exc:
            raise WorkspaceToolError(f"读取文件失败：{self.relative_path(path)}，{exc}") from exc
        if self.max_file_read_chars is not None and len(text) > self.max_file_read_chars:
            return text[: self.max_file_read_chars] + "\n... 文件内容已截断。"
        return text

    def iter_search_files(self, root: Path) -> list[Path]:
        files: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(root):
            current_dir = Path(dirpath)
            dirnames[:] = [
                dirname
                for dirname in sorted(dirnames, key=lambda value: value.lower())
                if not self.should_skip_path(current_dir / dirname)
            ]
            for filename in sorted(filenames, key=lambda value: value.lower()):
                file_path = current_dir / filename
                if not self.should_skip_path(file_path):
                    files.append(file_path)
        return files

    def should_skip_path(self, path: Path) -> bool:
        if self.is_protected_path(path):
            return True
        if self._extra_protection_message is None:
            return False
        return bool(self._extra_protection_message(path))

    @staticmethod
    def is_protected_path(path: Path) -> bool:
        return any(part in PROTECTED_NAMES or part.startswith(".env.") for part in path.parts)

    def relative_path(self, path: Path) -> str:
        try:
            return str(path.resolve().relative_to(self.workspace_root))
        except ValueError:
            return str(path)


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
