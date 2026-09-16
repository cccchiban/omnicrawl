"""跨项目工作知识库：Markdown + YAML frontmatter + 自动索引。

知识库独立于任何项目工作区，默认存放在 ``~/.OmniCrawl/knowledge/``。
它以纯 Markdown 为唯一事实源（Obsidian 兼容），通过轻量 frontmatter
提供结构化筛选，并由 ``INDEX.md`` 维护可读索引。当前规模下不引入
SQLite/向量库；文档量增长后再按需升级检索层。

安全边界：所有读写路径都必须位于知识库根目录内，禁止 ``..`` 逃逸。
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Sequence

from ..config.core.runtime import user_config_dir


DEFAULT_KNOWLEDGE_DIRNAME = "knowledge"
INDEX_FILENAME = "INDEX.md"
README_FILENAME = "README.md"
MAX_SEARCH_RESULTS = 50
MAX_LIST_RESULTS = 200

_FRONTMATTER_FIELD_ORDER = (
    "title",
    "created",
    "updated",
    "project",
    "tags",
    "type",
    "status",
)
_DEFAULT_TYPE = "note"
_DEFAULT_STATUS = "draft"
_VALID_TYPES = {"note", "meeting", "decision", "log", "research", "reference"}
_VALID_STATUSES = {"draft", "done", "archived"}


class KnowledgeBaseError(RuntimeError):
    """知识库参数校验或文件操作失败时抛出。"""


@dataclass(frozen=True)
class KnowledgeNoteMeta:
    """一篇笔记的结构化元数据（来自 frontmatter + 相对路径）。"""

    rel_path: str
    title: str
    created: str
    updated: str
    project: str
    type: str
    status: str
    tags: tuple[str, ...]


@dataclass(frozen=True)
class KnowledgeSearchResult:
    """一次搜索命中的笔记摘要。"""

    rel_path: str
    title: str
    project: str
    type: str
    status: str
    tags: tuple[str, ...]
    snippet: str
    score: int


_KNOWLEDGE_README = """# 工作知识库（Work Knowledge Base）

独立于任何项目代码库，用于存储工作记录、其他项目资料、会议纪要、
决策与研究笔记。由 OmniCrawl 的 `kb_*` 工具统一读写。

## 目录结构

- `projects/<项目名>/`：按项目归档
- `topics/<主题>/`：按领域归档
- `daily/YYYY/MM/`：工作日志
- `attachments/`：图片、PDF、表格等附件

## 命名规范

- 文件名建议：`YYYY-MM-DD-简短标题.md`
- 文件必须为 UTF-8 Markdown，并带 YAML frontmatter：

```markdown
---
title: 笔记标题
created: YYYY-MM-DD
updated: YYYY-MM-DD
project: 项目名（可空）
tags: [标签1, 标签2]
type: note|meeting|decision|log|research|reference
status: draft|done|archived
---
正文...
```

