from __future__ import annotations

import ast
import os
import re
import shutil
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


@dataclass(frozen=True)
class WorkspaceCommandInvocation:
    """已解析的命令执行方式。

    命令必须通过明确的 Bash 或 PowerShell 可执行文件启动，避免把一种 Shell 的
    语法误交给当前终端默认 Shell，也不再回退到 Windows CMD。
    """

    args: list[str]
    label: str


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
        function_name = _read_optional_text(arguments, "function_name")
        text_snippet = _read_optional_text(arguments, "text")
        if function_name and text_snippet:
            raise WorkspaceToolError("function_name 和 text 不能同时指定。")
        max_lines = _read_limited_int(arguments, "max_lines", default=200, minimum=1, maximum=500)
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")

        text = self.read_text(path)
        lines = text.splitlines()
        if function_name:
            function_range = _find_function_range(path, text, function_name)
            if function_range is None:
                raise WorkspaceToolError(
                    f"未找到函数或方法：{function_name}（文件：{self.relative_path(path)}）。"
                )
            start_line, end_line, resolved_name = function_range
            return self._format_read_lines(
                lines,
                start_line=start_line,
                end_line=end_line,
                max_lines=max_lines,
                header=f"定位：函数 {resolved_name}（第 {start_line}-{end_line} 行）",
                truncation_hint="函数内容超过 max_lines，可提高 max_lines 继续读取。",
            )

        if text_snippet:
            occurrence_offset = text.find(text_snippet)
            if occurrence_offset < 0:
                raise WorkspaceToolError(
                    f"未找到指定文字片段（文件：{self.relative_path(path)}）。"
                )
            context_lines = _read_limited_int(
                arguments,
                "context_lines",
                default=20,
                minimum=0,
                maximum=200,
            )
            anchor_start_line = text.count("\n", 0, occurrence_offset) + 1
            anchor_last_offset = occurrence_offset + len(text_snippet) - 1
            anchor_end_line = text.count("\n", 0, anchor_last_offset) + 1
            start_line = max(1, anchor_start_line - context_lines)
            end_line = min(len(lines), anchor_end_line + context_lines)
            return self._format_read_lines(
                lines,
                start_line=start_line,
                end_line=end_line,
                max_lines=max_lines,
                header=(
                    f"定位：文字片段首次匹配（第 {anchor_start_line}-{anchor_end_line} 行，"
                    f"上下文 {context_lines} 行）"
                ),
                truncation_hint="文字片段上下文超过 max_lines，可提高 max_lines 继续读取。",
            )

        start_line = _read_limited_int(arguments, "start_line", default=1, minimum=1, maximum=100_000)
        return self._format_read_lines(
            lines,
            start_line=start_line,
            end_line=len(lines),
            max_lines=max_lines,
            truncation_hint="已截断，可提高 start_line 继续读取。",
        )

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

    def run_shell_command(
        self,
        arguments: dict[str, Any],
        *,
        shell: str,
    ) -> WorkspaceCommandResult:
        """在指定的显式 Shell 中运行命令，不提供默认解释器回退。"""

        unsupported_keys = set(arguments) - {"command", "timeout_seconds"}
        if unsupported_keys:
            names = "、".join(sorted(unsupported_keys))
            raise WorkspaceToolError(f"显式命令工具不支持参数：{names}。")
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
        invocation = self.command_invocation(command, shell=shell)
        popen_kwargs: dict[str, Any] = {
            "cwd": str(self.workspace_root),
            "shell": False,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
            "text": True,
            "encoding": "utf-8",
            "errors": "replace",
        }
        if os.name == "nt":
            popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            popen_kwargs["start_new_session"] = True

        try:
            process = subprocess.Popen(invocation.args, **popen_kwargs)
        except OSError as exc:
            raise WorkspaceToolError(f"命令执行失败：{exc}") from exc

        # 复用 Monitor 已验证的进程树回收逻辑；局部导入避免模块初始化时循环依赖。
        from .monitor import (
            BackgroundMonitorManager,
            _assign_process_to_kill_on_close_job,
            _close_windows_handle,
        )

        job_handle = _assign_process_to_kill_on_close_job(process)
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            BackgroundMonitorManager._terminate_process_tree(process, job_handle=job_handle)
            job_handle = None
            raise WorkspaceToolError(f"命令执行超过 {timeout} 秒，已终止。")
        finally:
            _close_windows_handle(job_handle)

        output_parts = [f"退出码：{process.returncode}", f"Shell：{invocation.label}"]
        if stdout.strip():
            output_parts.append(f"stdout:\n{stdout.strip()}")
        if stderr.strip():
            output_parts.append(f"stderr:\n{stderr.strip()}")
        return WorkspaceCommandResult(
            ok=process.returncode == 0,
            output="\n\n".join(output_parts),
        )

    def command_invocation(self, command: str, *, shell: str) -> WorkspaceCommandInvocation:
        """把工具要求的 Shell 转换为可执行的 subprocess 调用。

        Windows 的 `bash.exe` 可能只是没有 Linux 发行版时会失败的 WSL 启动器，
        所以 Bash 优先使用 Git Bash；如果只发现 WSL 启动器，则明确报错而不是
        让用户面对难以理解的子进程输出。PowerShell 优先使用 PowerShell 7，
        没有时回退 Windows PowerShell。
        """

        normalized_shell = shell.strip().lower() if isinstance(shell, str) else ""
        if normalized_shell == "bash":
            executable = _find_bash_executable()
            if executable is None:
                raise WorkspaceToolError(
                    "未找到可用的 Git Bash。请安装 Git for Windows，或设置 PATH 后重试。"
                )
            return WorkspaceCommandInvocation(
                args=[str(executable), "-lc", command],
                label="Bash",
            )
        if normalized_shell == "powershell":
            executable = _find_powershell_executable()
            if executable is None:
                raise WorkspaceToolError("未找到 PowerShell 可执行文件。")
            return WorkspaceCommandInvocation(
                args=[str(executable), "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
                label="PowerShell",
            )
        raise WorkspaceToolError("shell 仅支持 bash 或 powershell。")

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

    @staticmethod
    def _format_read_lines(
        lines: list[str],
        *,
        start_line: int,
        end_line: int,
        max_lines: int,
        header: str = "",
        truncation_hint: str,
    ) -> str:
        total_lines = len(lines)
        selected_start = min(max(1, start_line), total_lines + 1)
        selected_end = min(max(selected_start - 1, end_line), total_lines)
        selected = lines[selected_start - 1 : min(selected_end, selected_start - 1 + max_lines)]
        numbered = [
            f"{line_no}: {line}"
            for line_no, line in enumerate(selected, start=selected_start)
        ]
        if selected_start <= selected_end and selected_start - 1 + max_lines < selected_end:
            numbered.append(f"... {truncation_hint}")
        if not numbered:
            numbered.append("文件为空，或指定范围没有内容。")
        return "\n".join(([header] if header else []) + numbered)


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


def _read_optional_text(arguments: dict[str, Any], key: str) -> str:
    value = arguments.get(key)
    return value.strip() if isinstance(value, str) else ""


def _find_function_range(
    path: Path,
    text: str,
    target_name: str,
) -> tuple[int, int, str] | None:
    """定位函数范围：Python 优先 AST，其余语言使用声明和大括号范围回退。"""

    if path.suffix.lower() == ".py":
        ast_range = _find_python_function_range(text, target_name)
        if ast_range is not None:
            return ast_range
    return _find_braced_function_range(text.splitlines(), target_name)


def _find_python_function_range(text: str, target_name: str) -> tuple[int, int, str] | None:
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return None

    matches: list[tuple[int, int, str]] = []

    class FunctionVisitor(ast.NodeVisitor):
        def __init__(self) -> None:
            self.scopes: list[str] = []

        def visit_ClassDef(self, node: ast.ClassDef) -> None:
            self.scopes.append(node.name)
            self.generic_visit(node)
            self.scopes.pop()

        def visit_FunctionDef(self, node: ast.FunctionDef) -> None:
            self._record(node)
            self.scopes.append(node.name)
            self.generic_visit(node)
            self.scopes.pop()

        def visit_AsyncFunctionDef(self, node: ast.AsyncFunctionDef) -> None:
            self._record(node)
            self.scopes.append(node.name)
            self.generic_visit(node)
            self.scopes.pop()

        def _record(self, node: ast.FunctionDef | ast.AsyncFunctionDef) -> None:
            qualified_name = ".".join([*self.scopes, node.name])
            if target_name not in {node.name, qualified_name}:
                return
            start_line = min(
                [node.lineno, *[decorator.lineno for decorator in node.decorator_list]],
            )
            end_line = getattr(node, "end_lineno", node.lineno)
            matches.append((start_line, end_line, qualified_name))

    FunctionVisitor().visit(tree)
    if not matches:
        return None
    exact_matches = [match for match in matches if match[2] == target_name]
    candidates = exact_matches or matches
    if len(candidates) == 1:
        return candidates[0]
    names = ", ".join(match[2] for match in candidates)
    raise WorkspaceToolError(f"函数名 {target_name} 存在多个匹配，请使用限定名：{names}。")


def _find_braced_function_range(lines: list[str], target_name: str) -> tuple[int, int, str] | None:
    leaf_name = target_name.rsplit(".", 1)[-1]
    escaped_name = re.escape(leaf_name)
    declaration_patterns = (
        re.compile(
            rf"^\s*(?:(?:export|default|public|private|protected|static|async|final|virtual|"
            rf"override|inline|extern|unsafe|pub)\s+)*(?:function|func|fn|def)\s+{escaped_name}\s*\(",
        ),
        re.compile(
            rf"^\s*(?:(?:export|default|public|private|protected|static|async|final|virtual|"
            rf"override|inline|extern|unsafe|pub)\s+)*(?:[A-Za-z_][\w<>,.?\[\]*&\s:]*)\s+{escaped_name}\s*\(",
        ),
        re.compile(
            rf"^\s*(?:const|let|var)\s+{escaped_name}\s*=\s*(?:async\s*)?(?:\([^)]*\)|[^=]*)=>",
        ),
        re.compile(rf"^\s*{escaped_name}\s*\([^;]*\)\s*(?:=>|\{{)"),
    )
    matches: list[tuple[int, int, str]] = []
    for index, line in enumerate(lines):
        if not any(pattern.search(line) for pattern in declaration_patterns):
            continue
        end_index = _find_braced_block_end(lines, index)
        if end_index is not None:
            matches.append((index + 1, end_index + 1, target_name if "." in target_name else leaf_name))

    if not matches:
        return None
    if len(matches) == 1:
        return matches[0]
    raise WorkspaceToolError(f"函数名 {target_name} 存在多个文本匹配，请改用更具体的文件或函数名。")


def _find_braced_block_end(lines: list[str], start_index: int) -> int | None:
    depth = 0
    started = False
    quote = ""
    escaped = False
    for index in range(start_index, min(len(lines), start_index + 500)):
        line = lines[index]
        position = 0
        while position < len(line):
            character = line[position]
            next_character = line[position + 1] if position + 1 < len(line) else ""
            if quote:
                if escaped:
                    escaped = False
                elif character == "\\":
                    escaped = True
                elif character == quote:
                    quote = ""
                position += 1
                continue
            if character in {"'", '"', "`"}:
                quote = character
                position += 1
                continue
            if character == "/" and next_character == "/":
                break
            if character == "{" :
                depth += 1
                started = True
            elif character == "}" and started:
                depth -= 1
                if depth == 0:
                    return index
            position += 1
    return None


def _find_bash_executable() -> Path | None:
    candidates: list[Path] = []
    git_executable = shutil.which("git")
    if git_executable:
        git_root = Path(git_executable).resolve().parent.parent
        candidates.extend([git_root / "bin" / "bash.exe", git_root / "usr" / "bin" / "bash.exe"])
    if os.name == "nt":
        for environment_key in ("ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"):
            root = os.getenv(environment_key)
            if root:
                candidates.extend(
                    [Path(root) / "Git" / "bin" / "bash.exe", Path(root) / "Git" / "usr" / "bin" / "bash.exe"]
                )
    else:
        bash = shutil.which("bash")
        if bash:
            candidates.append(Path(bash))

    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def _find_powershell_executable() -> Path | None:
    for name in ("pwsh", "powershell"):
        executable = shutil.which(name)
        if executable:
            return Path(executable)
    return None


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False
