from __future__ import annotations

import ast
import fnmatch
import os
import re
import shutil
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from ..documentation import (
    BUNDLED_DOC_URI_PREFIX,
    BundledDocumentationError,
    resolve_bundled_doc_uri,
)
from ..llm.stream_registry import current_stream_scope, registered_resource
from .search_index import ProjectSearchIndex, is_forbidden_content_search_root


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
    "config.yaml",
    "models.yaml",
}

# 仅索引层忽略的目录名：这些目录仍然可以被普通工具读取和搜索（直接扫描），
# 但不进入后台索引快照。它们要么是构建产物/缓存（频繁变化、无检索价值），
# 要么是其他 AI 工具的工作目录（含会话转录等私有数据，索引会泄露隐私并
# 导致快照持续失效）。不要把它们加入 PROTECTED_NAMES，否则 Agent 将完全
# 无法访问这些目录。
INDEX_EXCLUDED_NAMES = {
    "build",
    "dist",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".coverage",
    ".agents",
    ".claude",
    ".codex",
    ".pi-subagents",
    ".agent_tmp",
    "logs",
    "designs",
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
        search_index: ProjectSearchIndex | None = None,
        resource_owner: object | None = None,
    ) -> None:
        self.workspace_root = workspace_root.resolve()
        self.command_timeout_seconds = max(
            1,
            min(command_timeout_seconds, MAX_COMMAND_TIMEOUT_SECONDS),
        )
        self.max_file_read_chars = max_file_read_chars
        self._extra_protection_message = extra_protection_message
        self.search_index = search_index
        self._resource_owner = resource_owner

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

    def find_files(self, arguments: dict[str, Any]) -> str:
        """按名称或相对路径查找文件和目录，不读取文件内容。"""

        pattern = str(arguments.get("pattern") or "").strip()
        if not pattern:
            raise WorkspaceToolError("pattern 不能为空。")
        root = self.safe_path(str(arguments.get("path") or "."))
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

        indexed = None
        if self.search_index is not None:
            indexed = self.search_index.search_files(
                pattern,
                root=root,
                kind=kind,
                case_sensitive=case_sensitive,
                max_results=max_results,
            )
        if indexed is None:
            indexed = self._scan_file_names(
                pattern,
                root=root,
                kind=kind,
                case_sensitive=case_sensitive,
                max_results=max_results,
            )
        else:
            # 索引快照不包含 INDEX_EXCLUDED_NAMES 目录（构建产物/缓存/工具私有
            # 目录），开启索引后需要补充扫描这些目录，保证结果与直接扫描一致。
            extra = self._scan_index_excluded(
                pattern,
                root=root,
                kind=kind,
                case_sensitive=case_sensitive,
                max_results=max_results,
            )
            if extra:
                merged = dict(indexed)
                merged.update(extra)
                indexed = sorted(
                    merged.items(), key=lambda item: item[0].casefold()
                )[:max_results]
        lines = [f"{path}{'/' if is_dir else ''}" for path, is_dir in indexed]
        suffix = "\n... 已达到 max_results。" if len(indexed) >= max_results else ""
        return "\n".join(lines) + suffix if lines else "未找到匹配结果。"

    def grep(self, arguments: dict[str, Any]) -> str:
        """在 UTF-8 文本文件中执行 grep 风格搜索。

        pattern 默认按正则表达式解释；use_regex=false 时按精确子串匹配，
        此时索引层（FTS trigram）可用作加速。支持输出匹配行上下文
        （context_lines）、每文件匹配计数（count）、仅列出匹配文件
        （files_with_matches），以及 include/exclude 文件名 glob 过滤。
        """

        pattern = str(arguments.get("pattern") or "")
        if not pattern:
            raise WorkspaceToolError("pattern 不能为空。")

        use_regex = bool(arguments.get("use_regex", True))
        case_sensitive = bool(arguments.get("case_sensitive", False))
        if use_regex:
            try:
                regex = re.compile(pattern, 0 if case_sensitive else re.IGNORECASE)
            except re.error as exc:
                raise WorkspaceToolError(
                    f"无效的正则表达式：{exc}。可设置 use_regex=false 按精确子串匹配。"
                ) from exc
            needle = None
        else:
            regex = None
            needle = pattern if case_sensitive else pattern.casefold()

        root = self.safe_path(str(arguments.get("path") or "."))
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
        include_re = _compile_glob(_read_optional_text(arguments, "include"))
        exclude_re = _compile_glob(_read_optional_text(arguments, "exclude"))

        # 计数与仅列文件模式直接扫描：计数需要全量匹配行，索引只返回截断列表。
        if count_only:
            return self._grep_count_files(
                root,
                regex=regex,
                needle=needle,
                case_sensitive=case_sensitive,
                include_re=include_re,
                exclude_re=exclude_re,
                max_results=max_results,
            )
        if files_only:
            return self._grep_list_files(
                root,
                regex=regex,
                needle=needle,
                case_sensitive=case_sensitive,
                include_re=include_re,
                exclude_re=exclude_re,
                max_results=max_results,
            )

        # 默认输出匹配行：字面量模式优先走索引加速，正则模式直接扫描。
        truncated = False
        if not use_regex and self.search_index is not None:
            indexed = self.search_index.search_literal(
                pattern,
                root=root,
                case_sensitive=case_sensitive,
                max_results=max_results,
            )
            if indexed is not None:
                # 与 find_files 相同：补充扫描索引排除目录，保持结果一致。
                extra = self._scan_index_excluded_literal(
                    pattern,
                    root=root,
                    case_sensitive=case_sensitive,
                    max_results=max_results,
                    include_re=include_re,
                    exclude_re=exclude_re,
                )
                merged = sorted(
                    set(indexed) | set(extra),
                    key=lambda match: str(match[0]).casefold(),
                )[:max_results]
                match_lines = list(merged)
                truncated = len(merged) >= max_results
            else:
                match_lines, truncated = self._scan_grep(
                    root,
                    regex=regex,
                    needle=needle,
                    case_sensitive=case_sensitive,
                    include_re=include_re,
                    exclude_re=exclude_re,
                    max_results=max_results,
                )
        else:
            match_lines, truncated = self._scan_grep(
                root,
                regex=regex,
                needle=needle,
                case_sensitive=case_sensitive,
                include_re=include_re,
                exclude_re=exclude_re,
                max_results=max_results,
            )
        output = self._format_grep_matches(match_lines, context_lines=context_lines)
        if truncated:
            output += "\n... 已达到 max_results。"
        return output

    def _grep_count_files(
        self,
        root: Path,
        *,
        regex: re.Pattern[str] | None,
        needle: str | None,
        case_sensitive: bool,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
        max_results: int,
    ) -> str:
        """输出每个文件的匹配行数（grep -c 语义）。"""

        files = [root] if root.is_file() else self.iter_search_files(root)
        counts: list[str] = []
        truncated = False
        for file_path in files:
            if self.should_skip_path(file_path):
                continue
            if include_re is not None and not include_re.match(file_path.name):
                continue
            if exclude_re is not None and exclude_re.match(file_path.name):
                continue
            try:
                lines = self.read_text(file_path).splitlines()
            except WorkspaceToolError:
                continue
            if regex is not None:
                count = sum(1 for line in lines if regex.search(line))
            else:
                candidates = (
                    lines if case_sensitive else (line.casefold() for line in lines)
                )
                count = sum(1 for line in candidates if needle in line)
            if count:
                counts.append(f"{self.relative_path(file_path)}: {count}")
                if len(counts) >= max_results:
                    truncated = True
                    break
        result = "\n".join(counts) or "未找到匹配结果。"
        if truncated:
            result += "\n... 已达到 max_results。"
        return result

    def _grep_list_files(
        self,
        root: Path,
        *,
        regex: re.Pattern[str] | None,
        needle: str | None,
        case_sensitive: bool,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
        max_results: int,
    ) -> str:
        """只输出包含匹配的文件路径（grep -l 语义）。"""

        files = [root] if root.is_file() else self.iter_search_files(root)
        matched: list[str] = []
        truncated = False
        for file_path in files:
            if self.should_skip_path(file_path):
                continue
            if include_re is not None and not include_re.match(file_path.name):
                continue
            if exclude_re is not None and exclude_re.match(file_path.name):
                continue
            try:
                lines = self.read_text(file_path).splitlines()
            except WorkspaceToolError:
                continue
            if regex is not None:
                found = any(regex.search(line) for line in lines)
            else:
                found = any(
                    needle in (line if case_sensitive else line.casefold())
                    for line in lines
                )
            if found:
                matched.append(self.relative_path(file_path))
                if len(matched) >= max_results:
                    truncated = True
                    break
        result = "\n".join(matched) or "未找到匹配结果。"
        if truncated:
            result += "\n... 已达到 max_results。"
        return result

    def _scan_grep(
        self,
        root: Path,
        *,
        regex: re.Pattern[str] | None,
        needle: str | None,
        case_sensitive: bool,
        include_re: re.Pattern[str] | None,
        exclude_re: re.Pattern[str] | None,
        max_results: int,
    ) -> tuple[list[tuple[str, int, str]], bool]:
        """直接扫描收集匹配行；返回（匹配行列表，是否达到 max_results）。"""

        files = [root] if root.is_file() else self.iter_search_files(root)
        match_lines: list[tuple[str, int, str]] = []
        truncated = False
        for file_path in files:
            if self.should_skip_path(file_path):
                continue
            if include_re is not None and not include_re.match(file_path.name):
                continue
            if exclude_re is not None and exclude_re.match(file_path.name):
                continue
            try:
                lines = self.read_text(file_path).splitlines()
            except WorkspaceToolError:
                continue
            relative = self.relative_path(file_path)
            for line_no, line in enumerate(lines, start=1):
                if regex is not None:
                    hit = bool(regex.search(line))
                else:
                    candidate = line if case_sensitive else line.casefold()
                    hit = needle in candidate
                if hit:
                    match_lines.append((relative, line_no, line))
                    if len(match_lines) >= max_results:
                        truncated = True
                        break
            if truncated:
                break
        return match_lines, truncated

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
                    if current == line_no:
                        out.append(f"{relative}:{current}: {file_lines[index]}")
                    else:
                        out.append(f"{relative}-{current}- {file_lines[index]}")
        return "\n".join(out)

    def _scan_file_names(
        self,
        pattern: str,
        *,
        root: Path,
        kind: str,
        case_sensitive: bool,
        max_results: int,
    ) -> list[tuple[str, bool]]:
        needle = pattern if case_sensitive else pattern.casefold()
        candidates = [root] if root.is_file() else self._iter_search_entries(root)
        results: list[tuple[str, bool]] = []
        for entry in candidates:
            if self.should_skip_path(entry):
                continue
            relative = self.relative_path(entry)
            candidate = relative if case_sensitive else relative.casefold()
            name = entry.name if case_sensitive else entry.name.casefold()
            is_dir = entry.is_dir()
            if kind == "file" and is_dir:
                continue
            if kind == "directory" and not is_dir:
                continue
            if needle not in candidate and needle not in name:
                continue
            results.append((relative, is_dir))
            if len(results) >= max_results:
                break
        return results

    def _iter_search_entries(self, root: Path) -> list[Path]:
        entries: list[Path] = []
        for dirpath, dirnames, filenames in os.walk(root):
            current_dir = Path(dirpath)
            dirnames[:] = [
                dirname
                for dirname in sorted(dirnames, key=lambda value: value.lower())
                # 目录 symlink/junction 一律跳过，与索引快照的 _scan_entries
                # 行为保持一致，避免开启/关闭索引时 find_files 结果集不同。
                if not self.should_skip_path(current_dir / dirname)
                and not (current_dir / dirname).is_symlink()
            ]
            entries.extend(current_dir / dirname for dirname in dirnames)
            entries.extend(
                current_dir / filename
                for filename in sorted(filenames, key=lambda value: value.lower())
                if not self.should_skip_path(current_dir / filename)
            )
        return entries


    def _index_excluded_roots(self, root: Path) -> list[Path]:
        """root 下（含 root 自身，若它命中排除名单）的索引排除目录。

        索引快照跳过 INDEX_EXCLUDED_NAMES 目录后，工具层必须补充扫描这些
        目录，否则开启索引会丢失这些目录内的匹配结果。排除目录整棵子树
        都不进索引，因此无需继续下钻。
        """

        if root.is_file():
            return [root] if self.is_index_excluded_path(root) else []
        if self.is_index_excluded_path(root):
            return [root]
        roots: list[Path] = []
        for dirpath, dirnames, _filenames in os.walk(root):
            current_dir = Path(dirpath)
            for name in sorted(dirnames, key=lambda value: value.lower()):
                candidate = current_dir / name
                if self.is_index_excluded_path(candidate):
                    roots.append(candidate)
            dirnames[:] = [
                name for name in dirnames
                if not self.is_index_excluded_path(current_dir / name)
            ]
        return roots

    def _scan_index_excluded(
        self,
        pattern: str,
        *,
        root: Path,
        kind: str,
        case_sensitive: bool,
        max_results: int,
    ) -> list[tuple[str, bool]]:
        """补充扫描索引排除目录中的文件名匹配，与 find_files 合并。"""

        results: list[tuple[str, bool]] = []
        for extra_root in self._index_excluded_roots(root):
            results.extend(
                self._scan_file_names(
                    pattern,
                    root=extra_root,
                    kind=kind,
                    case_sensitive=case_sensitive,
                    max_results=max_results,
                )
            )
        return results

    def _scan_index_excluded_literal(
        self,
        pattern: str,
        *,
        root: Path,
        case_sensitive: bool,
        max_results: int,
        include_re: re.Pattern[str] | None = None,
        exclude_re: re.Pattern[str] | None = None,
    ) -> list[tuple[str, int, str]]:
        """补充扫描索引排除目录中的字面量匹配，与 grep 的索引结果合并。"""

        needle = pattern if case_sensitive else pattern.casefold()
        results: list[tuple[str, int, str]] = []
        for extra_root in self._index_excluded_roots(root):
            files = (
                [extra_root]
                if extra_root.is_file()
                else self.iter_search_files(extra_root)
            )
            for file_path in files:
                if self.should_skip_path(file_path):
                    continue
                if include_re is not None and not include_re.match(file_path.name):
                    continue
                if exclude_re is not None and exclude_re.match(file_path.name):
                    continue
                try:
                    lines = self.read_text(file_path).splitlines()
                except WorkspaceToolError:
                    continue
                for line_no, line in enumerate(lines, start=1):
                    candidate = line if case_sensitive else line.casefold()
                    if needle in candidate:
                        results.append(
                            (self.relative_path(file_path), line_no, line)
                        )
                        if len(results) >= max_results:
                            return results
        return results

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
        if self.search_index is not None:
            self.search_index.refresh_path(path)
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
        if self.search_index is not None:
            self.search_index.refresh_path(path)
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
        if diagnostic_value is not None and (
            not isinstance(diagnostic_value, str) or not diagnostic_value.strip()
        ):
            raise WorkspaceToolError("diagnostic_command 必须是非空字符串。")
        diagnostic_command = (
            diagnostic_value.strip() if isinstance(diagnostic_value, str) else ""
        )
        command_warning = test_output_filtering_command_warning(command, shell=shell)
        if command_warning:
            raise WorkspaceToolError(command_warning)

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
            BackgroundMonitorManager._terminate_process_tree(
                process,
                job_handle=job_handle,
            )

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
                # 与 _iter_search_entries 保持一致：目录 symlink/junction 不遍历。
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

    def should_index_skip(self, path: Path) -> bool:
        """索引层专属跳过规则：保护路径 + 索引忽略目录。

        与 :meth:`should_skip_path` 的区别是额外排除 INDEX_EXCLUDED_NAMES
        中的构建产物/缓存/工具私有目录；这些目录只是不进索引快照，
        直接扫描时仍然可以被 find_files/grep 命中。
        """

        return self.should_skip_path(path) or self.is_index_excluded_path(path)

    @staticmethod
    def is_index_excluded_path(path: Path) -> bool:
        return any(part in INDEX_EXCLUDED_NAMES for part in path.parts)

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


