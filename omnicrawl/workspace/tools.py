from __future__ import annotations

import ast
import fnmatch
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import threading
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from ..common.documentation import (
    BUNDLED_DOC_URI_PREFIX,
    BundledDocumentationError,
    resolve_bundled_doc_uri,
)
from ..llm.stream_registry import current_stream_scope, registered_resource
from ..state.session_locking import ProcessFileLock, atomic_write_text
from ..state.session_models import SessionStoreError
from .search_backend import (
    SearchBackendError,
    SearchRegexError,
    batch_paths,
    native_backend_available,
    run_search,
)
from .temp import DEFAULT_AGENT_TEMP_DIRECTORY


MAX_FILE_READ_CHARS = 200_000
# Edit_file 的目标级锁默认等待时间；锁仅保护同一文件的“读取、匹配、原子写入”事务。
EDIT_FILE_LOCK_TIMEOUT_SECONDS = 30.0
EDIT_FILE_LOCK_POLL_SECONDS = 0.05
# 与项目其他原子写路径一致，底层 atomic_write_text 会在 Windows 目标文件
# 短暂占用时进行有限退避重试。
EDIT_FILE_LOCK_FILE_SUFFIX = ".omnicrawl.edit.lock"
MAX_SEARCH_RESULTS = 200

_EDIT_FILE_LOCKS: dict[Path, ProcessFileLock] = {}
_EDIT_FILE_LOCKS_GUARD = threading.Lock()
MAX_LIST_ENTRIES = 500
DEFAULT_COMMAND_TIMEOUT_SECONDS = 360
MAX_COMMAND_TIMEOUT_SECONDS = 360

# read 工具的行窗口上限：单次调用最多返回的行数、单行最多保留的字符数。
# 超长行截断而不是吞掉，保证大文件/超长行不放大模型上下文，同时给出
# 续读提示（footer）让模型用 start_line 继续向后读。
READ_MAX_LINES = 500
READ_MAX_LINE_LENGTH = 2_000
EDIT_CONTEXT_LINES = 2

# grep 匹配行预览上限：单条匹配行最多保留的字符数，超长截断并标记
# （对齐 deepseek-harness 的 grepMaxLineBytes），避免 minified 代码等超长行
# 直接撑爆模型上下文。
GREP_MAX_LINE_LENGTH = 2_000

# 搜索结果解析安全上限：超过后连"完整结果"落盘也会截断。防止病态大结果
# （如匹配大量文件的宽泛正则）把 rg stdout 全量解析进内存。
SEARCH_PARSE_LINE_CAP = 100_000

# 命令输出受控头尾采样（Host 侧）：bash/powershell 等命令工具的超长输出由 Host
# 统一保留首尾并提示完整输出保存位置，避免测试/构建日志淹没模型上下文。
COMMAND_OUTPUT_HEAD_CHARS = 2_000
COMMAND_OUTPUT_TAIL_CHARS = 6_000
COMMAND_OUTPUT_FILES_SUBDIR = "files"
COMMAND_OUTPUT_FILE_PREFIX = "command_output_"
COMMAND_OUTPUT_FILE_SUFFIX = ".log"

PROTECTED_NAMES = {
    ".git",
    ".venv",
    "venv",
    "env",
    "__pycache__",
    ".codex-ref",
    ".env",
    "config.json",
    "config.toml",
    "models.toml",
    "config.yaml",
    "models.yaml",
}

# 保护路径转成 ripgrep glob 排除：任意层级的同名目录/文件都不进入搜索，
# 语义与 is_protected_path 的任意路径段匹配一致（含 .env.* 前缀变体）。
# rg 对匹配的目录会剪枝其下内容，因此无需显式追加 /**。
PROTECTED_PATH_GLOBS = tuple(
    f"!**/{name}" for name in sorted(PROTECTED_NAMES)
) + ("!**/.env.*",)

# 仅从搜索中额外排除的目录：agent 自身临时目录（命令输出、搜索结果落盘）。
# 项目 .gitignore 通常已忽略它，但非 git 工作区里 rg 不会自动跳过，落盘
# 文件会被后续 grep/find 搜到（自污染，且同一 pattern 越搜越多）。只加进
# 搜索 glob 剪枝、不进 PROTECTED_NAMES——否则 read 工具会拒绝读取落盘文件，
# 破坏"读完整结果"的恢复路径。
SEARCH_EXCLUDED_DIRS = (".omnicrawl",)
PROTECTED_PATH_GLOBS = PROTECTED_PATH_GLOBS + tuple(
    f"!**/{name}" for name in SEARCH_EXCLUDED_DIRS
)



def is_forbidden_content_search_root(path: Path) -> bool:
    """用户主目录或文件系统根目录本身不允许内容关键词搜索。"""

    resolved = Path(path).expanduser().resolve()
    try:
        home = Path.home().resolve()
    except OSError:
        home = Path.home()
    return resolved == home or resolved.parent == resolved