- `INDEX.md` 由系统自动维护，请勿手工编辑。
- 禁止写入 API 密钥、令牌、Cookie、密码等敏感信息。
"""


def _default_knowledge_root() -> Path:
    """返回默认知识库根目录。"""

    return user_config_dir() / DEFAULT_KNOWLEDGE_DIRNAME


def _today() -> str:
    return datetime.now().astimezone().date().isoformat()


def _unquote_yaml(value: str) -> str:
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        inner = value[1:-1]
        if value[0] == "'":
            return inner.replace("''", "'")
        return inner.replace('\\"', '"')
    return value


def _parse_scalar(value: str) -> Any:
    value = value.strip()
    if value.startswith("[") and value.endswith("]"):
        inner = value[1:-1].strip()
        if not inner:
            return []
        return [
            _unquote_yaml(item)
            for item in inner.split(",")
            if item.strip()
        ]
    return _unquote_yaml(value)


def _parse_frontmatter(
    text: str,
) -> tuple[dict[str, Any], str, dict[str, Any]]:
    """解析可选 YAML frontmatter。

    返回 ``(meta, body, extra)``：``meta`` 是已知字段，
    ``extra`` 是用户自定义的未知字段（写回时保留）。
    """

    if not text.startswith("---\n"):
        return {}, text, {}
    lines = text.splitlines()
    end_index = None
    for index in range(1, len(lines)):
        if lines[index].strip() == "---":
            end_index = index
            break
    if end_index is None:
        return {}, text, {}
    meta: dict[str, Any] = {}
    extra: dict[str, Any] = {}
    for line in lines[1:end_index]:
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or ":" not in line:
            continue
        key, _, value = line.partition(":")
        key = key.strip()
        if not key:
            continue
        parsed = _parse_scalar(value)
        if key in _FRONTMATTER_FIELD_ORDER:
            meta[key] = parsed
        else:
            extra[key] = parsed
    body = "\n".join(lines[end_index + 1 :])
    return meta, body, extra


def _yaml_quote(value: str) -> str:
    if value == "":
        return '""'
    needs_quote = (
        value != value.strip()
        or any(character in value for character in ":,[]{}#&*!|>'\"%@`")
        or value in {"true", "false", "null", "~", "yes", "no", "on", "off"}
        or value.startswith(("-", "?", " "))
    )
    if needs_quote:
        return "'" + value.replace("'", "''") + "'"
    return value


def _format_scalar(value: Any) -> str:
    if isinstance(value, (list, tuple)):
        items = [_yaml_quote(str(item)) for item in value]
        return "[" + ", ".join(items) + "]"
    return _yaml_quote(str(value))


def _format_frontmatter(meta: dict[str, Any], extra: dict[str, Any]) -> str:
    lines = ["---"]
    for key in _FRONTMATTER_FIELD_ORDER:
        lines.append(f"{key}: {_format_scalar(meta.get(key, ''))}")
    for key, value in extra.items():
        lines.append(f"{key}: {_format_scalar(value)}")
    lines.append("---")
    return "\n".join(lines)


def _make_snippet(body: str, terms: Sequence[str], radius: int = 80) -> str:
    if not body.strip():
        return ""
    lowered = body.casefold()
    first_pos = -1
    for term in terms:
        pos = lowered.find(term)
        if pos >= 0 and (first_pos < 0 or pos < first_pos):
            first_pos = pos
    if first_pos < 0:
        return body[: radius * 2].strip()
    start = max(0, first_pos - radius)
    end = min(len(body), first_pos + radius * 2)
    prefix = "…" if start > 0 else ""
    suffix = "…" if end < len(body) else ""
    return prefix + body[start:end].strip() + suffix


class KnowledgeBase:
    """Markdown 工作知识库的读写、搜索与索引实现。

    ``root`` 可显式传入（主要用于测试）；默认使用
    ``~/.OmniCrawl/knowledge/``。
    """

    def __init__(self, root: Path | None = None) -> None:
        self._root = Path(root).expanduser() if root is not None else _default_knowledge_root()
        # 扫描缓存：按 (mtime_ns, size) 失效，未变化文件不再重复读取与解析。
        self._notes_cache: dict[str, tuple[int, int, KnowledgeNoteMeta, str]] = {}

    @property
    def root(self) -> Path:
        return self._root

    def ensure_layout(self) -> None:
        """创建目录骨架、README 与自动索引。"""

        for name in ("projects", "topics", "daily", "attachments"):
            (self.root / name).mkdir(parents=True, exist_ok=True)
        readme = self.root / README_FILENAME
        if not readme.exists():
            readme.write_text(_KNOWLEDGE_README, encoding="utf-8")
        self.refresh_index()

    def refresh_index(self) -> None:
        """重建 INDEX.md（自动维护，不保留手工编辑内容）。"""

        self.root.mkdir(parents=True, exist_ok=True)
        notes = self.list_notes()
        lines = [
            "# Knowledge Base Index",
            "",
            "本文件由知识库自动维护，请勿手工编辑。",
            "",
            f"共 {len(notes)} 篇笔记。",
            "",
        ]
        for note in notes:
            tags = ", ".join(note.tags) or "-"
            lines.append(
                f"- [{note.title}]({note.rel_path}) — `{note.type}` | "
                f"项目：{note.project or '-'} | 状态：{note.status} | "
                f"标签：{tags} | 更新：{note.updated}"
            )
        lines.append("")
        text = "\n".join(lines)
        tmp = self.root / ".INDEX.md.tmp"
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, self.root / INDEX_FILENAME)

    def _safe_join(self, rel_path: str, *, default_suffix: str = "") -> Path:
        raw = str(rel_path or "").strip().replace("\\", "/")
        while raw.startswith("/"):
            raw = raw[1:]
        parts = [part for part in raw.split("/") if part not in ("", ".")]
        if any(part == ".." for part in parts):
            raise KnowledgeBaseError("知识库路径不允许包含 ..。")
        if not parts:
            return self.root.resolve()
        last = parts[-1]
        if default_suffix and "." not in last:
            parts[-1] = last + default_suffix
        candidate = self.root.joinpath(*parts)
        try:
            resolved = candidate.resolve()
        except OSError as exc:
            raise KnowledgeBaseError(f"知识库路径不可用：{raw}。") from exc
        root = self.root.resolve()
        if resolved != root and not resolved.is_relative_to(root):
            raise KnowledgeBaseError("知识库路径超出根目录。")
        return resolved

    @staticmethod
    def _display_path(target: Path, root: Path) -> str:
        try:
            return target.resolve().relative_to(root.resolve()).as_posix()
        except (OSError, ValueError):
            return target.name

    def _meta_from_dict(self, rel_path: str, meta: dict[str, Any]) -> KnowledgeNoteMeta:
        title = str(meta.get("title") or Path(rel_path).stem)
        tags = meta.get("tags") or []
        return KnowledgeNoteMeta(
            rel_path=rel_path,
            title=title,
            created=str(meta.get("created") or ""),
            updated=str(meta.get("updated") or ""),
            project=str(meta.get("project") or ""),
            type=str(meta.get("type") or _DEFAULT_TYPE),
            status=str(meta.get("status") or _DEFAULT_STATUS),
            tags=tuple(str(tag) for tag in tags if str(tag).strip()),
        )

    def _meta_for(self, target: Path) -> KnowledgeNoteMeta:
        meta, _body, _extra = _parse_frontmatter(target.read_text(encoding="utf-8"))
        rel = target.resolve().relative_to(self.root.resolve()).as_posix()
        return self._meta_from_dict(rel, meta)

    def list_notes(self) -> list[KnowledgeNoteMeta]:
        """扫描知识库全部笔记（跳过 INDEX.md/README.md 与隐藏目录）。"""

        notes = [meta for meta, _body in self._scan_notes()]
        return sorted(notes, key=lambda note: note.rel_path.lower())

    def _scan_notes(self) -> list[tuple[KnowledgeNoteMeta, str]]:
        """单次遍历全部笔记并返回 (元数据, 正文)。

        用 ``os.scandir`` 遍历替代逐文件 ``Path.resolve``（Windows 上后者是
        扫描的主要成本）；未变化文件按 (mtime_ns, size) 直接复用缓存，不再
        读取与解析。不跟随目录符号链接，符号链接文件仍校验落在根目录内。
        """

        if not self.root.is_dir():
            self._notes_cache.clear()
            return []
        root = self.root.resolve()
        root_str = str(root)
        documents: list[tuple[KnowledgeNoteMeta, str]] = []
        seen: set[str] = set()
        stack = [root_str]
        while stack:
            current = stack.pop()
            try:
                with os.scandir(current) as iterator:
                    for entry in iterator:
                        name = entry.name
                        if name.startswith("."):
                            continue
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(entry.path)
                            continue
                        if not name.endswith(".md"):
                            continue
                        if name in {INDEX_FILENAME, README_FILENAME}:
                            continue
                        path_str = entry.path
                        seen.add(path_str)
                        is_link = entry.is_symlink()
                        try:
                            resolved = Path(path_str).resolve() if is_link else Path(path_str)
                            if is_link and resolved.name in {INDEX_FILENAME, README_FILENAME}:
                                continue
                            rel_path = resolved.relative_to(root).as_posix()
                            file_stat = resolved.stat()
                        except (OSError, ValueError):
                            continue
                        cached = self._notes_cache.get(path_str)
                        if (
                            not is_link
                            and cached is not None
                            and cached[0] == file_stat.st_mtime_ns
                            and cached[1] == file_stat.st_size
                        ):
                            documents.append((cached[2], cached[3]))
                            continue
                        try:
                            text = resolved.read_text(encoding="utf-8")
                        except (OSError, UnicodeDecodeError):
                            continue
                        meta, body, _extra = _parse_frontmatter(text)
                        note_meta = self._meta_from_dict(rel_path, meta)
                        if not is_link:
                            self._notes_cache[path_str] = (
                                file_stat.st_mtime_ns,
                                file_stat.st_size,
                                note_meta,
                                body,
                            )
                        documents.append((note_meta, body))
            except OSError:
                continue
        if self._notes_cache:
            self._notes_cache = {
                key: value for key, value in self._notes_cache.items() if key in seen
            }
        return documents

    def read(self, rel_path: str) -> str:
        target = self._safe_join(rel_path, default_suffix=".md")
        if not target.is_file():
            raise KnowledgeBaseError(
                f"知识库笔记不存在：{self._display_path(target, self.root)}。"
            )
        try:
            return target.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            raise KnowledgeBaseError("知识库笔记必须是 UTF-8 文本。") from exc
        except OSError as exc:
            raise KnowledgeBaseError(f"读取知识库笔记失败：{exc}。") from exc

    def write(
        self,
        rel_path: str,
        content: str,
        *,
        title: str | None = None,
        project: str | None = None,
        tags: Sequence[str] | None = None,
        note_type: str | None = None,
        status: str | None = None,
        mode: str = "overwrite",
    ) -> KnowledgeNoteMeta:
        """新建、覆盖或追加一篇笔记。

        - ``create``：仅新建，已存在时报错；
        - ``overwrite``：替换正文，保留已有 frontmatter 字段并合并传入参数；
        - ``append``：在正文末尾追加内容，只更新 ``updated``。
        """

        mode = (mode or "overwrite").strip().lower()
        if mode not in {"create", "overwrite", "append"}:
            raise KnowledgeBaseError("mode 必须是 create、overwrite 或 append。")
        if not isinstance(content, str):
            raise KnowledgeBaseError("content 必须是字符串。")

        target = self._safe_join(rel_path, default_suffix=".md")
        existed = target.is_file()
        if mode == "create" and existed:
            raise KnowledgeBaseError(
                f"笔记已存在：{self._display_path(target, self.root)}。"
            )

        old_meta: dict[str, Any] = {}
        extra: dict[str, Any] = {}
        old_body = ""
        if existed:
            old_meta, old_body, extra = _parse_frontmatter(
                target.read_text(encoding="utf-8")
            )
        today = _today()
        merged = {
            "title": (
                title.strip() if isinstance(title, str) and title.strip()
                else old_meta.get("title") or target.stem
            ),
            "created": str(old_meta.get("created") or today),
            "updated": today,
            "project": (
                project.strip() if isinstance(project, str)
                else old_meta.get("project") or ""
            ),
            "tags": (
                [str(tag).strip() for tag in tags if str(tag).strip()]
                if tags is not None
                else list(old_meta.get("tags") or [])
            ),
            "type": (
                note_type.strip().lower() if isinstance(note_type, str) and note_type.strip()
                else old_meta.get("type") or _DEFAULT_TYPE
            ),
            "status": (
                status.strip().lower() if isinstance(status, str) and status.strip()
                else old_meta.get("status") or _DEFAULT_STATUS
            ),
        }
        if merged["type"] not in _VALID_TYPES:
            raise KnowledgeBaseError(
                f"type 必须是 {', '.join(sorted(_VALID_TYPES))} 之一。"
            )
        if merged["status"] not in _VALID_STATUSES:
            raise KnowledgeBaseError(
                f"status 必须是 {', '.join(sorted(_VALID_STATUSES))} 之一。"
            )

        if mode == "append":
            body = (
                old_body.rstrip() + "\n\n" + content.strip()
                if old_body.strip()
                else content.strip()
            )
        else:
            body = content.strip()

        target.parent.mkdir(parents=True, exist_ok=True)
        parts = [_format_frontmatter(merged, extra)]
        if body:
            parts.append(body)
        text = "\n\n".join(parts) + "\n"
        tmp = target.with_name(target.name + ".tmp")
        tmp.write_text(text, encoding="utf-8")
        os.replace(tmp, target)
        self.refresh_index()
        return self._meta_for(target)

    def append(self, rel_path: str, content: str) -> KnowledgeNoteMeta:
        """向已有笔记追加正文；不修改 frontmatter 其他字段。"""

        return self.write(rel_path, content, mode="append")

    def search(
        self,
        query: str,
        *,
        project: str | None = None,
        tags: Sequence[str] | None = None,
        note_type: str | None = None,
        status: str | None = None,
        max_results: int = 10,
    ) -> list[KnowledgeSearchResult]:
        """按关键词与结构化字段搜索笔记，返回摘要（先摘要后全文）。"""

        query = str(query or "").strip()
        if not query:
            raise KnowledgeBaseError("query 不能为空。")
        terms = [term.casefold() for term in re.split(r"\s+", query) if term.strip()]
        if not terms:
            raise KnowledgeBaseError("query 不能为空。")

        project_filter = str(project).strip().casefold() if project else None
        type_filter = str(note_type).strip().casefold() if note_type else None
        status_filter = str(status).strip().casefold() if status else None
        tag_filters = [
            str(tag).strip().casefold()
            for tag in (tags or [])
            if isinstance(tag, str) and tag.strip()
        ]

        results: list[KnowledgeSearchResult] = []
        for note, body in self._scan_notes():
            if project_filter and note.project.casefold() != project_filter:
                continue
            if type_filter and note.type.casefold() != type_filter:
                continue
            if status_filter and note.status.casefold() != status_filter:
                continue
            if tag_filters:
                note_tags = {tag.casefold() for tag in note.tags}
                if not any(tag in note_tags for tag in tag_filters):
                    continue
            title_blob = " ".join(
                [note.title, note.project, note.type, note.status, *note.tags]
            ).casefold()
            haystack = " ".join([title_blob, body]).casefold()
            if not all(term in haystack for term in terms):
                continue
            score = 3 if all(term in title_blob for term in terms) else 1
            results.append(
                KnowledgeSearchResult(
                    rel_path=note.rel_path,
                    title=note.title,
                    project=note.project,
                    type=note.type,
                    status=note.status,
                    tags=note.tags,
                    snippet=_make_snippet(body, terms),
                    score=score,
                )
            )
        results.sort(key=lambda result: (-result.score, result.rel_path.lower()))
        return results[: max(1, min(int(max_results), MAX_SEARCH_RESULTS))]

    def list_entries(
        self,
        *,
        rel_path: str | None = None,
        project: str | None = None,
        tags: Sequence[str] | None = None,
        note_type: str | None = None,
        status: str | None = None,
        max_results: int = 100,
    ) -> list[KnowledgeNoteMeta]:
        """浏览知识库目录或按字段筛选笔记。"""

        notes = self.list_notes()
        raw_path = str(rel_path or "").strip()
        if raw_path:
            target = self._safe_join(raw_path)
            target_rel = target.resolve().relative_to(self.root.resolve()).as_posix()
            if target.is_dir():
                prefix = target_rel + "/" if target_rel else ""
                notes = [
                    note for note in notes if note.rel_path.startswith(prefix)
                ]
            elif target.is_file():
                notes = [note for note in notes if note.rel_path == target_rel]
            else:
                raise KnowledgeBaseError(f"知识库路径不存在：{raw_path}。")

        project_filter = str(project).strip().casefold() if project else None
        type_filter = str(note_type).strip().casefold() if note_type else None
        status_filter = str(status).strip().casefold() if status else None
        tag_filters = [
            str(tag).strip().casefold()
            for tag in (tags or [])
            if isinstance(tag, str) and tag.strip()
        ]
        filtered: list[KnowledgeNoteMeta] = []
        for note in notes:
            if project_filter and note.project.casefold() != project_filter:
                continue
            if type_filter and note.type.casefold() != type_filter:
                continue
            if status_filter and note.status.casefold() != status_filter:
                continue
            if tag_filters:
                note_tags = {tag.casefold() for tag in note.tags}
                if not any(tag in note_tags for tag in tag_filters):
                    continue
            filtered.append(note)
        return filtered[: max(1, min(int(max_results), MAX_LIST_RESULTS))]