def _compile_glob(pattern: str) -> re.Pattern[str] | None:
    """把 include/exclude 文件名 glob 编译为正则；空模式返回 None。"""

    if not pattern:
        return None
    return re.compile(fnmatch.translate(pattern), re.IGNORECASE)


def test_output_filtering_command_warning(command: str, *, shell: str) -> str:
    """阻止测试/构建主命令用 head/tail 丢弃原始诊断输出。

    `diagnostic_command` 是单独的诊断通道，主命令必须保留完整 stdout/stderr，
    否则 pytest/unittest 的失败位置会在 Shell 层永久丢失。这里只拦截明确的
    测试或构建命令与常见裁剪器组合，不影响普通业务命令中的合法管道。
    """

    normalized = re.sub(r"\s+", " ", command.strip()).casefold()
    test_or_build = re.search(
        r"(?:pytest|unittest(?:\s+discover)?|(?:npm|pnpm|yarn)\s+(?:test|run\s+test)|"
        r"(?:cargo|go)\s+test|(?:mvn|gradle)\s+test|(?:cmake\s+--build))",
        normalized,
    )
    if test_or_build is None:
        return ""
    filter_names = (
        ("tail|head|grep|rg", "tail/head/grep/rg")
        if shell == "bash"
        else ("select-object|select-string|out-host", "Select-Object/Select-String/Out-Host")
    )
    filter_pattern = re.search(
        rf"(?:^|\|\s*)(?:{filter_names[0]})(?:\s|$)",
        normalized,
    )
    if filter_pattern is None:
        return ""
    return (
        f"不要在主命令中使用 {filter_names[1]} 裁剪测试或构建输出；"
        f"请让主命令完整执行，并将这些报告命令放到独立的 diagnostic_command。"
    )


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