class WorkspaceToolError(RuntimeError):
    """工作区工具参数校验或执行失败。

    ``code`` 为机器可识别的稳定错误标识；默认错误保持原有纯文本行为，只有
    需要区分失败原因的工具（例如 Edit_file）显式提供错误码。
    """

    def __init__(
        self,
        message: str,
        *,
        code: str | None = None,
        retryable: bool = False,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.retryable = retryable

    def formatted_message(self) -> str:
        if self.code:
            retry_hint = "；可重试" if self.retryable else ""
            return f"错误码：{self.code}{retry_hint}；{self.message}"
        return self.message

    def __str__(self) -> str:
        # 直接调用 WorkspaceTools 的旧异常文本保持不变；Agent 适配层通过
        # formatted_message 和 error_code 同时提供人类/机器可读信息。
        return self.message


@dataclass(frozen=True)
class _FileVersion:
    """Edit_file 事务中用于检测外部变化的文件版本。"""

    size: int
    mtime_ns: int
    digest: str


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


def _edit_file_lock_for(path: Path) -> ProcessFileLock:
    """返回同一目标文件共享的跨线程/跨进程编辑锁。"""

    resolved = path.resolve()
    with _EDIT_FILE_LOCKS_GUARD:
        lock = _EDIT_FILE_LOCKS.get(resolved)
        if lock is None:
            lock = ProcessFileLock(
                Path(f"{resolved}{EDIT_FILE_LOCK_FILE_SUFFIX}"),
                timeout_seconds=EDIT_FILE_LOCK_TIMEOUT_SECONDS,
                poll_seconds=EDIT_FILE_LOCK_POLL_SECONDS,
            )
            _EDIT_FILE_LOCKS[resolved] = lock
        return lock


def _read_utf8_bytes_for_edit(path: Path, display_path: str) -> bytes:
    """读取替换事务使用的 UTF-8 原始字节，避免 stat 与实际内容脱节。"""

    try:
        data = path.read_bytes()
    except OSError as exc:
        raise WorkspaceToolError(
            f"读取文件失败：{display_path}，{exc}",
            code="FS_READ_FAILED",
            retryable=True,
        ) from exc
    try:
        data.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise WorkspaceToolError(
            f"文件不是 UTF-8 文本或包含二进制内容：{display_path}",
            code="FS_INVALID_TEXT",
        ) from exc
    return data


def _file_version(path: Path, data: bytes) -> _FileVersion:
    """以大小、纳秒 mtime 和内容摘要组成稳定版本指纹。"""

    try:
        stat = path.stat()
    except OSError as exc:
        raise WorkspaceToolError(
            f"读取文件状态失败：{path}，{exc}",
            code="FS_STAT_FAILED",
            retryable=True,
        ) from exc
    return _FileVersion(
        size=len(data),
        mtime_ns=getattr(stat, "st_mtime_ns", int(stat.st_mtime * 1_000_000_000)),
        digest=hashlib.sha256(data).hexdigest(),
    )


def _normalize_line_endings(text: str) -> str:
    return text.replace("\r\n", "\n").replace("\r", "\n")


def _detect_line_endings(text: str) -> str:
    """返回原文行尾风格；混合文件按首次出现的风格恢复。"""

    match = re.search(r"\r\n|\r|\n", text)
    return match.group(0) if match else "\n"


def _restore_line_endings(text: str, line_ending: str) -> str:
    if line_ending == "\n":
        return text
    return text.replace("\n", line_ending)


def _format_edit_context(
    text: str,
    *,
    replacement_start_line: int,
    replacement_end_line: int,
    context_lines: int = EDIT_CONTEXT_LINES,
) -> tuple[int, int, str]:
    """返回编辑后首个替换位置附近的带行号文本。"""

    lines = text.splitlines()
    if not lines:
        return 0, 0, "文件修改后为空。"

    first_line = max(1, replacement_start_line - context_lines)
    last_line = min(len(lines), replacement_end_line + context_lines)

    numbered: list[str] = []
    for line_number in range(first_line, last_line + 1):
        line = lines[line_number - 1]
        if len(line) > READ_MAX_LINE_LENGTH:
            line = (
                f"{line[:READ_MAX_LINE_LENGTH]}... "
                f"(line truncated to {READ_MAX_LINE_LENGTH} chars)"
            )
        numbered.append(f"{line_number}: {line}")
    return first_line, last_line, "\n".join(numbered)


def _relative_under_prefix(text: str, prefix: str, folded_prefix: str) -> str | None:
    """文本落在前缀之下时返回相对部分，否则返回 None。

    ``prefix`` 已带尾部分隔符，避免 ``/opt/foo`` 误命中 ``/opt/foobar``；比较先用
    原样字符串（命中率最高的情形），大小写不同再折一次。``os.path.normcase`` 在
    Windows 上是 ``str.lower()``，极少数非 ASCII 字符会改变长度，此时前缀偏移量
    不可信，直接放弃快速路径让调用方回退到 ``os.path.relpath``。
    """

    if text.startswith(prefix):
        return text[len(prefix) :]
    folded = os.path.normcase(text)
    if len(folded) == len(text) and folded.startswith(folded_prefix):
        return text[len(prefix) :]
    return None


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
        resource_owner: object | None = None,
        ripgrep_binary: Path | None = None,
    ) -> None:
        self.workspace_root = workspace_root.resolve()
        # 搜索结果里的路径都是用工作区根拼出来的，预存前缀让 relative_path 走
        # 纯字符串比较（见 _relative_path_text）。
        self._workspace_root_text = str(self.workspace_root)
        # 调用方可能传进未解析形式的工作区根（8.3 短路径、经符号链接的路径、含
        # `..` 的路径）。旧实现用 Path.resolve() 把这类输入归一后再取相对路径，
        # 这里改为同时预存原始形式做前缀比较，保持语义又不用每条匹配做系统调用。
        root_forms = [self._workspace_root_text]
        original_root_text = str(workspace_root)
        if original_root_text != self._workspace_root_text:
            root_forms.append(original_root_text)
        self._workspace_root_forms = tuple(
            (
                form,
                prefix,
                os.path.normcase(prefix),
            )
            for form in root_forms
            for prefix in (os.path.join(form, ""),)
        )
        self.command_timeout_seconds = max(
            1,
            min(command_timeout_seconds, MAX_COMMAND_TIMEOUT_SECONDS),
        )
        self.max_file_read_chars = max_file_read_chars
        self._extra_protection_message = extra_protection_message
        self._resource_owner = resource_owner
        # 只在需要回退到外部 ripgrep 时使用；默认走随包的原生搜索扩展。
        self._ripgrep_binary_override = ripgrep_binary

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
        """读取本地 UTF-8 文本文件（或 omnicrawl://docs 内置文档）。

        普通读取按 start_line/max_lines 返回行窗口；大文件与超长行按流式处理，
        不把整个文件读入内存。返回文本包含行号与续读 footer。
        """

        text, _artifact = self.read_file_result(arguments)
        return text

    def read_file_result(
        self,
        arguments: dict[str, Any],
    ) -> tuple[str, dict[str, Any]]:
        """读取文件并返回 (模型可见文本, UI 结构化 artifact)。

        artifact 携带 {type: "read", path, start_line, lines, total_lines, lang}，
        供 TUI/API 渲染行号视图；模型文本仍走带行号与续读 footer 的格式。
        函数/片段定位需要完整文本，保持整读路径；普通行窗口走流式读取。
        """

        raw_path = str(arguments.get("path") or "").strip()
        if raw_path.startswith(BUNDLED_DOC_URI_PREFIX):
            try:
                path = resolve_bundled_doc_uri(raw_path)
            except BundledDocumentationError as exc:
                raise WorkspaceToolError(str(exc)) from exc
        else:
            path = self.safe_path(raw_path)
        function_name = _read_optional_text(arguments, "function_name")
        text_snippet = _read_optional_text(arguments, "text")
        if function_name and text_snippet:
            raise WorkspaceToolError("function_name 和 text 不能同时指定。")
        max_lines = _read_limited_int(
            arguments,
            "max_lines",
            default=READ_MAX_LINES,
            minimum=1,
            maximum=READ_MAX_LINES,
        )
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")

        if function_name or text_snippet:
            # 定位类读取需要完整文本（AST/片段搜索），保持整读路径。
            text = self.read_text(path)
            lines = text.splitlines()
            if function_name:
                function_range = _find_function_range(path, text, function_name)
                if function_range is None:
                    raise WorkspaceToolError(
                        f"未找到函数或方法：{function_name}（文件：{self.relative_path(path)}）。"
                    )
                start_line, end_line, resolved_name = function_range
                output = self._format_read_lines(
                    lines,
                    start_line=start_line,
                    end_line=end_line,
                    max_lines=max_lines,
                    header=f"定位：函数 {resolved_name}（第 {start_line}-{end_line} 行）",
                    truncation_hint="函数内容超过 max_lines，可提高 max_lines 继续读取。",
                )
            else:
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
                output = self._format_read_lines(
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
            return output, {"type": "read", "path": self.relative_path(path)}

        start_line = _read_limited_int(
            arguments,
            "start_line",
            default=1,
            minimum=1,
            maximum=100_000,
        )
        raw_max_lines = arguments.get("max_lines")
        if raw_max_lines is None or (
            isinstance(raw_max_lines, str) and not raw_max_lines.strip()
        ):
            # 未显式指定 max_lines 时按 READ_MAX_LINES 默认上限读取。
            max_lines = READ_MAX_LINES
        return self._read_file_window(
            path,
            start_line=start_line,
            max_lines=max_lines,
        )

    def _read_file_window(
        self,
        path: Path,
        *,
        start_line: int,
        max_lines: int,
    ) -> tuple[str, dict[str, Any]]:
        """流式读取文件行窗口，返回（文本, artifact）。

        逐行迭代而不是整文件读入内存：大文件只保留目标窗口行，超长行按
        READ_MAX_LINE_LENGTH 截断。footer 给出实际返回行号与续读提示，让模型
        知道文件还有更多内容以及从哪里继续。
        """

        display_path = self.relative_path(path)
        selected: list[tuple[int, str]] = []
        total_lines = 0
        try:
            with path.open("r", encoding="utf-8") as file:
                for line_no, raw_line in enumerate(file, start=1):
                    total_lines = line_no
                    if line_no < start_line:
                        continue
                    if len(selected) >= max_lines:
                        continue
                    line = raw_line.rstrip("\r\n")
                    if len(line) > READ_MAX_LINE_LENGTH:
                        line = (
                            f"{line[:READ_MAX_LINE_LENGTH]}... "
                            f"(line truncated to {READ_MAX_LINE_LENGTH} chars)"
                        )
                    selected.append((line_no, line))
        except UnicodeDecodeError as exc:
            raise WorkspaceToolError(
                f"文件不是 UTF-8 文本或包含二进制内容：{display_path}"
            ) from exc
        except OSError as exc:
            raise WorkspaceToolError(
                f"读取文件失败：{display_path}，{exc}"
            ) from exc

        # 起始行超出文件末尾时直接报错，而不是返回无意义的空窗口/footer。
        if start_line > total_lines and total_lines > 0:
            raise self._read_window_out_of_range_error(
                display_path,
                start_line,
                total_lines,
            )
        if total_lines == 0 and start_line > 1:
            raise self._read_window_out_of_range_error(
                display_path,
                start_line,
                0,
            )

        end_line = selected[-1][0] if selected else start_line - 1
        truncated = end_line < total_lines
        footer = self._format_read_footer(
            start_line=start_line,
            end_line=end_line,
            total_lines=total_lines,
            truncated=truncated,
        )
        numbered = [f"{line_no}: {line}" for line_no, line in selected]
        if not numbered:
            numbered.append("文件为空，或指定范围没有内容。")
        text = "\n".join(numbered)
        if footer:
            text += f"\n{footer}"
        artifact: dict[str, Any] = {
            "type": "read",
            "path": display_path,
            "start_line": start_line,
            "lines": [
                {"number": line_no, "text": line}
                for line_no, line in selected
            ],
            "total_lines": total_lines,
            "truncated": truncated,
        }
        lang = _lang_from_path(path)
        if lang is not None:
            artifact["lang"] = lang
        return text, artifact

    @staticmethod
    def _format_read_footer(
        *,
        start_line: int,
        end_line: int,
        total_lines: int,
        truncated: bool,
    ) -> str:
        """生成 read 输出末尾的续读 footer（Showing lines X-Y of Z 风格）。"""

        if not truncated:
            return f"(End of file - total {total_lines} lines)"
        next_line = end_line + 1
        return (
            f"(Showing lines {start_line}-{end_line} of {total_lines}. "
            f"Use start_line={next_line} to continue.)"
        )

    @staticmethod
    def _read_window_out_of_range_error(
        display_path: str,
        start_line: int,
        total_lines: int,
    ) -> WorkspaceToolError:
        return WorkspaceToolError(
            f"start_line {start_line} 超出文件范围：{display_path}（共 {total_lines} 行）。"
        )

    def find_files(self, arguments: dict[str, Any]) -> str:
        """按名称或相对路径查找工作区内的文件和目录，不读取文件内容。

        ``path`` 必须是工作区内的目标：工作区外的绝对路径、经 ``..`` 逃逸的
        相对路径以及指向区外的链接都拒绝——find 的契约是"查找工作区文件"，与
        grep 的根目录保护同源。``pattern`` 按文件名或工作区相对路径解释。
        """

        pattern = str(arguments.get("pattern") or "").strip()
        if not pattern:
            raise WorkspaceToolError("pattern 不能为空。")
        root = self.safe_path(str(arguments.get("path") or "."))
        if not self.is_within_workspace(root):
            raise WorkspaceToolError(
                f"find 只能在工作区内查找：{self.relative_path(root)} 不在工作区内；"
                "请把 path 指向工作区内的目录或文件。"
            )
        if not root.exists():
            raise WorkspaceToolError(f"路径不存在：{self.relative_path(root)}")
        kind = str(arguments.get("kind") or "all").strip().lower()
        if kind not in {"all", "file", "directory"}:
            raise WorkspaceToolError("kind 仅支持 all、file 或 directory。")
        case_sensitive = bool(arguments.get("case_sensitive", False))
        max_results = _read_limited_int(
            arguments,
            "max_results",
            default=50,
            minimum=1,
            maximum=MAX_SEARCH_RESULTS,
        )

        indexed = self._scan_file_names(
            pattern,
            root=root,
            kind=kind,
            case_sensitive=case_sensitive,
        )
        return self._render_search_result(
            [f"{path}{'/' if is_dir else ''}" for path, is_dir in indexed],
            max_results=max_results,
            prefix="find_results",
            label="条",
        )

    def grep(self, arguments: dict[str, Any]) -> str:
        """在 UTF-8 文本文件中执行 grep 风格搜索（由随包的原生搜索扩展执行）。

        pattern 默认按正则表达式解释，可使用 ``|`` 连接多个候选目标，
        例如 ``messages|context|tool_calls``；use_regex=false 时按精确子串匹配。
        支持输出匹配行上下文（context_lines）、每文件匹配计数（count）、
        仅列出匹配文件（files_with_matches），以及 include/exclude 文件名
        glob 过滤。
        """

        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            raise WorkspaceToolError("pattern 不能为空。")

        use_regex = bool(arguments.get("use_regex", True))
        case_sensitive = bool(arguments.get("case_sensitive", False))
        # 不在 Python 侧预编译校验：真正执行匹配的是后端的 RE2 系引擎，而
        # Python re 的方言与之不同（\p{...}、(?<name>...) 等 RE2 合法写法会被
        # 误判为非法，lookahead/反向引用等 Python 合法写法又会漏到后端才报错）。
        # 正则合法性以后端为准，由 _run_search_over_roots 统一翻译成中文提示。

        raw_path = str(arguments.get("path") or ".").strip()
        roots = self._resolve_grep_roots(raw_path)
        for root in roots:
            if is_forbidden_content_search_root(root):
                raise WorkspaceToolError(
                    "用户主目录或文件系统根目录本身不支持内容关键词搜索；"
                    "请把 path 指向其下的具体项目子目录。"
                )
            if not root.exists():
                raise WorkspaceToolError(f"路径不存在：{self.relative_path(root)}")

        max_results = _read_limited_int(
            arguments,
            "max_results",
            default=50,
            minimum=1,
            maximum=MAX_SEARCH_RESULTS,
        )
        context_lines = _read_limited_int(
            arguments,
            "context_lines",
            default=0,
            minimum=0,
            maximum=50,
        )
        count_only = bool(arguments.get("count", False))
        files_only = bool(arguments.get("files_with_matches", False))
        include_glob = _read_optional_text(arguments, "include")
        exclude_glob = _read_optional_text(arguments, "exclude")
        include_re = _compile_glob(include_glob)
        exclude_re = _compile_glob(exclude_glob)

        if count_only:
            return self._grep_count_files(
                roots,
                pattern=pattern,
                use_regex=use_regex,
                case_sensitive=case_sensitive,
                include_glob=include_glob,
                exclude_glob=exclude_glob,
                include_re=include_re,
                exclude_re=exclude_re,
                max_results=max_results,
            )
        if files_only:
            return self._grep_list_files(
                roots,
                pattern=pattern,
                use_regex=use_regex,
                case_sensitive=case_sensitive,
                include_glob=include_glob,
                exclude_glob=exclude_glob,
                include_re=include_re,
                exclude_re=exclude_re,
                max_results=max_results,
            )

        match_lines, parse_capped = self._scan_grep(
            roots,
            pattern=pattern,
            use_regex=use_regex,
            case_sensitive=case_sensitive,
            include_glob=include_glob,
            exclude_glob=exclude_glob,
            include_re=include_re,
            exclude_re=exclude_re,
        )
        if parse_capped:
            # 结果过大（超过 SEARCH_PARSE_LINE_CAP），完整落盘也只能保存前
            # SEARCH_PARSE_LINE_CAP 条；footer 明确提示截断而非假装完整。
            inline = self._format_grep_matches(
                match_lines[:max_results],
                context_lines=context_lines,
            )
            spill_lines = [
                self._format_match_line(relative, line_no, text)
                for relative, line_no, text in match_lines
            ]
            return inline + self._truncation_footer(
                spill_lines,
                max_results=max_results,
                prefix="grep_matches",
                label="条匹配",
                partial=True,
            )
        if len(match_lines) > max_results:
            inline = self._format_grep_matches(
                match_lines[:max_results],
                context_lines=context_lines,
            )
            spill_lines = [
                self._format_match_line(relative, line_no, text)
                for relative, line_no, text in match_lines
            ]
            return inline + self._truncation_footer(
                spill_lines,
                max_results=max_results,
                prefix="grep_matches",
                label="条匹配",
            )
        return self._format_grep_matches(match_lines, context_lines=context_lines)

    def _grep_count_files(
        self,
        roots: list[Path],
        *,
        pattern: str,
        use_regex: bool,
        case_sensitive: bool,
        include_glob: str | None,
        exclude_glob: str | None,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
        max_results: int,
    ) -> str:
        """输出每个文件的匹配行数（grep -c 语义）。"""

        stdout, returncode = self._run_search_over_roots(
            roots,
            pattern=pattern,
            use_regex=use_regex,
            case_sensitive=case_sensitive,
            include_glob=include_glob,
            exclude_glob=exclude_glob,
            extra_args=["--count", "--with-filename"],
        )
        counts: list[tuple[str, str]] = []
        parse_capped = False
        if returncode != 1:
            for line in stdout.splitlines():
                path_text, separator, count_text = line.rpartition(":")
                if not separator or not count_text.isdigit():
                    continue
                path = self._search_path_from_output(path_text)
                if not self._passes_grep_filters(path, include_re, exclude_re):
                    continue
                counts.append((self.relative_path(path), count_text))
                if len(counts) >= SEARCH_PARSE_LINE_CAP:
                    parse_capped = True
                    break
        counts.sort(key=lambda item: item[0].casefold())
        return self._render_search_result(
            [f"{path}: {count}" for path, count in counts],
            max_results=max_results,
            prefix="grep_counts",
            label="个文件",
            partial=parse_capped,
        )

    def _grep_list_files(
        self,
        roots: list[Path],
        *,
        pattern: str,
        use_regex: bool,
        case_sensitive: bool,
        include_glob: str | None,
        exclude_glob: str | None,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
        max_results: int,
    ) -> str:
        """只输出包含匹配的文件路径（grep -l 语义）。"""

        stdout, returncode = self._run_search_over_roots(
            roots,
            pattern=pattern,
            use_regex=use_regex,
            case_sensitive=case_sensitive,
            include_glob=include_glob,
            exclude_glob=exclude_glob,
            extra_args=["--files-with-matches", "-m", "1"],
        )
        matched: list[str] = []
        parse_capped = False
        if returncode != 1:
            for line in stdout.splitlines():
                if not line:
                    continue
                path = self._search_path_from_output(line)
                if not self._passes_grep_filters(path, include_re, exclude_re):
                    continue
                matched.append(self.relative_path(path))
                if len(matched) >= SEARCH_PARSE_LINE_CAP:
                    parse_capped = True
                    break
        matched.sort(key=lambda item: item.casefold())
        return self._render_search_result(
            matched,
            max_results=max_results,
            prefix="grep_files",
            label="个文件",
            partial=parse_capped,
        )

    def _scan_grep(
        self,
        roots: list[Path],
        *,
        pattern: str,
        use_regex: bool,
        case_sensitive: bool,
        include_glob: str | None,
        exclude_glob: str | None,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
    ) -> tuple[list[tuple[str, int, str]], bool]:
        """调用搜索后端的 --json 输出收集匹配行；返回（匹配行列表，是否超过解析上限）。

        ``--json`` 输出是 NDJSON，match 记录的路径/行号/行文本都是结构化字段，
        不存在文本格式 ``path:line:text`` 的冒号歧义（Windows 盘符、路径内冒号
        不再误切）；非 UTF-8 行给出占位文本而不是丢弃匹配。
        """

        stdout, returncode = self._run_search_over_roots(
            roots,
            pattern=pattern,
            use_regex=use_regex,
            case_sensitive=case_sensitive,
            include_glob=include_glob,
            exclude_glob=exclude_glob,
            extra_args=["--json", "--line-number"],
        )
        match_lines: list[tuple[str, int, str]] = []
        parse_capped = False
        # 匹配记录按文件连续输出，安全过滤只与文件有关：按文件缓存一次判定，
        # 避免命中密集时为每行都构造 Path 并重跑保护路径检查。
        filtered_relative: str | None = None
        filtered_accepted = False
        if returncode != 1:
            for raw_line in stdout.splitlines():
                if not raw_line:
                    continue
                parsed = self._parse_ripgrep_json_record(raw_line)
                if parsed is None:
                    continue
                relative, line_no, text = parsed
                if relative != filtered_relative:
                    filtered_relative = relative
                    filtered_accepted = self._passes_grep_filters(
                        self._search_path_from_output(relative),
                        include_re,
                        exclude_re,
                    )
                if not filtered_accepted:
                    continue
                match_lines.append((relative, line_no, text))
                if len(match_lines) >= SEARCH_PARSE_LINE_CAP:
                    parse_capped = True
                    break
        match_lines.sort(key=lambda item: (item[0].casefold(), item[1]))
        return match_lines, parse_capped

    @staticmethod
    def _format_match_line(relative: str, line_no: int, text: str) -> str:
        """格式化单条匹配行为 ``path:line: text``。"""

        return f"{relative}:{line_no}: {text}"

    @staticmethod
    def _cap_match_line(text: str) -> str:
        """对单条匹配行/上下文行做长度上限截断（保留前 N 字符并标记）。"""

        if len(text) <= GREP_MAX_LINE_LENGTH:
            return text
        return text[:GREP_MAX_LINE_LENGTH] + " (line truncated)"

    def _parse_ripgrep_json_record(
        self, raw_line: str
    ) -> tuple[str, int, str] | None:
        """解析一条 ``rg --json`` NDJSON 行；非 match 记录返回 None。

        match 记录的 path/line_number/lines.text 都是结构化字段，不存在
        文本格式 ``path:line:text`` 的冒号歧义。非 UTF-8 行 rg 只给 base64
        ``bytes`` 字段，返回占位文本而不是丢弃该匹配。
        """

        try:
            record = json.loads(raw_line)
        except ValueError:
            return None
        if not isinstance(record, dict) or record.get("type") != "match":
            return None
        data = record.get("data")
        if not isinstance(data, dict):
            return None
        path_obj = data.get("path")
        path_text = path_obj.get("text") if isinstance(path_obj, dict) else None
        line_number = data.get("line_number")
        lines_obj = data.get("lines")
        text = lines_obj.get("text") if isinstance(lines_obj, dict) else None
        if not isinstance(path_text, str) or not isinstance(line_number, int):
            return None
        if not isinstance(text, str):
            return (
                self._relative_path_text(path_text),
                line_number,
                "(line is not valid UTF-8)",
            )
        return (
            # 直接用路径文本换算，避免每条匹配都构造一次 Path 对象。
            self._relative_path_text(path_text),
            line_number,
            self._cap_match_line(text.rstrip("\r\n")),
        )

    def _save_search_results(self, content: str, *, prefix: str) -> str | None:
        """把完整搜索结果写入 Agent 临时目录；返回保存路径，失败返回 None。

        与命令输出落盘同一目录（.omnicrawl/.agent_tmp/files/），模型可用
        read 工具按返回路径读取完整结果。
        """

        save_path = (
            self.workspace_root
            / DEFAULT_AGENT_TEMP_DIRECTORY
            / COMMAND_OUTPUT_FILES_SUBDIR
            / f"{prefix}_{uuid.uuid4().hex[:8]}.txt"
        )
        try:
            save_path.parent.mkdir(parents=True, exist_ok=True)
            save_path.write_text(content, encoding="utf-8")
            return str(save_path)
        except OSError:
            return None

    def _truncation_footer(
        self,
        body_lines: list[str],
        *,
        max_results: int,
        prefix: str,
        label: str = "条",
        partial: bool = False,
    ) -> str:
        """构造超限 footer：完整结果落盘并给出恢复路径（deepseek 风格）。

        partial=True 表示结果本身也超过了解析上限，落盘内容同样不完整。
        """

        total = len(body_lines)
        saved = self._save_search_results(
            "\n".join(body_lines), prefix=prefix
        )
        if partial:
            hint = (
                f"完整结果过大，已保存前 {total} 条至：{saved}"
                if saved
                else "结果过大且完整结果未保存，请缩小 pattern/path/include 范围。"
            )
        else:
            hint = (
                f"完整结果已保存至：{saved}"
                if saved
                else "完整结果未保存，请缩小 pattern/path/include 范围。"
            )
        return f"\n... 已达到 max_results（{max_results}），共 {total} {label}。{hint}"

    def _render_search_result(
        self,
        all_items: list[str],
        *,
        max_results: int,
        prefix: str,
        empty_text: str = "未找到匹配结果。",
        label: str = "条",
        partial: bool = False,
    ) -> str:
        """渲染搜索结果：不超限原样返回，超限保留前 max_results 条并落盘完整结果。

        partial=True 表示 all_items 本身已被解析上限截断，落盘内容同样不完整。
        """

        if not all_items:
            return empty_text
        if len(all_items) <= max_results:
            return "\n".join(all_items)
        inline = "\n".join(all_items[:max_results])
        return inline + self._truncation_footer(
            all_items,
            max_results=max_results,
            prefix=prefix,
            label=label,
            partial=partial,
        )

    def _resolve_grep_roots(self, raw_path: str) -> list[Path]:
        """解析 grep 的 path，支持文件/目录以及绝对或相对 glob。

        glob 必须在 ``safe_path`` 之前展开：带 ``*`` 的 Windows 路径不是一个
        可直接 ``Path.exists()`` 判断的路径，且 ``Path.resolve()`` 会把通配符
        当作普通文件名。展开后的每个文件或目录再逐一经过 safe_path，继续沿用
        受保护路径和额外保护回调的安全边界。
        """

        candidate = Path(raw_path)
        if not candidate.is_absolute():
            candidate = self.workspace_root / candidate
        if not glob.has_magic(str(candidate)):
            return [self.safe_path(str(candidate))]

        matches = glob.glob(str(candidate), recursive=True)
        roots: list[Path] = []
        seen: set[Path] = set()
        for match in sorted(matches, key=str.casefold):
            matched_path = Path(match).resolve()
            # glob 可能覆盖 .git/.env 等受保护条目；与目录搜索一致，
            # 将其排除而不是让单个受保护匹配阻断其他文件搜索。
            if self.should_skip_path(matched_path):
                continue
            path = self.safe_path(match)
            if path in seen:
                continue
            seen.add(path)
            roots.append(path)
        if not roots:
            raise WorkspaceToolError(f"路径不存在或 glob 未匹配：{raw_path}")
        return roots

    def _search_path_from_output(self, raw_path: str) -> Path:
        """把 rg 输出里的路径解析回文件系统路径。

        rg 以 workspace_root 为 cwd 搜索，输出的路径可能是相对的（目录搜索）
        或绝对的（显式传入绝对路径）；两种都转成可继续做安全过滤的 Path。
        """

        path = Path(raw_path)
        if not path.is_absolute():
            path = self.workspace_root / path
        return path

    def _passes_grep_filters(
        self,
        path: Path,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
    ) -> bool:
        """对 rg 返回的匹配文件应用与旧实现一致的安全过滤和 include/exclude。

        rg 的 --glob 已对纯 basename 模式预剪枝（提高性能）；这里保留 Python
        侧过滤作为兜底，保证保护路径回调（_extra_protection_message）与含
        路径分隔符的 glob（rg 匹配相对路径、旧实现只匹配文件名）的行为与旧
        实现完全一致。
        """

        if self.should_skip_path(path):
            return False
        if include_re is not None and not include_re.match(path.name):
            return False
        if exclude_re is not None and exclude_re.match(path.name):
            return False
        return True

    def _run_search_over_roots(
        self,
        roots: list[Path],
        *,
        pattern: str,
        use_regex: bool,
        case_sensitive: bool,
        extra_args: list[str],
        include_glob: str | None,
        exclude_glob: str | None,
    ) -> tuple[str, int]:
        """让搜索后端直接遍历目录搜索（返回合并后的 (stdout, returncode)）。

        相比更早的纯 Python 实现（os.walk 全量枚举文件），把目录交给搜索后端
        遍历可以：
        - 原生读取 .gitignore / .ignore，自动跳过 node_modules、dist、构建产物
          等被忽略目录（旧实现会全量枚举这些目录，实测多枚举 20+ 倍文件）；
        - 用 Go 原生扫描的并行 I/O 加速匹配，不再受 Python 单线程遍历限制；
        - 保护路径（PROTECTED_PATH_GLOBS）与 include/exclude 转成 glob 在
          后端侧剪枝，进一步减少要扫描的文件数。

        include/exclude 的 glob 语义与旧实现一致（匹配文件名，大小写不敏感）：
        后端 glob 无 / 时匹配任意层级的 basename，因此把用户 glob 原样传入
        即可。保护路径 glob 与 include 一样是 basename 语义，对目录会剪枝其
        全部内容。

        原生后端不经命令行传参，一次调用即可覆盖全部 roots；只有回退到外部
        rg 时才需要按字符预算分批，避免 Windows 命令行过长。
        """

        args = list(extra_args)
        if not use_regex:
            args.append("--fixed-strings")
        if not case_sensitive:
            args.append("--ignore-case")
        # 旧实现（os.walk）会搜索隐藏文件/目录；rg 默认跳过，这里显式开启以
        # 保持行为一致（.git 等 VCS 目录 rg 永不搜索，即使 --hidden）。
        args.append("--hidden")
        # 非 git 目录也读 .gitignore/.ignore：让 node_modules、构建产物等被
        # 忽略目录在任意工作区都自动跳过，不再依赖是否初始化了 git 仓库。
        args.append("--no-require-git")
        # 保护路径 glob（! 前缀为排除）放在 include/exclude 之前：rg 按出现
        # 顺序应用 glob，先排除保护路径再应用用户过滤，与旧实现过滤顺序一致。
        for glob_pattern in PROTECTED_PATH_GLOBS:
            args.extend(["--glob", glob_pattern])
        # 只有纯 basename 模式（不含 /）才能转成 rg --glob：rg 的 glob 匹配
        # 相对路径，旧实现只匹配文件名，含路径分隔符的模式语义不同，留给
        # Python 侧 _passes_grep_filters 兜底过滤，保证结果与旧实现一致。
        if include_glob and _glob_is_basename_only(include_glob):
            args.extend(["--glob", include_glob])
        if exclude_glob and _glob_is_basename_only(exclude_glob):
            args.extend(["--glob", f"!{exclude_glob}"])
        if (include_glob and _glob_is_basename_only(include_glob)) or (
            exclude_glob and _glob_is_basename_only(exclude_glob)
        ):
            args.append("--glob-case-insensitive")
        if self._ripgrep_binary_override is None and native_backend_available():
            batches = [[str(root) for root in roots]]
        else:
            batches = batch_paths(roots)
        combined_stdout: list[str] = []
        matched = False
        for path_batch in batches:
            try:
                stdout, returncode = run_search(
                    [*args, "--", pattern, *path_batch],
                    cwd=self.workspace_root,
                    timeout=self.command_timeout_seconds,
                    ripgrep_binary=self._ripgrep_binary_override,
                )
            except SearchRegexError as exc:
                # pattern 语法不被后端支持属于用户输入问题，提示可以退化成
                # 精确子串匹配，而不是让模型误以为搜索后端故障。
                hint = "。可设置 use_regex=false 按精确子串匹配。" if use_regex else "。"
                raise WorkspaceToolError(f"{exc}{hint}") from exc
            except SearchBackendError as exc:
                raise WorkspaceToolError(str(exc)) from exc
            combined_stdout.append(stdout)
            matched = matched or returncode == 0
        return "".join(combined_stdout), 0 if matched else 1

    def _format_grep_matches(
        self,
        match_lines: list[tuple[str, int, str]],
        *,
        context_lines: int,
    ) -> str:
        """把匹配行格式化为 grep 风格输出。

        匹配行使用 ``path:line_no: line``，上下文行使用 ``path-line_no- line``，
        与 grep -n -C 的标记习惯一致；相邻匹配的上下文区间自动去重。
        """

        if not match_lines:
            return "未找到匹配结果。"
        # context_lines=0 时直接用 rg 报告的行文本（已做单行截断），不再
        # 重新读取文件，避免大文件二次 I/O 且保证文本与 rg 输出一致。
        if context_lines <= 0:
            return "\n".join(
                self._format_match_line(relative, line_no, text)
                for relative, line_no, text in match_lines
            )
        by_file: dict[str, list[int]] = {}
        file_order: list[str] = []
        for relative, line_no, _line in match_lines:
            if relative not in by_file:
                by_file[relative] = []
                file_order.append(relative)
            by_file[relative].append(line_no)

        out: list[str] = []
        for relative in file_order:
            try:
                file_lines = self.read_text(
                    self.workspace_root / relative
                ).splitlines()
            except WorkspaceToolError:
                continue
            covered: set[int] = set()
            for line_no in by_file[relative]:
                start = max(0, line_no - 1 - context_lines)
                end = min(len(file_lines), line_no + context_lines)
                for index in range(start, end):
                    current = index + 1
                    if current in covered:
                        continue
                    covered.add(current)
                    line_text = self._cap_match_line(file_lines[index])
                    if current == line_no:
                        out.append(f"{relative}:{current}: {line_text}")
                    else:
                        out.append(f"{relative}-{current}- {line_text}")
        return "\n".join(out)

    def _scan_file_names(
        self,
        pattern: str,
        *,
        root: Path,
        kind: str,
        case_sensitive: bool,
    ) -> list[tuple[str, bool]]:
        """按名称/相对路径过滤候选条目，返回全部匹配（截断由调用方处理）。"""

        needle = pattern if case_sensitive else pattern.casefold()
        candidates = self._list_search_entries(root, include_dirs=kind != "file")
        results: list[tuple[str, bool]] = []
        for entry_path, is_dir in candidates:
            if self.should_skip_path(entry_path):
                continue
            relative = self.relative_path(entry_path)
            candidate = relative if case_sensitive else relative.casefold()
            name = entry_path.name if case_sensitive else entry_path.name.casefold()
            if kind == "file" and is_dir:
                continue
            if kind == "directory" and not is_dir:
                continue
            if _has_glob_magic(pattern):
                # 保留旧的“无通配符时按子串查找”兼容行为；出现 glob
                # 元字符后按文件名或相对路径进行匹配，尤其使 ``*`` 表示
                # 匹配所有文件名，而不是把星号当作普通字符。
                glob_pattern = pattern if case_sensitive else pattern.casefold()
                relative_for_glob = (
                    relative if case_sensitive else relative.casefold()
                ).replace("\\", "/")
                glob_pattern = glob_pattern.replace("\\", "/")
                if not (
                    fnmatch.fnmatchcase(name, glob_pattern)
                    or fnmatch.fnmatchcase(relative_for_glob, glob_pattern)
                ):
                    continue
            elif needle not in candidate and needle not in name:
                continue
            results.append((relative, is_dir))
        return results

    def _list_search_entries(
        self,
        root: Path,
        *,
        include_dirs: bool,
    ) -> list[tuple[Path, bool]]:
        """用搜索后端的 ``--files`` 枚举候选（原生读 .gitignore，保护路径剪枝）。

        相比旧实现（os.walk 全量遍历、不读 .gitignore，node_modules/dist 等
        被忽略目录全部展开），把遍历交给搜索后端：跳过被忽略目录、并行 I/O。
        后端只列文件；需要目录时由文件路径的祖先推导（空目录不再返回，与
        deepseek glob 工具"只返回文件"的语义一致，但保留 kind 过滤能力）。
        """

        if root.is_file():
            return [(root, False)]
        args = ["--files", "--hidden", "--no-require-git"]
        for glob_pattern in PROTECTED_PATH_GLOBS:
            args.extend(["--glob", glob_pattern])
        try:
            stdout, returncode = run_search(
                [*args, "--", str(root)],
                cwd=self.workspace_root,
                timeout=self.command_timeout_seconds,
                ripgrep_binary=self._ripgrep_binary_override,
            )
        except SearchBackendError as exc:
            raise WorkspaceToolError(str(exc)) from exc
        if returncode == 1:
            return []
        files: list[Path] = []
        dirs: set[Path] = set()
        for line in stdout.splitlines():
            if not line:
                continue
            path = self._search_path_from_output(line)
            if not path.is_file():
                continue
            files.append(path)
            if include_dirs:
                parent = path.parent
                while parent != root and parent.is_relative_to(root):
                    dirs.add(parent)
                    parent = parent.parent
            if len(files) >= SEARCH_PARSE_LINE_CAP:
                break
        entries = [(path, False) for path in files]
        if include_dirs:
            entries.extend((path, True) for path in dirs)
        entries.sort(
            key=lambda item: (
                -self._search_entry_mtime(item[0]),
                str(item[0]).casefold(),
            )
        )
        return entries

    @staticmethod
    def _search_entry_mtime(path: Path) -> int:
        """返回搜索条目的修改时间；条目并发消失或不可访问时置于末尾。"""

        try:
            return path.stat().st_mtime_ns
        except OSError:
            return 0


    def edit_file(self, arguments: dict[str, Any]) -> str:
        """按字面替换文本，并以单文件事务保护读取、匹配和写回。

        省略 ``count`` 时要求 ``old_text`` 恰好匹配 1 处；显式提供 ``count``
        时保持历史语义：0 替换全部，正数替换至多指定数量。编辑内部把换行统一
        为 LF，写回时恢复文件原有的 CRLF/LF 风格；版本指纹在原子发布前再次校验，
        避免覆盖锁外部进程的修改。
        """

        path = self.safe_path(str(arguments.get("path") or ""))
        old_text = str(arguments.get("old_text") or "")
        new_text = str(arguments.get("new_text") or "")
        count_provided = "count" in arguments
        count = _read_limited_int(arguments, "count", default=1, minimum=0, maximum=10_000)
        if not path.is_file():
            raise WorkspaceToolError(f"不是文件：{self.relative_path(path)}")
        if not old_text:
            raise WorkspaceToolError("old_text 不能为空。")

        lock = _edit_file_lock_for(path)
        try:
            with lock:
                original_bytes = _read_utf8_bytes_for_edit(path, self.relative_path(path))
                version = _file_version(path, original_bytes)
                original = original_bytes.decode("utf-8")
                original_line_endings = _detect_line_endings(original)
                normalized_original = _normalize_line_endings(original)
                normalized_old = _normalize_line_endings(old_text)
                normalized_new = _normalize_line_endings(new_text)
                occurrences = normalized_original.count(normalized_old)
                if occurrences == 0:
                    raise WorkspaceToolError(
                        "未找到 old_text（匹配到 0 处），文件未修改；请重新读取文件并补充准确上下文。",
                        code="FS_EDIT_NOT_FOUND",
                    )
                if not count_provided and occurrences != 1:
                    raise WorkspaceToolError(
                        f"匹配到 {occurrences} 处 old_text，文件未修改；请提供 count 或补充上下文使其唯一。",
                        code="FS_EDIT_AMBIGUOUS",
                    )

                replace_count = occurrences if count == 0 else min(count, occurrences)
                edited = normalized_original.replace(
                    normalized_old,
                    normalized_new,
                    replace_count,
                )
                output = _restore_line_endings(edited, original_line_endings)

                latest_bytes = _read_utf8_bytes_for_edit(path, self.relative_path(path))
                if _file_version(path, latest_bytes) != version:
                    raise WorkspaceToolError(
                        "文件在替换期间发生变化，未写入；请重新读取后重试。",
                        code="FS_STALE_VERSION",
                        retryable=True,
                    )
                try:
                    atomic_write_text(
                        path,
                        output,
                        fsync=True,
                        prefix=f".{path.name}.",
                        suffix=".replace.tmp",
                    )
                except SessionStoreError as exc:
                    raise WorkspaceToolError(
                        f"原子写入失败：{self.relative_path(path)}，{exc}",
                        code="FS_ATOMIC_WRITE_FAILED",
                        retryable=True,
                    ) from exc
        except WorkspaceToolError:
            raise
        except SessionStoreError as exc:
            raise WorkspaceToolError(
                f"获取文件编辑锁失败：{self.relative_path(path)}，{exc}",
                code="FS_LOCK_TIMEOUT",
                retryable=True,
            ) from exc
        except OSError as exc:
            raise WorkspaceToolError(
                f"替换文件失败：{self.relative_path(path)}，{exc}",
                code="FS_EDIT_FAILED",
                retryable=True,
            ) from exc
        first_match_offset = normalized_original.find(normalized_old)
        first_match_line = normalized_original.count("\n", 0, first_match_offset) + 1
        replacement_prefix = normalized_original[:first_match_offset] + normalized_new
        first_match_end_line = replacement_prefix.count("\n") + 1
        context_start, context_end, context = _format_edit_context(
            output,
            replacement_start_line=first_match_line,
            replacement_end_line=first_match_end_line,
        )
        return (
            f"已修改 {self.relative_path(path)}，替换 {replace_count} 处。\n"
            f"首个替换位置上下文（第 {context_start}-{context_end} 行，前后各 {EDIT_CONTEXT_LINES} 行）：\n"
            f"{context}"
        )


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

        unsupported_keys = set(arguments) - {
            "command",
            "diagnostic_command",
            "timeout_seconds",
        }
        if unsupported_keys:
            names = "、".join(sorted(unsupported_keys))
            raise WorkspaceToolError(f"显式命令工具不支持参数：{names}。")
        command = str(arguments.get("command") or "").strip()
        if not command:
            raise WorkspaceToolError("command 不能为空。")
        diagnostic_value = arguments.get("diagnostic_command")
        if diagnostic_value is not None and not isinstance(diagnostic_value, str):
            raise WorkspaceToolError("diagnostic_command 必须是字符串。")
        diagnostic_command = (
            diagnostic_value.strip() if isinstance(diagnostic_value, str) else ""
        )

        timeout = _read_limited_int(
            arguments,
            "timeout_seconds",
            default=self.command_timeout_seconds,
            minimum=1,
            maximum=MAX_COMMAND_TIMEOUT_SECONDS,
        )
        primary_result = self._run_command_invocation(
            self.command_invocation(command, shell=shell),
            timeout_seconds=timeout,
            display_kind="Shell",
        )
        if not diagnostic_command:
            return primary_result

        diagnostic_result = self._run_command_invocation(
            self.command_invocation(diagnostic_command, shell=shell),
            timeout_seconds=timeout,
            display_kind="Shell",
        )
        return WorkspaceCommandResult(
            ok=primary_result.ok and diagnostic_result.ok,
            output=(
                f"主命令结果：\n{primary_result.output}\n\n"
                f"诊断命令结果：\n{diagnostic_result.output}"
            ),
        )

    def run_argv_command(
        self,
        arguments: tuple[str, ...] | list[str],
        *,
        timeout_seconds: int,
        label: str,
    ) -> WorkspaceCommandResult:
        """直接启动一组预验证 argv，不经任何 Shell 解析。

        该底座只供 Host 内部的固定命令策略调用，例如 SubAgent 的
        ``verify_command``。它不属于面向模型的通用命令工具：调用者必须先完成
        命令白名单和参数边界校验，不能把模型提供的原始文本直接传入这里。
        """

        if not isinstance(arguments, (tuple, list)) or not arguments:
            raise WorkspaceToolError("受控命令参数不能为空。")
        if any(not isinstance(item, str) or not item for item in arguments):
            raise WorkspaceToolError("受控命令参数必须全部是非空字符串。")
        if isinstance(timeout_seconds, bool) or not isinstance(timeout_seconds, int):
            raise WorkspaceToolError("受控命令 timeout_seconds 必须是整数。")
        if timeout_seconds < 1 or timeout_seconds > MAX_COMMAND_TIMEOUT_SECONDS:
            raise WorkspaceToolError(
                f"受控命令 timeout_seconds 必须在 1 到 {MAX_COMMAND_TIMEOUT_SECONDS} 之间。"
            )
        if not isinstance(label, str) or not label.strip():
            raise WorkspaceToolError("受控命令标签不能为空。")

        return self._run_command_invocation(
            WorkspaceCommandInvocation(args=list(arguments), label=label.strip()),
            timeout_seconds=timeout_seconds,
            display_kind="命令",
        )

    def _run_command_invocation(
        self,
        invocation: WorkspaceCommandInvocation,
        *,
        timeout_seconds: int,
        display_kind: str,
    ) -> WorkspaceCommandResult:
        """执行已经完成解释器/参数校验的进程，并复用既有超时回收逻辑。"""

        process_environment = os.environ.copy()
        process_environment.update(
            {
                "LANG": "C.UTF-8",
                "LC_ALL": "C.UTF-8",
                "PYTHONIOENCODING": "utf-8",
            }
        )
        popen_kwargs: dict[str, Any] = {
            "cwd": str(self.workspace_root),
            "env": process_environment,
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

        from .monitor import (
            BackgroundMonitorManager,
            _assign_process_to_kill_on_close_job,
            _close_windows_handle,
        )

        job_handle = _assign_process_to_kill_on_close_job(process)

        def terminate_process() -> None:
            nonlocal job_handle
            BackgroundMonitorManager._terminate_process_tree(
                process,
                job_handle=job_handle,
                wait=False,
            )
            job_handle = None

        resource_owner = current_stream_scope()
        with registered_resource(
            process,
            owner=resource_owner,
            close_callback=terminate_process,
        ):
            try:
                stdout, stderr = process.communicate(timeout=timeout_seconds)
            except subprocess.TimeoutExpired:
                BackgroundMonitorManager._terminate_process_tree(process, job_handle=job_handle)
                job_handle = None
                raise WorkspaceToolError(f"命令执行超过 {timeout_seconds} 秒，已终止。")
            _close_windows_handle(job_handle)

        output_parts = [
            f"退出码：{process.returncode}",
            f"{display_kind}：{invocation.label}",
        ]
        if stdout.strip():
            output_parts.append(f"stdout:\n{self._sampled_output(stdout.strip())}")
        if stderr.strip():
            output_parts.append(f"stderr:\n{self._sampled_output(stderr.strip())}")
        return WorkspaceCommandResult(
            ok=process.returncode == 0,
            output="\n\n".join(output_parts),
        )

    def _sampled_output(self, text: str) -> str:
        """对单段命令输出做受控头尾采样，超长时把完整输出写入 Agent 临时目录。"""
        save_path = (
            self.workspace_root
            / DEFAULT_AGENT_TEMP_DIRECTORY
            / COMMAND_OUTPUT_FILES_SUBDIR
            / (
                f"{COMMAND_OUTPUT_FILE_PREFIX}"
                f"{uuid.uuid4().hex[:8]}"
                f"{COMMAND_OUTPUT_FILE_SUFFIX}"
            )
        )
        return _sample_command_output(text, save_path=save_path)

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
                args=[str(executable), "-o", "pipefail", "-lc", command],
                label="Bash",
            )
        if normalized_shell == "powershell":
            executable = _find_powershell_executable()
            if executable is None:
                raise WorkspaceToolError("未找到 PowerShell 可执行文件。")
            utf8_prefix = (
                "$__OmniCrawlUtf8 = [System.Text.UTF8Encoding]::new($false); "
                "[Console]::InputEncoding = $__OmniCrawlUtf8; "
                "[Console]::OutputEncoding = $__OmniCrawlUtf8; "
                "$OutputEncoding = $__OmniCrawlUtf8; "
            )
            return WorkspaceCommandInvocation(
                args=[
                    str(executable),
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    (
                        "$ErrorActionPreference = 'Stop'; "
                        f"{utf8_prefix}"
                        "& {\n"
                        f"{command}\n"
                        "}\n"
                        "$__OmniCrawlExitCode = $LASTEXITCODE; "
                        "if ($null -ne $__OmniCrawlExitCode) "
                        "{ exit $__OmniCrawlExitCode }"
                    ),
                ],
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
            # 相对路径仍以工作区为基准解析；绝对路径可指向工作区外的本机路径。
            candidate = self.workspace_root / candidate
        resolved = candidate.resolve()
        if self.is_protected_path(resolved):
            raise WorkspaceToolError(f"拒绝访问受保护路径：{self.relative_path(resolved)}")
        extra_message = self._extra_protection_message(resolved) if self._extra_protection_message else None
        if extra_message:
            raise WorkspaceToolError(extra_message)
        return resolved

    def is_within_workspace(self, path: Path) -> bool:
        """判断路径是否位于工作区内；先解析链接与 ``..``，逃逸到区外即为 False。

        ``safe_path`` 有意允许工作区外的绝对路径（read/list 等工具借此读取
        本机其它文件），所以"只查工作区"的工具要自己补这道检查。
        """

        try:
            Path(path).resolve().relative_to(self.workspace_root)
        except (OSError, ValueError):
            return False
        return True

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
                # 目录 symlink/junction 一律不遍历（与 find 的 rg --files 枚举一致）。
                if not self.should_skip_path(current_dir / dirname)
                and not (current_dir / dirname).is_symlink()
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
        """返回工作区相对路径；工作区之外的路径原样返回。"""

        return self._relative_path_text(str(path))

    def _relative_path_text(self, text: str) -> str:
        """把路径文本换算成工作区相对路径，纯字符串运算、不做系统调用。

        这里刻意不走 ``Path.resolve()``，也尽量不依赖 ``os.path.relpath``：前者
        给每条匹配做一次系统调用（Windows 上是 GetFinalPathNameByHandle，单次
        几十微秒），后者内部要对两个路径各做一次 normpath/normcase（约 80 微秒）。
        搜索结果里的路径都是用工作区根拼出来的，因此先用前缀比较命中（大小写差异
        折一次）；仍不命中才回退到 relpath。工作区之外的路径 relpath 会给出 ``..``，
        此时原样返回输入文本（与旧实现一致）。
        """

        for form_text, prefix, folded_prefix in self._workspace_root_forms:
            if text == form_text:
                return os.curdir
            relative = _relative_under_prefix(text, prefix, folded_prefix)
            if relative is not None:
                return relative
        try:
            relative = os.path.relpath(text, self._workspace_root_text)
        except ValueError:
            # Windows 上跨盘符，relpath 无法给出相对路径。
            return text
        if relative == os.pardir or relative.startswith(f"{os.pardir}{os.sep}"):
            return text
        return relative

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


def _sample_command_output(
    text: str,
    *,
    head_chars: int = COMMAND_OUTPUT_HEAD_CHARS,
    tail_chars: int = COMMAND_OUTPUT_TAIL_CHARS,
    save_path: Path | None = None,
) -> str:
    """对命令输出做受控头尾采样，超长时保留首尾并记录完整输出位置。

    未超过 head_chars + tail_chars 时原样返回；超过时按行对齐保留首部与尾部
    （至少各一行，不切断多字节字符），中间以提示行代替，避免超大输出淹没模型
    上下文。save_path 非空时把完整输出写入该文件并在提示中给出路径；写入失败
    静默降级为只截断，不影响命令结果。空文本直接返回。
    """
    if not text or len(text) <= head_chars + tail_chars:
        return text

    lines = text.splitlines(keepends=True)
    head_lines: list[str] = []
    head_length = 0
    for line in lines:
        if head_lines and head_length + len(line) > head_chars:
            break
        head_lines.append(line)
        head_length += len(line)

    tail_lines: list[str] = []
    tail_length = 0
    for line in reversed(lines):
        if tail_lines and tail_length + len(line) > tail_chars:
            break
        tail_lines.append(line)
        tail_length += len(line)
    tail_lines.reverse()

    if len(head_lines) + len(tail_lines) >= len(lines):
        return text

    omitted_lines = len(lines) - len(head_lines) - len(tail_lines)
    hint = (
        f"\n… 系统已截断：共 {len(lines)} 行，仅保留首部 {len(head_lines)} 行"
        f"与尾部 {len(tail_lines)} 行（省略 {omitted_lines} 行）。"
    )
    if save_path is not None:
        try:
            save_path.parent.mkdir(parents=True, exist_ok=True)
            save_path.write_text(text, encoding="utf-8")
            hint += f" 完整输出已保存至：{save_path}"
        except OSError:
            pass
    return "".join(head_lines) + hint + "\n" + "".join(tail_lines)


def _has_glob_magic(pattern: str) -> bool:
    """判断搜索目标是否包含 glob 通配符。"""
    return any(character in pattern for character in "*?[")


def _compile_glob(pattern: str) -> re.Pattern[str] | None:
    """把 include/exclude 文件名 glob 编译为正则；空模式返回 None。"""

    if not pattern:
        return None
    return re.compile(fnmatch.translate(pattern), re.IGNORECASE)


def _glob_is_basename_only(pattern: str) -> bool:
    """判断 include/exclude glob 是否只匹配文件名（不含路径分隔符）。

    旧实现用 fnmatch 匹配 Path.name（basename）；rg 的 -g 无 / 时也匹配
    basename，语义一致，可以转成 --glob 预剪枝。含 / 或 \\ 的模式匹配的是
    相对路径，与旧实现语义不同，只能留给 Python 侧过滤。
    """

    return "/" not in pattern and "\\" not in pattern


_LANG_BY_EXTENSION: dict[str, str] = {
    "py": "python", "pyw": "python",
    "js": "javascript", "jsx": "javascript", "mjs": "javascript", "cjs": "javascript",
    "ts": "typescript", "tsx": "typescript", "mts": "typescript", "cts": "typescript",
    "json": "json", "jsonc": "json", "toml": "toml", "yaml": "yaml", "yml": "yaml",
    "md": "markdown", "markdown": "markdown",
    "go": "go", "rs": "rust", "java": "java", "kt": "kotlin", "rb": "ruby",
    "c": "c", "h": "c", "cc": "cpp", "cpp": "cpp", "hpp": "cpp",
    "cs": "csharp", "swift": "swift", "php": "php", "sql": "sql",
    "sh": "bash", "bash": "bash", "zsh": "bash", "ps1": "powershell",
    "html": "html", "htm": "html", "css": "css", "scss": "scss", "less": "less",
    "xml": "xml", "lua": "lua",
}


def _lang_from_path(path: Path) -> str | None:
    """从文件扩展名推导语法高亮语言提示；未知扩展名返回 None。"""

    suffix = path.suffix.lstrip(".").lower()
    return _LANG_BY_EXTENSION.get(suffix)


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
